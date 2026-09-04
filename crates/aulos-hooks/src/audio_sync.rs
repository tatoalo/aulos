//! The `best_remux` audio-sync fix, in-process (DESIGN §13.3).
//!
//! A port of legacy `audio_sync_fix.py` with no `Exec` postprocessor and no hard-coded
//! `/app/app/...` path. Two things about it are deliberately different from every other hook:
//!
//! - It is [`HookPhase::PreTerminal`]. Legacy ran it as a late yt-dlp `Exec` postprocessor, i.e.
//!   *inside* the download, before the item was terminal. A post-terminal port would have to move a
//!   `finished` item back to `postprocessing`, which DESIGN §4.2 does not allow. So the engine
//!   writes `postprocessing`, publishes `Finishing`, waits for `HooksFinished`, and only then
//!   writes `finished` — which also means the single `completed` frame carries the **post**-re-encode
//!   `size`.
//! - A failure is not a failed download. Legacy's `Exec` failure made yt-dlp report a
//!   postprocessor error, so a perfectly good file looked broken. Here the temp file is removed,
//!   a WARN is logged, and the item becomes `finished` with the **original** file and its original
//!   size.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aulos_core::item::ItemView;
use aulos_core::ports::HookPhase;
use aulos_core::progress::{PhaseTag, RawProgress};
use aulos_core::selection::DownloadType;
use aulos_core::status::TerminalStatus;
use aulos_provider::proc::SpawnSpec;
use aulos_provider::sink::Stage;

use crate::error::HookError;
use crate::ffprobe::{self, FFMPEG, MediaTools};
use crate::hook::{Hook, HookCtx, HookHealth};

/// `ordering` — first, because it rewrites the file the NFO and the Jellyfin scan describe
/// (DESIGN §13).
pub const ORDERING: i16 = 10;

/// The `healthz` component key and the hook id.
pub const ID: &str = "audio_sync";

/// The format this hook applies to.
const FORMAT: &str = "mp4";
/// The quality this hook applies to.
const QUALITY: &str = "best_remux";

/// `msg` while the re-encode runs (DESIGN §13.3 step 5).
pub const MSG: &str = "Re-encoding audio";

/// The floor of the duration-scaled ffmpeg timeout (DESIGN §13.3 step 3).
pub const MIN_ENCODE_TIMEOUT: Duration = Duration::from_secs(600);
/// The ffmpeg timeout used when the duration cannot be determined.
pub const UNKNOWN_ENCODE_TIMEOUT: Duration = Duration::from_secs(1800);

/// The dispatcher's outer bound. The effective bound is the duration-scaled one computed in
/// [`encode_timeout`]; this only exists so a bug in the hook itself cannot park an item forever.
pub const OUTER_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// The `source_tag` every frame this hook publishes carries (DESIGN §4.7).
///
/// One constant tag rather than zero: a *changing* tag resets the normaliser's monotonic floor, so
/// a per-frame or zero tag would make `percent` fall back to 0 for the duration of the re-encode.
/// The frames also repeat the finished byte counts, so `percent` sits at the active ceiling
/// instead of collapsing while the secondary bar moves.
pub const SOURCE_TAG: u64 = 0xA0D1_0537_9C00_0001;

/// The globals that precede `-i`: DESIGN §13.3 step 4's `-y -loglevel warning`, plus
/// `-progress pipe:1` for the secondary progress bar of step 5.
const PRE_INPUT: [&str; 5] = ["-y", "-loglevel", "warning", "-progress", "pipe:1"];

/// Everything after `-i <file>`, verbatim from DESIGN §13.3 step 4.
const POST_INPUT: [&str; 12] = [
    "-map",
    "0",
    "-dn",
    "-ignore_unknown",
    "-c",
    "copy",
    "-c:a",
    "aac",
    "-b:a",
    "256k",
    "-movflags",
    "+faststart",
];

/// `timeout = max(600, ceil(duration / 2))`, or `1800` when the duration is unknown
/// (DESIGN §13.3 step 3).
#[must_use]
pub fn encode_timeout(duration: Option<f64>) -> Duration {
    match duration {
        None => UNKNOWN_ENCODE_TIMEOUT,
        Some(d) => {
            let halved = (d / 2.0).ceil();
            // `d` came from ffprobe and is finite and positive; the clamp keeps the cast sound.
            let secs = if halved.is_finite() && halved > 0.0 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let s = halved.min(u64::MAX as f64) as u64;
                s
            } else {
                0
            };
            Duration::from_secs(secs).max(MIN_ENCODE_TIMEOUT)
        }
    }
}

/// The sibling temp file the re-encode writes before the atomic rename.
///
/// A sibling rather than `TEMP_DIR` so the rename is a rename and never a cross-device copy, and
/// item-id-suffixed so two concurrent re-encodes of the same name cannot collide.
#[must_use]
pub fn temp_path(file: &Path, id: aulos_core::id::ItemId) -> PathBuf {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = format!(".{stem}.{id}.audiosync.mp4");
    match file.parent() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    }
}

