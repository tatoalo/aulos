//! What a row is left carrying after the terminal write (DESIGN §8.7, §8.10, §13; PROTOCOL §2.4,
//! §3.1).
//!
//! `msg` is the **live** status line — the stage label a provider last wrote, or the pre-terminal
//! hook's label — and production shipped it straight into the terminal row: yt-dlp's last
//! postprocessor frame (`"MoveFiles…"`, DESIGN §9.5) was still there on every finished download,
//! so a completed item rendered as a job stuck in its final postprocessor. These are the
//! regressions for that, and for the late frames that could put it back.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use aulos_core::{ErrorCode, Status};
use aulos_provider::Stage;
use aulos_provider::fake::FakeProvider;
use support::{AlwaysPreTerminal, Harness};

/// The postprocessor line yt-dlp's shim writes for its last `pp` frame (DESIGN §9.5).
const MOVE_FILES: &str = "MoveFiles…";

/// A provider that holds the item in `downloading` long enough for a test to slip a postprocessor
/// frame in behind it — the same race production loses — and then finishes.
fn slow_finish() -> FakeProvider {
    scripted("{ kind = \"finish\", filename = \"A video.mp4\", size = 1024 }")
}

/// The same, failing instead of finishing. `unavailable` is not retryable (DESIGN §8.8), so the
/// item terminates rather than arming a backoff.
fn slow_failure() -> FakeProvider {
    scripted("{ kind = \"fail\", code = \"unavailable\" }")
}

/// `preparing → downloading → a window a test can act in → `last``.
fn scripted(last: &str) -> FakeProvider {
    FakeProvider::from_toml(&format!(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        download = [
            {{ kind = "stage", stage = "preparing" }},
            {{ kind = "stage", stage = "downloading" }},
            {{ kind = "wait", ms = 900 }},
            {last},
        ]
    "#
    ))
    .unwrap()
}

/// The bug, end to end: a postprocessor line is on the row while it runs, and gone the moment it
/// is `finished` — on the persisted row and on the view the `completed` frame is built from.
#[tokio::test]
async fn a_finished_item_carries_no_postprocessor_message() {
    let h = Harness::builder()
        .provider(Arc::new(slow_finish()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/pp").await;
    h.until_status(id, Status::Downloading).await;

    // Exactly what the shim emits for `{"t":"pp","postprocessor":"MoveFiles","status":"started"}`.
    h.sink
        .for_item(id)
        .stage(Stage::Postprocessing, Some(MOVE_FILES.into()))
        .await;
    let running = h
        .until(id, "the postprocessor line", |i| {
            i.msg.as_deref() == Some(MOVE_FILES)
        })
        .await;
    assert_eq!(
        running.status,
        Status::Postprocessing,
        "the line belongs to a real state while it is live"
    );

    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(
        done.msg, None,
        "a finished row's status line is null, not the last postprocessor it ran"
    );
    assert_eq!(done.error, None);
    assert!(done.filename.is_some(), "and it really did produce a file");

    let completed = h.events.completed();
    assert_eq!(completed.len(), 1, "one terminal frame");
    assert_eq!(completed[0].id, id);
    assert_eq!(
        completed[0].msg, None,
        "the published view is what the wire's `completed` frame carries"
    );
}

/// The other half of the rule: the clear is on **success**, not on every terminal write. Legacy
/// overloaded `msg` with the failure text and the v1 shim still projects it that way, so a failed
/// row keeps its last line.
#[tokio::test]
async fn a_failed_item_keeps_its_message_beside_its_error() {
    let h = Harness::builder()
        .provider(Arc::new(slow_failure()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/pp").await;
    h.until_status(id, Status::Downloading).await;

    h.sink
        .for_item(id)
        .stage(Stage::Postprocessing, Some(MOVE_FILES.into()))
        .await;
    h.until(id, "the postprocessor line", |i| {
        i.msg.as_deref() == Some(MOVE_FILES)
    })
    .await;

    let failed = h.until_status(id, Status::Error).await;
    assert_eq!(
        failed.msg.as_deref(),
        Some(MOVE_FILES),
        "an errored row keeps the last thing it was doing"
    );
    assert_eq!(failed.error.unwrap().code, ErrorCode::Unavailable);
}

/// The pre-terminal phase writes its own label (DESIGN §13). It is a live line like any other, so
/// the terminal write drops it too.
#[tokio::test]
async fn a_pre_terminal_hook_label_does_not_survive_the_terminal_write() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/remux").await;
    h.until(id, "the pre-terminal phase", |i| {
        i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
    })
    .await;

    h.handle.hooks_finished(id).await;
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.msg, None, "the hook's label is not a terminal summary");
    assert_eq!(done.size, Some(1_024), "and the outcome still landed");
}

/// A stage frame the provider emitted before it finished can be handled after the outcome was:
/// `ProgressMsg::Stage` goes provider → aggregator → engine, `EngineCmd::Finished` goes provider →
/// engine. On a terminal row the late frame is dropped, not written.
#[tokio::test]
async fn a_stage_frame_that_arrives_after_the_terminal_write_is_refused() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/x").await;
    h.until_status(id, Status::Finished).await;

    h.sink
        .for_item(id)
        .stage(Stage::Postprocessing, Some(MOVE_FILES.into()))
        .await;
    h.settle().await;

    let done = h.item(id).await.unwrap();
    assert_eq!(done.status, Status::Finished, "no edge out of terminal");
    assert_eq!(done.msg, None, "and nothing put the stale line back");
}

/// The same race one step earlier: between `Finished` and `HooksFinished` the row is not terminal
/// yet, but the download is over and the pre-terminal phase owns its `msg` and `status`
/// (DESIGN §13). A late frame there must not overwrite the hook's label either.
#[tokio::test]
async fn a_stage_frame_that_arrives_during_the_finalizer_phase_is_refused() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/remux").await;
    h.until(id, "the pre-terminal phase", |i| {
        i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
    })
    .await;

    h.sink
        .for_item(id)
        .stage(Stage::Downloading, Some(MOVE_FILES.into()))
        .await;
    h.settle().await;

    let parked = h.item(id).await.unwrap();
    assert_eq!(
        parked.status,
        Status::Postprocessing,
        "a late frame cannot drag a finalising row back to downloading"
    );
    assert_eq!(
        parked.msg.as_deref(),
        Some("Re-encoding audio"),
        "the hook owns the line until HooksFinished"
    );

    h.handle.hooks_finished(id).await;
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.msg, None);
}
