//! The real transport: spawn, fd 3, stdout isolation, cancellation, `killpg`, the stderr drain
//! and the line cap.
//!
//! These tests run the **actual** `python/ytdlp_runner.py` against a **stubbed `yt_dlp`** on
//! `PYTHONPATH` (`tests/fixtures/pystub/`), so they need an interpreter but no network, no
//! yt-dlp and no ffmpeg. The stub is scripted through a JSON scenario file, which is how one
//! transcript shape after another gets produced without pretending to download anything.
//!
//! The load-bearing one is [`stdout_pollution_cannot_corrupt_the_protocol`]: the stub prints a
//! forged frame to stdout on import, a stub *plugin* prints another on import, and a
//! "postprocessor" prints a third during the download. If the protocol were on stdout — as
//! BRIEF §9 says — any one of them would corrupt the stream. This is the regression test for the
//! whole fd-3 decision (DESIGN §9.1, §23.1 B1).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use aulos_core::error::ErrorCode;
use aulos_core::id::ItemId;
use aulos_provider::entry::EntryKind;
use aulos_provider::provider::ProviderError;
use aulos_provider::sink::{ProgressMsg, ProgressSink, ProgressSinkFactory};
use aulos_provider_ytdlp::job::{ExtractOpts, Job, Policy};
use aulos_provider_ytdlp::runner::{RunnerHandle, RunnerOutcome};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python/ytdlp_runner.py")
}

/// The interpreter to run the shim under. `AULOS_TEST_PYTHON` overrides it, which is how the
/// optional real-yt-dlp smoke of `real_ytdlp.rs` points at a venv.
fn python() -> String {
    std::env::var("AULOS_TEST_PYTHON").unwrap_or_else(|_| "python3".to_owned())
}

/// Whether an interpreter is available at all. Everything here is skipped without one.
fn have_python() -> bool {
    std::process::Command::new(python())
        .arg("-c")
        .arg("import sys")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A handle that runs the shim with the stubbed `yt_dlp` on `PYTHONPATH`.
fn handle(scenario: Option<&Path>) -> RunnerHandle {
    let mut h = RunnerHandle::new(python(), shim())
        .with_env_var("PYTHONPATH", fixtures().join("pystub"))
        .with_kill_grace(Duration::from_millis(300))
        .with_stall(None);
    if let Some(path) = scenario {
        h = h.with_env_var("AULOS_STUB_SCENARIO", path);
    }
    h
}

/// Writes a stub scenario and returns its path.
fn scenario(dir: &TempDir, value: &Value) -> PathBuf {
    let path = dir.path().join("scenario.json");
    std::fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    path
}

fn sink() -> (ProgressSink, mpsc::Receiver<ProgressMsg>) {
    let (factory, rx) = ProgressSinkFactory::channel();
    (factory.for_item(ItemId::new()), rx)
}

fn url() -> Url {
    Url::parse("https://stub.test/watch/1").unwrap()
}

fn download_job(dir: &TempDir) -> Job {
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    Job::download("01JBQ7Z5T9K3M2R8V4XW6Y0AAA", url())
        .with_download_root(&out)
        .with_policy(Policy {
            download_dir: out.clone(),
            temp_dir: out,
            emit_progress_every_ms: 0,
            ..Policy::default()
        })
}

macro_rules! skip_without_python {
    () => {
        if !have_python() {
            eprintln!("skipping: no {} on PATH", python());
            return;
        }
    };
}

// ---------------------------------------------------------------------------------------------
// The regression test the whole design exists for
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stdout_pollution_cannot_corrupt_the_protocol() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let media = out.join("clip.mp4");

    // Three separate sources of stdout noise, each of which is a real thing in production:
    // the stub `yt_dlp` prints on import, `yt_dlp_plugins.extractor.noisy` prints on import
    // (like the BgUtils POT plugin), and these lines stand in for a postprocessor or a `deno`
    // grandchild. Two of them are *forged frames*, so a stdout protocol would not merely be
    // noisy — it would be wrong.
    let path = scenario(
        &dir,
        &json!({
            "print_stdout": [
                "POSTPROCESSOR NOISE on stdout",
                r#"{"v":1,"t":"error","n":2,"code":"internal","message":"forged"}"#,
                r#"{"v":1,"t":"bye","n":3}"#
            ],
            "write_files": [media],
            "progress": [
                { "status": "downloading", "tmpfilename": format!("{}.part", media.display()),
                  "downloaded_bytes": 6, "total_bytes": 13,
                  "info_dict": { "vcodec": "h264", "acodec": "none" } },
                { "status": "finished", "filename": media,
                  "downloaded_bytes": 13, "total_bytes": 13,
                  "info_dict": { "vcodec": "h264" } }
            ],
            "pp": [
                { "postprocessor": "MoveFiles", "status": "started",
                  "info_dict": { "filepath": media } },
                { "postprocessor": "MoveFiles", "status": "finished",
                  "info_dict": { "filepath": media } }
            ]
        }),
    );

    let job = Job::download("j", url())
        .with_download_root(&out)
        .with_policy(Policy {
            download_dir: out.clone(),
            temp_dir: out,
            emit_progress_every_ms: 0,
            ..Policy::default()
        });
    let (sink, _rx) = sink();
    let outcome = handle(Some(&path))
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("the run must succeed despite the stdout noise");

    let RunnerOutcome::Downloaded(out) = outcome else {
        panic!("expected a download outcome")
    };
    assert_eq!(out.filename.as_ref().unwrap().as_str(), "clip.mp4");
    assert_eq!(out.size, Some(13));
}

