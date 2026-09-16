//! The `N_m3u8DL-RE` engine (DESIGN §10.5), argv-identical to legacy `app/ytdl.py:700-843`.
//!
//! The tool is a .NET HLS downloader that fetches segments in parallel and muxes them with ffmpeg
//! when it is done. Three things about driving it are not obvious:
//!
//! 1. **Its progress is not lines.** Spectre.Console repaints, so the output has to be read as
//!    *chunks* and assembled with [`crate::progress::Nm3u8Progress`]; a line reader would deliver
//!    nothing for minutes and then one enormous line.
//! 2. **Exit 0 does not mean "the file is there".** When its own mux step fails — which is what a
//!    16-thread run on a slow disk does — it exits 0 having left a directory of segments. That is
//!    what the gapless fallback in [`crate::mux`] is for.
//! 3. **A non-zero exit is not the item's error.** Legacy passed `report_error=False` here and
//!    retried with ffmpeg, which does report. So this returns `Err` with the legacy message and
//!    lets [`crate::engines::download`] decide.

use std::collections::VecDeque;
use std::path::Path;

use aulos_provider::outcome::Outcome;
use aulos_provider::proc::{Child, ProcError, SpawnSpec};
use aulos_provider::provider::{DownloadCtx, ProviderError};
use aulos_provider::sink::ProgressSink;
use tokio::time::{Instant, MissedTickBehavior};

use crate::engines::{EngineCfg, OutputNames, cleanup_partial, error_tail};
use crate::jit::StreamTarget;
use crate::progress::{MIN_PROGRESS_INTERVAL, Nm3u8Progress, strip_ansi};

/// The tool's name, as `argv[0]` and as the [`ProviderError::ToolMissing`] label.
pub const TOOL: &str = "N_m3u8DL-RE";

/// How many output lines the error tail keeps (legacy `output_lines[-30:]`).
const TAIL_LINES: usize = 30;

/// The argv, byte-identical to legacy's `nm3u8_cmd` (`app/ytdl.py:703-724`).
///
/// The header trio is looked up by name rather than taken in [`StreamTarget::headers`] order,
/// because legacy emitted `User-Agent`, `Referer`, `Origin` in *that* order while the target
/// carries them in the order the extractor produced them.
#[must_use]
pub fn nm3u8_spec(cfg: &EngineCfg, names: &OutputNames, tmp: &Path, t: &StreamTarget) -> SpawnSpec {
    let mut spec = SpawnSpec::new(TOOL, &cfg.nm3u8dl)
        .arg(t.m3u8.as_str())
        .arg("--save-dir")
        .arg(&names.save_dir)
        .arg("--save-name")
        .arg(&names.stem)
        .arg("--tmp-dir")
        .arg(tmp)
        .arg("--thread-count")
        .arg(cfg.thread_count.to_string())
        .arg("--auto-select")
        .arg("--del-after-done")
        .arg("--no-log")
        .arg("--mux-after-done")
        .arg("format=mp4:muxer=ffmpeg")
        .arg("--log-level")
        .arg("INFO")
        .arg("-H")
        .arg(format!(
            "User-Agent: {}",
            t.header("User-Agent").unwrap_or_default()
        ))
        .arg("-H")
        .arg(format!(
            "Referer: {}",
            t.header("Referer").unwrap_or_default()
        ))
        .arg("-H")
        .arg(format!(
            "Origin: {}",
            t.header("Origin").unwrap_or_default()
        ));
    if !t.cookies.is_empty() {
        spec = spec.arg("-H").arg(format!("Cookie: {}", t.cookies));
    }
    spec.stdout_piped(true).kill_grace(cfg.kill_grace)
}

/// The rolling tail of the tool's own output, ANSI-stripped, for the error message.
#[derive(Debug, Default)]
struct Tail {
    lines: VecDeque<String>,
    residual: String,
}

impl Tail {
    /// Feeds one chunk, keeping whole lines only.
    fn push(&mut self, chunk: &str) {
        // A carriage return is a repaint boundary, not a continuation: treating it as a line break
        // is what keeps the tail readable instead of one 4 KiB line.
        self.residual
            .push_str(&strip_ansi(chunk).replace('\r', "\n"));
        while let Some(idx) = self.residual.find('\n') {
            let line = self.residual[..idx].trim().to_owned();
            self.residual.drain(..=idx);
            if line.is_empty() {
                continue;
            }
            if self.lines.len() == TAIL_LINES {
                self.lines.pop_front();
            }
            self.lines.push_back(line);
        }
    }

