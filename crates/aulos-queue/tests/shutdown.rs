//! `EngineCmd::Shutdown` — DESIGN §16.4 steps 5 and 6, from inside the engine.
//!
//! Two claims, and both of them used to be `aulos-server`'s problem:
//!
//! 1. the engine's loop **ends**, which "dropping the last `EngineHandle`" cannot achieve on its
//!    own (the engine hands a clone of its own sender to every job task it spawns);
//! 2. a download interrupted by a shutdown is handed to the next boot as `queued`, never as the
//!    terminal `canceled` a plain cancel would write.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::Status;
use aulos_provider::fake::FakeProvider;
use aulos_queue::SHUTDOWN_MSG;
use support::Harness;

/// A provider whose download hangs until cancelled, so the item is genuinely mid-flight.
fn hanging() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        download = [
            { kind = "stage", stage = "preparing" },
            { kind = "stage", stage = "downloading" },
            { kind = "hang" },
        ]
    "#,
    )
    .unwrap()
}

/// A provider whose resolution hangs until cancelled, so the item is genuinely mid-resolve.
fn slow_resolve() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "hang" }]
    "#,
    )
    .unwrap()
}

/// Waits until the engine's command channel has closed, i.e. `Engine::run` has returned.
async fn until_stopped(h: &Harness) {
    for _ in 0..2_000 {
        if !h.handle.is_open() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the engine loop never ended after a shutdown");
}

#[tokio::test]
async fn shutdown_hands_a_running_download_back_as_queued_and_ends_the_loop() {
    let h = Harness::builder()
        .provider(Arc::new(hanging()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;

    let report = h.handle.shutdown().await;
    assert_eq!(report.interrupted, 1);
    assert!(
        report.persisted,
        "the write is Sync, so it is on the platter"
    );

    let row = h.item(id).await.expect("the row survives the shutdown");
    assert_eq!(
        row.status,
        Status::Queued,
        "`canceled` is terminal and the next boot would never resume it"
    );
    assert_eq!(row.msg.as_deref(), Some(SHUTDOWN_MSG));
    assert!(
        row.auto_start,
        "under the default `resume` policy the next boot schedules it without asking"
    );

    until_stopped(&h).await;

    // And the dying job's own `canceled` never lands: the loop that would have handled it is gone.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let row = h.item(id).await.unwrap();
    assert_eq!(row.status, Status::Queued);
    assert_eq!(row.msg.as_deref(), Some(SHUTDOWN_MSG));
}

/// The shutdown write feeds boot recovery, so it has to apply the same DESIGN §8.9 policy.
///
/// `AULOS_RESTART_POLICY=pause` exists for an operator who wants to inspect before resuming. It
/// used to work only after a *crash*: an ordinary `docker restart` runs this path first, and the
/// hardcoded `auto_start = true` it wrote turned every parked row back into a scheduled one before
/// recovery ever saw it.
#[tokio::test]
async fn the_pause_policy_hands_an_interrupted_download_back_parked() {
    let h = Harness::builder()
        .provider(Arc::new(hanging()))
        .env("AULOS_RESTART_POLICY", "pause")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;

    assert_eq!(h.handle.shutdown().await.interrupted, 1);
    let row = h.item(id).await.expect("the row survives the shutdown");
    assert_eq!(row.status, Status::Queued);
    assert_eq!(row.msg.as_deref(), Some(SHUTDOWN_MSG));
    assert!(
        !row.auto_start,
        "DESIGN §8.9: `pause` parks in-flight items, restart or crash"
    );
    until_stopped(&h).await;
}

/// An item added into the pending bucket still resolves (DESIGN §8.3). Being stopped mid-resolve
/// must not turn it into a download the user never asked to start.
#[tokio::test]
async fn an_interrupted_resolution_keeps_its_pending_bucket() {
    let h = Harness::builder()
        .provider(Arc::new(slow_resolve()))
        .build()
        .await;
    let mut req = support::request("https://fake.test/watch/slow");
    req.auto_start = false;
    let id = h.add_request(req).await.unwrap().ids[0];
    h.until_status(id, Status::Resolving).await;

    assert_eq!(h.handle.shutdown().await.interrupted, 1);
    let row = h.item(id).await.expect("the row survives the shutdown");
    assert_eq!(row.status, Status::Queued);
    assert!(
        !row.auto_start,
        "it was never asked to start, so the next boot must not start it"
    );
    until_stopped(&h).await;
}

#[tokio::test]
async fn shutdown_with_an_idle_queue_reports_nothing_and_still_ends_the_loop() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/quick").await;
    h.until_status(id, Status::Finished).await;

    let report = h.handle.shutdown().await;
    assert_eq!(report.interrupted, 0, "a finished item is not interrupted");
    assert!(report.persisted);
    until_stopped(&h).await;

    // A second shutdown against a stopped engine is a no-op rather than a failure.
    assert_eq!(h.handle.shutdown().await.interrupted, 0);

    let row = h.item(id).await.unwrap();
    assert_eq!(row.status, Status::Finished, "terminal rows are left alone");
}