// ---------------------------------------------------------------------------------------------
// The four modes over the real pipe
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_selftest_reports_the_interpreter_and_the_loaded_plugins() {
    skip_without_python!();
    let h = handle(None);
    let (sink, _rx) = sink();
    let RunnerOutcome::Selftest(identity) = h
        .run(&Job::selftest("probe"), &sink, &CancellationToken::new())
        .await
        .expect("selftest")
    else {
        panic!("expected a selftest outcome")
    };
    assert_eq!(identity.yt_dlp.as_deref(), Some("2026.8.30.232658.dev0"));
    assert!(identity.python.is_some());
    // `yt_dlp_plugins.extractor.noisy` is loaded by the stub's `load_all_plugins`.
    assert_eq!(identity.plugins, ["noisy"]);
    assert_eq!(h.identity(), Some(identity));
}

#[tokio::test]
async fn an_extract_streams_a_playlist_over_the_pipe() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let path = scenario(&dir, &json!({ "extract_entries": 250 }));
    let job = Job::extract("j", url()).with_extract(ExtractOpts {
        max_entries: 100,
        ..ExtractOpts::default()
    });
    let (sink, _rx) = sink();
    let RunnerOutcome::Extracted { entries, truncated } = handle(Some(&path))
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("extract")
    else {
        panic!("expected an extraction")
    };
    assert!(truncated, "max_entries must cut the list short");
    let EntryKind::Playlist { entries, .. } = &entries[0].kind else {
        panic!("expected a playlist")
    };
    assert_eq!(entries.len(), 100);
    assert_eq!(&*entries[99].media_id, "v100");
}

#[tokio::test]
async fn an_outtmpl_round_trip_uses_the_shims_own_evaluator() {
    skip_without_python!();
    let mut info = serde_json::Map::new();
    info.insert("playlist_title".to_owned(), json!("Mix - lofi"));
    info.insert("playlist_index".to_owned(), json!(3));
    let job = Job::outtmpl(
        "j",
        vec![
            "%(playlist_title)s".to_owned(),
            "%(playlist_index)s".to_owned(),
        ],
        info,
        vec!["playlist".to_owned()],
    );
    let (sink, _rx) = sink();
    let RunnerOutcome::OutTmpl(evaluated) = handle(None)
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("outtmpl")
    else {
        panic!("expected an outtmpl outcome")
    };
    assert_eq!(evaluated, ["Mix - lofi", "3"]);
}

