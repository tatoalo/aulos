//! The dispatcher's contract: two phases, deterministic ordering, `HooksFinished` from every
//! path, panic and timeout isolation, and the `DropNewest` inbox (DESIGN §13, §2.2.1).
//!
//! Nothing here touches SQLite, an engine or the network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::event::{DomainEvent, DropPolicy, EventFilter, EventRouter, SubscriberSpec};
use aulos_core::status::Status;
use aulos_hooks::hook::Hook;
use aulos_hooks::{HookDispatcher, NoopFinalizer};
use common::{
    Behaviour, FakeStore, ItemBuilder, LoggingFinalizer, ScriptHook, config, events, log, read_log,
    settle, sink, until,
};

/// PLAN WP-11: "the observed run order is audio_sync → nfo → jellyfin, and a community hook at
/// `ordering = 50` lands between nfo and jellyfin. audio_sync runs on `Finishing` and the other
/// two on `Completed`, asserted by a fake engine that records the command sequence."
#[tokio::test]
async fn the_two_phases_and_the_ordering_are_exactly_the_documented_sequence() {
    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        Arc::new(ScriptHook::new("jellyfin", 90, Arc::clone(&seq))),
        Arc::new(ScriptHook::new("hook:media/plex", 50, Arc::clone(&seq))),
        Arc::new(ScriptHook::new("audio_sync", 10, Arc::clone(&seq)).pre_terminal()),
        Arc::new(ScriptHook::new("nfo", 20, Arc::clone(&seq))),
    ];
    let finalizer = LoggingFinalizer::new(Arc::clone(&seq));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        hooks,
        Arc::new(aulos_core::clock::FakeClock::default()),
    )
    .with_finalizer(Arc::clone(&finalizer) as Arc<_>);
    assert_eq!(
        dispatcher
            .hook_ids()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["audio_sync", "nfo", "hook:media/plex", "jellyfin"],
        "registration order is (ordering, id)"
    );

    let mut ev = events();
    let (factory, _rx) = sink();
    let store = FakeStore::new();
    let task = dispatcher.spawn(ev.inbox(), factory, store);

    // The engine's `Finished` command becomes a `Finishing` event...
    let item = ItemBuilder::finished("Clip").status(Status::Postprocessing);
    let id = item.id();
    let view = item.view();
    ev.finishing(&view).await;
    assert!(
        until(|| finalizer.ids() == vec![id]).await,
        "the pre-terminal phase must end with exactly one HooksFinished"
    );
    assert_eq!(read_log(&seq), ["audio_sync", "HooksFinished"]);

    // ...and only then does the engine finalise and publish `Completed`.
    let done = ItemBuilder::finished("Clip").view();
    ev.completed(&done).await;
    assert!(
        until(|| read_log(&seq).len() == 5).await,
        "{:?}",
        read_log(&seq)
    );
    assert_eq!(
        read_log(&seq),
        [
            "audio_sync",
            "HooksFinished",
            "nfo",
            "hook:media/plex",
            "jellyfin"
        ]
    );

    drop(ev.tx);
    task.await.expect("the dispatcher stops cleanly");
}

/// PLAN WP-11: "`HooksFinished` is still sent when the pre-terminal hook returns an error, panics,
/// or exceeds its timeout (three separate cases), because an item that never finalises is worse
/// than a failed re-encode."
#[tokio::test]
async fn hooks_finished_is_sent_even_when_the_pre_terminal_hook_fails() {
    for behaviour in [Behaviour::Fail, Behaviour::Panic, Behaviour::Hang] {
        let seq = log();
        let hook = Arc::new(
            ScriptHook::new("audio_sync", 10, Arc::clone(&seq))
                .pre_terminal()
                .behaving(behaviour)
                .timing_out_after(Duration::from_millis(50)),
        );
        let finalizer = LoggingFinalizer::new(Arc::clone(&seq));
        let dispatcher = HookDispatcher::with_hooks(
            config(&[]),
            vec![hook as Arc<dyn Hook>],
            Arc::new(aulos_core::clock::FakeClock::default()),
        )
        .with_finalizer(Arc::clone(&finalizer) as Arc<_>);
        let health = dispatcher.health_handle();

        let mut ev = events();
        let (factory, _rx) = sink();
        let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

        let item = ItemBuilder::finished("Clip").status(Status::Postprocessing);
        let id = item.id();
        ev.finishing(&item.view()).await;

        assert!(
            until(|| finalizer.ids() == vec![id]).await,
            "{behaviour:?} must still finalise the item"
        );
        let stat = health
            .health()
            .stat("audio_sync")
            .cloned()
            .expect("audio_sync is registered");
        assert_eq!(stat.failures_total, 1, "{behaviour:?} counts as a failure");
        assert!(
            stat.last_error.is_some(),
            "{behaviour:?} records its reason"
        );
        assert!(stat.last_success_at.is_none());

        drop(ev.tx);
        task.await.expect("the dispatcher survives");
    }
}

