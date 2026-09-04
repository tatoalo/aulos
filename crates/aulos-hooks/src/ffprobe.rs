//! The ffmpeg/ffprobe plumbing the `audio_sync` hook needs (DESIGN §13.3).
//!
//! Both tools are spawned through [`aulos_provider::proc`], so they get their own process group,
//! `nice(5)`, a bounded stderr ring and a `SIGTERM`→`SIGKILL` kill on timeout or shutdown — the
//! same treatment every other child process in the server gets, and the reason a wedged ffmpeg
//! cannot outlive the shutdown grace.
//!
//! There is no configuration knob for either path (DESIGN §17.3 has none), so the default is the
//! bare tool name resolved through `PATH`; [`MediaTools`] exists so a test can point the hook at a
//! script instead of installing ffmpeg.

use std::path::PathBuf;
use std::time::Duration;

use aulos_provider::proc::{Child, ProcError, SpawnSpec};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::HookError;

/// The canonical tool name of the encoder, as [`HookError::ToolMissing`] reports it.
pub const FFMPEG: &str = "ffmpeg";
/// The canonical tool name of the prober.
pub const FFPROBE: &str = "ffprobe";

/// The ffprobe timeout of DESIGN §13.3 steps 2–3.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the two media tools live.
///
/// The default is `PATH` lookup, which is what the image provides (DESIGN §18.1). A test replaces
/// them with scripts so the whole `audio_sync` suite runs without ffmpeg installed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MediaTools {
    /// The encoder.
    pub ffmpeg: PathBuf,
    /// The prober.
    pub ffprobe: PathBuf,
}

impl Default for MediaTools {
    fn default() -> Self {
        Self {
            ffmpeg: PathBuf::from(FFMPEG),
            ffprobe: PathBuf::from(FFPROBE),
        }
    }
}

/// What a finished child produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Captured {
    /// Whether the process exited 0.
    pub success: bool,
    /// The exit code, when the process exited normally.
    pub code: Option<i32>,
    /// Everything the child wrote to stdout, newline-joined.
    pub stdout: String,
    /// The tail of stderr, ANSI-stripped by the ring.
    pub stderr: String,
}

/// Runs a child to completion, feeding every stdout line to `on_line`.
///
/// Bounded by `timeout` and by `cancel`; either one kills the whole process group. The stdout
/// drain runs concurrently with the wait, which is what makes `-progress pipe:1` usable and what
/// stops a chatty child from filling its pipe and deadlocking.
///
/// # Errors
/// [`HookError::ToolMissing`] when the binary is absent, [`HookError::Timeout`] on the deadline,
/// [`HookError::Canceled`] on shutdown, [`HookError::Tool`] for any other spawn or I/O failure.
pub async fn capture(
    spec: &SpawnSpec,
    timeout: Duration,
    cancel: &CancellationToken,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<Captured, HookError> {
    let tool = spec.tool_name();
    let mut child = Child::spawn(spec).map_err(|e| match e {
        ProcError::NotFound { tool, .. } => HookError::ToolMissing(tool),
        other => HookError::Tool {
            tool,
            detail: other.to_string().into(),
        },
    })?;

    let mut stdout = String::new();
    let outcome = {
        let drive = async {
            if let Some(lines) = child.stdout_lines() {
                while let Some(line) = lines.next_line().await? {
                    on_line(&line);
                    stdout.push_str(&line);
                    stdout.push('\n');
                }
            }
            child.wait().await
        };
        tokio::select! {
            () = cancel.cancelled() => Err(HookError::Canceled),
            r = tokio::time::timeout(timeout, drive) => match r {
                Err(_elapsed) => Err(HookError::Timeout(timeout)),
                Ok(Ok(status)) => Ok(status),
                Ok(Err(e)) => Err(HookError::Tool { tool, detail: e.to_string().into() }),
            },
        }
    };

    match outcome {
        Ok(status) => {
            let stderr = if status.success() {
                child.stderr().tail(STDERR_TAIL)
            } else {
                settled_stderr(&child).await
            };
            Ok(Captured {
                success: status.success(),
                code: status.code(),
                stdout,
                stderr,
            })
        }
        Err(e) => {
            // The deadline and the cancel path both leave a live process group behind.
            child.kill_group().await;
            Err(e)
        }
    }
}

/// How many bytes of stderr a failure reports.
pub const STDERR_TAIL: usize = 2048;

/// The stderr tail, once the drain has caught up.
///
/// `aulos_provider::proc::Child::wait` reaps the child without joining the stderr drain task, so
/// reading the ring the instant `wait` returns is a race — the tail is empty about one run in
/// twenty. This gives the drain a bounded chance to finish, and is only on the failure path,
/// where the tail is the whole point of the error message.
async fn settled_stderr(child: &Child) -> String {
    for _ in 0..50 {
        let tail = child.stderr().tail(STDERR_TAIL);
        if !tail.is_empty() {
            return tail;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    String::new()
}

/// `ffprobe -v error -select_streams v -show_entries stream=codec_type -of json` (DESIGN §13.3
/// step 2): whether the file has at least one video stream.
///
/// A probe that fails, times out or prints something unparseable answers `false`, exactly as
/// legacy's `has_video_stream` did — an unprobeable file is not one to re-encode.
pub async fn has_video_stream(
    tools: &MediaTools,
    file: &std::path::Path,
    cancel: &CancellationToken,
) -> bool {
    let spec = SpawnSpec::new(FFPROBE, &tools.ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "json",
        ])
        .arg(file)
        .stdout_piped(true);
    let captured = match capture(&spec, PROBE_TIMEOUT, cancel, &mut |_| {}).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, path = %file.display(), "ffprobe failed");
            return false;
        }
    };
    let Ok(json) = serde_json::from_str::<Value>(&captured.stdout) else {
        tracing::warn!(path = %file.display(), "ffprobe printed no usable JSON");
        return false;
    };
    json.get("streams")
        .and_then(Value::as_array)
        .is_some_and(|s| !s.is_empty())
}