/// One `-progress pipe:1` line, as seconds of output written.
///
/// ffmpeg emits `out_time_us`, `out_time_ms` (also microseconds, a long-standing upstream quirk)
/// and `out_time=HH:MM:SS.ffffff`. The first two are preferred because they need no parsing.
#[must_use]
pub fn progress_line_secs(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    let value = value.trim();
    match key.trim() {
        "out_time_us" | "out_time_ms" => value.parse::<f64>().ok().map(|us| us / 1_000_000.0),
        "out_time" => {
            // `HH:MM:SS.ffffff`, or `N/A` before the first frame.
            let mut secs = 0.0;
            for part in value.split(':') {
                let n = part.parse::<f64>().ok()?;
                secs = secs * 60.0 + n;
            }
            Some(secs)
        }
        _ => None,
    }
}

/// The `best_remux` audio re-encode (DESIGN §13.3).
#[derive(Clone, Debug, Default)]
pub struct AudioSyncHook {
    tools: MediaTools,
}

impl AudioSyncHook {
    /// The hook, using `ffmpeg`/`ffprobe` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The hook, using explicit tool paths. For tests and for a future config knob.
    #[must_use]
    pub fn with_tools(tools: MediaTools) -> Self {
        Self { tools }
    }

    /// The argv this hook will exec for `file` → `tmp`, for the audit view and for tests.
    #[must_use]
    pub fn argv(&self, file: &Path, tmp: &Path) -> Vec<String> {
        self.spec(file, tmp).argv()
    }

    fn spec(&self, file: &Path, tmp: &Path) -> SpawnSpec {
        SpawnSpec::new(FFMPEG, &self.tools.ffmpeg)
            .args(PRE_INPUT)
            .arg("-i")
            .arg(file)
            .args(POST_INPUT)
            .arg(tmp)
            .stdout_piped(true)
    }
}

#[async_trait::async_trait]
impl Hook for AudioSyncHook {
    fn id(&self) -> Arc<str> {
        Arc::from(ID)
    }

    fn ordering(&self) -> i16 {
        ORDERING
    }

    fn phase(&self) -> HookPhase {
        HookPhase::PreTerminal
    }

    fn timeout(&self) -> Duration {
        OUTER_TIMEOUT
    }

    /// `outcome is success && selection == (video, mp4, best_remux)` (DESIGN §13).
    ///
    /// `outcome` is the *prospective* status: the row still reads `postprocessing` when this is
    /// called, because the engine has not written the terminal status yet.
    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool {
        outcome == TerminalStatus::Finished
            && item.selection.download_type == DownloadType::Video
            && &*item.selection.format == FORMAT
            && &*item.selection.quality == QUALITY
            && item.filename.is_some()
    }

    fn health(&self) -> HookHealth {
        // `phase` is added by the dispatcher for every pre-terminal hook, so nothing to add here.
        HookHealth::ok()
    }

    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError> {
        if ctx.outcome() != TerminalStatus::Finished {
            return Ok(());
        }
        let Some(file) = ctx.file else {
            return Ok(());
        };

        // 1. Skip unless the produced file exists and ends in `.mp4`.
        let ext_is_mp4 = file
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
        if !ext_is_mp4 {
            tracing::debug!(path = %file.display(), "audio_sync: skipping a non-mp4 file");
            return Ok(());
        }
        match tokio::fs::metadata(file).await {
            Ok(m) if m.is_file() => {}
            _ => {
                tracing::warn!(path = %file.display(), "audio_sync: the produced file is gone");
                return Ok(());
            }
        }

        // 2. Skip when there is no video stream (an audio-only mp4 has nothing to drift).
        if !ffprobe::has_video_stream(&self.tools, file, ctx.cancel).await {
            tracing::info!(path = %file.display(), "audio_sync: skipping an audio-only file");
            return Ok(());
        }

        // 3. Duration-scaled timeout.
        let duration = ffprobe::duration_secs(&self.tools, file, ctx.cancel).await;
        let timeout = encode_timeout(duration);
        tracing::info!(
            path = %file.display(),
            timeout_s = timeout.as_secs(),
            duration = duration.unwrap_or_default(),
            "audio_sync: re-encoding audio"
        );

        // 4. `postprocessing` with a moving secondary bar, through the ordinary sink (DESIGN §13.3).
        ctx.sink
            .stage(Stage::Postprocessing, Some(MSG.into()))
            .await;
        let size_before = ctx.item.size;
        ctx.sink.progress(frame(size_before, Some(0.0)));

        let tmp = temp_path(file, ctx.item.id);
        let spec = self.spec(file, &tmp);
        let sink = ctx.sink;
        let mut on_line = |line: &str| {
            if let (Some(secs), Some(total)) = (progress_line_secs(line), duration)
                && total > 0.0
            {
                let pct = (secs / total * 100.0).clamp(0.0, 100.0);
                sink.progress(frame(size_before, Some(pct)));
            }
        };
        let captured = ffprobe::capture(&spec, timeout, ctx.cancel, &mut on_line).await;

        // 5. On any failure: remove the temp file and leave the original alone.
        match captured {
            Ok(c) if c.success => {}
            Ok(c) => {
                remove_temp(&tmp).await;
                return Err(HookError::Tool {
                    tool: FFMPEG,
                    detail: format!(
                        "exit {}: {}",
                        c.code
                            .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                        c.stderr.trim()
                    )
                    .into(),
                });
            }
            Err(e) => {
                remove_temp(&tmp).await;
                return Err(e);
            }
        }

        // 6. Atomic rename, then the one item write this hook makes.
        tokio::fs::rename(&tmp, file).await.map_err(|e| {
            HookError::io(format!("rename {} -> {}", tmp.display(), file.display()), e)
        })?;
        let size = tokio::fs::metadata(file)
            .await
            .map(|m| m.len())
            .map_err(|e| HookError::io(format!("stat {}", file.display()), e))?;
        ctx.sink.progress(frame(Some(size), Some(100.0)));
        ctx.store.set_size(ctx.item.id, size).await?;
        tracing::info!(path = %file.display(), size, "audio_sync: applied");
        Ok(())
    }
}