    /// The reported tail, in legacy's shape — see [`error_tail`].
    fn text(&self) -> String {
        let lines: Vec<String> = self.lines.iter().cloned().collect();
        error_tail(&lines)
    }
}

/// What one iteration of the read loop observed.
///
/// The branches of the `select!` produce one of these and act afterwards, because killing the
/// child needs `&mut child` and so does `child.wait()` — the two cannot be borrowed inside the
/// same `select!` (the same shape `aulos-provider`'s `command` engine uses).
enum Step {
    /// `ctx.cancel` fired.
    Cancel,
    /// Publish the newest assembled progress row.
    Progress,
    /// A chunk of the tool's output, or `None` at end of stream.
    Out(Option<Vec<u8>>),
    /// Reading the output failed.
    Read(ProcError),
    /// The child exited.
    Exit(std::process::ExitStatus),
    /// Waiting on the child failed.
    Wait(ProcError),
}

/// Runs `N_m3u8DL-RE` and resolves the produced file, with the gapless mux as the fallback.
///
/// # Errors
/// [`ProviderError::Canceled`] on cancel, [`ProviderError::ToolMissing`] when the binary is
/// absent, [`ProviderError::Other`] with the legacy message on a non-zero exit or a missing
/// output, and [`ProviderError::Postprocessing`] when the fallback mux failed.
pub async fn download_nm3u8(
    cfg: &EngineCfg,
    ctx: &DownloadCtx<'_>,
    t: &StreamTarget,
    names: &OutputNames,
    tmp: &Path,
    sink: &ProgressSink,
) -> Result<Outcome, ProviderError> {
    let spec = nm3u8_spec(cfg, names, tmp, t);
    tracing::info!(
        item = %ctx.item_id,
        threads = cfg.thread_count,
        "running N_m3u8DL-RE for a StreamingCommunity download"
    );
    let mut child = Child::spawn(&spec)?;
    let mut stdout = child.take_stdout();
    let mut tail = Tail::default();
    let mut progress = Nm3u8Progress::default();
    let mut tick = tokio::time::interval_at(
        Instant::now() + MIN_PROGRESS_INTERVAL,
        MIN_PROGRESS_INTERVAL,
    );
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut output_bytes = 0_u64;
    let mut progress_frames = 0_u64;

    let status = loop {
        let step = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Step::Cancel,
            _ = tick.tick() => Step::Progress,
            r = read_chunk(stdout.as_mut()), if stdout.is_some() => match r {
                Ok(chunk) => Step::Out(chunk),
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
            Step::Out(Some(chunk)) => {
                let text = String::from_utf8_lossy(&chunk);
                tail.push(&text);
                output_bytes += chunk.len() as u64;
                progress.feed(&chunk);
            }
            Step::Progress => {
                if let Some(frame) = progress.take_frame() {
                    sink.progress(frame);
                    progress_frames += 1;
                }
            }
            // End of stream: stop polling it and wait for the exit.
            Step::Out(None) => stdout = None,
            Step::Read(e) => {
                tracing::debug!(error = %e, "reading N_m3u8DL-RE output failed");
                stdout = None;
            }
            Step::Exit(status) => break status,
            Step::Wait(e) => {
                child.kill_group().await;
                return Err(e.into());
            }
        }
    };

    if let Some(frame) = progress.finish() {
        sink.progress(frame);
        progress_frames += 1;
    }
    tracing::debug!(item = %ctx.item_id, output_bytes, progress_frames, "N_m3u8DL-RE progress summary");
    if status.success() && progress_frames == 0 {
        tracing::warn!(item = %ctx.item_id, output_bytes, "N_m3u8DL-RE completed without readable progress");
    }

    if !status.success() {
        let code = status.code().unwrap_or(1);
        let tail = tail.text();
        tracing::error!(code, output = %tail, "N_m3u8DL-RE failed");
        let mut msg = format!("{TOOL} failed with code {code}");
        if !tail.is_empty() {
            msg.push_str(": ");
            msg.push_str(&tail);
        }
        return Err(ProviderError::Other(msg));
    }

    resolve_output(cfg, names).await
}