/// A `Finishing` event with **no** applicable pre-terminal hook must still finalise: the engine
/// decided to wait for us, and disagreeing about `applies()` would hang the item forever.
#[tokio::test]
async fn a_finishing_event_with_no_applicable_hook_still_finalises() {
    let seq = log();
    let finalizer = LoggingFinalizer::new(Arc::clone(&seq));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        Vec::new(),
        Arc::new(aulos_core::clock::FakeClock::default()),
    )
    .with_finalizer(Arc::clone(&finalizer) as Arc<_>);

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    let item = ItemBuilder::finished("Clip").status(Status::Postprocessing);
    let id = item.id();
    ev.finishing(&item.view()).await;
    assert!(until(|| finalizer.ids() == vec![id]).await);
    drop(ev.tx);
    task.await.expect("clean stop");
}

/// PLAN WP-11: "A hook that panics is caught, counted, and does not take down the dispatcher."
#[tokio::test]
async fn a_panicking_hook_is_counted_and_the_dispatcher_keeps_serving() {
    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        Arc::new(ScriptHook::new("boom", 10, Arc::clone(&seq)).behaving(Behaviour::Panic)),
        Arc::new(ScriptHook::new("survivor", 20, Arc::clone(&seq))),
    ];
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        hooks,
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let health = dispatcher.health_handle();

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    for _ in 0..3 {
        ev.completed(&ItemBuilder::finished("Clip").view()).await;
    }
    assert!(
        until(|| health
            .health()
            .stat("survivor")
            .is_some_and(|s| s.runs_total == 3))
        .await,
        "every later event still ran: {:?}",
        health.health()
    );
    let boom = health.health().stat("boom").cloned().expect("boom");
    assert_eq!(boom.failures_total, 3);
    assert_eq!(boom.runs_total, 3);
    assert_eq!(
        boom.health.status,
        aulos_core::health::ComponentStatus::Degraded,
        "a failing hook is degraded, not down"
    );

    drop(ev.tx);
    task.await.expect("the dispatcher survives three panics");
}

/// A hook failure never changes the item's status: the dispatcher has no way to say otherwise —
/// it holds no `EngineCmd` sender other than the finalizer, which carries an id and nothing else.
/// What is observable here is that a post-terminal failure produces no engine traffic at all.
#[tokio::test]
async fn a_post_terminal_failure_produces_no_engine_traffic() {
    let seq = log();
    let hook = Arc::new(ScriptHook::new("nfo", 20, Arc::clone(&seq)).behaving(Behaviour::Fail));
    let finalizer = LoggingFinalizer::new(Arc::clone(&seq));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![hook as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    )
    .with_finalizer(Arc::clone(&finalizer) as Arc<_>);
    let health = dispatcher.health_handle();

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    ev.completed(&ItemBuilder::finished("Clip").view()).await;
    assert!(
        until(|| health
            .health()
            .stat("nfo")
            .is_some_and(|s| s.failures_total == 1))
        .await
    );
    assert!(
        finalizer.ids().is_empty(),
        "no HooksFinished for a post-terminal hook"
    );
    drop(ev.tx);
    task.await.expect("clean stop");
}

