//! Group aggregates through the engine: the roll-up, the wire counters and the drift recompute
//! (DESIGN §8.6).
//!
//! The pure arithmetic — byte-weighted percent, the count-weighted fallback, the eight-value
//! roll-up, `correct()` — is unit-tested in `src/groups.rs`. What is asserted here is that the
//! engine keeps the accumulator in step with real children and publishes it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::{DomainEvent, Kind, Status};
use aulos_queue::GroupAcc;
use support::{Harness, expanding, request};

/// The group's last published view, from either a change or a completion frame.
fn last_group_view(h: &Harness, id: aulos_core::ItemId) -> Arc<aulos_core::ItemView> {
    h.events
        .all()
        .iter()
        .rev()
        .find_map(|e| match &**e {
            DomainEvent::StatusChanged { id: got, view, .. } if *got == id => {
                Some(Arc::clone(view))
            }
            DomainEvent::Completed(view) if view.id == id => Some(Arc::clone(view)),
            DomainEvent::Added(views, _) => views.iter().find(|v| v.id == id).map(Arc::clone),
            _ => None,
        })
        .expect("the group was published at least once")
}

#[tokio::test]
async fn a_group_publishes_the_three_child_counters_and_children_inline() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(3)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let mut req = request("https://fake.test/playlist/counters");
    req.auto_start = false;
    let group = h.add_request(req).await.unwrap().ids[0];
    h.until_all("the children", |rows| rows.len() == 4).await;
    h.settle().await;

    let view = last_group_view(&h, group);
    assert_eq!(view.kind, Kind::Group);
    assert_eq!(view.children_total, Some(3));
    assert_eq!(view.children_done, Some(0));
    assert_eq!(view.children_error, Some(0));
    assert_eq!(view.children_active, Some(0));
    assert_eq!(
        view.children_inline,
        Some(true),
        "v1.0: the snapshot always carries every child (BRIEF scope trim)"
    );
    assert_eq!(view.status, Status::Queued);
    assert_eq!(view.percent, 0.0);
}

#[tokio::test]
async fn the_counters_follow_the_children_all_the_way_to_finished() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(3)))
        .build()
        .await;
    let group = h.add("https://fake.test/playlist/run").await;
    h.until(group, "the group roll-up", |i| i.status == Status::Finished)
        .await;
    h.settle().await;

    let view = last_group_view(&h, group);
    assert_eq!(view.children_total, Some(3));
    assert_eq!(view.children_done, Some(3));
    assert_eq!(view.children_error, Some(0));
    assert_eq!(view.children_active, Some(0));
    assert_eq!(view.status, Status::Finished);
    assert_eq!(view.percent, 100.0);
    assert_eq!(view.speed, None);
    assert_eq!(view.eta, None);
}

#[tokio::test]
async fn a_group_whose_children_all_failed_rolls_up_to_error() {
    let failing = aulos_provider::fake::FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        url_regex = "playlist"
        resolve = [{ kind = "expand_playlist", count = 2 }]
        download = [{ kind = "fail", code = "unavailable" }]

        [[timeline]]
        resolve = []
    "#,
    )
    .unwrap();
    let h = Harness::builder().provider(Arc::new(failing)).build().await;
    let group = h.add("https://fake.test/playlist/broken").await;
    let row = h
        .until(group, "the error roll-up", |i| i.status == Status::Error)
        .await;
    assert_eq!(row.status, Status::Error);
    h.settle().await;
    let view = last_group_view(&h, group);
    assert_eq!(view.children_error, Some(2));
    assert_eq!(view.children_done, Some(0));
}

#[tokio::test]
async fn a_mixed_group_prefers_error_over_canceled_and_downloading_over_both() {
    // Asserted directly on the accumulator so every branch of the closed roll-up is covered
    // without needing four providers.
    let mut acc = GroupAcc::new(3);
    acc.add_child(Status::Downloading, None);
    acc.add_child(Status::Error, None);
    acc.add_child(Status::Canceled, None);
    assert_eq!(acc.status(), Status::Downloading);
    acc.on_child_status(Status::Downloading, Status::Finished);
    assert_eq!(acc.status(), Status::Error, "error outranks canceled");
    acc.on_child_status(Status::Error, Status::Finished);
    assert_eq!(acc.status(), Status::Canceled);
    acc.on_child_status(Status::Canceled, Status::Finished);
    assert_eq!(acc.status(), Status::Finished);
    for _ in 0..4 {
        assert!(aulos_core::Status::ALL.contains(&acc.status()));
    }
}

#[tokio::test]
async fn the_drift_recompute_corrects_a_deliberately_corrupted_accumulator() {
    // The engine's five-minute pass rebuilds every accumulator from its children. Corrupting one
    // and then rebuilding is the same operation the engine performs, so this asserts the
    // correction end to end against real rows.
    let h = Harness::builder()
        .provider(Arc::new(expanding(4)))
        .build()
        .await;
    let group = h.add("https://fake.test/playlist/drift").await;
    h.until(group, "the group roll-up", |i| i.status == Status::Finished)
        .await;
    let children = h.children(group).await;
    assert_eq!(children.len(), 4);

    let fresh = GroupAcc::recomputed(4, children.iter(), aulos_queue::entry::size_hint);
    let mut corrupted = fresh.clone();
    corrupted.counts[Status::Finished as usize] = 1;
    corrupted.counts[Status::Downloading as usize] = 3;
    corrupted.finished_bytes = 7;
    assert_eq!(
        corrupted.status(),
        Status::Downloading,
        "the corrupted accumulator reports a lie"
    );

    assert!(corrupted.correct(&fresh), "the drift is detected");
    assert_eq!(corrupted, fresh, "and corrected");
    assert_eq!(corrupted.status(), Status::Finished);
    assert!(
        !corrupted.correct(&fresh),
        "a second pass over a healthy accumulator reports no drift"
    );

    // The engine's own pass runs on the tick once the five minutes are up.
    h.advance(Duration::from_secs(301)).await;
    h.settle().await;
    assert_eq!(
        h.item(group).await.unwrap().status,
        Status::Finished,
        "and it does not disturb a correct one"
    );
}

#[tokio::test]
async fn the_byte_weighted_percent_survives_a_round_trip_through_the_entry_hints() {
    // The engine reads a child's byte estimate out of its persisted entry blob, which is what
    // makes the byte-weighted branch available after a restart too.
    let h = Harness::builder()
        .provider(Arc::new(expanding(2)))
        .build()
        .await;
    let group = h.add("https://fake.test/playlist/bytes").await;
    h.until(group, "the group roll-up", |i| i.status == Status::Finished)
        .await;
    let children = h.children(group).await;
    let acc = GroupAcc::recomputed(2, children.iter(), aulos_queue::entry::size_hint);
    assert_eq!(acc.n_with_total, 2, "both children know their size");
    assert!(acc.byte_weighted());
    assert_eq!(acc.total_est, 2 * 1_024);
}
