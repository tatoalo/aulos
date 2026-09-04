//! The ffmpeg engine (DESIGN §10.5), ported from legacy `app/ytdl.py:608-686`.
//!
//! ffmpeg reads the HLS playlist itself, so this path is one process and no segment directory. It
//! is the `SC_USE_FFMPEG=true` engine and, more importantly, the automatic retry when
//! `N_m3u8DL-RE` fails — the retry that turns "the download broke" into "the download was a bit
//! slower".
//!
//! Two details are easy to get wrong:
//!
//! - **The headers are one CRLF-joined blob.** ffmpeg's `-headers` takes a single string
//!   containing the whole header block, not repeated flags, and vixcloud rejects a request without
//!   the `Referer`/`Origin` pair.
//! - **Progress needs a duration, and the duration needs the headers too.** `total_size` alone
//!   cannot produce a percent for a stream of unknown length, so `ffprobe` is asked for
//!   `format=duration` first; a failure is tolerated and the download then reports bytes without a
//!   percent, exactly as legacy did.

use std::path::Path;
use std::time::Duration;

use aulos_provider::outcome::Outcome;
use aulos_provider::proc::{Child, ProcError, SpawnSpec};
use aulos_provider::provider::{DownloadCtx, ProviderError};
use aulos_provider::sink::ProgressSink;
use tokio::time::Instant;

use crate::engines::{EngineCfg, OutputNames, cleanup_partial, settled_tail};
use crate::jit::StreamTarget;
use crate::progress::{FfmpegProgress, MIN_PROGRESS_INTERVAL};

/// The tool's name, as the [`ProviderError::ToolMissing`] label.
pub const TOOL: &str = "ffmpeg";
/// `ffprobe`'s label, so a missing probe is distinguishable from a missing ffmpeg.
pub const PROBE_TOOL: &str = "ffprobe";

/// The `ffprobe` timeout (legacy `timeout=30`).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The header block ffmpeg wants: one string, CRLF-terminated lines, cookies last
/// (legacy `app/ytdl.py:610-615`).
///
/// The three names are emitted unconditionally — with an empty value when the target has none —
/// because that is what legacy sent and vixcloud has never objected.
#[must_use]
pub fn header_blob(t: &StreamTarget) -> String {
    let mut s = String::new();
    for name in ["User-Agent", "Referer", "Origin"] {
        s.push_str(name);
        s.push_str(": ");
        s.push_str(t.header(name).unwrap_or_default());
        s.push_str("\r\n");
    }
    if !t.cookies.is_empty() {
        s.push_str("Cookie: ");
        s.push_str(&t.cookies);
        s.push_str("\r\n");
    }
    s
}

/// The `ffprobe` argv (legacy `app/ytdl.py:619-627`).
#[must_use]
pub fn probe_spec(cfg: &EngineCfg, headers: &str, m3u8: &str) -> SpawnSpec {
    SpawnSpec::new(PROBE_TOOL, &cfg.ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-headers")
        .arg(headers)
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(m3u8)
        .stdout_piped(true)
        .kill_grace(cfg.kill_grace)
}

/// The download argv (legacy `app/ytdl.py:632-641`).
///
/// `-c copy` never re-encodes, `aac_adtstoasc` rewrites the ADTS audio headers HLS carries into
/// the ASC form mp4 needs, and `-progress pipe:1` is what makes the run observable at all.
#[must_use]
pub fn ffmpeg_spec(cfg: &EngineCfg, headers: &str, m3u8: &str, out: &Path) -> SpawnSpec {
    SpawnSpec::new(TOOL, &cfg.ffmpeg)
        .arg("-y")
        .arg("-headers")
        .arg(headers)
        .arg("-i")
        .arg(m3u8)
        .arg("-c")
        .arg("copy")
        .arg("-bsf:a")
        .arg("aac_adtstoasc")
        .arg("-progress")
        .arg("pipe:1")
        .arg(out)
        .stdout_piped(true)
        .kill_grace(cfg.kill_grace)
}

/// Probes the stream's duration in seconds. `None` when the probe failed, timed out or printed
/// something unparseable — all of which legacy tolerated with a warning.
pub async fn probe_duration(cfg: &EngineCfg, headers: &str, m3u8: &str) -> Option<f64> {
    let spec = probe_spec(cfg, headers, m3u8);
    let mut child = match Child::spawn(&spec) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "could not get duration");
            return None;
        }
    };
    let first = async {
        let line = match child.stdout_lines() {
            Some(l) => l.next_line().await.ok().flatten(),
            None => None,
        };
        let status = child.wait().await;
        (line, status)
    };
    let probed = tokio::time::timeout(PROBE_TIMEOUT, first).await;
    match probed {
        Ok((line, Ok(status))) if status.success() => {
            let d = line?.trim().parse::<f64>().ok()?;
            (d.is_finite() && d > 0.0).then_some(d)
        }
        Ok((_, Ok(status))) => {
            tracing::warn!(code = ?status.code(), "ffprobe could not read the duration");
            None
        }
        Ok((_, Err(e))) => {
            tracing::warn!(error = %e, "could not get duration");
            None
        }
        Err(_) => {
            tracing::warn!(
                timeout_s = PROBE_TIMEOUT.as_secs(),
                "ffprobe timed out; the download will report bytes without a percent"
            );
            child.kill_group().await;
            None
        }
    }
}

