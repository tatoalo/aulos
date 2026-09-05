//! Boot recovery, against the DESIGN §8.9 table exactly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use aulos_core::{DownloadRequest, GroupId, Item, ItemId, Kind, SourceKind, SourceRef, Status};
use support::{Harness, histogram, selection};

/// A row in whatever state the test needs.
fn row(ord: i64, status: Status, auto_start: bool) -> Item {
    let url = url::Url::parse(&format!("https://fake.test/watch/{ord}")).unwrap();
    Item {
        id: ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord,
        url: url.clone(),
        canonical_key: format!("fake\u{1f}seed-{ord}").into(),
        provider: Some(support::pid("fake")),
        media_id: Some(format!("seed-{ord}").into()),
        title: format!("Seed {ord}").into(),
        status,
        auto_start,
        msg: None,
        error: None,
        request: DownloadRequest::new(url, selection()),
        entry: None,
        filename: None,
        size: None,
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 1_700_000_000_000 + ord,
        started_at: matches!(status, Status::Downloading | Status::Postprocessing)
            .then_some(1_700_000_000_000),
        finished_at: status.is_terminal().then_some(1_700_000_000_500),
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV2),
        children_total: None,
        clear_after: None,
    }
}

/// A group row plus `children` child rows in the given statuses.
fn group_with(ord: i64, children: &[Status]) -> Vec<Item> {
    let mut group = row(ord, Status::Queued, true);
    group.kind = Kind::Group;
    group.children_total = Some(u32::try_from(children.len()).unwrap());
    let gid: GroupId = group.id;
    let mut out = vec![group];
    for (i, status) in children.iter().enumerate() {
        let mut child = row(ord + 1 + i as i64, *status, true);
        child.group_id = Some(gid);
        child.group_index = Some(u32::try_from(i).unwrap() + 1);
        out.push(child);
    }
    out
}

#[tokio::test]
async fn every_status_follows_the_design_8_9_table_under_resume() {
    // The row at `ord = 0` is the *blocker*: recovery schedules in `ord` order and there is one
    // download slot, so it takes it and every other row stays exactly where recovery put it.
    let seed = vec![
        row(0, Status::Queued, true),
        row(1, Status::Resolving, true),
        row(2, Status::Preparing, true),
        row(3, Status::Downloading, true),
        row(4, Status::Postprocessing, true),
        row(5, Status::Queued, true),
        row(6, Status::Queued, false),
        row(7, Status::Finished, true),
        row(8, Status::Error, true),
        row(9, Status::Canceled, true),
    ];
    let ids: Vec<ItemId> = seed.iter().skip(1).map(|i| i.id).collect();
    let blocker = seed[0].id;
    let (h, report) = Harness::builder()
        .provider(Arc::new(hanging()))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .seed(seed)
        .build_reporting()
        .await;
    let report = report.expect("recovery ran");

    assert_eq!(report.policy, "resume");
    assert_eq!(report.requeued_resolving, 1);
    assert_eq!(
        report.requeued_running, 3,
        "preparing + downloading + postprocessing"
    );
    assert_eq!(report.terminal, 3);
    assert_eq!(report.terminal_total, 3);
    h.until(blocker, "the blocker holding the slot", |i| {
        i.status.is_running()
    })
    .await;

    // `resolving` → `queued`, with the documented message.
    let resolving = h.item(ids[0]).await.unwrap();
    assert_eq!(resolving.status, Status::Queued);
    assert_eq!(resolving.msg.as_deref(), Some("Re-queued after restart"));
    assert_eq!(
        resolving.attempt, 0,
        "a lost resolve is not a failed attempt"
    );

    // The three in-flight statuses → `queued`, `BumpAttempt`, `SetSource { restart }`.
    for id in &ids[1..4] {
        let item = h.item(*id).await.unwrap();
        assert_eq!(item.status, Status::Queued, "{id}");
        assert_eq!(item.attempt, 1, "{id}: BumpAttempt");
        assert_eq!(item.source.kind, SourceKind::Restart, "{id}");
        assert_eq!(item.msg.as_deref(), Some("Re-queued after restart"));
    }

    // `queued` is left exactly as it was, both ways.
    let scheduled = h.item(ids[4]).await.unwrap();
    assert_eq!(
        (scheduled.status, scheduled.auto_start),
        (Status::Queued, true)
    );
    assert_eq!(scheduled.attempt, 0);
    let parked = h.item(ids[5]).await.unwrap();
    assert_eq!((parked.status, parked.auto_start), (Status::Queued, false));

    // Terminal rows are untouched.
    for (id, status) in ids[6..]
        .iter()
        .zip([Status::Finished, Status::Error, Status::Canceled])
    {
        let item = h.item(*id).await.unwrap();
        assert_eq!(item.status, status);
        assert_eq!(item.attempt, 0);
        assert_eq!(item.source.kind, SourceKind::ApiV2, "no re-attribution");
    }
}

