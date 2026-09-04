//! The `--replay` suite: every frame type, every ordering violation and every §9.6 error class,
//! driven through the production consumer with no Python, no process and no network.
//!
//! This is the insurance policy against nightly yt-dlp churn (DESIGN §9.7). The transcripts under
//! `tests/fixtures/transcripts/` are checked in, and [`RunnerHandle::replay`] feeds them to the
//! *same* state machine `RunnerHandle::run` uses — so a regression in ordering checks, artifact
//! de-duplication, entry mapping or error classification fails here, in milliseconds, instead of
//! against YouTube.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use aulos_core::error::ErrorCode;
use aulos_core::id::ItemId;
use aulos_core::progress::Normalizer;
use aulos_core::status::Status;
use aulos_provider::entry::{EntryKind, LiveStatus};
use aulos_provider::provider::ProviderError;
use aulos_provider::sink::{ProgressMsg, ProgressSink, ProgressSinkFactory, Stage};
use aulos_provider_ytdlp::job::{Job, Policy};
use aulos_provider_ytdlp::runner::{EMPTY_DATA, RunnerHandle, RunnerOutcome};
use serde_json::Map;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

fn transcript(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts")
        .join(name)
}

fn url() -> Url {
    Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap()
}

/// A download job rooted at `/downloads`, so the produced paths rebase the way the engine expects.
fn download_job() -> Job {
    Job::download("01JBQ7Z5T9K3M2R8V4XW6Y0AAA", url())
        .with_download_root("/downloads")
        .with_policy(Policy {
            download_dir: PathBuf::from("/downloads"),
            ..Policy::default()
        })
}

fn sink() -> (ProgressSink, mpsc::Receiver<ProgressMsg>) {
    let (factory, rx) = ProgressSinkFactory::channel();
    (factory.for_item(ItemId::new()), rx)
}

async fn replay(name: &str, job: &Job) -> Result<RunnerOutcome, ProviderError> {
    let (sink, _rx) = sink();
    RunnerHandle::default()
        .replay(transcript(name), job, &sink, &CancellationToken::new())
        .await
}

/// Replays and returns the messages the sink received, so a test can assert on the *behaviour*
/// the aggregator would see rather than only on the return value.
async fn replay_with_messages(
    name: &str,
    job: &Job,
) -> (Result<RunnerOutcome, ProviderError>, Vec<ProgressMsg>) {
    let (sink, mut rx) = sink();
    let outcome = RunnerHandle::default()
        .replay(transcript(name), job, &sink, &CancellationToken::new())
        .await;
    drop(sink);
    let mut msgs = Vec::new();
    while let Some(m) = rx.recv().await {
        msgs.push(m);
    }
    (outcome, msgs)
}

// ---------------------------------------------------------------------------------------------
// Downloads
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_normal_download_produces_the_expected_outcome_and_stage_sequence() {
    let (outcome, msgs) = replay_with_messages("download_ok.jsonl", &download_job()).await;
    let RunnerOutcome::Downloaded(out) = outcome.unwrap() else {
        panic!("expected a download outcome")
    };
    assert_eq!(out.filename.as_ref().unwrap().as_str(), "Rick.mp4");
    assert_eq!(out.size, Some(62_390_272));
    assert!(out.chapter_files.is_empty() && out.subtitle_files.is_empty());

    let stages: Vec<Stage> = msgs
        .iter()
        .filter_map(|m| match m {
            ProgressMsg::Stage { stage, .. } => Some(*stage),
            _ => None,
        })
        .collect();
    // The first `progress` frame moves the item to `downloading`; `MoveFiles started` moves it to
    // `postprocessing`. Legacy could not express the second one at all.
    assert_eq!(
        stages,
        [Stage::Downloading, Stage::Postprocessing],
        "got {stages:?}"
    );
    assert_eq!(
        msgs.iter()
            .filter(|m| matches!(m, ProgressMsg::Progress { .. }))
            .count(),
        3
    );
}