/// What one iteration of the read loop observed. See [`crate::nm3u8dl`] for why this exists.
enum Step {
    /// `ctx.cancel` fired.
    Cancel,
    /// One `key=value` line, or `None` at end of stream.
    Line(Option<String>),
    /// Reading the progress stream failed.
    Read(ProcError),
    /// The child exited.
    Exit(std::process::ExitStatus),
    /// Waiting on the child failed.
    Wait(ProcError),
}

/// Runs ffmpeg against the playlist and reports progress from `-progress pipe:1`.
///
/// # Errors
/// [`ProviderError::Canceled`] on cancel, [`ProviderError::ToolMissing`] when ffmpeg is absent,
/// and [`ProviderError::Other`] carrying legacy's `FFmpeg failed with code …` message on a
/// non-zero exit or a missing output.
pub async fn download_ffmpeg(
    cfg: &EngineCfg,
    ctx: &DownloadCtx<'_>,
    t: &StreamTarget,
    names: &OutputNames,
    tmp: &Path,
    sink: &ProgressSink,
) -> Result<Outcome, ProviderError> {
    let headers = header_blob(t);
    let m3u8 = t.m3u8.as_str();
    tracing::info!(item = %ctx.item_id, "running ffmpeg for a StreamingCommunity download");

    let duration = tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => return Err(ProviderError::Canceled),
        d = probe_duration(cfg, &headers, m3u8) => d,
    };

    let spec = ffmpeg_spec(cfg, &headers, m3u8, &names.out_path);
    let mut child = Child::spawn(&spec)?;
    let mut stdout = child.take_stdout();
    let mut progress = FfmpegProgress::new(duration, Instant::now(), MIN_PROGRESS_INTERVAL);

    let status = loop {
        let step = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Step::Cancel,
            r = next_line(stdout.as_mut()), if stdout.is_some() => match r {
                Ok(l) => Step::Line(l),
                Err(e) => Step::Read(e),
            },
            r = child.wait() => match r {
                Ok(s) => Step::Exit(s),
                Err(e) => Step::Wait(e),
            },
        };
        match step {
            Step::Cancel => {
                child.kill_group().await;
                cleanup_partial(names, tmp).await;
                return Err(ProviderError::Canceled);
            }
            Step::Line(Some(line)) => {
                if let Some(frame) = progress.feed(&line, Instant::now()) {
                    sink.progress(frame);
                }
            }
            Step::Line(None) => stdout = None,
            Step::Read(e) => {
                tracing::debug!(error = %e, "reading the ffmpeg progress stream failed");
                stdout = None;
            }
            Step::Exit(status) => break status,
            Step::Wait(e) => {
                child.kill_group().await;
                return Err(e.into());
            }
        }
    };

    let size = tokio::fs::metadata(&names.out_path)
        .await
        .ok()
        .filter(|m| m.is_file() && m.len() > 0)
        .map(|m| m.len());
    match (status.success(), size) {
        (true, Some(size)) => Ok(Outcome::file(names.rel.clone(), size)),
        _ => {
            let code = status.code().unwrap_or(1);
            let tail = settled_tail(&mut child).await;
            tracing::error!(code, stderr = %tail, "FFmpeg failed");
            let mut msg = format!("FFmpeg failed with code {code}");
            if !tail.is_empty() {
                msg.push_str(": ");
                msg.push_str(&tail);
            }
            Err(ProviderError::Other(msg))
        }
    }
}