/// `AULOS_HOOKS_ENABLED=false` disables the lot (DESIGN §13).
#[tokio::test]
async fn hooks_enabled_false_runs_nothing() {
    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        Arc::new(ScriptHook::new("audio_sync", 10, Arc::clone(&seq)).pre_terminal()),
        Arc::new(ScriptHook::new("nfo", 20, Arc::clone(&seq))),
    ];
    let finalizer = LoggingFinalizer::new(Arc::clone(&seq));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[("AULOS_HOOKS_ENABLED", "false")]),
        hooks,
        Arc::new(aulos_core::clock::FakeClock::default()),
    )
    .with_finalizer(Arc::clone(&finalizer) as Arc<_>);

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    let item = ItemBuilder::finished("Clip").status(Status::Postprocessing);
    let id = item.id();
    ev.finishing(&item.view()).await;
    ev.completed(&ItemBuilder::finished("Clip").view()).await;
    // The item must still finalise — disabling hooks must not wedge the queue.
    assert!(until(|| finalizer.ids() == vec![id]).await);
    settle().await;
    assert_eq!(read_log(&seq), ["HooksFinished"], "no hook ran");
    drop(ev.tx);
    task.await.expect("clean stop");
}

/// A `Completed` event whose view is not terminal is a wiring bug, and must be ignored rather than
/// dispatched: `applies()` would otherwise be asked about an outcome that does not exist.
#[tokio::test]
async fn a_non_terminal_completed_event_is_ignored() {
    let seq = log();
    let hook = Arc::new(ScriptHook::new("nfo", 20, Arc::clone(&seq)));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![hook as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    )
    .with_finalizer(Arc::new(NoopFinalizer));
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    ev.completed(
        &ItemBuilder::finished("Clip")
            .status(Status::Downloading)
            .view(),
    )
    .await;
    settle().await;
    assert!(read_log(&seq).is_empty());
    drop(ev.tx);
    task.await.expect("clean stop");
}

/// PLAN WP-11: "A saturated dispatcher inbox drops the **newest** `Completed` event, increments
/// `aulos_event_dropped_total{subscriber="hooks"}`, and does not stall the aggregator."
#[tokio::test]
async fn a_saturated_inbox_drops_the_newest_event_and_does_not_stall_the_aggregator() {
    let (mut router, tx) = EventRouter::new(64);
    let hooks_inbox = router.subscribe(SubscriberSpec {
        name: "hooks",
        capacity: 2,
        policy: DropPolicy::DropNewest,
        filter: EventFilter::hooks(),
    });
    let mut aggregator = router.subscribe(SubscriberSpec::aggregator());
    let router = router.spawn();

    // Nothing is consuming the hooks inbox, so it saturates after two events.
    let mut views = Vec::new();
    for i in 0..20 {
        let view = ItemBuilder::finished(&format!("Clip {i}")).view();
        views.push(Arc::clone(&view));
        tx.publish(DomainEvent::Completed(view)).await;
    }

    // The aggregator is a `Block` subscriber and must have received all twenty.
    let mut seen = 0;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), aggregator.recv()).await {
        if matches!(&*ev, DomainEvent::Completed(_)) {
            seen += 1;
        }
        if seen == 20 {
            break;
        }
    }
    assert_eq!(seen, 20, "the aggregator never stalls or loses an event");
    assert!(
        hooks_inbox.dropped() >= 18,
        "a capacity-2 hooks inbox drops the rest: {}",
        hooks_inbox.dropped()
    );
    assert_eq!(aggregator.dropped(), 0, "a Block subscriber never drops");

    drop(tx);
    router.await.expect("the router stops");
}

