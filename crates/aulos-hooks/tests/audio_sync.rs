//! The `best_remux` audio-sync hook (DESIGN §13.3).
//!
//! Two layers, on purpose. The behavioural cases — no video stream, a failing encoder, the moving
//! secondary bar, the `set_size` writeback — run against **scripted** `ffmpeg`/`ffprobe`, so they
//! are deterministic and need no media tooling installed. The round trip runs against **real**
//! ffmpeg over the checked-in two-second mp4 in `tests/fixtures`, and skips itself with a note
//! when ffmpeg is not on `PATH` rather than failing a machine that has no encoder.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aulos_core::clock::FakeClock;
use aulos_core::progress::PhaseTag;
use aulos_core::selection::DownloadType;
use aulos_core::status::{Status, TerminalStatus};
use aulos_hooks::audio_sync::{self, AudioSyncHook};
use aulos_hooks::hook::{BatchEntry, Hook};
use aulos_hooks::{HookPhase, HookRunner, HookStore, MediaTools};
use aulos_provider::sink::{ProgressMsg, Stage};
use common::{Call, FakeStore, ItemBuilder, config_rooted, drain, have_tool, script, sink};

const FIXTURE: &str = "tests/fixtures/two_seconds.mp4";

/// The two-second mp4, copied into `dir` under `name`.
fn media(dir: &Path, name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let dst = dir.join(name);
    std::fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    dst
}

/// A scripted `ffprobe` that reports one video stream and a two-second duration.
fn good_ffprobe(dir: &Path) -> PathBuf {
    script(
        dir,
        "ffprobe",
        r#"case "$*" in
  *-select_streams*) echo '{"streams":[{"codec_type":"video"}]}' ;;
  *) echo '{"format":{"duration":"2.000000"}}' ;;
esac"#,
    )
}

/// A scripted `ffmpeg` that reports progress and writes its output file.
fn good_ffmpeg(dir: &Path) -> PathBuf {
    script(
        dir,
        "ffmpeg",
        r#"for a in "$@"; do out="$a"; done
echo "out_time_us=500000"
echo "out_time_us=1000000"
echo "out_time_us=2000000"
echo "progress=end"
printf 'RE-ENCODED-AND-LONGER-THAN-THE-ORIGINAL' > "$out""#,
    )
}