/// `ffprobe -v error -show_entries format=duration -of json` (DESIGN §13.3 step 3): the container
/// duration in seconds, or `None` when it cannot be determined.
pub async fn duration_secs(
    tools: &MediaTools,
    file: &std::path::Path,
    cancel: &CancellationToken,
) -> Option<f64> {
    let spec = SpawnSpec::new(FFPROBE, &tools.ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(file)
        .stdout_piped(true);
    let captured = match capture(&spec, PROBE_TIMEOUT, cancel, &mut |_| {}).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, path = %file.display(), "could not determine duration");
            return None;
        }
    };
    let json = serde_json::from_str::<Value>(&captured.stdout).ok()?;
    // Legacy accepted `float(probe["format"]["duration"])`, and ffprobe prints it as a string.
    let raw = json.get("format")?.get("duration")?;
    let secs = match raw {
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        other => other.as_f64()?,
    };
    (secs.is_finite() && secs > 0.0).then_some(secs)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_binary_is_tool_missing() {
        let spec = SpawnSpec::new("ffprobe", "/nonexistent/aulos-ffprobe").stdout_piped(true);
        let e = capture(
            &spec,
            Duration::from_secs(5),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .expect_err("a missing binary cannot spawn");
        assert!(matches!(e, HookError::ToolMissing("ffprobe")), "{e:?}");
    }

    #[tokio::test]
    async fn stdout_lines_are_streamed_and_collected() {
        let spec = SpawnSpec::new("ffprobe", "/bin/sh")
            .args(["-c", "printf 'a\\nb\\n'"])
            .stdout_piped(true);
        let mut seen = Vec::new();
        let c = capture(
            &spec,
            Duration::from_secs(5),
            &CancellationToken::new(),
            &mut |l| seen.push(l.to_owned()),
        )
        .await
        .expect("sh runs");
        assert!(c.success);
        assert_eq!(c.code, Some(0));
        assert_eq!(seen, ["a", "b"]);
        assert_eq!(c.stdout, "a\nb\n");
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_reported_with_its_stderr() {
        let spec = SpawnSpec::new("ffmpeg", "/bin/sh")
            .args(["-c", "echo boom >&2; exit 7"])
            .stdout_piped(true);
        let c = capture(
            &spec,
            Duration::from_secs(5),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .expect("sh runs");
        assert!(!c.success);
        assert_eq!(c.code, Some(7));
        assert!(c.stderr.contains("boom"), "{:?}", c.stderr);
    }

    #[tokio::test]
    async fn a_deadline_kills_the_process_group() {
        let spec = SpawnSpec::new("ffmpeg", "/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout_piped(true);
        let e = capture(
            &spec,
            Duration::from_millis(150),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .await
        .expect_err("the deadline fires");
        assert!(matches!(e, HookError::Timeout(_)), "{e:?}");
    }

    #[tokio::test]
    async fn a_cancel_stops_the_child() {
        let spec = SpawnSpec::new("ffmpeg", "/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout_piped(true);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });
        let e = capture(&spec, Duration::from_secs(30), &cancel, &mut |_| {})
            .await
            .expect_err("cancel wins");
        assert!(matches!(e, HookError::Canceled), "{e:?}");
    }

    #[tokio::test]
    async fn an_unprobeable_file_has_no_video_stream_and_no_duration() {
        let tools = MediaTools {
            ffmpeg: PathBuf::from("/nonexistent/aulos-ffmpeg"),
            ffprobe: PathBuf::from("/nonexistent/aulos-ffprobe"),
        };
        let cancel = CancellationToken::new();
        let path = std::path::Path::new("/tmp/does-not-matter.mp4");
        assert!(!has_video_stream(&tools, path, &cancel).await);
        assert_eq!(duration_secs(&tools, path, &cancel).await, None);
    }
}