// ---------------------------------------------------------------------------------------------
// Cancellation and the process group
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_kills_the_whole_group_and_cleans_the_partials() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out");
    let pid_file = dir.path().join("grandchild.pid");
    let partial = out.join("hanging.mp4.part");

    let job = Job::download("j", url()).with_policy(Policy {
        download_dir: out.clone(),
        temp_dir: out.clone(),
        ..Policy::default()
    });

    // The runner passes exactly one argument — the script path — which is what production does.
    // `hang.py` needs two positional arguments, so it runs through a tiny argv wrapper.
    let wrapper = dir.path().join("hang_wrapper.py");
    std::fs::write(
        &wrapper,
        format!(
            "import runpy, sys\nsys.argv = ['hang.py', {out:?}, {pid:?}]\nrunpy.run_path({script:?}, run_name='__main__')\n",
            out = out.to_string_lossy(),
            pid = pid_file.to_string_lossy(),
            script = fixtures().join("scripts/hang.py").to_string_lossy(),
        ),
    )
    .unwrap();

    let h = RunnerHandle::new(python(), &wrapper)
        .with_kill_grace(Duration::from_millis(200))
        .with_stall(None);
    let (sink, _rx) = sink();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let e = h
        .run(&job, &sink, &cancel)
        .await
        .expect_err("a cancelled job must not succeed");
    let elapsed = started.elapsed();

    assert!(matches!(e, ProviderError::Canceled), "{e:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the kill must land inside the grace window, took {elapsed:?}"
    );
    assert!(
        !partial.exists(),
        "the partial file must be cleaned up (Δ C18)"
    );

    // The grandchild ignores SIGTERM too, so only a `killpg` + SIGKILL reaches it. This is the
    // legacy bug: `proc.kill()` left ffmpeg writing into a cancelled item's `.part`.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the script must have recorded its grandchild")
        .trim()
        .parse()
        .unwrap();
    let mut alive = true;
    for _ in 0..100 {
        alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !alive,
        "the grandchild {pid} survived the process-group kill"
    );
}

#[tokio::test]
async fn an_already_cancelled_token_stops_before_any_frame_is_consumed() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let path = scenario(&dir, &json!({ "sleep": 30 }));
    let (sink, _rx) = sink();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let e = handle(Some(&path))
        .run(&download_job(&dir), &sink, &cancel)
        .await
        .expect_err("a pre-cancelled token must abort the run");
    assert!(matches!(e, ProviderError::Canceled), "{e:?}");
}

#[tokio::test]
async fn the_hard_timer_cancels_a_wedged_job() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let path = scenario(&dir, &json!({ "sleep": 30 }));
    let (sink, _rx) = sink();
    let e = handle(Some(&path))
        .with_timeout(Some(Duration::from_millis(400)))
        .run(&download_job(&dir), &sink, &CancellationToken::new())
        .await
        .expect_err("the hard deadline must fire");
    assert_eq!(e.code(), ErrorCode::Timeout, "{e:?}");
}

// ---------------------------------------------------------------------------------------------
// Contract violations over the real pipe
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_line_kills_the_child_and_reports_a_contract_failure() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let h = RunnerHandle::new(python(), fixtures().join("scripts/oversized.py"))
        .with_max_line_bytes(4096)
        .with_kill_grace(Duration::from_millis(200))
        .with_stall(None);
    let (sink, _rx) = sink();
    let e = h
        .run(&download_job(&dir), &sink, &CancellationToken::new())
        .await
        .expect_err("an over-long line must be rejected");
    assert_eq!(e.code(), ErrorCode::Contract, "{e:?}");
    assert!(e.message().contains("4096"), "{}", e.message());
}