/// The next chunk from the tool's output, `None` at end of stream.
async fn read_chunk(
    lines: Option<&mut aulos_provider::proc::Lines<tokio::process::ChildStdout>>,
) -> Result<Option<Vec<u8>>, ProcError> {
    match lines {
        Some(l) => l.next_chunk().await,
        // Unreachable while the `if stdout.is_some()` guard holds; a pending future keeps the
        // branch from resolving if it ever does not.
        None => std::future::pending().await,
    }
}

/// Finds what the tool produced: the expected mp4, a suffixed sibling, or a segment directory that
/// needs the gapless mux (legacy `app/ytdl.py:786-843`).
async fn resolve_output(cfg: &EngineCfg, names: &OutputNames) -> Result<Outcome, ProviderError> {
    if let Some(size) = file_size(&names.out_path).await {
        return Ok(Outcome::file(names.rel.clone(), size));
    }
    // Legacy globbed `{safe_title}*.mp4` and took the newest, because the tool appends a suffix
    // when it muxes a multi-track stream.
    if let Some((path, size)) = newest_sibling(names).await {
        tracing::info!(path = %path.display(), "N_m3u8DL-RE produced a suffixed output");
        let rel = sibling_rel(names, &path)?;
        return Ok(Outcome::file(rel, size));
    }
    if !names.seg_dir.is_dir() {
        tracing::error!(path = %names.out_path.display(), "N_m3u8DL-RE exited OK but the output is missing");
        return Err(ProviderError::Other(
            crate::engines::MSG_NO_OUTPUT.to_owned(),
        ));
    }

    let size = crate::mux::gapless_mux(&cfg.ffmpeg, &names.seg_dir, &names.out_path).await?;
    if let Err(e) = tokio::fs::remove_dir_all(&names.seg_dir).await {
        tracing::warn!(dir = %names.seg_dir.display(), error = %e, "could not remove the segment directory");
    }
    Ok(Outcome::file(names.rel.clone(), size))
}

/// The file's size, or `None` when it does not exist or is empty.
async fn file_size(path: &Path) -> Option<u64> {
    let m = tokio::fs::metadata(path).await.ok()?;
    (m.is_file() && m.len() > 0).then_some(m.len())
}

/// The newest `<stem>*.mp4` in the save directory that is not the expected path.
async fn newest_sibling(names: &OutputNames) -> Option<(std::path::PathBuf, u64)> {
    let mut best: Option<(std::path::PathBuf, u64, std::time::SystemTime)> = None;
    let mut dir = tokio::fs::read_dir(&names.save_dir).await.ok()?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if !name.starts_with(&names.stem) || !name.ends_with(".mp4") {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(_, _, best_m)| mtime > *best_m) {
            best = Some((path, meta.len(), mtime));
        }
    }
    best.map(|(p, size, _)| (p, size))
}