/// The drop counter reaches `healthz` through the dispatcher (DESIGN §16.3's `components.events`).
#[tokio::test]
async fn the_drop_counter_is_surfaced_in_health() {
    let seq = log();
    let hook = Arc::new(ScriptHook::new("nfo", 20, Arc::clone(&seq)));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![hook as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let health = dispatcher.health_handle();
    assert_eq!(health.health().events_dropped, 0);

    let (mut router, tx) = EventRouter::new(64);
    let inbox = router.subscribe(SubscriberSpec {
        name: "hooks",
        capacity: 1,
        policy: DropPolicy::DropNewest,
        filter: EventFilter::hooks(),
    });
    let router_task = router.spawn();

    // Publish before the dispatcher starts consuming, so the inbox is already over capacity.
    for i in 0..8 {
        tx.publish(DomainEvent::Completed(
            ItemBuilder::finished(&format!("Clip {i}")).view(),
        ))
        .await;
    }
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(inbox, factory, FakeStore::new());
    assert!(until(|| health.health().events_dropped > 0).await);

    drop(tx);
    task.await.expect("clean stop");
    router_task.await.expect("the router stops");
}

/// The bug this was written for: `runs_total: 0, failures_total: 0, status: "ok"` is what a hook
/// that silently declines every completion looked like, and it is indistinguishable from a hook
/// that has simply had nothing to do. A skip is now counted, carries its reason, and says so in
/// the component detail — while staying `ok`, because declining is not a failure.
#[tokio::test]
async fn a_hook_that_declines_every_event_is_visible_in_health() {
    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        Arc::new(
            ScriptHook::new("nfo", 20, Arc::clone(&seq)).skipping("AULOS_NFO_ENABLED is false"),
        ),
        Arc::new(ScriptHook::new("jellyfin", 90, Arc::clone(&seq))),
    ];
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        hooks,
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let health = dispatcher.health_handle();

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    ev.completed(&ItemBuilder::finished("Clip").view()).await;
    ev.completed(&ItemBuilder::finished("Altro").view()).await;

    assert!(
        until(|| health
            .health()
            .stat("jellyfin")
            .is_some_and(|s| s.runs_total == 2))
        .await,
        "the rest of the chain ran, so the completion path itself worked"
    );
    assert!(
        until(|| health
            .health()
            .stat("nfo")
            .is_some_and(|s| s.skipped_total == 2))
        .await
    );

    let view = health.health();
    let nfo = view.stat("nfo").expect("nfo");
    assert_eq!(nfo.runs_total, 0);
    assert_eq!(nfo.failures_total, 0);
    assert_eq!(nfo.skipped_total, 2);
    assert_eq!(
        nfo.last_skip_reason.as_deref(),
        Some("AULOS_NFO_ENABLED is false")
    );

    let component = view.component("nfo").expect("the nfo component");
    assert_eq!(
        component.status,
        aulos_core::health::ComponentStatus::Ok,
        "a skip is not a failure"
    );
    assert_eq!(component.detail["runs_total"], 0);
    assert_eq!(component.detail["skipped_total"], 2);
    assert_eq!(
        component.detail["last_skip_reason"],
        "AULOS_NFO_ENABLED is false"
    );
    assert_eq!(
        component.detail["detail"],
        "never ran: 2 event(s) skipped, most recently because AULOS_NFO_ENABLED is false",
        "the one thing `runs_total: 0` could not say"
    );

    // A hook that never declined anything publishes the shape it always did: no counter, no
    // reason, nothing for the documented stock payload or its snapshot to drift against.
    let jellyfin = view.component("jellyfin").expect("the jellyfin component");
    assert!(
        !jellyfin.detail.contains_key("skipped_total"),
        "a zero counter is not published: {:?}",
        jellyfin.detail
    );
    assert!(!jellyfin.detail.contains_key("last_skip_reason"));
    assert!(!jellyfin.detail.contains_key("detail"));

    drop(ev.tx);
    task.await.expect("clean stop");
}

/// A hook of the *other* phase was never offered the event, so it is not counted as a skip — only
/// a hook that was asked and declined is.
#[tokio::test]
async fn the_other_phase_is_not_a_skip() {
    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        Arc::new(ScriptHook::new("audio_sync", 10, Arc::clone(&seq)).pre_terminal()),
        Arc::new(ScriptHook::new("nfo", 20, Arc::clone(&seq))),
    ];
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        hooks,
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let health = dispatcher.health_handle();

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    ev.completed(&ItemBuilder::finished("Clip").view()).await;
    assert!(
        until(|| health
            .health()
            .stat("nfo")
            .is_some_and(|s| s.runs_total == 1))
        .await
    );
    settle().await;
    assert_eq!(
        health.health().stat("audio_sync").map(|s| s.skipped_total),
        Some(0),
        "a pre-terminal hook is not skipping the Completed event, it never sees it"
    );
    drop(ev.tx);
    task.await.expect("clean stop");
}