#[tokio::test]
async fn a_shim_that_writes_no_frames_reports_its_exit_code_and_stderr_tail() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let wrapper = dir.path().join("silent_wrapper.py");
    std::fs::write(
        &wrapper,
        format!(
            "import runpy, sys\nsys.argv = ['silent.py', '3']\nrunpy.run_path({script:?}, run_name='__main__')\n",
            script = fixtures().join("scripts/silent.py").to_string_lossy(),
        ),
    )
    .unwrap();
    let h = RunnerHandle::new(python(), &wrapper).with_stall(None);
    let (sink, _rx) = sink();
    let e = h
        .run(&download_job(&dir), &sink, &CancellationToken::new())
        .await
        .expect_err("a frameless run is a contract failure");
    assert_eq!(e.code(), ErrorCode::Contract, "{e:?}");
    assert!(e.message().contains("no hello"), "{}", e.message());
    assert!(e.message().contains("exit code 3"), "{}", e.message());
    assert!(
        e.message().contains("could not start"),
        "the stderr tail must be attached: {}",
        e.message()
    );
}

#[tokio::test]
async fn a_missing_interpreter_is_a_tool_missing_error() {
    let h = RunnerHandle::new("/nonexistent/python-does-not-exist", shim());
    let (sink, _rx) = sink();
    let dir = TempDir::new().unwrap();
    let e = h
        .run(&download_job(&dir), &sink, &CancellationToken::new())
        .await
        .expect_err("a missing interpreter must be reported");
    assert_eq!(e.code(), ErrorCode::ToolMissing, "{e:?}");
    assert!(matches!(e, ProviderError::ToolMissing("python3")));
}

// ---------------------------------------------------------------------------------------------
// The stderr drain
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_megabyte_of_stderr_does_not_deadlock_the_child() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let media = out.join("noisy.mp4");
    let path = scenario(
        &dir,
        &json!({
            "stderr_bytes": 1_048_576,
            "write_files": [media],
            "progress": [{ "status": "finished", "filename": media,
                           "downloaded_bytes": 13, "total_bytes": 13,
                           "info_dict": { "vcodec": "h264" } }],
            "pp": [{ "postprocessor": "MoveFiles", "status": "finished",
                     "info_dict": { "filepath": media } }]
        }),
    );
    let (sink, _rx) = sink();
    // A 64 KiB stderr pipe with nobody reading it blocks the child's next `write(2)` forever.
    // `aulos_provider::proc::Child` starts the drain before returning, which is what makes this
    // complete at all.
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        handle(Some(&path)).run(&download_job(&dir), &sink, &CancellationToken::new()),
    )
    .await
    .expect("a megabyte of stderr must not deadlock the child")
    .expect("the run must still succeed");
    assert!(matches!(outcome, RunnerOutcome::Downloaded(_)));
}

#[tokio::test]
async fn the_last_stderr_is_available_in_the_error_tail() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let path = scenario(&dir, &json!({ "stderr_bytes": 1_048_576, "retcode": 1 }));
    let (sink, _rx) = sink();
    let e = handle(Some(&path))
        .run(&download_job(&dir), &sink, &CancellationToken::new())
        .await
        .expect_err("retcode 1 must fail");
    assert!(
        e.message().contains("STDERR TAIL MARKER"),
        "the tail must survive a megabyte of noise: {}",
        e.message()
    );
}

