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
    assert!(row.auto_start, "the next boot schedules it without asking");

    until_stopped(&h).await;

    // And the dying job's own `canceled` never lands: the loop that would have handled it is gone.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let row = h.item(id).await.unwrap();
    assert_eq!(row.status, Status::Queued);
    assert_eq!(row.msg.as_deref(), Some(SHUTDOWN_MSG));
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
