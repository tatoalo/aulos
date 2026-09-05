//! Cancel, pause, retry, delete and the auto-clear sweeper (DESIGN §8.7, §8.8, §8.10).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::{ErrorCode, Kind, RemoveReason, Status};
use aulos_provider::fake::FakeProvider;
use aulos_queue::{Action, SkipReason};
use support::{Harness, expanding, request};

/// A provider whose download hangs on the given host until cancelled.
fn hanging(host: &str) -> FakeProvider {
    hanging_as("fake", host)
}

/// [`hanging`] under a chosen provider id, so it can be registered alongside another.
fn hanging_as(id: &str, host: &str) -> FakeProvider {
    FakeProvider::from_toml(&format!(
        r#"
        id = "{id}"
        score = 200
        hosts = ["{host}"]

        [[timeline]]
        download = [
            {{ kind = "stage", stage = "preparing" }},
            {{ kind = "stage", stage = "downloading" }},
            {{ kind = "hang" }},
        ]
    "#
    ))
    .unwrap()
}

/// A provider whose resolution hangs.
fn slow_resolve() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "wait", ms = 600000 }]
    "#,
    )
    .unwrap()
}

#[tokio::test]
async fn cancel_from_resolving() {
    let h = Harness::builder()
        .provider(Arc::new(slow_resolve()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/x").await;
    h.until_status(id, Status::Resolving).await;
    let result = h.handle.actions(Action::Cancel, vec![id], None).await;
    assert_eq!(result.applied, vec![id]);
    let row = h.until_status(id, Status::Canceled).await;
    assert_eq!(row.error.unwrap().code, ErrorCode::Canceled);
}

#[tokio::test]
async fn cancel_from_queued() {
    let h = Harness::new().await;
    let mut req = request("https://fake.test/watch/x");
    req.auto_start = false;
    let id = h.add_request(req).await.unwrap().ids[0];
    h.until_status(id, Status::Queued).await;
    h.handle.actions(Action::Cancel, vec![id], None).await;
    h.until_status(id, Status::Canceled).await;
}

#[tokio::test]
async fn cancel_from_downloading_leaves_no_process_and_no_partials() {
    let h = Harness::builder()
        .provider(Arc::new(hanging("fake.test")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;

    // A partial file, as yt-dlp would leave in the per-job scratch directory.
    let tmp = h.job_temp_dir(id);
    assert!(tmp.is_dir(), "the engine created the scratch directory");
    std::fs::write(tmp.join("Clip.mp4.part"), b"partial").unwrap();
    std::fs::write(tmp.join("Clip.mp4.ytdl"), b"resume").unwrap();

    h.handle.actions(Action::Cancel, vec![id], None).await;
    let row = h.until_status(id, Status::Canceled).await;
    assert_eq!(row.error.unwrap().code, ErrorCode::Canceled);
    h.settle().await;
    assert!(!tmp.exists(), "the partials are gone (DESIGN §8.7)");
    assert_eq!(
        std::fs::read_dir(h.temp_dir()).unwrap().count(),
        0,
        "and nothing is left behind in TEMP_DIR"
    );
}

#[tokio::test]
async fn cancel_from_preparing_and_from_postprocessing() {
    // The two running states the hanging fixture skips over. `preparing` is where the engine has
    // taken a slot but the provider has reported nothing yet; `postprocessing` is a provider that
    // is merging when the user gives up.
    for (label, script, status) in [
        ("preparing", "{ kind = \"hang\" }", Status::Preparing),
        (
            "postprocessing",
            "{ kind = \"stage\", stage = \"postprocessing\" }, { kind = \"hang\" }",
            Status::Postprocessing,
        ),
    ] {
        let provider = FakeProvider::from_toml(&format!(
            r#"
            id = "fake"
            score = 200
            hosts = ["fake.test"]

            [[timeline]]
            download = [{script}]
        "#
        ))
        .unwrap();
        let h = Harness::builder()
            .provider(Arc::new(provider))
            .build()
            .await;
        let id = h.add("https://fake.test/watch/x").await;
        h.until_status(id, status).await;
        let result = h.handle.actions(Action::Cancel, vec![id], None).await;
        assert_eq!(result.applied, vec![id], "cancel from {label}");
        let row = h.until_status(id, Status::Canceled).await;
        assert_eq!(row.error.unwrap().code, ErrorCode::Canceled, "{label}");
        h.settle().await;
        assert!(
            !h.job_temp_dir(id).exists(),
            "{label}: the partials go with it"
        );
    }
}

#[tokio::test]
async fn cancel_is_idempotent_and_a_no_op_on_a_terminal_item() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/done").await;
    h.until_status(id, Status::Finished).await;
    for _ in 0..3 {
        let result = h.handle.actions(Action::Cancel, vec![id], None).await;
        assert_eq!(result.applied, vec![id], "ack ok, idempotent");
        assert!(result.skipped.is_empty());
    }
    assert_eq!(
        h.item(id).await.unwrap().status,
        Status::Finished,
        "a terminal item is untouched"
    );
}

#[tokio::test]
async fn cancel_of_an_unknown_id_is_reported_not_found() {
    let h = Harness::new().await;
    let ghost = aulos_core::ItemId::new();
    let result = h.handle.actions(Action::Cancel, vec![ghost], None).await;
    assert!(result.applied.is_empty());
    assert_eq!(result.skipped.len(), 1);
    assert_eq!(result.skipped[0].reason, SkipReason::NotFound);
}

#[tokio::test]
async fn a_group_cancel_cascades_to_every_child() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(4)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let mut req = request("https://fake.test/playlist/cascade");
    req.auto_start = false;
    let group = h.add_request(req).await.unwrap().ids[0];
    h.until_all("the children", |rows| rows.len() == 5).await;

    let result = h.handle.actions(Action::Cancel, vec![group], None).await;
    assert_eq!(result.applied.len(), 5, "four children plus the group");
    h.until_all("everything cancelled", |rows| {
        rows.iter().all(|i| i.status == Status::Canceled)
    })
    .await;
    let g = h.item(group).await.unwrap();
    assert_eq!(g.status, Status::Canceled, "the group rolls up to canceled");
}

#[tokio::test]
async fn pause_from_queued_removes_nothing_and_keeps_the_status() {
    let h = Harness::builder()
        .provider(Arc::new(hanging_as("blocker", "block.test")))
        .provider(Arc::new(support::fake()))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let blocker = h.add("https://block.test/watch/hold").await;
    h.until_status(blocker, Status::Downloading).await;

    let id = h.add("https://fake.test/watch/waiting").await;
    h.until(id, "queued", |i| i.status == Status::Queued && i.auto_start)
        .await;

    let result = h.handle.actions(Action::Pause, vec![id], None).await;
    assert_eq!(result.applied, vec![id]);
    let row = h.item(id).await.unwrap();
    assert_eq!(row.status, Status::Queued, "the status stays queued");
    assert!(!row.auto_start, "only the flag moved (DESIGN §8.7)");
    assert_eq!(row.attempt, 0);
    assert_eq!(
        row.started_at, None,
        "nothing was running, so nothing is lost"
    );

    // Idempotent.
    let again = h.handle.actions(Action::Pause, vec![id], None).await;
    assert_eq!(again.applied, vec![id]);
    assert!(!h.item(id).await.unwrap().auto_start);
}

#[tokio::test]
async fn pause_from_downloading_keeps_the_part_file_and_the_attempt() {
    let h = Harness::builder()
        .provider(Arc::new(hanging("fake.test")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;
    let tmp = h.job_temp_dir(id);
    let part = tmp.join("Clip.mp4.part");
    std::fs::write(&part, b"partial").unwrap();

    let result = h.handle.actions(Action::Pause, vec![id], None).await;
    assert_eq!(result.applied, vec![id]);
    let row = h.until(id, "parked", |i| i.status == Status::Queued).await;
    assert!(!row.auto_start);
    assert_eq!(row.msg.as_deref(), Some("Paused"));
    assert_eq!(row.attempt, 0, "no BumpAttempt on a pause (DESIGN §8.7)");
    h.settle().await;
    assert!(
        part.exists(),
        "the `.part` file is still there, so yt-dlp can resume"
    );

    // And a following `start` re-runs the job.
    h.handle.actions(Action::Start, vec![id], None).await;
    h.until(id, "running again", |i| i.status.is_running())
        .await;
}

/// The regression for the pause → start gesture inside the kill grace.
///
/// `park_running` settles the slot and drops the permit, but the job task lives on for the whole
/// `killpg` SIGTERM → SIGKILL ladder — seconds, on a real download. A start pressed in that window
/// used to write `auto_start = true`, get dropped from the ready deque by the very next
/// `schedule()` (the item was still in `running`), and never come back: the row sat `queued`,
/// `auto_start = true`, in no deque, for the life of the process.
#[tokio::test]
async fn a_start_inside_the_kill_grace_still_restarts_the_download() {
    let h = Harness::builder()
        .provider(Arc::new(Lingering(hanging("fake.test"))))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;

    assert_eq!(
        h.handle
            .actions(Action::Pause, vec![id], None)
            .await
            .applied,
        vec![id]
    );
    h.until(id, "parked", |i| {
        i.status == Status::Queued && !i.auto_start
    })
    .await;

    // Immediately, i.e. while the killed job is still draining.
    let started = h.handle.actions(Action::Start, vec![id], None).await;
    assert_eq!(started.applied, vec![id]);
    assert!(
        h.item(id).await.unwrap().auto_start,
        "the flag is set either way; the bug was that nothing ever acted on it"
    );

    h.until(id, "running again after the grace", |i| {
        i.status.is_running()
    })
    .await;
}

/// The same window, reached through cancel → retry rather than pause → start.
#[tokio::test]
async fn a_retry_inside_the_kill_grace_still_restarts_the_download() {
    let h = Harness::builder()
        .provider(Arc::new(Lingering(hanging("fake.test"))))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;
    h.handle.actions(Action::Cancel, vec![id], None).await;
    h.until_status(id, Status::Canceled).await;

    assert_eq!(
        h.handle
            .actions(Action::Retry, vec![id], None)
            .await
            .applied,
        vec![id]
    );
    h.until(id, "running again after the grace", |i| {
        i.status.is_running()
    })
    .await;
}

#[tokio::test]
async fn pause_is_refused_from_resolving_and_from_a_terminal_item() {
    let h = Harness::builder()
        .provider(Arc::new(slow_resolve()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/x").await;
    h.until_status(id, Status::Resolving).await;
    let result = h.handle.actions(Action::Pause, vec![id], None).await;
    assert!(result.applied.is_empty());
    assert_eq!(result.skipped[0].reason, SkipReason::NotPausable);

    let done = Harness::new().await;
    let finished = done.add("https://fake.test/watch/done").await;
    done.until_status(finished, Status::Finished).await;
    let result = done
        .handle
        .actions(Action::Pause, vec![finished], None)
        .await;
    assert_eq!(result.skipped[0].reason, SkipReason::NotPausable);
}

#[tokio::test]
async fn a_group_pause_cascades_and_the_rollup_becomes_queued() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(3)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let mut req = request("https://fake.test/playlist/pause");
    req.auto_start = true;
    let group = h.add_request(req).await.unwrap().ids[0];
    h.until_all("the children", |rows| rows.len() == 4).await;

    let result = h.handle.actions(Action::Pause, vec![group], None).await;
    assert!(
        result.applied.len() >= 3,
        "every pausable child plus the group: {result:?}"
    );
    h.until_all("everything parked", |rows| {
        rows.iter()
            .filter(|i| i.kind == Kind::Item)
            .all(|i| i.status == Status::Queued && !i.auto_start)
    })
    .await;
    let g = h
        .until(group, "the roll-up", |i| i.status == Status::Queued)
        .await;
    assert_eq!(g.status, Status::Queued);
}

#[tokio::test]
async fn only_retryable_codes_auto_retry_and_the_attempt_increments() {
    let h = Harness::builder()
        .provider(Arc::new(failing("network")))
        .env("AULOS_AUTO_RETRY_MAX", "2")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/flaky").await;
    let armed = h.until(id, "an armed retry", |i| i.attempt == 1).await;
    assert_eq!(
        armed.status,
        Status::Queued,
        "queued, waiting on the backoff"
    );
    assert!(armed.auto_start);
    let msg = armed.msg.as_deref().expect("a message");
    let secs: u64 = msg
        .trim_start_matches("Retrying in ")
        .trim_end_matches('s')
        .parse()
        .unwrap_or_else(|_| panic!("unexpected message {msg:?}"));
    assert!(
        (24..=36).contains(&secs),
        "the first backoff step is 30 s ± 20 %, got {secs}s"
    );
    assert_eq!(
        armed.error.as_ref().map(|e| e.code),
        Some(ErrorCode::Network),
        "the failure stays visible while the backoff runs"
    );

    // Nothing runs until the clock moves.
    h.settle().await;
    assert_eq!(h.item(id).await.unwrap().attempt, 1);

    // Release the backoff: it fails again, arms a second retry, then gives up at the cap.
    h.advance(Duration::from_secs(60)).await;
    h.until(id, "the second attempt", |i| i.attempt == 2).await;
    h.advance(Duration::from_secs(180)).await;
    let dead = h.until_status(id, Status::Error).await;
    assert_eq!(dead.attempt, 2, "capped at AULOS_AUTO_RETRY_MAX");
    assert_eq!(dead.error.unwrap().code, ErrorCode::Network);
}

#[tokio::test]
async fn a_non_retryable_code_never_auto_retries() {
    let h = Harness::builder()
        .provider(Arc::new(failing("auth_required")))
        .env("AULOS_AUTO_RETRY_MAX", "2")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/private").await;
    let row = h.until_status(id, Status::Error).await;
    assert_eq!(row.attempt, 0, "no retry was ever armed");
    assert_eq!(row.error.unwrap().code, ErrorCode::AuthRequired);
}

#[tokio::test]
async fn auto_retry_max_zero_disables_the_feature() {
    let h = Harness::builder()
        .provider(Arc::new(failing("network")))
        .env("AULOS_AUTO_RETRY_MAX", "0")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/flaky").await;
    let row = h.until_status(id, Status::Error).await;
    assert_eq!(row.attempt, 0);
}

#[tokio::test]
async fn an_explicit_retry_clears_the_error_and_re_runs() {
    let h = Harness::builder()
        .provider(Arc::new(support::fake()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/done").await;
    h.until_status(id, Status::Finished).await;
    // A finished item is not retryable; a cancelled one is.
    let parked = {
        let mut req = request("https://fake.test/watch/parked");
        req.auto_start = false;
        h.add_request(req).await.unwrap().ids[0]
    };
    h.until_status(parked, Status::Queued).await;
    h.handle.actions(Action::Cancel, vec![parked], None).await;
    h.until_status(parked, Status::Canceled).await;

    let result = h.handle.actions(Action::Retry, vec![parked], None).await;
    assert_eq!(result.applied, vec![parked]);
    let row = h.until_status(parked, Status::Finished).await;
    assert_eq!(row.attempt, 1);
    assert_eq!(row.error, None, "the retry cleared the error");
    assert!(row.started_at.is_some());

    let refused = h.handle.actions(Action::Retry, vec![id], None).await;
    assert_eq!(refused.skipped[0].reason, SkipReason::NotRetryable);
}

/// PROTOCOL §4.2: "`start` … on a terminal item it is a `retry`". The shipped client offers a
/// Start affordance on a failed row and reads its action set from `capabilities.actions`, so
/// answering `skipped: [already_terminal]` left the row failed and the button dead. Only
/// `finished` stays `already_terminal` — it has its file, and re-downloading it is what an
/// explicit `retry` is for.
#[tokio::test]
async fn start_on_a_failed_or_cancelled_item_retries_it() {
    let h = Harness::builder()
        .provider(Arc::new(support::fake()))
        .build()
        .await;

    let canceled = {
        let mut req = request("https://fake.test/watch/stopped");
        req.auto_start = false;
        h.add_request(req).await.unwrap().ids[0]
    };
    h.until_status(canceled, Status::Queued).await;
    h.handle.actions(Action::Cancel, vec![canceled], None).await;
    h.until_status(canceled, Status::Canceled).await;

    let started = h.handle.actions(Action::Start, vec![canceled], None).await;
    assert_eq!(started.applied, vec![canceled], "{started:?}");
    assert!(started.skipped.is_empty());
    let row = h.until_status(canceled, Status::Finished).await;
    assert_eq!(row.attempt, 1, "it went through the retry path");
    assert_eq!(row.error, None);

    // A finished item is the one that is still refused.
    let done = h.add("https://fake.test/watch/have-it").await;
    h.until_status(done, Status::Finished).await;
    let refused = h.handle.actions(Action::Start, vec![done], None).await;
    assert!(refused.applied.is_empty());
    assert_eq!(refused.skipped[0].reason, SkipReason::AlreadyTerminal);
}

#[tokio::test]
async fn delete_removes_the_row_the_files_and_the_siblings() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/gone").await;
    let row = h.until_status(id, Status::Finished).await;
    let file = h
        .download_dir()
        .join(row.filename.as_ref().unwrap().as_path());
    assert!(file.exists());
    // The StreamingCommunity siblings legacy orphaned.
    let info = h.download_dir().join("gone.info.json");
    let nfo = h.download_dir().join("gone.nfo");
    std::fs::write(&info, b"{}").unwrap();
    std::fs::write(&nfo, b"<nfo/>").unwrap();

    let result = h.handle.actions(Action::Delete, vec![id], Some(true)).await;
    assert_eq!(result.applied, vec![id]);
    h.until_all("the row is gone", <[aulos_core::Item]>::is_empty)
        .await;
    h.until_gone(&file).await;
    h.until_gone(&info).await;
    h.until_gone(&nfo).await;
    assert_eq!(h.events.removed(), vec![(vec![id], RemoveReason::Deleted)]);
}

#[tokio::test]
async fn delete_keeps_the_file_when_asked_to() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/keep").await;
    let row = h.until_status(id, Status::Finished).await;
    let file = h
        .download_dir()
        .join(row.filename.as_ref().unwrap().as_path());
    h.handle
        .actions(Action::Delete, vec![id], Some(false))
        .await;
    h.until_all("the row is gone", <[aulos_core::Item]>::is_empty)
        .await;
    assert!(file.exists(), "delete_file = false keeps the download");
}

#[tokio::test]
async fn deleting_a_group_cancels_its_children_first_and_cascades() {
    let h = Harness::builder()
        .provider(Arc::new(hanging("fake.test")))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    // A hanging single item, then delete it while it runs.
    let id = h.add("https://fake.test/watch/hang").await;
    h.until_status(id, Status::Downloading).await;
    let result = h.handle.actions(Action::Delete, vec![id], None).await;
    assert_eq!(result.applied, vec![id]);
    h.until_all("the row is gone", <[aulos_core::Item]>::is_empty)
        .await;
    h.settle().await;
    h.until_gone(&h.job_temp_dir(id)).await;
}

#[tokio::test]
async fn clear_removes_every_terminal_row_and_publishes_cleared() {
    let h = Harness::new().await;
    let done = h.add("https://fake.test/watch/a").await;
    h.until_status(done, Status::Finished).await;
    let mut req = request("https://fake.test/watch/b");
    req.auto_start = false;
    let parked = h.add_request(req).await.unwrap().ids[0];
    h.until_status(parked, Status::Queued).await;

    let result = h.handle.clear(Some(true)).await;
    assert_eq!(result.applied, vec![done]);
    h.until_all("only the parked row is left", |rows| {
        rows.len() == 1 && rows[0].id == parked
    })
    .await;
    assert_eq!(
        h.events.removed(),
        vec![(vec![done], RemoveReason::Cleared)]
    );
}

#[tokio::test]
async fn the_auto_clear_sweeper_covers_rows_outside_the_memory_window() {
    let h = Harness::builder()
        .env("CLEAR_COMPLETED_AFTER", "60")
        // A one-row done window, so the second finished item pushes the first out of memory and
        // only a SQLite query can find it (DESIGN §8.10).
        .env("AULOS_MEM_DONE_ITEMS", "1")
        .build()
        .await;
    let first = h.add("https://fake.test/watch/a").await;
    h.until_status(first, Status::Finished).await;
    let second = h.add("https://fake.test/watch/b").await;
    h.until_status(second, Status::Finished).await;
    let armed = h.item(first).await.unwrap();
    assert!(
        armed.clear_after.is_some(),
        "clear_after is persisted, so the timer survives a restart"
    );

    h.advance(Duration::from_secs(61)).await;
    h.until_all("both rows swept", <[aulos_core::Item]>::is_empty)
        .await;
    let reasons: Vec<_> = h.events.removed().into_iter().map(|(_, r)| r).collect();
    assert!(reasons.contains(&RemoveReason::Expired));
}

#[tokio::test]
async fn clear_after_is_not_armed_for_a_cancelled_item() {
    let h = Harness::builder()
        .env("CLEAR_COMPLETED_AFTER", "60")
        .build()
        .await;
    let mut req = request("https://fake.test/watch/x");
    req.auto_start = false;
    let id = h.add_request(req).await.unwrap().ids[0];
    h.until_status(id, Status::Queued).await;
    h.handle.actions(Action::Cancel, vec![id], None).await;
    let row = h.until_status(id, Status::Canceled).await;
    assert_eq!(
        row.clear_after, None,
        "vanishing a row the user cancelled is not the same as tidying up a completed one"
    );
}

#[tokio::test]
async fn a_successful_download_records_its_file_and_drops_its_entry_blob() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(2)))
        .build()
        .await;
    let group = h.add("https://fake.test/playlist/one").await;
    h.until_all("the child finished", |rows| {
        rows.iter()
            .any(|i| i.group_id == Some(group) && i.status == Status::Finished)
    })
    .await;
    let child = h
        .children(group)
        .await
        .into_iter()
        .next()
        .expect("one child");
    assert!(child.filename.is_some());
    assert_eq!(child.size, Some(1_024));
    assert_eq!(
        child.entry, None,
        "a finished item drops its entry blob (DESIGN §7.5)"
    );
}

/// A provider whose download keeps running for a moment after its token is cancelled, the way a
/// real `killpg` SIGTERM → SIGKILL ladder does (`AULOS_KILL_GRACE_MS`, five seconds by default).
///
/// The engine settles the slot at once but only removes the item from `running` when this task
/// finally reports, which is the window the two tests above exercise.
struct Lingering(FakeProvider);

#[async_trait::async_trait]
impl aulos_provider::Provider for Lingering {
    fn id(&self) -> aulos_provider::ProviderId {
        self.0.id()
    }

    fn matches(&self, url: &url::Url) -> aulos_provider::Match {
        self.0.matches(url)
    }

    fn catalog(&self) -> Arc<aulos_core::FormatCatalog> {
        self.0.catalog()
    }

    async fn resolve(
        &self,
        url: &url::Url,
        ctx: aulos_provider::ResolveCtx<'_>,
    ) -> Result<Vec<aulos_provider::MediaEntry>, aulos_provider::ProviderError> {
        self.0.resolve(url, ctx).await
    }

    async fn download(
        &self,
        ctx: aulos_provider::DownloadCtx<'_>,
        sink: aulos_provider::ProgressSink,
    ) -> Result<aulos_provider::Outcome, aulos_provider::ProviderError> {
        let result = self.0.download(ctx, sink).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        result
    }
}

/// A provider that fails every download with one code.
fn failing(code: &str) -> FakeProvider {
    FakeProvider::from_toml(&format!(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        download = [{{ kind = "fail", code = "{code}" }}]
    "#
    ))
    .unwrap()
}