// ---------------------------------------------------------------------------------------------
// Error classification, end to end through the real classifier
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_shims_own_classifier_produces_the_design_codes() {
    skip_without_python!();
    let cases: &[(Value, ErrorCode)] = &[
        (
            json!({ "class": "UnsupportedError", "message": "Unsupported URL: https://nope.test" }),
            ErrorCode::UnsupportedUrl,
        ),
        (
            json!({ "class": "GeoRestrictedError", "message": "not available in your country" }),
            ErrorCode::GeoRestricted,
        ),
        (
            json!({ "class": "ExtractorError", "message": "Sign in to confirm your age" }),
            ErrorCode::AuthRequired,
        ),
        (
            json!({ "class": "ExtractorError", "message": "Video unavailable" }),
            ErrorCode::Unavailable,
        ),
        (
            json!({ "class": "ExtractorError", "message": "This live event premieres in 3 hours" }),
            ErrorCode::NotYetLive,
        ),
        (
            json!({ "class": "DownloadError", "message": "Requested format is not available" }),
            ErrorCode::NoFormat,
        ),
        (
            json!({ "class": "ExtractorError", "message": "Sign in to confirm you're not a bot" }),
            ErrorCode::BotCheck,
        ),
        (
            json!({ "class": "DownloadError", "message": "HTTP Error 503: Service Unavailable" }),
            ErrorCode::Network,
        ),
        (
            json!({ "class": "DownloadError", "message": "unreachable", "wraps": "URLError" }),
            ErrorCode::Network,
        ),
        (
            json!({ "class": "DownloadError", "message": "HTTP Error 429: Too Many Requests" }),
            ErrorCode::Throttled,
        ),
        (
            json!({ "class": "PostProcessingError", "message": "ffmpeg exited with code 1" }),
            ErrorCode::PostprocessingFailed,
        ),
        (
            json!({ "class": "OSError", "message": "No space left on device", "errno": 28 }),
            ErrorCode::DiskFull,
        ),
        (
            json!({ "class": "KeyboardInterrupt", "message": "terminated" }),
            ErrorCode::Canceled,
        ),
        (
            json!({ "class": "RuntimeError", "message": "a bug" }),
            ErrorCode::Internal,
        ),
    ];

    for (spec, want) in cases {
        let dir = TempDir::new().unwrap();
        let path = scenario(&dir, &json!({ "raise": spec }));
        let (sink, _rx) = sink();
        let e = handle(Some(&path))
            .run(&download_job(&dir), &sink, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(e.code(), *want, "{spec} produced {e:?}");
        assert!(e.code().item_terminal(), "{spec}");
    }
}

#[tokio::test]
async fn an_unknown_coercion_is_a_bad_job_and_the_shim_still_writes_a_full_transcript() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let mut job = download_job(&dir);
    job.coerce
        .insert("impersonate".to_owned(), "NotAThing".to_owned());
    job.options
        .insert("impersonate".to_owned(), json!("chrome"));
    let (sink, _rx) = sink();
    let e = handle(None)
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect_err("an unknown coercion must fail the job");
    // `bad_job` maps to `contract`: Rust built the job, so the shim rejecting it is a protocol
    // violation, not a user error.
    assert_eq!(e.code(), ErrorCode::Contract, "{e:?}");
    assert!(e.message().contains("NotAThing"), "{}", e.message());
}

#[tokio::test]
async fn an_impersonate_string_is_coerced_not_rejected() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let path = scenario(&dir, &json!({ "extract_entries": 2 }));
    let mut options = serde_json::Map::new();
    options.insert("impersonate".to_owned(), json!("chrome-120"));
    let job = Job::extract("j", url()).with_options(options);
    assert_eq!(
        job.coerce.get("impersonate").map(String::as_str),
        Some("ImpersonateTarget")
    );
    let (sink, _rx) = sink();
    let outcome = handle(Some(&path))
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("a coercible impersonate target must not fail the job");
    assert!(matches!(outcome, RunnerOutcome::Extracted { .. }));
}