#[tokio::test]
async fn the_pause_policy_parks_the_in_flight_items_instead() {
    let seed = vec![
        row(1, Status::Resolving, true),
        row(2, Status::Downloading, true),
        row(3, Status::Queued, true),
    ];
    let ids: Vec<ItemId> = seed.iter().map(|i| i.id).collect();
    let (h, report) = Harness::builder()
        .provider(Arc::new(hanging()))
        .env("AULOS_RESTART_POLICY", "pause")
        .seed(seed)
        .build_reporting()
        .await;
    let report = report.expect("recovery ran");
    assert_eq!(report.policy, "pause");

    for id in &ids[..2] {
        let item = h.item(*id).await.unwrap();
        assert_eq!(item.status, Status::Queued, "{id}");
        assert!(!item.auto_start, "{id} is parked for inspection");
    }
    let untouched = h.item(ids[2]).await.unwrap();
    assert!(
        untouched.auto_start,
        "an item that was already queued keeps its flag"
    );
    assert_eq!(report.scheduled, 1);
    assert_eq!(report.parked, 2);
}

#[tokio::test]
async fn group_counters_are_recomputed_from_the_children() {
    let mut seed = vec![row(0, Status::Queued, true)];
    let blocker = seed[0].id;
    seed.extend(group_with(
        1,
        &[
            Status::Finished,
            Status::Error,
            Status::Downloading,
            Status::Queued,
            Status::Canceled,
        ],
    ));
    let group = seed[1].id;
    let (h, report) = Harness::builder()
        .provider(Arc::new(hanging()))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .seed(seed)
        .build_reporting()
        .await;
    assert_eq!(report.expect("recovery ran").groups, 1);
    h.until(blocker, "the blocker holding the slot", |i| {
        i.status.is_running()
    })
    .await;
    h.settle().await;

    // Recovery publishes the whole recovered working set as one `added` event, which is what
    // seeds the aggregator's snapshot after a restart.
    let view = h
        .events
        .all()
        .iter()
        .rev()
        .find_map(|e| match &**e {
            aulos_core::DomainEvent::Added(views, _) => {
                views.iter().find(|v| v.id == group).cloned()
            }
            aulos_core::DomainEvent::StatusChanged { id, view, .. } if *id == group => {
                Some(view.clone())
            }
            _ => None,
        })
        .expect("the group was published");
    assert_eq!(view.children_total, Some(5));
    assert_eq!(view.children_done, Some(1), "from the GROUP BY query");
    assert_eq!(view.children_error, Some(1));
    // The `downloading` child was re-queued by recovery, so nothing is active any more.
    assert_eq!(view.children_active, Some(0));
    assert_eq!(view.status, Status::Queued, "two children are still queued");
}

#[tokio::test]
async fn clear_after_is_re_armed_for_terminal_rows_that_have_none() {
    let mut finished = row(1, Status::Finished, true);
    finished.clear_after = None;
    let id = finished.id;
    let (h, report) = Harness::builder()
        .env("CLEAR_COMPLETED_AFTER", "600")
        .seed(vec![finished])
        .build_reporting()
        .await;
    let report = report.expect("recovery ran");
    assert_eq!(report.clear_after_armed, 1);
    let row = h.item(id).await.unwrap();
    let armed = row.clear_after.expect("re-armed");
    assert_eq!(
        armed,
        1_700_000_000_500 + 600_000,
        "the window is measured from when it finished, not from boot"
    );
    assert!(h.handle.is_open());
}