/// A phase-only progress frame. See [`SOURCE_TAG`] for why the byte counts are repeated.
fn frame(size: Option<u64>, phase_percent: Option<f64>) -> RawProgress {
    #[allow(clippy::cast_precision_loss)] // file sizes stay far inside f64's exact range
    let bytes = size.map(|s| s as f64);
    RawProgress {
        downloaded_bytes: bytes,
        total_bytes: bytes,
        phase: Some(PhaseTag::AudioSync),
        phase_percent,
        source_tag: SOURCE_TAG,
        ..RawProgress::default()
    }
}

async fn remove_temp(tmp: &Path) {
    if let Err(e) = tokio::fs::remove_file(tmp).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(path = %tmp.display(), error = %e, "audio_sync: could not remove the temp file");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::id::ItemId;

    #[test]
    fn the_timeout_is_the_documented_formula() {
        assert_eq!(encode_timeout(None), Duration::from_secs(1800));
        assert_eq!(encode_timeout(Some(2.0)), Duration::from_secs(600));
        assert_eq!(encode_timeout(Some(1199.0)), Duration::from_secs(600));
        // ceil(1201/2) = 601 > 600
        assert_eq!(encode_timeout(Some(1201.0)), Duration::from_secs(601));
        assert_eq!(encode_timeout(Some(7200.0)), Duration::from_secs(3600));
        assert_eq!(encode_timeout(Some(f64::NAN)), Duration::from_secs(600));
    }

    #[test]
    fn progress_lines_parse_in_every_shape_ffmpeg_emits() {
        assert_eq!(progress_line_secs("out_time_us=1500000"), Some(1.5));
        assert_eq!(progress_line_secs("out_time_ms=1500000"), Some(1.5));
        assert_eq!(progress_line_secs("out_time=00:00:01.500000"), Some(1.5));
        assert_eq!(progress_line_secs("out_time=01:00:00.000000"), Some(3600.0));
        assert_eq!(progress_line_secs("progress=continue"), None);
        assert_eq!(progress_line_secs("out_time=N/A"), None);
        assert_eq!(progress_line_secs("garbage"), None);
    }

    #[test]
    fn the_temp_file_is_a_hidden_sibling_with_an_mp4_extension() {
        let id = ItemId::new();
        let tmp = temp_path(Path::new("/downloads/Show/Clip.mp4"), id);
        assert_eq!(tmp.parent().unwrap(), Path::new("/downloads/Show"));
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".Clip."), "{name}");
        assert!(name.ends_with(".audiosync.mp4"), "{name}");
        assert!(name.contains(&id.to_string()), "{name}");
    }

    #[test]
    fn the_argv_is_the_design_argv() {
        let hook = AudioSyncHook::new();
        let argv = hook.argv(Path::new("/d/Clip.mp4"), Path::new("/d/.tmp.mp4"));
        assert_eq!(
            argv,
            [
                "ffmpeg",
                "-y",
                "-loglevel",
                "warning",
                "-progress",
                "pipe:1",
                "-i",
                "/d/Clip.mp4",
                "-map",
                "0",
                "-dn",
                "-ignore_unknown",
                "-c",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "256k",
                "-movflags",
                "+faststart",
                "/d/.tmp.mp4",
            ]
        );
    }

    #[test]
    fn the_phase_frame_keeps_percent_from_collapsing() {
        let f = frame(Some(4096), Some(12.5));
        assert_eq!(f.phase, Some(PhaseTag::AudioSync));
        assert_eq!(f.phase_percent, Some(12.5));
        assert_eq!(f.source_tag, SOURCE_TAG);
        assert_eq!(f.downloaded_bytes, Some(4096.0));
        assert_eq!(f.total_bytes, Some(4096.0));
        assert_ne!(
            f.source_tag, 0,
            "a zero tag would reset the monotonic floor"
        );
    }
}