#[tokio::test]
async fn a_merge_resets_the_percent_floor_between_legs() {
    let (outcome, msgs) = replay_with_messages("download_merge.jsonl", &download_job()).await;
    assert!(matches!(outcome.unwrap(), RunnerOutcome::Downloaded(_)));

    let mut norm = Normalizer::new();
    let percents: Vec<f64> = msgs
        .iter()
        .filter_map(|m| match m {
            ProgressMsg::Progress { raw, .. } => Some(norm.apply(raw, Status::Downloading)),
            _ => None,
        })
        .collect();
    assert_eq!(percents.len(), 4);
    // video leg: 90 % then the clamped finish; audio leg: back down to ~1.8 %, because the
    // `source_tag` changed. Without the reset the floor would pin it at 99.9.
    assert!((percents[0] - 90.0).abs() < 1e-6, "{percents:?}");
    assert!((percents[1] - 99.9).abs() < 1e-6, "{percents:?}");
    assert!(
        percents[2] < 5.0,
        "the audio leg must restart: {percents:?}"
    );
}

#[tokio::test]
async fn the_postprocessor_sequence_collects_chapters_and_subtitles_exactly_once() {
    let (outcome, msgs) = replay_with_messages("pp_sequence.jsonl", &download_job()).await;
    let RunnerOutcome::Downloaded(out) = outcome.unwrap() else {
        panic!("expected a download outcome")
    };

    // `MoveFiles` applied `__finaldir`, and the path is rebased onto the download root.
    assert_eq!(
        out.filename.as_ref().unwrap().as_str(),
        "Show/Season 01/S01E02.mkv"
    );
    let chapters: Vec<&str> = out.chapter_files.iter().map(|f| &*f.filename).collect();
    assert_eq!(
        chapters,
        ["S01E02 - 01 - Intro.mkv", "S01E02 - 02 - Outro.mkv"],
        "SplitChapters fires more than once; each path must appear once"
    );
    let subs: Vec<(&str, Option<&str>)> = out
        .subtitle_files
        .iter()
        .map(|f| (&*f.filename, f.lang.as_deref()))
        .collect();
    assert_eq!(
        subs,
        [
            ("Show/Season 01/S01E02.en.srt", Some("en")),
            ("Show/Season 01/S01E02.it.srt", Some("it")),
        ]
    );

    // Every artifact reached the engine losslessly, and none twice.
    let files: Vec<_> = msgs
        .iter()
        .filter_map(|m| match m {
            ProgressMsg::File { slot, file, .. } => Some((*slot, file.filename.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 4, "{files:?}");
}

#[tokio::test]
async fn a_phase_frame_updates_the_message_without_regressing_the_status() {
    let (_, msgs) = replay_with_messages("pp_sequence.jsonl", &download_job()).await;
    let phase = msgs
        .iter()
        .find_map(|m| match m {
            ProgressMsg::Stage {
                stage,
                msg: Some(msg),
                ..
            } if &**msg == "Converting captions" => Some(*stage),
            _ => None,
        })
        .expect("the phase frame must reach the sink");
    assert_eq!(
        phase,
        Stage::Postprocessing,
        "a phase frame carries the current stage, never an earlier one"
    );
}

#[tokio::test]
async fn a_non_zero_retcode_is_a_failure_even_without_an_error_frame() {
    let e = replay("download_retcode.jsonl", &download_job())
        .await
        .expect_err("retcode 1 must fail the item");
    assert_eq!(e.code(), ErrorCode::Internal);
    assert!(e.message().contains("code 1"), "{}", e.message());
}

#[tokio::test]
async fn an_unknown_frame_type_and_an_unknown_artifact_role_are_both_survivable() {
    let (outcome, msgs) = replay_with_messages("forward_compat.jsonl", &download_job()).await;
    let RunnerOutcome::Downloaded(out) = outcome.unwrap() else {
        panic!("expected a download outcome")
    };
    assert_eq!(out.filename.as_ref().unwrap().as_str(), "a.mp4");
    // A `thumbnail` artifact has no `FileSlot` in v1.0, so it is logged and dropped rather than
    // failing a download that otherwise succeeded.
    assert!(out.chapter_files.is_empty() && out.subtitle_files.is_empty());
    assert!(!msgs.iter().any(|m| matches!(m, ProgressMsg::File { .. })));
}

// ---------------------------------------------------------------------------------------------
// Extractions
// ---------------------------------------------------------------------------------------------

fn extract_job() -> Job {
    Job::extract("01JBQ7Z5T9K3M2R8V4XW6Y0AAA", url())
}

#[tokio::test]
async fn a_five_hundred_entry_playlist_becomes_one_container_with_ordered_children() {
    let RunnerOutcome::Extracted { entries, truncated } =
        replay("extract_playlist_500.jsonl", &extract_job())
            .await
            .unwrap()
    else {
        panic!("expected an extraction")
    };
    assert!(!truncated);
    assert_eq!(entries.len(), 1, "a container resolves to one shell entry");
    let parent = &entries[0];
    assert_eq!(&*parent.media_id, "PL9tY0BWXOZFv");
    assert_eq!(parent.hints.playlist_count, Some(500));

    let EntryKind::Playlist { title, entries } = &parent.kind else {
        panic!("expected a playlist kind")
    };
    assert_eq!(&**title, "Mix - lofi");
    assert_eq!(entries.len(), 500);
    assert_eq!(&*entries[0].media_id, "trk001");
    assert_eq!(&*entries[499].title, "Track 500");
    // The engine's group counters read these, so every child must carry its position.
    assert_eq!(entries[0].hints.playlist_index, Some(1));
    assert_eq!(entries[499].hints.playlist_index, Some(500));
    assert_eq!(entries[42].hints.playlist_count, Some(500));
    assert_eq!(
        entries[42].hints.playlist_title.as_deref(),
        Some("Mix - lofi")
    );
    assert_eq!(entries[7].live, LiveStatus::NotLive);
}

#[tokio::test]
async fn a_single_video_carries_the_full_info_dict_as_its_state() {
    let RunnerOutcome::Extracted { entries, .. } = replay("extract_single.jsonl", &extract_job())
        .await
        .unwrap()
    else {
        panic!("expected an extraction")
    };
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(&*entry.media_id, "dQw4w9WgXcQ");
    assert_eq!(&*entry.title, "Never Gonna Give You Up");
    assert_eq!(entry.kind, EntryKind::Video);
    assert!(entry.pre_error.is_none());
    assert_eq!(entry.hints.ext.as_deref(), Some("mp4"));
    assert_eq!(entry.hints.duration, Some(212.0));
    assert_eq!(entry.hints.filesize_approx, Some(58_720_256));
    assert_eq!(entry.hints.uploader.as_deref(), Some("Rick Astley"));
    // The `info` frame wins over the flat `entry` frame for a single video: it is the blob the
    // NFO hook and the entry-final projection want.
    assert!(entry.state["formats"].is_array(), "state = {}", entry.state);
}

#[tokio::test]
async fn an_upcoming_premiere_carries_a_pre_error_with_the_legacy_text() {
    let RunnerOutcome::Extracted { entries, .. } = replay("extract_upcoming.jsonl", &extract_job())
        .await
        .unwrap()
    else {
        panic!("expected an extraction")
    };
    let entry = &entries[0];
    assert_eq!(
        entry.live,
        LiveStatus::IsUpcoming {
            at: Some(1_788_700_800_000)
        }
    );
    let pre = entry
        .pre_error
        .as_ref()
        .expect("a note becomes a pre_error");
    assert_eq!(pre.code, ErrorCode::NotYetLive);
    assert_eq!(
        &*pre.message, "Live stream is scheduled to start at 2026-09-05 20:00:00 +0000",
        "the legacy text is preserved verbatim"
    );
}

#[tokio::test]
async fn a_truncated_extraction_reports_it() {
    let RunnerOutcome::Extracted { entries, truncated } =
        replay("extract_truncated.jsonl", &extract_job())
            .await
            .unwrap()
    else {
        panic!("expected an extraction")
    };
    assert!(truncated);
    assert_eq!(entries[0].children().len(), 2);
}

#[tokio::test]
async fn an_extraction_that_yielded_nothing_uses_the_verbatim_legacy_message() {
    let e = replay("extract_empty.jsonl", &extract_job())
        .await
        .expect_err("zero entries is an error");
    assert!(matches!(e, ProviderError::Unsupported(_)));
    assert_eq!(e.code(), ErrorCode::UnsupportedUrl);
    // The text is carried verbatim, with no prefix: DESIGN §8.4 and §11.7 need this string
    // byte-identical on the wire (the v1 shim echoes it as `{"status":"error","msg":…}`).
    assert_eq!(e.message(), EMPTY_DATA);
}

#[tokio::test]
async fn a_url_transparent_root_becomes_a_redirect_entry() {
    let RunnerOutcome::Extracted { entries, .. } = replay("extract_redirect.jsonl", &extract_job())
        .await
        .unwrap()
    else {
        panic!("expected an extraction")
    };
    assert_eq!(entries.len(), 1);
    let EntryKind::Redirect { url } = &entries[0].kind else {
        panic!("expected a redirect, got {:?}", entries[0].kind)
    };
    assert_eq!(url.as_str(), "https://elsewhere.test/real");
}

// ---------------------------------------------------------------------------------------------
// outtmpl and selftest
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_outtmpl_result_comes_back_in_request_order() {
    let job = Job::outtmpl(
        "j",
        vec![
            "%(playlist_title)s".to_owned(),
            "%(playlist_index)03d".to_owned(),
        ],
        Map::new(),
        vec!["playlist".to_owned()],
    );
    let RunnerOutcome::OutTmpl(evaluated) = replay("outtmpl.jsonl", &job).await.unwrap() else {
        panic!("expected an outtmpl outcome")
    };
    assert_eq!(evaluated, ["Mix - lofi", "103"]);
}

#[tokio::test]
async fn a_selftest_reports_the_shim_identity() {
    let handle = RunnerHandle::default();
    let (sink, _rx) = sink();
    let job = Job::selftest("probe");
    let RunnerOutcome::Selftest(identity) = handle
        .replay(
            transcript("selftest.jsonl"),
            &job,
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    else {
        panic!("expected a selftest outcome")
    };
    assert_eq!(identity.yt_dlp.as_deref(), Some("2026.8.30.232658.dev0"));
    assert_eq!(identity.python.as_deref(), Some("3.13.2"));
    assert_eq!(identity.plugins, ["bgutil_ytdlp_pot_provider"]);
    assert!(identity.pot_available);
    assert_eq!(identity.pot_url.as_deref(), Some("http://127.0.0.1:4416"));
    // The handle remembers it, which is what `healthz` and `GET <p>version` read.
    assert_eq!(handle.identity(), Some(identity));
}

// ---------------------------------------------------------------------------------------------
// The error taxonomy
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_error_code_in_the_design_table_has_a_transcript_and_maps_correctly() {
    let expected: &[(&str, ErrorCode, bool)] = &[
        ("canceled", ErrorCode::Canceled, false),
        ("unsupported_url", ErrorCode::UnsupportedUrl, false),
        ("auth_required", ErrorCode::AuthRequired, false),
        ("geo_restricted", ErrorCode::GeoRestricted, false),
        ("unavailable", ErrorCode::Unavailable, false),
        ("not_yet_live", ErrorCode::NotYetLive, false),
        ("no_format", ErrorCode::NoFormat, false),
        ("bot_check", ErrorCode::BotCheck, false),
        ("network", ErrorCode::Network, true),
        ("throttled", ErrorCode::Throttled, true),
        (
            "postprocessing_failed",
            ErrorCode::PostprocessingFailed,
            false,
        ),
        ("disk_full", ErrorCode::DiskFull, false),
        ("timeout", ErrorCode::Timeout, false),
        ("bad_job", ErrorCode::Contract, false),
        ("internal", ErrorCode::Internal, false),
    ];
    assert_eq!(
        expected.len(),
        aulos_provider_ytdlp::known_codes().len(),
        "the fixture table and the code table must agree"
    );

    for (code, want, retryable) in expected {
        let e = replay(&format!("errors/{code}.jsonl"), &download_job())
            .await
            .expect_err(code);
        assert_eq!(e.code(), *want, "{code}");
        assert_eq!(e.retryable(), *retryable, "{code}");
        assert!(e.code().item_terminal(), "{code}");
        assert!(!e.message().is_empty(), "{code}");
    }
}

// ---------------------------------------------------------------------------------------------
// Ordering violations
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_ordering_violation_is_a_contract_failure() {
    let cases = [
        ("violation_gap.jsonl", "sequence gap"),
        ("violation_no_bye.jsonl", "no bye"),
        ("violation_no_hello.jsonl", "not hello"),
        (
            "violation_no_terminal.jsonl",
            "neither a result nor an error",
        ),
        ("violation_after_terminal.jsonl", "after its terminator"),
        ("violation_two_terminals.jsonl", "after its terminator"),
        ("violation_bad_protocol.jsonl", "protocol 2"),
        ("violation_envelope_version.jsonl", "envelope version 2"),
    ];
    for (name, needle) in cases {
        let e = replay(name, &download_job()).await.expect_err(name);
        assert_eq!(e.code(), ErrorCode::Contract, "{name}");
        assert!(
            e.message().contains(needle),
            "{name}: {:?} does not mention {needle:?}",
            e.message()
        );
    }
}

#[tokio::test]
async fn a_line_over_the_cap_is_a_contract_failure_rather_than_an_allocation() {
    let handle = RunnerHandle::default().with_max_line_bytes(1024);
    let (sink, _rx) = sink();
    let e = handle
        .replay(
            transcript("violation_oversized.jsonl"),
            &download_job(),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .expect_err("an over-long line must be rejected");
    assert_eq!(e.code(), ErrorCode::Contract);
    assert!(e.message().contains("1024"), "{}", e.message());
}

#[tokio::test]
async fn a_replay_observes_cancellation() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (sink, _rx) = sink();
    let e = RunnerHandle::default()
        .replay(
            transcript("extract_playlist_500.jsonl"),
            &extract_job(),
            &sink,
            &cancel,
        )
        .await
        .expect_err("a cancelled token stops the replay");
    assert!(matches!(e, ProviderError::Canceled));
}

#[tokio::test]
async fn a_missing_transcript_is_reported_not_panicked() {
    let (sink, _rx) = sink();
    let e = RunnerHandle::default()
        .replay(
            transcript("does-not-exist.jsonl"),
            &download_job(),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .expect_err("a missing file must not panic");
    assert!(e.message().contains("does-not-exist"), "{}", e.message());
}

/// `Arc` is only used here to prove the handle is shareable, which the provider relies on.
#[tokio::test]
async fn a_handle_is_cheap_to_share_and_shares_its_identity() {
    let handle = Arc::new(RunnerHandle::default());
    let clone = (*handle).clone();
    let (sink, _rx) = sink();
    let job = Job::selftest("probe");
    clone
        .replay(
            transcript("selftest.jsonl"),
            &job,
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        handle.identity().is_some(),
        "a clone must share the identity slot with its origin"
    );
}