#[tokio::test]
async fn a_re_queued_item_is_actually_schedulable_after_recovery() {
    let seed = vec![row(1, Status::Downloading, true)];
    let id = seed[0].id;
    let h = Harness::builder().seed(seed).build().await;
    // The default fake provider finishes instantly, so the recovered item runs on its own.
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.attempt, 1, "the interrupted attempt was counted");
    assert!(done.filename.is_some());
}

#[tokio::test]
async fn orphan_temp_files_are_logged_not_deleted_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("temp");
    std::fs::create_dir_all(&temp).unwrap();

    // Two orphans: a loose partial file and a per-job scratch directory whose item is gone.
    let ghost = ItemId::new();
    std::fs::create_dir_all(temp.join(ghost.to_string())).unwrap();
    std::fs::write(temp.join("Something.mp4.part"), b"x").unwrap();

    let (kept, report) = Harness::builder()
        .env("TEMP_DIR", temp.to_str().unwrap())
        .recovering()
        .build_reporting()
        .await;
    let report = report.expect("recovery ran");
    assert_eq!(report.orphan_temp.len(), 2, "{:?}", report.orphan_temp);
    assert_eq!(report.orphan_temp_deleted, 0);
    assert!(
        temp.join("Something.mp4.part").exists(),
        "deleting user data on boot by default is not acceptable"
    );
    drop(kept);

    let (_swept, report) = Harness::builder()
        .env("TEMP_DIR", temp.to_str().unwrap())
        .env("AULOS_CLEAN_ORPHAN_TEMP", "true")
        .recovering()
        .build_reporting()
        .await;
    let report = report.expect("recovery ran");
    assert_eq!(report.orphan_temp_deleted, 2, "unless asked to");
    assert!(!temp.join("Something.mp4.part").exists());
    assert!(!temp.join(ghost.to_string()).exists());
}

/// A resolved row owns **two** dedupe keys — the URL-derived one it was added under and the
/// `media_id`-derived one resolution produced (DESIGN §8.5). Boot recovery used to reinstate only
/// the second, so re-posting the very same URL after a restart quietly created a second item for
/// the same video: the legacy bug §8.5 exists to close, reopened across the restart.
#[tokio::test]
async fn a_recovered_row_still_dedupes_the_url_it_was_added_under() {
    const URL: &str = "https://fake.test/watch/dupe";
    let mut seed = row(7, Status::Queued, false);
    let url = url::Url::parse(URL).unwrap();
    seed.url = url.clone();
    seed.media_id = Some("fake:dupe".into());
    seed.canonical_key = aulos_store::canonical_key("fake", URL, Some("fake:dupe"));
    seed.request = DownloadRequest::new(url, selection());

    let h = Harness::builder().seed(vec![seed]).build().await;
    let outcome = h.add_request(support::request(URL)).await.unwrap();
    assert!(
        outcome.ids.is_empty(),
        "the URL is already queued, so nothing new is minted"
    );
    assert_eq!(outcome.duplicates.len(), 1, "{outcome:?}");
}

#[tokio::test]
async fn recovery_on_an_empty_database_finds_nothing() {
    let (_h, report) = Harness::builder().recovering().build_reporting().await;
    let report = report.expect("recovery ran");
    assert_eq!(report.requeued_resolving, 0);
    assert_eq!(report.requeued_running, 0);
    assert_eq!(report.scheduled, 0);
    assert_eq!(report.terminal, 0);
    assert_eq!(report.groups, 0);
    assert!(report.orphan_temp.is_empty());
}

#[tokio::test]
async fn the_seeded_histogram_helper_sees_what_recovery_produced() {
    let seed = vec![
        row(1, Status::Queued, true),
        row(2, Status::Queued, false),
        row(3, Status::Finished, true),
    ];
    let h = Harness::builder()
        .provider(Arc::new(hanging()))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .seed(seed)
        .build()
        .await;
    h.settle().await;
    let hist = histogram(&h.rows().await);
    assert_eq!(hist.get(&(Status::Queued, false)), Some(&1));
    assert_eq!(hist.get(&(Status::Finished, true)), Some(&1));
}

/// A provider that hangs, so a recovered item does not race the assertions.
fn hanging() -> aulos_provider::fake::FakeProvider {
    aulos_provider::fake::FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        download = [{ kind = "hang" }]
    "#,
    )
    .unwrap()
}