// ---------------------------------------------------------------------------------------------
// Captions and thumbnails: the two policy decisions the shim makes locally
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_captions_policy_converts_srt_to_txt_and_drops_media_placeholders() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let srt = out.join("Clip.en.srt");
    std::fs::write(
        &srt,
        "1\n00:00:01,000 --> 00:00:02,000\n<i>Hello</i> there\n\n2\n00:00:03,000 --> 00:00:04,000\nSecond cue\n",
    )
    .unwrap();

    let path = scenario(
        &dir,
        &json!({
            "pp": [{
                "postprocessor": "MoveFiles", "status": "finished",
                "info_dict": {
                    // A media-like placeholder: legacy dropped it by extension in captions mode.
                    "filepath": out.join("Clip.mp4"),
                    "requested_subtitles": { "en": { "filepath": srt, "ext": "srt" } }
                }
            }]
        }),
    );

    let job = Job::download("j", url())
        .with_download_root(&out)
        .with_policy(Policy {
            download_type: aulos_core::selection::DownloadType::Captions,
            download_dir: out.clone(),
            temp_dir: out.clone(),
            convert_srt_to_txt: true,
            emit_progress_every_ms: 0,
            ..Policy::default()
        });
    let (sink, _rx) = sink();
    let RunnerOutcome::Downloaded(outcome) = handle(Some(&path))
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("captions download")
    else {
        panic!("expected a download outcome")
    };

    let txt = out.join("Clip.en.txt");
    assert!(txt.exists(), "the .txt must be written");
    assert!(!srt.exists(), "the source .srt must be removed");
    assert_eq!(
        std::fs::read_to_string(&txt).unwrap(),
        "Hello there\nSecond cue\n",
        "cue numbers, timestamps and tags are stripped"
    );
    let names: Vec<&str> = outcome
        .subtitle_files
        .iter()
        .map(|f| &*f.filename)
        .collect();
    assert_eq!(names, ["Clip.en.txt"]);
    assert_eq!(
        outcome.filename.as_ref().unwrap().as_str(),
        "Clip.en.txt",
        "captions mode links the caption file, never the media placeholder"
    );
}

#[tokio::test]
async fn a_thumbnail_download_rewrites_a_webm_path_to_jpg() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let path = scenario(
        &dir,
        &json!({
            "pp": [{ "postprocessor": "MoveFiles", "status": "finished",
                     "info_dict": { "filepath": out.join("Clip.webm") } }]
        }),
    );
    let job = Job::download("j", url())
        .with_download_root(&out)
        .with_policy(Policy {
            download_type: aulos_core::selection::DownloadType::Thumbnail,
            download_dir: out.clone(),
            temp_dir: out.clone(),
            thumbnail_ext_rewrite: true,
            ..Policy::default()
        });
    let (sink, _rx) = sink();
    let RunnerOutcome::Downloaded(outcome) = handle(Some(&path))
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("thumbnail download")
    else {
        panic!("expected a download outcome")
    };
    assert_eq!(outcome.filename.as_ref().unwrap().as_str(), "Clip.jpg");
}

// ---------------------------------------------------------------------------------------------
// The shim-side progress rate limit
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_shim_rate_limits_progress_per_stream_but_never_a_terminal_frame() {
    skip_without_python!();
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let media = out.join("clip.mp4");

    // 40 `downloading` frames across two streams, then one `finished` per stream. With a 10 s
    // budget only the first frame of each stream survives the limit, and both `finished` frames
    // pass unconditionally.
    let mut progress: Vec<Value> = Vec::new();
    for stream in ["h264", "none"] {
        for i in 1..=20 {
            progress.push(json!({
                "status": "downloading",
                "downloaded_bytes": i * 10,
                "total_bytes": 200,
                "info_dict": { "vcodec": stream, "acodec": if stream == "none" { "aac" } else { "none" } }
            }));
        }
        progress.push(json!({
            "status": "finished",
            "filename": media,
            "downloaded_bytes": 200,
            "total_bytes": 200,
            "info_dict": { "vcodec": stream, "acodec": if stream == "none" { "aac" } else { "none" } }
        }));
    }

    let path = scenario(
        &dir,
        &json!({ "write_files": [media], "progress": progress }),
    );
    let job = Job::download("j", url())
        .with_download_root(&out)
        .with_policy(Policy {
            download_dir: out.clone(),
            temp_dir: out,
            emit_progress_every_ms: 10_000,
            ..Policy::default()
        });

    let (sink, mut rx) = sink();
    handle(Some(&path))
        .run(&job, &sink, &CancellationToken::new())
        .await
        .expect("download");
    drop(sink);

    let mut frames = 0;
    while let Some(msg) = rx.recv().await {
        if matches!(msg, ProgressMsg::Progress { .. }) {
            frames += 1;
        }
    }
    assert_eq!(
        frames, 4,
        "one `downloading` per stream plus both `finished` frames"
    );
}
