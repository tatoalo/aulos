//! The typed reads: `items`, `v1_done`, `resolve_v1_token`, `due_clears`, `boot_state` and the
//! `PruneSeen` boundaries (DESIGN §7.1, §8.9, §8.10, §11.3, §11.4; PLAN WP-04).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use aulos_core::{Item, ItemId, Kind, Ord0, Status, SubId, SubscriptionRecord};
use aulos_store::{Durability, GroupScope, ItemFilter, Store, WriteOp};

/// Inserts rows with the given `(ord, status)` shape and returns them.
async fn insert(store: &Store, rows: Vec<Item>) -> Vec<Item> {
    store
        .write(
            vec![WriteOp::InsertItems {
                items: rows.clone(),
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    rows
}

fn with(ord: Ord0, status: Status) -> Item {
    let mut i = support::item(ord);
    i.status = status;
    if status.is_terminal() {
        i.finished_at = Some(1_000 + ord);
    }
    i
}

// ---------------------------------------------------------------------------
// items / paging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn items_are_ordered_by_ord_and_can_be_filtered_and_paged() {
    let h = support::harness();
    let rows = insert(
        &h.store,
        vec![
            with(2, Status::Queued),
            with(0, Status::Downloading),
            with(1, Status::Finished),
            with(3, Status::Queued),
        ],
    )
    .await;

    let all = h.store.items(ItemFilter::default()).await.unwrap();
    assert_eq!(
        all.rows.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [0, 1, 2, 3],
        "ORDER BY ord ASC, id ASC"
    );
    assert_eq!(all.total, 4);
    assert_eq!(all.next, None, "an unpaged read has no cursor");

    let queued = h
        .store
        .items(ItemFilter::default().with_status(Status::Queued))
        .await
        .unwrap();
    assert_eq!(queued.rows.len(), 2);
    assert_eq!(queued.total, 2);

    let non_terminal = h.store.items(ItemFilter::non_terminal()).await.unwrap();
    assert_eq!(
        non_terminal.rows.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [0, 2, 3]
    );

    // Paging: two pages of two, then an empty third.
    let first = h
        .store
        .items(ItemFilter::default().with_limit(2))
        .await
        .unwrap();
    assert_eq!(first.rows.len(), 2);
    assert_eq!(first.total, 4, "total ignores the limit");
    let cursor = first.next.expect("a full page yields a cursor");
    let second = h
        .store
        .items(ItemFilter::default().with_limit(2).after(cursor))
        .await
        .unwrap();
    assert_eq!(
        second.rows.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [2, 3]
    );
    let third = h
        .store
        .items(
            ItemFilter::default()
                .with_limit(2)
                .after(second.next.unwrap()),
        )
        .await
        .unwrap();
    assert!(third.rows.is_empty());

    assert_eq!(rows.len(), 4);
}

#[tokio::test]
async fn the_group_scope_separates_containers_from_children() {
    let h = support::harness();
    let mut group = support::item(0);
    group.kind = Kind::Group;
    group.children_total = Some(2);
    let group_id = group.id;
    let mut a = support::item(1);
    a.group_id = Some(group_id);
    a.group_index = Some(1);
    let mut b = support::item(2);
    b.group_id = Some(group_id);
    b.group_index = Some(2);
    let loose = support::item(3);
    insert(&h.store, vec![group, a, b, loose]).await;

    let top = h
        .store
        .items(ItemFilter::default().with_group(GroupScope::TopLevel))
        .await
        .unwrap();
    assert_eq!(top.rows.iter().map(|i| i.ord).collect::<Vec<_>>(), [0, 3]);

    let children = h
        .store
        .items(ItemFilter::default().with_group(GroupScope::Of(group_id)))
        .await
        .unwrap();
    assert_eq!(
        children.rows.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [1, 2]
    );

    let groups_only = h
        .store
        .items(ItemFilter::default().with_kind(Kind::Group))
        .await
        .unwrap();
    assert_eq!(groups_only.rows.len(), 1);
}

// ---------------------------------------------------------------------------
// v1_done
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_done_is_ordered_terminal_only_and_capped_from_the_oldest_end() {
    let h = support::harness();
    insert(
        &h.store,
        vec![
            with(0, Status::Finished),
            with(1, Status::Downloading),
            with(2, Status::Error),
            with(3, Status::Canceled),
            with(4, Status::Finished),
            with(5, Status::Queued),
        ],
    )
    .await;

    let all = h.store.v1_done(None).await.unwrap();
    assert_eq!(
        all.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [0, 2, 4],
        "finished and error only, ord ascending; canceled is omitted (DESIGN §11.4)"
    );

    let capped = h.store.v1_done(Some(2)).await.unwrap();
    assert_eq!(
        capped.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [2, 4],
        "a limit keeps the most recent rows, still oldest-first"
    );

    assert_eq!(
        h.store.v1_done(Some(0)).await.unwrap().len(),
        3,
        "0 means unlimited, matching AULOS_V1_HISTORY_MAX"
    );
}

#[tokio::test]
async fn aulos_v1_history_max_tightens_the_caller_s_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = support::options(dir.path());
    opts.v1_history_max = 2;
    let store = Store::open(opts).unwrap();
    insert(
        &store,
        vec![
            with(0, Status::Finished),
            with(1, Status::Finished),
            with(2, Status::Finished),
            with(3, Status::Finished),
        ],
    )
    .await;

    assert_eq!(
        store
            .v1_done(None)
            .await
            .unwrap()
            .iter()
            .map(|i| i.ord)
            .collect::<Vec<_>>(),
        [2, 3],
        "the operator's cap applies even when the caller asks for everything"
    );
    assert_eq!(store.v1_done(Some(1)).await.unwrap().len(), 1);
    assert_eq!(
        store.v1_done(Some(100)).await.unwrap().len(),
        2,
        "the cap can only tighten"
    );
}

// ---------------------------------------------------------------------------
// resolve_v1_token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_v1_token_walks_the_ladder() {
    let h = support::harness();
    let mut first = support::item(0);
    first.media_id = Some("vid-1".into());
    let mut second = support::item(1);
    // The same URL added twice — legitimate (mp3 of a video already queued as mp4).
    second.url = first.url.clone();
    second.media_id = Some("vid-2".into());
    let ids = (first.id, second.id);
    let url = first.url.to_string();
    insert(&h.store, vec![first, second]).await;

    // 1. a ULID that exists wins outright, even though its url matches two rows
    assert_eq!(
        h.store.resolve_v1_token(&ids.0.to_string()).await.unwrap(),
        vec![ids.0]
    );

    // 2. an exact url match resolves to *all* matching rows
    let mut by_url = h.store.resolve_v1_token(&url).await.unwrap();
    by_url.sort_unstable();
    let mut expected = vec![ids.0, ids.1];
    expected.sort_unstable();
    assert_eq!(by_url, expected);

    // 3. an exact media_id match
    assert_eq!(
        h.store.resolve_v1_token("vid-2").await.unwrap(),
        vec![ids.1]
    );

    // 4. anything else, including a well-formed ULID for a row that does not exist
    assert!(h.store.resolve_v1_token("nope").await.unwrap().is_empty());
    assert!(
        h.store
            .resolve_v1_token(&ItemId::new().to_string())
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// due_clears
// ---------------------------------------------------------------------------

#[tokio::test]
async fn due_clears_returns_only_armed_rows_that_have_come_due() {
    let h = support::harness();
    let mut a = with(0, Status::Finished);
    a.clear_after = Some(100);
    let mut b = with(1, Status::Finished);
    b.clear_after = Some(300);
    let c = with(2, Status::Finished);
    let ids = (a.id, b.id);
    insert(&h.store, vec![a, b, c]).await;

    assert!(h.store.due_clears(99).await.unwrap().is_empty());
    assert_eq!(h.store.due_clears(100).await.unwrap(), vec![ids.0]);
    assert_eq!(
        h.store.due_clears(300).await.unwrap(),
        vec![ids.0, ids.1],
        "ordered by clear_after"
    );
}

// ---------------------------------------------------------------------------
// boot_state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn boot_state_reports_the_working_set_the_done_window_and_group_counters() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = support::options(dir.path());
    opts.done_window = 2;
    let store = Store::open(opts).unwrap();

    let mut group = support::item(0);
    group.kind = Kind::Group;
    group.children_total = Some(4);
    let group_id = group.id;
    let mut kids = Vec::new();
    for (i, st) in [
        Status::Finished,
        Status::Error,
        Status::Downloading,
        Status::Queued,
    ]
    .into_iter()
    .enumerate()
    {
        let mut k = with(1 + i as i64, st);
        k.group_id = Some(group_id);
        k.group_index = Some(u32::try_from(i).unwrap() + 1);
        kids.push(k);
    }
    let mut aged = with(9, Status::Finished);
    aged.clear_after = Some(4_242);
    let mut rows = vec![group];
    rows.extend(kids);
    rows.push(aged);
    insert(&store, rows).await;

    let boot = store.boot_state().await.unwrap();
    assert_eq!(
        boot.non_terminal.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [0, 3, 4],
        "the group container is non-terminal too, and the order is by ord"
    );
    assert_eq!(boot.items_total, 6);
    assert_eq!(boot.done_total, 3, "finished + error + finished");
    assert_eq!(
        boot.done_window.iter().map(|i| i.ord).collect::<Vec<_>>(),
        [2, 9],
        "the window keeps the most recent AULOS_MEM_DONE_ITEMS, oldest-first"
    );
    assert_eq!(boot.next_clear_at, Some(4_242));

    let counts = boot
        .group_counts
        .get(&group_id)
        .copied()
        .unwrap_or_default();
    assert_eq!(counts.children, 4);
    assert_eq!(counts.done, 1);
    assert_eq!(counts.error, 1);
    assert_eq!(counts.active, 1, "only downloading is active here");
}

#[tokio::test]
async fn boot_state_of_an_empty_database_is_empty() {
    let h = support::harness();
    let boot = h.store.boot_state().await.unwrap();
    assert!(boot.non_terminal.is_empty());
    assert!(boot.done_window.is_empty());
    assert_eq!(boot.done_total, 0);
    assert_eq!(boot.items_total, 0);
    assert!(boot.group_counts.is_empty());
    assert_eq!(boot.next_clear_at, None);
}

// ---------------------------------------------------------------------------
// PruneSeen boundaries
// ---------------------------------------------------------------------------

/// Keep 0, keep 1, keep exactly N, keep N + 1.
#[tokio::test]
async fn prune_seen_boundaries() {
    for (keep, expected) in [(0_u32, 0_usize), (1, 1), (4, 4), (5, 4)] {
        let h = support::harness();
        let sub = SubscriptionRecord::new(
            SubId::new(),
            "feed",
            url::Url::parse("https://example.com/feed").unwrap(),
            support::selection(),
        );
        h.store
            .write(
                vec![WriteOp::UpsertSubscription(Box::new(sub.clone()))],
                Durability::Sync,
            )
            .await
            .unwrap();
        // Distinct `seen_at` values, so "keep the newest" is unambiguous.
        for (n, id) in ["a", "b", "c", "d"].into_iter().enumerate() {
            h.store
                .write(
                    vec![WriteOp::MarkSeen {
                        sub: sub.id.clone(),
                        ids: vec![id.into()],
                        at: 100 + n as i64,
                    }],
                    Durability::Sync,
                )
                .await
                .unwrap();
        }
        assert_eq!(h.store.seen(&sub.id).await.unwrap().len(), 4);

        h.store
            .write(
                vec![WriteOp::PruneSeen {
                    sub: sub.id.clone(),
                    keep,
                }],
                Durability::Sync,
            )
            .await
            .unwrap();
        let left = h.store.seen(&sub.id).await.unwrap();
        assert_eq!(left.len(), expected, "keep = {keep}");
        if keep == 1 {
            assert!(left.contains(&Box::from("d")), "the newest survives");
        }
    }
}

#[tokio::test]
async fn mark_seen_keeps_the_first_sighting_time() {
    let h = support::harness();
    let sub = SubscriptionRecord::new(
        SubId::new(),
        "feed",
        url::Url::parse("https://example.com/feed").unwrap(),
        support::selection(),
    );
    h.store
        .write(
            vec![WriteOp::UpsertSubscription(Box::new(sub.clone()))],
            Durability::Sync,
        )
        .await
        .unwrap();
    for at in [10, 20] {
        h.store
            .write(
                vec![WriteOp::MarkSeen {
                    sub: sub.id.clone(),
                    ids: vec!["a".into()],
                    at,
                }],
                Durability::Sync,
            )
            .await
            .unwrap();
    }
    let id = sub.id.clone();
    let seen_at: i64 = h
        .store
        .read(move |c| {
            Ok(c.query_row(
                "SELECT seen_at FROM subscription_seen WHERE subscription_id = ?1",
                [id.as_str()],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(seen_at, 10, "the first sighting wins");
}

// ---------------------------------------------------------------------------
// entry_blob (the HookStore read `EngineHookStore` delegates here)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entry_blob_reads_the_compacted_entry_and_reports_a_missing_row() {
    use aulos_core::EntryBlob;

    let h = support::harness();
    let mut item = support::item(0);
    let blob = EntryBlob::new(serde_json::json!({ "state": { "title_id": 42 } }));
    item.entry = Some(blob.clone());
    let id = item.id;
    insert(&h.store, vec![item]).await;

    assert_eq!(h.store.entry_blob(id).await.unwrap(), Some(blob));
    h.store
        .write(vec![WriteOp::DropEntryBlob { id }], Durability::Sync)
        .await
        .unwrap();
    assert_eq!(h.store.entry_blob(id).await.unwrap(), None);
    assert!(h.store.entry_blob(ItemId::new()).await.is_err());
}