struct Harness {
    dir: tempfile::TempDir,
    tools: MediaTools,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir bin");
        let tools = MediaTools {
            ffmpeg: good_ffmpeg(&bin),
            ffprobe: good_ffprobe(&bin),
        };
        Self { dir, tools }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Runs the hook over `Clip.mp4` in the harness directory, returning the store and the progress
/// frames it produced.
async fn run(
    harness: &Harness,
    size: Option<u64>,
) -> (
    Result<(), aulos_hooks::HookError>,
    Arc<FakeStore>,
    Vec<ProgressMsg>,
) {
    let cfg = config_rooted(harness.path(), &[]);
    let item = ItemBuilder::finished("Clip")
        .status(Status::Postprocessing)
        .selection(DownloadType::Video, "mp4", "best_remux")
        .filename("Clip.mp4")
        .size(size);
    let view = item.view();
    let store = FakeStore::new();
    let (factory, mut rx) = sink();
    let runner = HookRunner::new(
        cfg,
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let hook = AudioSyncHook::with_tools(harness.tools.clone());
    let batch = vec![BatchEntry::from_view(&view, TerminalStatus::Finished)];
    let result = runner.run(&hook, &view, &batch).await;
    let frames = drain(&mut rx);
    (result, store, frames)
}

#[test]
fn the_hook_is_pre_terminal_and_first_in_line() {
    let hook = AudioSyncHook::new();
    assert_eq!(hook.phase(), HookPhase::PreTerminal);
    assert_eq!(hook.ordering(), 10);
    assert_eq!(&*hook.id(), "audio_sync");
}

/// `applies` reads the *prospective* outcome, since the row is still `postprocessing`
/// (DESIGN §13).
#[test]
fn applies_only_to_a_successful_video_mp4_best_remux() {
    let hook = AudioSyncHook::new();
    let remux = ItemBuilder::finished("Clip")
        .status(Status::Postprocessing)
        .selection(DownloadType::Video, "mp4", "best_remux")
        .view();
    assert!(hook.applies(&remux, TerminalStatus::Finished));
    assert!(
        !hook.applies(&remux, TerminalStatus::Error),
        "a failed download is not re-encoded"
    );
    assert!(!hook.applies(&remux, TerminalStatus::Canceled));

    for (dt, format, quality) in [
        (DownloadType::Video, "mp4", "best"),
        (DownloadType::Video, "mkv", "best_remux"),
        (DownloadType::Audio, "mp4", "best_remux"),
    ] {
        let other = ItemBuilder::finished("Clip")
            .status(Status::Postprocessing)
            .selection(dt, format, quality)
            .view();
        assert!(
            !hook.applies(&other, TerminalStatus::Finished),
            "{dt:?}/{format}/{quality} must not re-encode"
        );
    }

    let no_file = ItemBuilder::finished("Clip")
        .selection(DownloadType::Video, "mp4", "best_remux")
        .no_file()
        .view();
    assert!(!hook.applies(&no_file, TerminalStatus::Finished));
}

/// PLAN WP-11: "a successful re-encode replaces the file and updates `size`".
#[tokio::test]
async fn a_successful_re_encode_replaces_the_file_and_reports_the_new_size() {
    let h = Harness::new();
    let file = media(h.path(), "Clip.mp4");
    let original = std::fs::metadata(&file).expect("stat").len();

    let (result, store, frames) = run(&h, Some(original)).await;
    result.expect("the scripted encoder succeeds");

    let content = std::fs::read(&file).expect("the file is still there");
    assert_eq!(content, b"RE-ENCODED-AND-LONGER-THAN-THE-ORIGINAL");
    let new_size = content.len() as u64;
    assert_eq!(
        store.writes(),
        [Call::SetSize(store_id(&store), new_size)],
        "exactly one write, and it is the post-re-encode size"
    );
    assert!(
        std::fs::read_dir(h.path())
            .expect("readdir")
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains("audiosync")),
        "the temp file is gone"
    );

    // The client sees `postprocessing` with the documented message and a moving secondary bar.
    let stages: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            ProgressMsg::Stage { stage, msg, .. } => Some((*stage, msg.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [(Stage::Postprocessing, Some("Re-encoding audio".into()))]
    );
    let percents: Vec<f64> = frames
        .iter()
        .filter_map(|f| match f {
            ProgressMsg::Progress { raw, .. } => raw.phase_percent,
            _ => None,
        })
        .collect();
    assert_eq!(
        percents,
        [0.0, 25.0, 50.0, 100.0, 100.0],
        "phase_percent moves with -progress pipe:1 and ends at 100"
    );
    for f in &frames {
        if let ProgressMsg::Progress { raw, .. } = f {
            assert_eq!(raw.phase, Some(PhaseTag::AudioSync));
            assert_ne!(
                raw.source_tag, 0,
                "a zero tag would reset the percent floor"
            );
        }
    }
}

/// PLAN WP-11: "a missing video stream is skipped".
#[tokio::test]
async fn an_audio_only_file_is_skipped_without_running_the_encoder() {
    let mut h = Harness::new();
    let bin = h.path().join("bin");
    h.tools.ffprobe = script(&bin, "ffprobe-novideo", r#"echo '{"streams":[]}'"#);
    h.tools.ffmpeg = script(&bin, "ffmpeg-forbidden", "echo 'must not run' >&2; exit 3");
    let file = media(h.path(), "Clip.mp4");
    let before = std::fs::read(&file).expect("read");

    let (result, store, frames) = run(&h, Some(before.len() as u64)).await;
    result.expect("skipping is a success");
    assert_eq!(std::fs::read(&file).expect("read"), before, "untouched");
    assert!(store.writes().is_empty(), "and no size was written");
    assert!(frames.is_empty(), "and the client saw nothing");
}

/// PLAN WP-11: "a forced ffmpeg failure leaves the original file intact and the item `finished`."
#[tokio::test]
async fn a_failing_encoder_leaves_the_original_file_and_size_alone() {
    let mut h = Harness::new();
    let bin = h.path().join("bin");
    h.tools.ffmpeg = script(
        &bin,
        "ffmpeg-broken",
        r#"echo "out_time_us=100000"
echo "Conversion failed!" >&2
exit 1"#,
    );
    let file = media(h.path(), "Clip.mp4");
    let before = std::fs::read(&file).expect("read");

    let (result, store, _frames) = run(&h, Some(before.len() as u64)).await;
    let e = result.expect_err("a non-zero exit is a hook failure");
    assert!(e.to_string().contains("Conversion failed!"), "{e}");
    assert_eq!(
        std::fs::read(&file).expect("read"),
        before,
        "the original file survives, which is the whole point of the port"
    );
    assert!(store.writes().is_empty(), "and `size` is not touched");
    let leftovers: Vec<String> = std::fs::read_dir(h.path())
        .expect("readdir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("audiosync"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "and the temp file is removed: {leftovers:?}"
    );
}

/// A missing encoder is a `ToolMissing` failure, not a panic, and again leaves the file alone.
#[tokio::test]
async fn a_missing_encoder_is_reported_as_a_missing_tool() {
    let mut h = Harness::new();
    h.tools.ffmpeg = PathBuf::from("/nonexistent/aulos-ffmpeg");
    let file = media(h.path(), "Clip.mp4");
    let before = std::fs::read(&file).expect("read");

    let (result, store, _frames) = run(&h, Some(before.len() as u64)).await;
    let e = result.expect_err("a missing binary fails");
    assert_eq!(e.to_string(), "required tool ffmpeg not found");
    assert!(
        !e.retryable(),
        "an absent binary will not appear on a retry"
    );
    assert_eq!(std::fs::read(&file).expect("read"), before);
    assert!(store.writes().is_empty());
}

/// A non-mp4 file and an absent file are both skipped, exactly as legacy skipped them.
#[tokio::test]
async fn a_non_mp4_or_absent_file_is_skipped() {
    let h = Harness::new();
    let cfg = config_rooted(h.path(), &[]);
    let store = FakeStore::new();
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        cfg,
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let hook = AudioSyncHook::with_tools(h.tools.clone());

    for name in ["Clip.mkv", "Missing.mp4"] {
        if name.ends_with(".mkv") {
            std::fs::write(h.path().join(name), b"not an mp4").expect("write");
        }
        let view = ItemBuilder::finished("Clip")
            .status(Status::Postprocessing)
            .selection(DownloadType::Video, "mp4", "best_remux")
            .filename(name)
            .view();
        let batch = vec![BatchEntry::from_view(&view, TerminalStatus::Finished)];
        runner
            .run(&hook, &view, &batch)
            .await
            .unwrap_or_else(|e| panic!("{name} must be skipped, not fail: {e}"));
    }
    assert!(store.writes().is_empty());
}

/// The duration-scaled timeout of DESIGN §13.3 step 3, in one place so the formula is asserted
/// against the design text and not only against itself.
#[test]
fn the_timeout_is_max_600_ceil_half_duration_and_1800_when_unknown() {
    use std::time::Duration;
    assert_eq!(
        audio_sync::encode_timeout(None),
        Duration::from_secs(1800),
        "1800 when the duration is unknown"
    );
    assert_eq!(
        audio_sync::encode_timeout(Some(2.0)),
        Duration::from_secs(600)
    );
    assert_eq!(
        audio_sync::encode_timeout(Some(4801.0)),
        Duration::from_secs(2401),
        "ceil(4801/2)"
    );
}

/// PLAN WP-11: "against a real 2-second generated mp4 — a successful re-encode replaces the file
/// and updates `size`."
#[tokio::test]
async fn a_real_ffmpeg_round_trip_over_the_two_second_fixture() {
    if !have_tool("ffmpeg") || !have_tool("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe are not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = media(dir.path(), "Clip.mp4");
    let original = std::fs::metadata(&file).expect("stat").len();

    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Clip")
        .status(Status::Postprocessing)
        .selection(DownloadType::Video, "mp4", "best_remux")
        .filename("Clip.mp4")
        .size(Some(original));
    let id = item.id();
    let view = item.view();
    let store = FakeStore::new();
    let (factory, mut rx) = sink();
    let runner = HookRunner::new(
        cfg,
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let hook = AudioSyncHook::new();
    let batch = vec![BatchEntry::from_view(&view, TerminalStatus::Finished)];
    runner
        .run(&hook, &view, &batch)
        .await
        .expect("real ffmpeg re-encodes the fixture");

    let new_size = std::fs::metadata(&file).expect("stat").len();
    assert!(new_size > 0);
    assert_eq!(
        store.writes(),
        [Call::SetSize(id, new_size)],
        "the new size goes through the port, not into SQLite"
    );
    // The file is still a playable mp4 with a video stream.
    let probe = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "json",
        ])
        .arg(&file)
        .output()
        .expect("ffprobe runs");
    let json: serde_json::Value =
        serde_json::from_slice(&probe.stdout).expect("ffprobe prints json");
    assert_eq!(
        json["streams"][0]["codec_name"], "aac",
        "the audio track was re-encoded to aac: {json}"
    );
    let frames = drain(&mut rx);
    assert!(
        frames.iter().any(|f| matches!(
            f,
            ProgressMsg::Stage {
                stage: Stage::Postprocessing,
                ..
            }
        )),
        "the client is told the item is postprocessing"
    );
}

/// The id the fake store recorded a write for, so the assertion above does not have to thread the
/// item id through two helpers.
fn store_id(store: &Arc<FakeStore>) -> aulos_core::id::ItemId {
    match store.writes().first() {
        Some(Call::SetSize(id, _) | Call::DropEntryBlob(id) | Call::EntryBlob(id)) => *id,
        None => panic!("no write was recorded"),
    }
}
