//! The `DeviceStore` port on the store handle: upsert semantics, the two cascades, and the
//! idempotence every removal promises (DESIGN §25, PROTOCOL §4.8).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use aulos_core::{ApnsEnvironment, DeviceRecord, DeviceStore, ItemId, LiveActivityRecord, Ord0};
use aulos_store::{Durability, Store, WriteOp};

/// A 64-character lowercase-hex token built from one nibble, so a test can name several.
fn token(seed: char) -> Box<str> {
    std::iter::repeat_n(seed, 64).collect::<String>().into()
}

fn device(seed: char, environment: ApnsEnvironment) -> DeviceRecord {
    DeviceRecord {
        token: token(seed),
        platform: "ios".into(),
        bundle_id: "com.tatoalo.aulos".into(),
        environment,
        alerts: true,
        live_activity_start_token: None,
        app_version: Some("1.0.0 (3)".into()),
        registered_at: 1_757_000_000_000,
        last_seen_at: 1_757_000_000_000,
    }
}

fn activity(device: &DeviceRecord, item: ItemId, update: char) -> LiveActivityRecord {
    LiveActivityRecord {
        device_token: device.token.clone(),
        item_id: item,
        update_token: token(update),
        environment: device.environment,
        registered_at: 1_757_000_000_500,
    }
}

/// Inserts one item row and returns its id.
async fn seed_item(store: &Store, ord: Ord0) -> ItemId {
    let item = support::item(ord);
    let id = item.id;
    store
        .write(
            vec![WriteOp::InsertItems { items: vec![item] }],
            Durability::Sync,
        )
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn a_repeat_registration_refreshes_every_field_but_registered_at() {
    let h = support::harness();
    let first = device('a', ApnsEnvironment::Sandbox);
    h.store.upsert_device(first.clone()).await.unwrap();

    let again = DeviceRecord {
        environment: ApnsEnvironment::Production,
        alerts: false,
        live_activity_start_token: Some(token('b')),
        app_version: Some("1.1.0 (9)".into()),
        registered_at: 1_999_999_999_999,
        last_seen_at: 1_757_000_600_000,
        ..first.clone()
    };
    h.store.upsert_device(again.clone()).await.unwrap();

    let stored = DeviceStore::devices(&h.store).await.unwrap();
    assert_eq!(stored.len(), 1, "the token is the key: {stored:?}");
    assert_eq!(
        stored[0],
        DeviceRecord {
            registered_at: first.registered_at,
            ..again
        },
        "everything but registered_at, which keeps saying when the token was first seen"
    );
}

#[tokio::test]
async fn forgetting_a_device_cascades_to_its_live_activities() {
    let h = support::harness();
    let kept = device('a', ApnsEnvironment::Sandbox);
    let dropped = device('b', ApnsEnvironment::Production);
    h.store.upsert_device(kept.clone()).await.unwrap();
    h.store.upsert_device(dropped.clone()).await.unwrap();

    let item = seed_item(&h.store, 0).await;
    h.store
        .upsert_live_activity(activity(&kept, item, 'c'))
        .await
        .unwrap();
    h.store
        .upsert_live_activity(activity(&dropped, item, 'd'))
        .await
        .unwrap();
    assert_eq!(h.store.live_activities_for(item).await.unwrap().len(), 2);

    h.store.remove_device(&dropped.token).await.unwrap();

    let left = h.store.live_activities_for(item).await.unwrap();
    assert_eq!(left.len(), 1, "ON DELETE CASCADE took the other: {left:?}");
    assert_eq!(left[0].device_token, kept.token);
    assert_eq!(
        DeviceStore::devices(&h.store)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.token)
            .collect::<Vec<_>>(),
        vec![kept.token]
    );
}

/// Every removal the port declares is idempotent, because APNs reports tokens the app has already
/// deleted through the REST route.
#[tokio::test]
async fn every_removal_is_idempotent() {
    let h = support::harness();
    let unknown = ItemId::new();

    h.store.remove_device(&token('f')).await.unwrap();
    h.store
        .remove_live_activity(&token('f'), unknown)
        .await
        .unwrap();
    h.store.remove_live_activities_for(unknown).await.unwrap();

    let d = device('a', ApnsEnvironment::Sandbox);
    let item = seed_item(&h.store, 0).await;
    h.store.upsert_device(d.clone()).await.unwrap();
    h.store
        .upsert_live_activity(activity(&d, item, 'c'))
        .await
        .unwrap();

    for _ in 0..2 {
        h.store.remove_live_activity(&d.token, item).await.unwrap();
    }
    assert!(h.store.live_activities_for(item).await.unwrap().is_empty());
    for _ in 0..2 {
        h.store.remove_device(&d.token).await.unwrap();
    }
    assert!(DeviceStore::devices(&h.store).await.unwrap().is_empty());
}