/// The wire-relative path of a suffixed sibling: the planned path with its file name swapped.
fn sibling_rel(
    names: &OutputNames,
    path: &Path,
) -> Result<aulos_core::paths::RelPath, ProviderError> {
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| ProviderError::Other(crate::engines::MSG_NO_OUTPUT.to_owned()))?;
    let planned = names.rel.as_str();
    let rel = match planned.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_owned(),
    };
    aulos_core::paths::RelPath::parse(&rel)
        .map_err(|e| ProviderError::Other(format!("the output path is not usable: {e}")))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use aulos_core::paths::RelPath;

    use super::*;
    use crate::testing::{EngineFixture, fixture_bin, write};

    fn target() -> StreamTarget {
        StreamTarget {
            m3u8: url::Url::parse("https://vixcloud.co/playlist/98765?b=1&token=t&expires=1")
                .expect("url"),
            headers: vec![
                (
                    "Referer".to_owned(),
                    "https://vixcloud.co/embed/98765".to_owned(),
                ),
                ("Origin".to_owned(), "https://vixcloud.co".to_owned()),
                ("User-Agent".to_owned(), crate::http::USER_AGENT.to_owned()),
            ],
            cookies: "sid=abc".to_owned(),
        }
    }

    #[tokio::test]
    async fn the_argv_is_the_legacy_argv_string_for_string() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let tmp = f.tmp_dir();
        let spec = nm3u8_spec(&f.cfg, &names, &tmp, &target());

        let out_dir = f.out_dir();
        let want = vec![
            "N_m3u8DL-RE".to_owned(),
            "https://vixcloud.co/playlist/98765?b=1&token=t&expires=1".to_owned(),
            "--save-dir".to_owned(),
            out_dir.display().to_string(),
            "--save-name".to_owned(),
            "Una Serie S01E02 - Pilota".to_owned(),
            "--tmp-dir".to_owned(),
            tmp.display().to_string(),
            "--thread-count".to_owned(),
            "16".to_owned(),
            "--auto-select".to_owned(),
            "--del-after-done".to_owned(),
            "--no-log".to_owned(),
            "--mux-after-done".to_owned(),
            "format=mp4:muxer=ffmpeg".to_owned(),
            "--log-level".to_owned(),
            "INFO".to_owned(),
            "-H".to_owned(),
            format!("User-Agent: {}", crate::http::USER_AGENT),
            "-H".to_owned(),
            "Referer: https://vixcloud.co/embed/98765".to_owned(),
            "-H".to_owned(),
            "Origin: https://vixcloud.co".to_owned(),
            "-H".to_owned(),
            "Cookie: sid=abc".to_owned(),
        ];
        assert_eq!(spec.argv(), want);
        assert_eq!(spec.tool_name(), "N_m3u8DL-RE");
    }

    #[tokio::test]
    async fn an_empty_cookie_jar_omits_the_cookie_header_exactly_as_legacy_did() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let mut t = target();
        t.cookies = String::new();
        let argv = nm3u8_spec(&f.cfg, &names, &f.tmp_dir(), &t).argv();
        assert_eq!(
            argv.iter().filter(|a| *a == "-H").count(),
            3,
            "three headers, not four: {argv:?}"
        );
        assert!(!argv.iter().any(|a| a.starts_with("Cookie:")));
    }

    #[tokio::test]
    async fn the_thread_count_comes_from_sc_thread_count() {
        let mut f = EngineFixture::new().await;
        f.cfg.thread_count = 4;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let argv = nm3u8_spec(&f.cfg, &names, &f.tmp_dir(), &target()).argv();
        let idx = argv
            .iter()
            .position(|a| a == "--thread-count")
            .expect("flag");
        assert_eq!(argv[idx + 1], "4");
    }

    #[test]
    fn the_tail_keeps_the_end_and_caps_the_message() {
        let mut tail = Tail::default();
        for i in 0..40 {
            tail.push(&format!("\u{1b}[2Kline {i}\n"));
        }
        let text = tail.text();
        assert!(text.starts_with("line 20"), "{text}");
        assert!(text.ends_with("line 39"), "{text}");
        assert_eq!(text.lines().count(), crate::engines::TAIL_REPORTED);

        let mut long = Tail::default();
        long.push(&format!("{}\n", "x".repeat(2000)));
        assert_eq!(long.text().chars().count(), crate::engines::TAIL_CHARS);
    }

    #[test]
    fn the_tail_treats_a_repaint_as_a_line_and_drops_the_escapes() {
        let mut tail = Tail::default();
        tail.push("\u{1b}[2Kvideo 1/2 50.00%\rvideo 2/2 100.00%\r");
        assert_eq!(tail.text(), "video 1/2 50.00%\nvideo 2/2 100.00%");
    }

    #[tokio::test]
    async fn a_successful_run_reports_the_expected_file() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_ok.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, mut rx) = f.sink();
        let outcome = download_nm3u8(&f.cfg, &ctx, &target(), &names, &f.tmp_dir(), &sink)
            .await
            .expect("a download");
        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some(names.rel.as_str())
        );
        assert_eq!(outcome.size, Some(14));
        let frames = crate::testing::drain_frames(&mut rx);
        assert!(!frames.is_empty(), "progress must reach the sink");
    }

    #[tokio::test]
    async fn split_progress_reaches_the_sink_before_the_downloader_finishes() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_split.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, mut rx) = f.sink();
        let target = target();
        let tmp = f.tmp_dir();
        let run = download_nm3u8(&f.cfg, &ctx, &target, &names, &tmp, &sink);
        tokio::pin!(run);
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("downloader finished before progress: {result:?}"),
                msg = rx.recv() => match msg.expect("progress channel") {
                    aulos_provider::sink::ProgressMsg::Progress { raw, .. } => raw,
                    other => panic!("expected progress: {other:?}"),
                },
            }
        })
        .await
        .expect("progress while the process is running");
        assert_eq!(
            (frame.fragment_index, frame.fragment_count),
            (Some(4), Some(12))
        );
        assert_eq!(frame.downloaded_bytes, Some((369.02_f64 * 1024.0).trunc()));
        assert_eq!(
            frame.total_bytes,
            Some((2.16_f64 * 1024.0 * 1024.0).trunc())
        );
        assert_eq!(frame.speed, Some((369.02_f64 * 1024.0).trunc()));
        assert_eq!(frame.eta, Some(6));
        assert!(
            !names.out_path.exists(),
            "progress arrived before completion"
        );
        write(&f.out_dir().join("allow-finish"), b"");
        run.await.expect("download completes");
    }

    #[tokio::test]
    async fn a_non_zero_exit_carries_the_legacy_message_and_the_output_tail() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_fail.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let err = download_nm3u8(&f.cfg, &ctx, &target(), &names, &f.tmp_dir(), &sink)
            .await
            .expect_err("a failing tool");
        let msg = err.to_string();
        assert!(msg.starts_with("N_m3u8DL-RE failed with code 7: "), "{msg}");
        assert!(msg.contains("ERROR: master playlist rejected"), "{msg}");
        assert_eq!(err.code(), aulos_core::error::ErrorCode::Internal);
    }

    #[tokio::test]
    async fn a_missing_binary_is_tool_missing() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = OsString::from("aulos-no-such-binary");
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let err = download_nm3u8(&f.cfg, &ctx, &target(), &names, &f.tmp_dir(), &sink)
            .await
            .expect_err("no binary");
        assert_eq!(err.code(), aulos_core::error::ErrorCode::ToolMissing);
    }

    #[tokio::test]
    async fn exit_zero_with_segments_falls_back_to_the_gapless_mux() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_segments.sh").into();
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_mux.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let outcome = download_nm3u8(&f.cfg, &ctx, &target(), &names, &f.tmp_dir(), &sink)
            .await
            .expect("the fallback mux must produce the file");
        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some(names.rel.as_str())
        );
        assert!(
            !names.seg_dir.exists(),
            "the segment directory is removed once it is muxed"
        );
        let log = std::fs::read_to_string(f.out_dir().join("ffmpeg-invocations.log")).expect("log");
        assert!(!log.contains("concat"), "{log}");
    }

    #[tokio::test]
    async fn exit_zero_with_nothing_at_all_reports_the_legacy_message() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_empty.sh").into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let (sink, _rx) = f.sink();
        let err = download_nm3u8(&f.cfg, &ctx, &target(), &names, &f.tmp_dir(), &sink)
            .await
            .expect_err("nothing produced");
        assert_eq!(err.to_string(), crate::engines::MSG_NO_OUTPUT);
    }

    #[tokio::test]
    async fn a_suffixed_output_is_found_the_way_legacys_glob_found_it() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        write(
            &f.out_dir().join("Una Serie S01E02 - Pilota.HD.mp4"),
            b"suffixed",
        );
        let outcome = resolve_output(&f.cfg, &names).await.expect("the sibling");
        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some("Una Serie S01E02 - Pilota.HD.mp4")
        );
        assert_eq!(outcome.size, Some(8));
    }

    #[tokio::test]
    async fn a_suffixed_output_keeps_the_folder_prefix_in_the_wire_path() {
        let mut f = EngineFixture::new().await;
        f.with_folder("Serie").await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        write(
            &names.save_dir.join("Una Serie S01E02 - Pilota.HD.mp4"),
            b"suffixed",
        );
        let outcome = resolve_output(&f.cfg, &names).await.expect("the sibling");
        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some("Serie/Una Serie S01E02 - Pilota.HD.mp4")
        );
    }

    #[tokio::test]
    async fn an_empty_output_file_is_not_an_output() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        write(&names.out_path, b"");
        let err = resolve_output(&f.cfg, &names)
            .await
            .expect_err("a zero-byte mp4 is not a download");
        assert_eq!(err.to_string(), crate::engines::MSG_NO_OUTPUT);
    }
}
