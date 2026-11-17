//! Successful download history, migration and queue deletion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use aulos_core::{FieldUpdate, Status};
use aulos_store::{Durability, ReleaseIdentity, Store, WriteOp};
use rusqlite::{Connection, params};

fn identity(item: &aulos_core::Item) -> ReleaseIdentity {
    ReleaseIdentity {
        canonical_key: item.canonical_key.clone(),
        url_key: "unresolved-url".into(),
    }
}

#[tokio::test]
async fn only_successful_downloads_survive_item_deletion_and_restart() {
    let h = support::harness();
    let item = support::item(1);
    h.store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![item.clone()],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    for status in [Status::Error, Status::Canceled, Status::Finished] {
        h.store
            .write(
                vec![WriteOp::SetStatus {
                    id: item.id,
                    status,
                    msg: FieldUpdate::Clear,
                    error: FieldUpdate::Clear,
                    auto_start: None,
                    at: 42,
                }],
                Durability::Sync,
            )
            .await
            .unwrap();
        let states = h
            .store
            .release_states(vec![identity(&item)], "video")
            .await
            .unwrap();
        assert_eq!(
            states[0].downloaded_at,
            (status == Status::Finished).then_some(42)
        );
    }
    h.store
        .write(vec![WriteOp::DeleteItems(vec![item.id])], Durability::Sync)
        .await
        .unwrap();
    h.store.close().await.unwrap();
    let reopened = Store::open(support::options(h.dir.path())).unwrap();
    assert_eq!(
        reopened
            .release_states(vec![identity(&item)], "video")
            .await
            .unwrap()[0]
            .downloaded_at,
        Some(42)
    );
    assert_eq!(
        reopened
            .release_states(vec![identity(&item)], "audio")
            .await
            .unwrap()[0]
            .downloaded_at,
        None
    );
}

#[tokio::test]
async fn an_active_item_is_reported_separately_from_successful_history() {
    let h = support::harness();
    let item = support::item(1);
    h.store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![item.clone()],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    let states = h
        .store
        .release_states(vec![identity(&item)], "video")
        .await
        .unwrap();
    assert_eq!(states[0].queued_item_id, Some(item.id));
    assert_eq!(states[0].downloaded_at, None);
    assert_eq!(
        h.store
            .release_states(vec![identity(&item)], "audio")
            .await
            .unwrap()[0]
            .queued_item_id,
        None
    );
}

#[tokio::test]
async fn migration_backfills_successes_and_subscription_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = support::db_path(dir.path());
    let conn = Connection::open(&path).unwrap();
    for migration in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_clear_finished_msg.sql"),
        include_str!("../migrations/0003_devices.sql"),
        include_str!("../migrations/0004_device_install_id.sql"),
    ] {
        conn.execute_batch(migration).unwrap();
    }
    conn.pragma_update(None, "user_version", 4).unwrap();
    let item = support::item(1);
    let request = serde_json::to_string(&item.request).unwrap();
    conn.execute("INSERT INTO subscriptions(id,name,url,request_json,created_at,updated_at) VALUES ('feed','Original name','https://example.com/feed',?1,1,1)", [&request]).unwrap();
    conn.execute(
        "INSERT INTO items(id,kind,ord,url,canonical_key,title,status,request_json,source_json,created_at,updated_at,finished_at) \
         VALUES (?1,'item',1,?2,?3,'Downloaded','finished',?4,'{\"kind\":\"subscription\",\"ref\":\"feed\"}',1,42,42)",
        params![item.id.to_string(), item.url.as_str(), &*item.canonical_key, request],
    ).unwrap();
    drop(conn);
    let store = Store::open(support::options(dir.path())).unwrap();
    assert_eq!(
        store
            .release_states(vec![identity(&item)], "video")
            .await
            .unwrap()[0]
            .downloaded_at,
        Some(42)
    );
    assert_eq!(
        store
            .item(item.id)
            .await
            .unwrap()
            .unwrap()
            .source
            .name
            .as_deref(),
        Some("Original name")
    );
    store
        .write(
            vec![WriteOp::DeleteSubscriptions(vec![
                aulos_core::SubId::parse("feed").unwrap(),
            ])],
            Durability::Sync,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .item(item.id)
            .await
            .unwrap()
            .unwrap()
            .source
            .name
            .as_deref(),
        Some("Original name")
    );
}
