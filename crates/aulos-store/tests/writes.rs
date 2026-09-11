//! Every `WriteOp` round-trips through a typed read, and `SetStatus` implements the DESIGN §7.1
//! timestamp table exactly (PLAN WP-04).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use aulos_core::{
    ApnsEnvironment, ChatConfig, DeviceRecord, DeviceStore, EntryBlob, ErrorCode, FieldUpdate,
    FileSlot, Item, ItemId, Kind, LiveActivityRecord, ProviderId, RelPath, SourceKind, SourceRef,
    Status, SubId, SubscriptionRecord, WireError,
};
use aulos_store::{Durability, Store, StoreError, WriteOp, retry_ops};

/// Applies `ops` and returns the row.
async fn apply(store: &Store, id: ItemId, ops: Vec<WriteOp>) -> Item {
    store.write(ops, Durability::Sync).await.unwrap();
    store.item(id).await.unwrap().expect("row must survive")
}

async fn seeded(store: &Store) -> Item {
    let item = support::item(0);
    store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![item.clone()],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    item
}

/// The headline acceptance bullet: every variant, each observed through a typed read.
///
/// The `covered` set is asserted against [`WriteOp::NAMES`] at the end, so a variant added to the
/// enum without a case here fails the test instead of quietly going untested.
#[tokio::test]
async fn every_write_op_round_trips_through_a_typed_read() {
    let h = support::harness();
    let s = &h.store;
    let mut covered: BTreeSet<&'static str> = BTreeSet::new();

    // 1. InsertItems — a fully-populated row, so every column is exercised.
    let seed = seeded(s).await;
    let id = seed.id;
    covered.insert("insert_items");
    let read = s.item(id).await.unwrap().unwrap();
    assert_eq!(read, seed, "the inserted row must read back byte for byte");

    // 2. SetStatus
    let row = apply(
        s,
        id,
        vec![WriteOp::SetStatus {
            id,
            status: Status::Resolving,
            msg: FieldUpdate::Set("Resolving…".into()),
            error: FieldUpdate::Keep,
            auto_start: None,
            at: 1_000,
        }],
    )
    .await;
    assert_eq!(row.status, Status::Resolving);
    assert_eq!(row.msg.as_deref(), Some("Resolving…"));
    covered.insert("set_status");

    // 3. SetAutoStart — and *only* auto_start.
    let row = apply(
        s,
        id,
        vec![WriteOp::SetAutoStart {
            id,
            auto_start: false,
            at: 1_100,
        }],
    )
    .await;
    assert!(!row.auto_start);
    assert_eq!(row.status, Status::Resolving, "the status must not move");
    assert_eq!(row.msg.as_deref(), Some("Resolving…"));
    covered.insert("set_auto_start");

    // 4. SetSource
    let row = apply(
        s,
        id,
        vec![WriteOp::SetSource {
            id,
            source: SourceRef::bare(SourceKind::Restart),
        }],
    )
    .await;
    assert_eq!(row.source, SourceRef::bare(SourceKind::Restart));
    covered.insert("set_source");

    // 5. SetResolved
    let entry = EntryBlob::new(serde_json::json!({ "playlist": "PL1", "n_entries": 3 }));
    let row = apply(
        s,
        id,
        vec![WriteOp::SetResolved {
            id,
            provider: ProviderId::parse("ytdlp").unwrap(),
            media_id: Some("dQw4w9WgXcQ".into()),
            title: "Real Title".into(),
            entry: Some(entry.clone()),
            canonical_key: "ytdlp:dQw4w9WgXcQ".into(),
        }],
    )
    .await;
    assert_eq!(row.provider.as_ref().map(ProviderId::as_str), Some("ytdlp"));
    assert_eq!(row.media_id.as_deref(), Some("dQw4w9WgXcQ"));
    assert_eq!(&*row.title, "Real Title");
    assert_eq!(row.entry.as_ref(), Some(&entry));
    assert_eq!(&*row.canonical_key, "ytdlp:dQw4w9WgXcQ");
    covered.insert("set_resolved");

    // 6. PromoteToGroup — the id survives (DESIGN §8.6).
    let row = apply(
        s,
        id,
        vec![WriteOp::PromoteToGroup {
            id,
            children_total: 12,
            title: "A Playlist".into(),
        }],
    )
    .await;
    assert_eq!(row.kind, Kind::Group);
    assert_eq!(row.children_total, Some(12));
    assert_eq!(row.id, id, "promotion keeps the immutable id");
    covered.insert("promote_to_group");

    // 7. SetOutput
    let row = apply(
        s,
        id,
        vec![WriteOp::SetOutput {
            id,
            filename: Some(RelPath::parse("sub/dir/video.mp4").unwrap()),
            size: Some(4_096),
        }],
    )
    .await;
    assert_eq!(
        row.filename.as_ref().map(RelPath::as_str),
        Some("sub/dir/video.mp4")
    );
    assert_eq!(row.size, Some(4_096));
    covered.insert("set_output");

    // 8. SetSize — size only, filename untouched (DESIGN §13.3).
    let row = apply(s, id, vec![WriteOp::SetSize { id, size: 8_192 }]).await;
    assert_eq!(row.size, Some(8_192));
    assert_eq!(
        row.filename.as_ref().map(RelPath::as_str),
        Some("sub/dir/video.mp4"),
        "a hook that rewrites the file changes the size, not the name"
    );
    covered.insert("set_size");

    // 9. PushFile — both slots, appending in order.
    let row = apply(
        s,
        id,
        vec![
            WriteOp::PushFile {
                id,
                slot: FileSlot::Chapter,
                file: support::file_ref("ch01.mp4", None),
            },
            WriteOp::PushFile {
                id,
                slot: FileSlot::Chapter,
                file: support::file_ref("ch02.mp4", None),
            },
            WriteOp::PushFile {
                id,
                slot: FileSlot::Subtitle,
                file: support::file_ref("video.en.srt", Some("en")),
            },
        ],
    )
    .await;
    assert_eq!(
        row.chapter_files
            .iter()
            .map(|f| &*f.filename)
            .collect::<Vec<_>>(),
        ["ch01.mp4", "ch02.mp4"],
        "appends keep submission order inside one transaction"
    );
    assert_eq!(row.subtitle_files.len(), 1);
    assert_eq!(row.subtitle_files[0].lang.as_deref(), Some("en"));
    covered.insert("push_file");

    // 10. DropEntryBlob
    let row = apply(s, id, vec![WriteOp::DropEntryBlob { id }]).await;
    assert!(row.entry.is_none());
    covered.insert("drop_entry_blob");

    // 11. BumpAttempt
    let row = apply(
        s,
        id,
        vec![WriteOp::BumpAttempt { id }, WriteOp::BumpAttempt { id }],
    )
    .await;
    assert_eq!(row.attempt, 2);
    covered.insert("bump_attempt");

    // 12. SetClearAfter, armed and disarmed.
    let row = apply(
        s,
        id,
        vec![WriteOp::SetClearAfter {
            id,
            at: Some(5_000),
        }],
    )
    .await;
    assert_eq!(row.clear_after, Some(5_000));
    assert_eq!(s.due_clears(5_000).await.unwrap(), vec![id]);
    let row = apply(s, id, vec![WriteOp::SetClearAfter { id, at: None }]).await;
    assert_eq!(row.clear_after, None);
    assert!(s.due_clears(i64::MAX).await.unwrap().is_empty());
    covered.insert("set_clear_after");

    // 13. DeleteItems
    s.write(vec![WriteOp::DeleteItems(vec![id])], Durability::Sync)
        .await
        .unwrap();
    assert!(s.item(id).await.unwrap().is_none());
    covered.insert("delete_items");

    // 14. UpsertSubscription
    let mut sub = SubscriptionRecord::new(
        SubId::parse("9c1f0f38-8f7a-4a7c-9f2f-1f4d1d0f4a11").unwrap(),
        "A Channel",
        url::Url::parse("https://example.com/@chan").unwrap(),
        support::selection(),
    );
    sub.check_interval_minutes = 30;
    sub.next_due = Some(9_000);
    sub.consecutive_failures = 2;
    sub.error = Some("boom".into());
    sub.last_checked = Some(8_000);
    sub.custom_name_prefix = "chan".into();
    sub.playlist_item_limit = 5;
    sub.ytdl_options_presets = vec!["fast".into()];
    s.write(
        vec![WriteOp::UpsertSubscription(Box::new(sub.clone()))],
        Durability::Sync,
    )
    .await
    .unwrap();
    let read = s.subscription(&sub.id).await.unwrap().unwrap();
    assert_eq!(read, sub, "the subscription must read back field for field");
    assert_eq!(s.subscriptions().await.unwrap(), vec![sub.clone()]);
    covered.insert("upsert_subscription");

    // 15. MarkSeen
    s.write(
        vec![WriteOp::MarkSeen {
            sub: sub.id.clone(),
            ids: vec!["a".into(), "b".into(), "c".into()],
            at: 10,
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    let seen = s.seen(&sub.id).await.unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen.contains(&Box::from("b")));
    assert_eq!(
        s.subscription(&sub.id).await.unwrap().unwrap().seen_count,
        3,
        "seen_count is denormalised from subscription_seen"
    );
    covered.insert("mark_seen");

    // 16. PruneSeen
    s.write(
        vec![WriteOp::PruneSeen {
            sub: sub.id.clone(),
            keep: 1,
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(s.seen(&sub.id).await.unwrap().len(), 1);
    covered.insert("prune_seen");

    // 17. DeleteSubscriptions — and the seen rows cascade.
    s.write(
        vec![WriteOp::DeleteSubscriptions(vec![sub.id.clone()])],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert!(s.subscription(&sub.id).await.unwrap().is_none());
    assert!(s.seen(&sub.id).await.unwrap().is_empty());
    covered.insert("delete_subscriptions");

    // 18. UpsertTelegramChat
    let cfg = ChatConfig::legacy_defaults(10, "chapter");
    s.write(
        vec![WriteOp::UpsertTelegramChat {
            chat_id: -1_001,
            config: cfg.clone(),
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(s.telegram_chats().await.unwrap().get(&-1_001), Some(&cfg));
    covered.insert("upsert_telegram_chat");

    // 19. SetKv — set, overwrite, delete.
    s.write(
        vec![WriteOp::SetKv {
            key: "cookiefile".into(),
            value: Some(serde_json::json!({ "path": "/config/cookies.txt" })),
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(
        s.kv_get("cookiefile").await.unwrap(),
        Some(serde_json::json!({ "path": "/config/cookies.txt" }))
    );
    s.write(
        vec![WriteOp::SetKv {
            key: "cookiefile".into(),
            value: None,
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(s.kv_get("cookiefile").await.unwrap(), None);
    assert!(s.kv_all().await.unwrap().is_empty());
    covered.insert("set_kv");

    // 20. SetMeta — insert then overwrite, observed through `Store::meta` (WP-05: the importer's
    // provenance keys have to land in the same transaction as the rows).
    s.write(
        vec![WriteOp::SetMeta {
            key: "imported_at".into(),
            value: "1757000000000".into(),
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(
        s.meta().await.unwrap().get("imported_at").map(|v| &**v),
        Some("1757000000000")
    );
    s.write(
        vec![WriteOp::SetMeta {
            key: "imported_at".into(),
            value: "1757000000001".into(),
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(
        s.meta().await.unwrap().get("imported_at").map(|v| &**v),
        Some("1757000000001")
    );
    // The seeded keys are untouched by a `meta` write.
    assert!(s.meta().await.unwrap().contains_key("schema_version"));
    covered.insert("set_meta");

    // 21-25. The five device ops (DESIGN §25). Observed through the `DeviceStore` reads, which is
    // the only way the notifier ever sees these tables.
    let device = DeviceRecord {
        token: "a1b2".repeat(8).into(),
        platform: "ios".into(),
        bundle_id: "com.tatoalo.aulos".into(),
        environment: ApnsEnvironment::Sandbox,
        alerts: true,
        live_activity_start_token: Some("c3d4".repeat(8).into()),
        install_id: Some("3F2504E0-4F89-11D3-9A0C-0305E82C3301".into()),
        app_version: Some("1.0.0 (3)".into()),
        registered_at: 1_757_000_000_000,
        last_seen_at: 1_757_000_000_000,
    };
    s.write(
        vec![WriteOp::UpsertDevice(Box::new(device.clone()))],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(DeviceStore::devices(s).await.unwrap(), vec![device.clone()]);
    covered.insert("upsert_device");

    let activity = LiveActivityRecord {
        device_token: device.token.clone(),
        item_id: id,
        update_token: "e5f6".repeat(8).into(),
        environment: ApnsEnvironment::Sandbox,
        registered_at: 1_757_000_000_100,
    };
    s.write(
        vec![WriteOp::UpsertLiveActivity(Box::new(activity.clone()))],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert_eq!(
        s.live_activities_for(id).await.unwrap(),
        vec![activity.clone()]
    );
    covered.insert("upsert_live_activity");

    s.write(
        vec![WriteOp::RemoveLiveActivity {
            device_token: device.token.clone(),
            item: id,
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert!(s.live_activities_for(id).await.unwrap().is_empty());
    covered.insert("remove_live_activity");

    s.write(
        vec![
            WriteOp::UpsertLiveActivity(Box::new(activity)),
            WriteOp::RemoveLiveActivitiesFor { item: id },
        ],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert!(s.live_activities_for(id).await.unwrap().is_empty());
    covered.insert("remove_live_activities_for");

    s.write(
        vec![WriteOp::RemoveDevice {
            token: device.token.clone(),
        }],
        Durability::Sync,
    )
    .await
    .unwrap();
    assert!(DeviceStore::devices(s).await.unwrap().is_empty());
    covered.insert("remove_device");

    let expected: BTreeSet<&'static str> = WriteOp::NAMES.into_iter().collect();
    assert_eq!(
        covered, expected,
        "every WriteOp variant needs a round-trip case"
    );
}

// ---------------------------------------------------------------------------
// SetStatus semantics
// ---------------------------------------------------------------------------

/// `Keep`, `Clear` and `Set` must be three distinguishable outcomes for both patchable columns.
#[tokio::test]
async fn field_update_has_three_distinguishable_outcomes() {
    let h = support::harness();
    let id = seeded(&h.store).await.id;
    let err = WireError::new(ErrorCode::Network, "transport failed");

    // Set both.
    let row = apply(
        &h.store,
        id,
        vec![WriteOp::SetStatus {
            id,
            status: Status::Error,
            msg: FieldUpdate::Set("failed".into()),
            error: FieldUpdate::Set(err.clone()),
            auto_start: None,
            at: 100,
        }],
    )
    .await;
    assert_eq!(row.msg.as_deref(), Some("failed"));
    assert_eq!(row.error.as_ref(), Some(&err));

    // Keep both.
    let row = apply(
        &h.store,
        id,
        vec![WriteOp::SetStatus {
            id,
            status: Status::Error,
            msg: FieldUpdate::Keep,
            error: FieldUpdate::Keep,
            auto_start: None,
            at: 101,
        }],
    )
    .await;
    assert_eq!(row.msg.as_deref(), Some("failed"), "Keep leaves the column");
    assert_eq!(row.error.as_ref(), Some(&err), "Keep leaves the column");

    // Clear both.
    let row = apply(
        &h.store,
        id,
        vec![WriteOp::SetStatus {
            id,
            status: Status::Queued,
            msg: FieldUpdate::Clear,
            error: FieldUpdate::Clear,
            auto_start: None,
            at: 102,
        }],
    )
    .await;
    assert_eq!(row.msg, None, "Clear nulls the column");
    assert_eq!(row.error, None, "Clear nulls the column");
}

#[tokio::test]
async fn auto_start_none_leaves_the_column_and_some_writes_it() {
    let h = support::harness();
    let id = seeded(&h.store).await.id;
    assert!(h.store.item(id).await.unwrap().unwrap().auto_start);

    let row = apply(
        &h.store,
        id,
        vec![WriteOp::SetStatus {
            id,
            status: Status::Queued,
            msg: FieldUpdate::Keep,
            error: FieldUpdate::Keep,
            auto_start: None,
            at: 1,
        }],
    )
    .await;
    assert!(row.auto_start, "None must not touch the column");

    let row = apply(
        &h.store,
        id,
        vec![WriteOp::SetStatus {
            id,
            status: Status::Queued,
            msg: FieldUpdate::Keep,
            error: FieldUpdate::Keep,
            auto_start: Some(false),
            at: 2,
        }],
    )
    .await;
    assert!(!row.auto_start);
}

/// One case per row of the DESIGN §7.1 timestamp table.
#[tokio::test]
async fn set_status_implements_the_timestamp_table() {
    let h = support::harness();
    let s = &h.store;
    let id = seeded(s).await.id;

    let status = |st: Status, at: i64| WriteOp::SetStatus {
        id,
        status: st,
        msg: FieldUpdate::Keep,
        error: FieldUpdate::Keep,
        auto_start: None,
        at,
    };

    // Row "any other status": neither timestamp is touched.
    let row = apply(s, id, vec![status(Status::Resolving, 10)]).await;
    assert_eq!(row.started_at, None);
    assert_eq!(row.finished_at, None);

    // Row "Preparing and started_at IS NULL": started_at is stamped.
    let row = apply(s, id, vec![status(Status::Preparing, 20)]).await;
    assert_eq!(row.started_at, Some(20));
    assert_eq!(row.finished_at, None);

    // Back through Preparing after a bounce: started_at is NOT re-stamped.
    apply(s, id, vec![status(Status::Queued, 25)]).await;
    let row = apply(s, id, vec![status(Status::Preparing, 30)]).await;
    assert_eq!(row.started_at, Some(20), "only the first Preparing stamps");

    // Row "terminal": finished_at is stamped.
    let row = apply(s, id, vec![status(Status::Finished, 40)]).await;
    assert_eq!(row.finished_at, Some(40));
    assert_eq!(row.started_at, Some(20));

    // Row "terminal → non-terminal": finished_at is nulled, started_at survives.
    let row = apply(s, id, vec![status(Status::Queued, 50)]).await;
    assert_eq!(row.finished_at, None);
    assert_eq!(row.started_at, Some(20), "when it first started survives");

    // `updated_at` is always written — observable through the column itself.
    let updated: i64 = s
        .read(move |c| {
            Ok(c.query_row(
                "SELECT updated_at FROM items WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(updated, 50);
}

/// The retry pair: `error → queued` clears the error, nulls `finished_at`, keeps `started_at` and
/// bumps `attempt` — and leaves the origin alone (DESIGN §7.1, §8.8, §4.4).
#[tokio::test]
async fn a_retry_clears_the_error_and_keeps_started_at_and_the_origin() {
    let h = support::harness();
    let s = &h.store;
    let seed = seeded(s).await;
    let id = seed.id;

    apply(
        s,
        id,
        vec![
            WriteOp::SetStatus {
                id,
                status: Status::Preparing,
                msg: FieldUpdate::Keep,
                error: FieldUpdate::Keep,
                auto_start: None,
                at: 100,
            },
            WriteOp::SetStatus {
                id,
                status: Status::Error,
                msg: FieldUpdate::Set("boom".into()),
                error: FieldUpdate::Set(WireError::new(ErrorCode::Network, "boom")),
                auto_start: Some(false),
                at: 200,
            },
        ],
    )
    .await;

    let row = apply(s, id, retry_ops(id, 300)).await;
    assert_eq!(row.status, Status::Queued);
    assert_eq!(row.error, None, "a retry must not leave a stale error");
    assert_eq!(row.msg, None);
    assert!(row.auto_start);
    assert_eq!(row.finished_at, None);
    assert_eq!(row.started_at, Some(100));
    assert_eq!(row.attempt, 1);
    // DESIGN §4.4: the origin outlives the retry, so per-origin routing still knows who asked.
    assert_eq!(row.source, seed.source);
}

// ---------------------------------------------------------------------------
// Failure modes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_write_against_a_deleted_row_reports_not_found() {
    let h = support::harness();
    let id = ItemId::new();
    let err = h
        .store
        .write(vec![WriteOp::BumpAttempt { id }], Durability::Sync)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::NotFound(got) if got == id),
        "{err}"
    );
    assert_eq!(err.code(), ErrorCode::NotFound);
}

/// One poisoned job must not take its batch-mates down with it.
#[tokio::test]
async fn one_failing_job_does_not_fail_the_rest_of_its_batch() {
    let h = support::harness();
    let good = support::item(0);
    let good_id = good.id;
    let doomed = ItemId::new();

    let (a, b) = tokio::join!(
        h.store.write(
            vec![WriteOp::InsertItems {
                items: vec![good.clone()]
            }],
            Durability::Batched
        ),
        h.store.write(
            vec![WriteOp::BumpAttempt { id: doomed }],
            Durability::Batched
        )
    );
    assert!(a.is_ok(), "the good job must commit: {a:?}");
    assert!(b.is_err(), "the doomed job must fail on its own");
    assert!(h.store.item(good_id).await.unwrap().is_some());
}

/// A duplicate subscription URL is the DESIGN §7.2 `UNIQUE` guard, surfaced as `409`.
#[tokio::test]
async fn a_duplicate_subscription_url_is_a_conflict() {
    let h = support::harness();
    let url = url::Url::parse("https://example.com/@chan").unwrap();
    let first = SubscriptionRecord::new(SubId::new(), "one", url.clone(), support::selection());
    let second = SubscriptionRecord::new(SubId::new(), "two", url, support::selection());

    h.store
        .write(
            vec![WriteOp::UpsertSubscription(Box::new(first))],
            Durability::Sync,
        )
        .await
        .unwrap();
    let err = h
        .store
        .write(
            vec![WriteOp::UpsertSubscription(Box::new(second))],
            Durability::Sync,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");
    assert_eq!(err.code(), ErrorCode::Conflict);
}

/// The DESIGN §7.5 hard cap: an over-sized entry becomes the truncation marker, not a 3 MB column.
#[tokio::test]
async fn an_oversized_entry_blob_is_replaced_by_the_truncation_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = support::options(dir.path());
    opts.entry_max_bytes = 128;
    let store = Store::open(opts).unwrap();

    let mut item = support::item(0);
    item.entry = Some(EntryBlob::new(
        serde_json::json!({ "blob": "x".repeat(512) }),
    ));
    let id = item.id;
    store
        .write(
            vec![WriteOp::InsertItems { items: vec![item] }],
            Durability::Sync,
        )
        .await
        .unwrap();
    let row = store.item(id).await.unwrap().unwrap();
    assert!(row.entry.as_ref().is_some_and(EntryBlob::is_truncated));

    // The same cap applies on the resolution path.
    store
        .write(
            vec![WriteOp::SetResolved {
                id,
                provider: ProviderId::parse("ytdlp").unwrap(),
                media_id: None,
                title: "t".into(),
                entry: Some(EntryBlob::new(
                    serde_json::json!({ "blob": "y".repeat(512) }),
                )),
                canonical_key: "k".into(),
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    let row = store.item(id).await.unwrap().unwrap();
    assert!(row.entry.as_ref().is_some_and(EntryBlob::is_truncated));
}

#[tokio::test]
async fn an_empty_op_list_is_a_no_op() {
    let h = support::harness();
    let before = h.store.commit_count();
    h.store.write(Vec::new(), Durability::Sync).await.unwrap();
    assert_eq!(h.store.commit_count(), before);
}

#[tokio::test]
async fn writes_are_rejected_after_close() {
    let h = support::harness();
    h.store.close().await.unwrap();
    let err = h
        .store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(0)],
            }],
            Durability::Sync,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Closed), "{err}");
    assert!(err.is_unavailable());
}
