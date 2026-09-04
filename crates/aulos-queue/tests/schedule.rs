//! Priority classes, slots and the bounded lookahead (DESIGN §8.2, §8.7).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use aulos_core::{SourceKind, SourceRef, Status};
use aulos_provider::Provider;
use aulos_provider::fake::FakeProvider;
use aulos_queue::{Action, Priority};
use support::{Harness, expanding, request};

/// A provider that hangs, under a chosen id/score/host and optional own-slot pool.
fn parked(id: &str, hosts: &str, own_slots: Option<usize>) -> FakeProvider {
    let slots = own_slots.map_or_else(String::new, |n| format!("own_slots = {n}\n"));
    FakeProvider::from_toml(&format!(
        r#"
        id = "{id}"
        score = 200
        hosts = [{hosts}]
        {slots}
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

/// A provider that fails every download, so a test can produce a retryable terminal item.
fn failing(id: &str, hosts: &str) -> FakeProvider {
    FakeProvider::from_toml(&format!(
        r#"
        id = "{id}"
        score = 200
        hosts = [{hosts}]

        [[timeline]]
        download = [{{ kind = "fail", code = "unavailable" }}]
    "#
    ))
    .unwrap()
}

#[tokio::test]
async fn the_four_priority_classes_are_served_in_order() {
    // One slot, held by a hanging job, with one item of each class queued behind it. Freeing the
    // slot must serve them `Retry` → `Interactive` → `Subscription` → `Bulk`, which is the whole
    // point of DESIGN §8.2: a link you just pasted does not queue behind 486 playlist children.
    let h = Harness::builder()
        .provider(Arc::new(parked("blocker", "\"block.test\"", None)))
        .provider(Arc::new(failing("failer", "\"fail.test\"")))
        .provider(Arc::new(expanding(3)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;

    // The retry candidate has to fail *before* the slot is occupied, so it is created first.
    let failed = h.add("https://fail.test/watch/broken").await;
    h.until_status(failed, Status::Error).await;

    let blocker = h.add("https://block.test/watch/hold").await;
    h.until_status(blocker, Status::Downloading).await;

    // Bulk: three playlist children.
    let group = h.add("https://fake.test/playlist/bulk").await;
    h.until_all("the children", |rows| {
        rows.iter().filter(|i| i.group_id == Some(group)).count() == 3
    })
    .await;

    // Subscription, then Interactive.
    let subscription = h
        .handle
        .add(
            vec![request("https://fake.test/watch/sub")],
            SourceRef::with_ref(SourceKind::Subscription, "sub-1"),
        )
        .await
        .unwrap()
        .ids[0];
    let interactive = h.add("https://fake.test/watch/pasted").await;

    // Retry: the failed item, re-queued at `Priority::Retry`.
    let applied = h.handle.actions(Action::Retry, vec![failed], None).await;
    assert_eq!(applied.applied, vec![failed], "a failed item is retryable");
    let retried = h.item(failed).await.unwrap();
    assert_eq!(retried.attempt, 1, "attempt increments (DESIGN §8.8)");
    assert_eq!(retried.source.kind, SourceKind::Retry);

    for id in [subscription, interactive] {
        h.until(id, "queued", |i| i.status == Status::Queued).await;
    }
    h.settle().await;
    h.events.clear();

    // Free the slot. The `failer` provider fails again, so the retry finishes first and the
    // remaining classes follow in order.
    h.handle.actions(Action::Cancel, vec![blocker], None).await;
    h.until_all("everything settled", |rows| {
        rows.iter()
            .filter(|i| i.kind == aulos_core::Kind::Item)
            .all(|i| i.status.is_terminal())
    })
    .await;

    let order: Vec<_> = h.events.completed().iter().map(|v| v.id).collect();
    let at = |id| {
        order
            .iter()
            .position(|got| *got == id)
            .unwrap_or_else(|| panic!("{id} never completed: {order:?}"))
    };
    assert!(
        at(failed) < at(interactive),
        "Retry beats Interactive: {order:?}"
    );
    assert!(
        at(interactive) < at(subscription),
        "Interactive beats Subscription: {order:?}"
    );
    let first_child = h
        .rows()
        .await
        .into_iter()
        .filter(|i| i.group_id == Some(group))
        .min_by_key(|i| i.ord)
        .expect("a child")
        .id;
    assert!(
        at(subscription) < at(first_child),
        "Subscription beats Bulk: {order:?}"
    );
}

#[tokio::test]
async fn own_slots_providers_do_not_consume_a_global_permit() {
    let sc = Arc::new(parked("sc", "\"sc.test\"", Some(2)));
    let ytdlp = Arc::new(parked("ytdlp", "\"yt.test\"", None));
    let h = Harness::builder()
        .provider(Arc::clone(&sc) as Arc<dyn Provider>)
        .provider(Arc::clone(&ytdlp) as Arc<dyn Provider>)
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;

    let a = h.add("https://sc.test/watch/a").await;
    let b = h.add("https://sc.test/watch/b").await;
    h.until_status(a, Status::Downloading).await;
    h.until_status(b, Status::Downloading).await;

    // Both SC jobs are running on the provider's own pool, and the single global slot is still
    // free for a yt-dlp item.
    let y = h.add("https://yt.test/watch/y").await;
    h.until_status(y, Status::Downloading).await;
    assert_eq!(sc.download_count(), 2);
    assert_eq!(ytdlp.download_count(), 1);
}

#[tokio::test]
async fn a_saturated_provider_pool_does_not_block_another_provider() {
    // The lookahead test: three SC items ahead of a yt-dlp item in the same priority class, with
    // the SC pool full. Without the bounded scan the yt-dlp item would sit behind them forever.
    let sc = Arc::new(parked("sc", "\"sc.test\"", Some(1)));
    let ytdlp = Arc::new(parked("ytdlp", "\"yt.test\"", None));
    let h = Harness::builder()
        .provider(Arc::clone(&sc) as Arc<dyn Provider>)
        .provider(Arc::clone(&ytdlp) as Arc<dyn Provider>)
        .env("MAX_CONCURRENT_DOWNLOADS", "4")
        .build()
        .await;

    let mut queued = Vec::new();
    for i in 0..3 {
        queued.push(h.add(&format!("https://sc.test/watch/{i}")).await);
    }
    h.until_status(queued[0], Status::Downloading).await;
    let y = h.add("https://yt.test/watch/late").await;
    h.until_status(y, Status::Downloading).await;

    assert_eq!(sc.download_count(), 1, "the SC pool is capped at one");
    assert_eq!(
        h.item(queued[1]).await.unwrap().status,
        Status::Queued,
        "its queue is still waiting"
    );
}

#[tokio::test]
async fn the_global_cap_is_never_exceeded() {
    let ytdlp = Arc::new(parked("ytdlp", "\"yt.test\"", None));
    let h = Harness::builder()
        .provider(Arc::clone(&ytdlp) as Arc<dyn Provider>)
        .env("MAX_CONCURRENT_DOWNLOADS", "2")
        .build()
        .await;
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(h.add(&format!("https://yt.test/watch/{i}")).await);
    }
    for id in &ids {
        h.until_resolved(*id).await;
    }
    h.settle().await;

    let rows = h.rows().await;
    let running = rows.iter().filter(|i| i.status.is_running()).count();
    assert_eq!(running, 2, "exactly the configured cap, never more");
    assert_eq!(ytdlp.download_count(), 2);
}

#[tokio::test]
async fn a_lookahead_of_one_still_makes_progress() {
    // A pathological lookahead must not wedge the scheduler: the freed slot re-runs the scan.
    let h = Harness::builder()
        .env("AULOS_SCHED_LOOKAHEAD", "1")
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(h.add(&format!("https://fake.test/watch/{i}")).await);
    }
    for id in &ids {
        h.until_status(*id, Status::Finished).await;
    }
}

#[tokio::test]
async fn a_group_row_is_never_scheduled() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(2)))
        .build()
        .await;
    let id = h.add("https://fake.test/playlist/two").await;
    h.until_all("both children finished", |rows| {
        rows.iter()
            .filter(|i| i.group_id == Some(id))
            .all(|i| i.status == Status::Finished)
            && rows.len() == 3
    })
    .await;
    let group = h
        .until(id, "the group roll-up", |i| i.status == Status::Finished)
        .await;
    assert_eq!(group.filename, None, "a group never downloads a file");
    assert_eq!(group.started_at, None, "and is never `preparing`");
    assert_eq!(group.status, Status::Finished, "it rolls up instead");
}

#[tokio::test]
async fn a_subscription_add_is_its_own_class() {
    let h = Harness::new().await;
    let out = h
        .handle
        .add(
            vec![request("https://fake.test/watch/sub")],
            SourceRef::with_ref(SourceKind::Subscription, "sub-1"),
        )
        .await
        .unwrap();
    let row = h.until_resolved(out.ids[0]).await;
    assert_eq!(row.source.kind, SourceKind::Subscription);
    assert_eq!(row.source.reference.as_deref(), Some("sub-1"));
    assert_eq!(
        Priority::of(SourceKind::Subscription, false),
        Priority::Subscription
    );
    assert_eq!(
        Priority::of(SourceKind::Subscription, true),
        Priority::Bulk,
        "but a subscription's playlist child is Bulk"
    );
}