/// The next line of the progress stream, `None` at end of stream.
async fn next_line(
    lines: Option<&mut aulos_provider::proc::Lines<tokio::process::ChildStdout>>,
) -> Result<Option<String>, ProcError> {
    match lines {
        Some(l) => l.next_line().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use aulos_core::paths::RelPath;

    use super::*;
    use crate::testing::{EngineFixture, fixture_bin};

    fn target(cookies: &str) -> StreamTarget {
        StreamTarget {
            m3u8: url::Url::parse("https://vixcloud.co/playlist/1?token=t").expect("url"),
            headers: vec![
                (
                    "Referer".to_owned(),
                    "https://vixcloud.co/embed/1".to_owned(),
                ),
                ("Origin".to_owned(), "https://vixcloud.co".to_owned()),
                ("User-Agent".to_owned(), "UA".to_owned()),
            ],
            cookies: cookies.to_owned(),
        }
    }

    #[test]
    fn the_header_blob_is_crlf_joined_in_the_legacy_order() {
        assert_eq!(
            header_blob(&target("sid=abc")),
            "User-Agent: UA\r\nReferer: https://vixcloud.co/embed/1\r\nOrigin: https://vixcloud.co\r\nCookie: sid=abc\r\n"
        );
        assert_eq!(
            header_blob(&target("")),
            "User-Agent: UA\r\nReferer: https://vixcloud.co/embed/1\r\nOrigin: https://vixcloud.co\r\n",
            "no cookies, no Cookie line"
        );
    }

    #[test]
    fn a_target_missing_a_header_still_emits_the_empty_line_legacy_emitted() {
        let t = StreamTarget {
            m3u8: url::Url::parse("https://vixcloud.co/playlist/1").expect("url"),
            headers: vec![],
            cookies: String::new(),
        };
        assert_eq!(header_blob(&t), "User-Agent: \r\nReferer: \r\nOrigin: \r\n");
    }

    #[test]
    fn the_argv_matches_legacy() {
        let cfg = EngineCfg::default();
        let headers = header_blob(&target("sid=abc"));
        assert_eq!(
            ffmpeg_spec(&cfg, &headers, "https://p/1.m3u8", Path::new("/out/Ep.mp4")).argv(),
            [
                "ffmpeg",
                "-y",
                "-headers",
                &headers,
                "-i",
                "https://p/1.m3u8",
                "-c",
                "copy",
                "-bsf:a",
                "aac_adtstoasc",
                "-progress",
                "pipe:1",
                "/out/Ep.mp4",
            ]
        );
        assert_eq!(
            probe_spec(&cfg, &headers, "https://p/1.m3u8").argv(),
            [
                "ffprobe",
                "-v",
                "error",
                "-headers",
                &headers,
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
                "https://p/1.m3u8",
            ]
        );
    }

    /// An [`EngineCfg`] whose `ffprobe` is a stand-in script.
    fn probe_cfg(script: &str) -> EngineCfg {
        EngineCfg {
            ffprobe: fixture_bin(script).into(),
            ..EngineCfg::default()
        }
    }

    #[tokio::test]
    async fn the_duration_probe_reads_ffprobes_one_line_of_output() {
        let cfg = probe_cfg("fake_ffprobe.sh");
        assert_eq!(
            probe_duration(&cfg, "User-Agent: UA\r\n", "https://p/1.m3u8").await,
            Some(1200.5)
        );
    }

    #[tokio::test]
    async fn a_failing_or_missing_probe_is_tolerated() {
        let missing = EngineCfg {
            ffprobe: "aulos-no-such-binary".into(),
            ..EngineCfg::default()
        };
        assert_eq!(probe_duration(&missing, "", "https://p/1.m3u8").await, None);
        let failing = probe_cfg("fake_ffprobe_fail.sh");
        assert_eq!(probe_duration(&failing, "", "https://p/1.m3u8").await, None);
        let garbage = probe_cfg("fake_ffprobe_garbage.sh");
        assert_eq!(
            probe_duration(&garbage, "", "https://p/1.m3u8").await,
            None,
            "`N/A` is not a duration"
        );
    }

    #[tokio::test]
    async fn a_successful_run_reports_progress_and_the_produced_file() {
        let mut f = EngineFixture::new().await;
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_dl.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, mut rx) = f.sink();
        let outcome = download_ffmpeg(&f.cfg, &ctx, &target(""), &names, &f.tmp_dir(), &sink)
            .await
            .expect("a download");
        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some(names.rel.as_str())
        );
        assert_eq!(outcome.size, Some(15));
        let frames = crate::testing::drain_frames(&mut rx);
        assert!(
            frames
                .iter()
                .any(|p| p.downloaded_bytes == Some(10_485_760.0)),
            "the -progress stream must reach the sink: {frames:?}"
        );
        assert!(
            frames.iter().any(|p| p.total_bytes_estimate.is_some()),
            "with a probed duration the estimate is derived: {frames:?}"
        );
    }

    #[tokio::test]
    async fn a_non_zero_exit_reports_the_legacy_message_with_the_stderr_tail() {
        let mut f = EngineFixture::new().await;
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_fail.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let err = download_ffmpeg(&f.cfg, &ctx, &target(""), &names, &f.tmp_dir(), &sink)
            .await
            .expect_err("a failing ffmpeg");
        let msg = err.to_string();
        assert!(msg.starts_with("FFmpeg failed with code 3: "), "{msg}");
        assert!(msg.contains("Server returned 403 Forbidden"), "{msg}");
    }

    #[tokio::test]
    async fn exit_zero_without_a_file_is_still_a_failure() {
        let mut f = EngineFixture::new().await;
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_empty.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let err = download_ffmpeg(&f.cfg, &ctx, &target(""), &names, &f.tmp_dir(), &sink)
            .await
            .expect_err("no file, no success");
        assert!(
            err.to_string().starts_with("FFmpeg failed with code 0"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_cancel_kills_ffmpeg_and_removes_the_partial() {
        let mut f = EngineFixture::new().await;
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_hang.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        f.cfg.kill_grace = Duration::from_millis(200);
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let cancel = f.cancel.clone();
        let partial = names.out_path.clone();
        let waiter = tokio::spawn(async move {
            for _ in 0..200 {
                if partial.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel();
        });
        let err = download_ffmpeg(&f.cfg, &ctx, &target(""), &names, &f.tmp_dir(), &sink)
            .await
            .expect_err("cancelled");
        waiter.await.expect("the canceller");
        assert!(matches!(err, ProviderError::Canceled));
        assert!(!names.out_path.exists(), "the partial mp4 is removed");
    }
}