#[tokio::test]
async fn re_forwarding_an_update_token_replaces_it_in_place() {
    let h = support::harness();
    let d = device('a', ApnsEnvironment::Sandbox);
    h.store.upsert_device(d.clone()).await.unwrap();
    let item = seed_item(&h.store, 0).await;

    h.store
        .upsert_live_activity(activity(&d, item, 'c'))
        .await
        .unwrap();
    let rotated = LiveActivityRecord {
        registered_at: 1_757_000_900_000,
        ..activity(&d, item, 'e')
    };
    h.store.upsert_live_activity(rotated.clone()).await.unwrap();

    assert_eq!(
        h.store.live_activities_for(item).await.unwrap(),
        vec![rotated],
        "(device_token, item_id) is the key; the token rotates under it"
    );
}

/// The registration may race ahead of the item row, so `item_id` is not a foreign key.
#[tokio::test]
async fn an_activity_may_be_registered_for_an_item_that_does_not_exist_yet() {
    let h = support::harness();
    let d = device('a', ApnsEnvironment::Sandbox);
    h.store.upsert_device(d.clone()).await.unwrap();

    let ghost = ItemId::new();
    h.store
        .upsert_live_activity(activity(&d, ghost, 'c'))
        .await
        .unwrap();
    assert_eq!(h.store.live_activities_for(ghost).await.unwrap().len(), 1);
}

/// An activity under a device that was never registered is refused by the foreign key rather than
/// silently orphaned — the API answers `404` before it ever gets here (PROTOCOL §4.8).
#[tokio::test]
async fn an_activity_under_an_unknown_device_is_refused() {
    let h = support::harness();
    let stranger = device('9', ApnsEnvironment::Sandbox);
    let item = seed_item(&h.store, 0).await;
    let err = h
        .store
        .upsert_live_activity(activity(&stranger, item, 'c'))
        .await
        .expect_err("the FOREIGN KEY must reject it");
    assert!(
        err.to_string().to_lowercase().contains("constraint"),
        "{err}"
    );
}

/// Deleting an item takes its Live Activity rows with it — and a group takes its children's,
/// which is the case a foreign key on `item_id` could not have covered.
#[tokio::test]
async fn deleting_an_item_or_a_group_takes_the_live_activities_with_it() {
    use aulos_core::Kind;

    let h = support::harness();
    let d = device('a', ApnsEnvironment::Sandbox);
    h.store.upsert_device(d.clone()).await.unwrap();

    let mut group = support::item(0);
    group.kind = Kind::Group;
    group.children_total = Some(1);
    let group_id = group.id;
    let mut child = support::item(1);
    child.group_id = Some(group_id);
    child.group_index = Some(0);
    let child_id = child.id;
    let lone_id = seed_item(&h.store, 2).await;
    h.store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![group, child],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();

    for item in [group_id, child_id, lone_id] {
        h.store
            .upsert_live_activity(activity(&d, item, 'c'))
            .await
            .unwrap();
    }

    h.store
        .write(vec![WriteOp::DeleteItems(vec![group_id])], Durability::Sync)
        .await
        .unwrap();

    assert!(
        h.store
            .live_activities_for(group_id)
            .await
            .unwrap()
            .is_empty(),
        "the group's own registration goes"
    );
    assert!(
        h.store
            .live_activities_for(child_id)
            .await
            .unwrap()
            .is_empty(),
        "and so does the child's, which only items.group_id could reach"
    );
    assert_eq!(
        h.store.live_activities_for(lone_id).await.unwrap().len(),
        1,
        "an unrelated item keeps its own"
    );

    h.store
        .write(vec![WriteOp::DeleteItems(vec![lone_id])], Durability::Sync)
        .await
        .unwrap();
    assert!(
        h.store
            .live_activities_for(lone_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn registrations_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path())).unwrap();
    let d = device('a', ApnsEnvironment::Production);
    store.upsert_device(d.clone()).await.unwrap();
    let item = seed_item(&store, 0).await;
    store
        .upsert_live_activity(activity(&d, item, 'c'))
        .await
        .unwrap();
    store.close().await.unwrap();

    let reopened = Store::open(support::options(dir.path())).unwrap();
    assert_eq!(DeviceStore::devices(&reopened).await.unwrap(), vec![d]);
    assert_eq!(reopened.live_activities_for(item).await.unwrap().len(), 1);
}
