//! The `healthz` component publisher (DESIGN §16.3).
//!
//! [`aulos_core::HealthRegistry`] is a map of `name → ComponentHealth` that `aulos-api` serves
//! verbatim. Most entries are written once, by a boot probe (`ytdlp_runner`, `ffmpeg`, `deno`,
//! `importer`) or by the component that owns the fact (`pot` by the supervisor, `ytdl_options` by
//! the config watcher). This module owns the ones that **move**: the store's latency and WAL, the
//! queue's composition and slots, each hook's counters, the event-router drop counters and the
//! subscription schedule.
//!
//! It publishes `DomainEvent::HealthChanged` only when the view actually changed
//! ([`aulos_core::HealthRegistry::set`] reports that), because an unchanging `health` frame every
//! second would be a frame per second per client forever.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, EventSender};
use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_core::subscription::{SubCmd, SubscriptionsHandle};
use aulos_hooks::HooksHealthHandle;
use aulos_provider::Registry;
use aulos_provider::sink::ProgressSinkFactory;
use aulos_queue::StateView;
use aulos_store::Store;
use aulos_telegram::{TelegramHealth, TelegramHealthHandle};
use tokio_util::sync::CancellationToken;

/// How often the moving components are recomputed.
pub const TICK: Duration = Duration::from_secs(2);

/// Everything the publisher reads.
pub struct Probes {
    /// The store handle: WAL size, database size, and a real read for the latency figure.
    pub store: Store,
    /// The published snapshot: the queue composition, with no database round trip.
    pub state: StateView,
    /// Which providers keep their own slots (DESIGN §8.7).
    pub registry: Arc<RwLock<Registry>>,
    /// The effective configuration: the slot totals.
    pub cfg: Arc<Config>,
    /// The progress channel's drop counter (DESIGN §2.3).
    pub sink: ProgressSinkFactory,
    /// Each hook's counters, taken **before** `HookDispatcher::spawn` consumed the dispatcher.
    pub hooks: Option<HooksHealthHandle>,
    /// The subscription manager, asked for its aggregate once a tick.
    pub subs: SubscriptionsHandle,
    /// The Telegram actor's counters, taken **before** `TelegramActor::spawn` consumed the actor.
    /// `None` when the bot is not running.
    pub telegram: Option<TelegramHealthHandle>,
    /// The `telegram` subscriber's event-drop counter, taken **before** the inbox was moved into
    /// the actor. `None` when the bot is not running, and reported as `0` in that case.
    pub telegram_dropped: Option<Arc<AtomicU64>>,
}

/// Publishes the moving components until `shutdown` is cancelled.
pub async fn run(
    probes: Probes,
    health: Arc<HealthRegistry>,
    events: EventSender,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                let changed = publish_once(&probes, &health).await;
                if changed {
                    events
                        .publish(DomainEvent::HealthChanged(health.snapshot()))
                        .await;
                }
            }
        }
    }
}

/// One pass. Returns whether any component changed.
pub async fn publish_once(probes: &Probes, health: &HealthRegistry) -> bool {
    let mut changed = false;
    changed |= health.set("store", store_component(&probes.store).await);
    changed |= health.set("queue", queue_component(probes));
    if let Some(hooks) = &probes.hooks {
        let view = hooks.health();
        changed |= view.apply(health);
        let telegram_dropped = probes
            .telegram_dropped
            .as_ref()
            .map_or(0, |c| c.load(Ordering::Relaxed));
        changed |= health.set(
            "events",
            events_component(view.events_dropped, telegram_dropped),
        );
    }
    if let Some(tg) = &probes.telegram {
        changed |= health.set("telegram", telegram_component(Some(&tg.health())));
    }
    if let Some(subs) = subscriptions(&probes.subs).await {
        changed |= health.set("subscriptions", subs);
    }
    changed
}

/// `components.store` (DESIGN §16.3).
///
/// The latency figure is a real read through the read pool, not a guess: a pool whose threads have
/// died answers nothing, and that is the **one** condition DESIGN §16.3 says makes `healthz` answer
/// 503, so it has to be measured rather than assumed.
pub async fn store_component(store: &Store) -> ComponentHealth {
    let started = std::time::Instant::now();
    let probe = store.kv_get("__health_probe").await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let wal_bytes = store.wal_bytes();
    let status = match &probe {
        Ok(_) => ComponentStatus::Ok,
        Err(_) => ComponentStatus::Down,
    };
    let mut c = ComponentHealth::new(status)
        .with("latency_ms", (latency_ms * 1000.0).round() / 1000.0)
        .with("wal_bytes", wal_bytes)
        .with("db_bytes", store.db_bytes())
        .with("commits_total", store.commit_count());
    if let Err(e) = probe {
        c = c.with("detail", e.to_string());
    }
    c
}

/// `components.queue` (DESIGN §16.3), slots included.
///
/// `slots.<pool>.used` is derived from the published snapshot rather than read out of the engine's
/// semaphores: the engine owns them and exposes no accessor (it is a single task with no `Mutex`,
/// which is the property that makes it testable), and the snapshot already carries every
/// non-terminal row with its provider. A StreamingCommunity job holds its own pool's permit
/// *instead of* a global one (DESIGN §8.7), so the two counts partition the running set.
#[must_use]
pub fn queue_component(probes: &Probes) -> ComponentHealth {
    let published = probes.state.load();
    let counts = published.counts;

    let own_slots: BTreeMap<String, usize> = {
        let guard = probes
            .registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .iter()
            .filter_map(|(id, provider, _)| provider.own_slots().map(|n| (id.to_string(), n)))
            .collect()
    };

    let mut used_per_provider: BTreeMap<String, u64> = BTreeMap::new();
    let mut global_used: u64 = 0;
    for item in published.items.iter() {
        if !item.status.is_running() {
            continue;
        }
        let provider = item.provider.as_deref().unwrap_or_default();
        if own_slots.contains_key(provider) {
            *used_per_provider.entry(provider.to_owned()).or_default() += 1;
        } else {
            global_used += 1;
        }
    }

    let mut slots = serde_json::Map::new();
    slots.insert(
        "global".to_owned(),
        serde_json::json!({
            "total": probes.cfg.max_concurrent_downloads,
            "used": global_used,
        }),
    );
    for (id, total) in &own_slots {
        slots.insert(
            id.clone(),
            serde_json::json!({
                "total": total,
                "used": used_per_provider.get(id).copied().unwrap_or(0),
            }),
        );
    }

    ComponentHealth::new(ComponentStatus::Ok)
        .with("downloading", counts.downloading)
        .with("postprocessing", counts.postprocessing)
        .with("queued", counts.queued)
        .with("resolving", counts.resolving)
        .with("slots", serde_json::Value::Object(slots))
        .with("progress_dropped_total", probes.sink.dropped())
}

/// `components.events` — `aulos_event_dropped_total{subscriber}` (DESIGN §16.3, §2.2.1).
///
/// A non-zero value means a hook run or a Telegram notification was silently skipped, which is the
/// only user-visible consequence the fan-out can have, so it is a `degraded` component rather than
/// a detail field on something else.
///
/// Both subscribers are **measured**: the hooks half comes from `HooksHealth::events_dropped`, the
/// Telegram half from the `Arc<AtomicU64>` that `EventInbox::dropped_handle()` hands over before
/// `TelegramActor::spawn` consumes the inbox. `telegram` reads `0` when the bot is not running,
/// which is the truth — an inbox nobody was given cannot drop anything.
#[must_use]
pub fn events_component(hooks_dropped: u64, telegram_dropped: u64) -> ComponentHealth {
    let status = if hooks_dropped == 0 && telegram_dropped == 0 {
        ComponentStatus::Ok
    } else {
        ComponentStatus::Degraded
    };
    ComponentHealth::new(status).with(
        "dropped",
        serde_json::json!({ "hooks": hooks_dropped, "telegram": telegram_dropped }),
    )
}

/// `components.subscriptions` (DESIGN §16.3), or `None` when the manager is gone.
pub async fn subscriptions(handle: &SubscriptionsHandle) -> Option<ComponentHealth> {
    if !handle.is_open() {
        return None;
    }
    let (ack, rx) = tokio::sync::oneshot::channel();
    handle.send(SubCmd::Health { ack }).await.ok()?;
    let aggregate = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .ok()?
        .ok()?;
    let status = if aggregate.failing == 0 {
        ComponentStatus::Ok
    } else {
        ComponentStatus::Degraded
    };
    Some(
        ComponentHealth::new(status)
            .with("total", aggregate.total)
            .with("failing", aggregate.failing)
            .with("next_due_in_s", aggregate.next_due_in_s),
    )
}

/// `components.telegram` (DESIGN §16.3).
///
/// Republished every [`TICK`] from the `TelegramHealthHandle` the wiring takes before
/// `TelegramActor::spawn` consumes the actor, so `edits_throttled_total` and the board counts move
/// while the bot runs. `None` is the disabled component, which `healthz` names on purpose.
#[must_use]
pub fn telegram_component(health: Option<&TelegramHealth>) -> ComponentHealth {
    match health {
        None => ComponentHealth::new(ComponentStatus::Disabled)
            .with("detail", "the Telegram bot is not enabled"),
        Some(h) => ComponentHealth::new(ComponentStatus::Ok)
            .with("chats", h.chats)
            .with("boards", h.boards)
            .with("watched_jobs", h.watched_jobs)
            .with("edits_throttled_total", h.edits_throttled_total),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::config::RawEnv;
    use aulos_store::StoreOptions;

    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Arc<Config> {
        Arc::new(aulos_core::config::load(&RawEnv::from_pairs(pairs.iter().copied())).unwrap())
    }

    fn probes(dir: &std::path::Path, cfg: Arc<Config>) -> Probes {
        let store = Store::open(StoreOptions::new(dir.join("aulos.db")).with_flush_ms(5)).unwrap();
        let (sink, rx) = ProgressSinkFactory::channel();
        // The receiver is kept alive: dropping it would make every `try_send` fail and inflate
        // the drop counter this component reports.
        std::mem::forget(rx);
        let (subs, sub_rx) = SubscriptionsHandle::channel(4);
        std::mem::forget(sub_rx);
        Probes {
            store,
            state: StateView::new(aulos_core::BootId::new()),
            registry: Arc::new(RwLock::new(Registry::new())),
            cfg,
            sink,
            hooks: None,
            subs,
            telegram: None,
            telegram_dropped: None,
        }
    }

    #[tokio::test]
    async fn the_store_component_measures_a_real_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = probes(dir.path(), cfg(&[]));
        let c = store_component(&p.store).await;
        assert_eq!(c.status, ComponentStatus::Ok);
        for key in ["latency_ms", "wal_bytes", "db_bytes", "commits_total"] {
            assert!(c.detail.contains_key(key), "{key} missing: {c:?}");
        }
        assert!(
            c.detail["latency_ms"].as_f64().unwrap_or(-1.0) >= 0.0,
            "{c:?}"
        );
        p.store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_closed_store_is_down_which_is_the_only_503_condition() {
        let dir = tempfile::tempdir().unwrap();
        let p = probes(dir.path(), cfg(&[]));
        p.store.close().await.unwrap();
        let c = store_component(&p.store).await;
        assert_eq!(c.status, ComponentStatus::Down, "{c:?}");
        assert!(c.detail.contains_key("detail"), "{c:?}");

        // And that is what makes `healthz` answer 503.
        let registry = HealthRegistry::new();
        registry.set("store", c);
        assert!(registry.snapshot().is_fatal());
    }

    #[tokio::test]
    async fn the_queue_component_reports_counts_slots_and_the_drop_counter() {
        let dir = tempfile::tempdir().unwrap();
        let p = probes(
            dir.path(),
            cfg(&[
                ("MAX_CONCURRENT_DOWNLOADS", "5"),
                ("SC_MAX_CONCURRENT_DOWNLOADS", "2"),
            ]),
        );
        let c = queue_component(&p);
        assert_eq!(c.status, ComponentStatus::Ok);
        for key in [
            "downloading",
            "postprocessing",
            "queued",
            "resolving",
            "slots",
            "progress_dropped_total",
        ] {
            assert!(c.detail.contains_key(key), "{key} missing: {c:?}");
        }
        assert_eq!(c.detail["slots"]["global"]["total"], 5);
        assert_eq!(c.detail["slots"]["global"]["used"], 0);
        assert_eq!(c.detail["progress_dropped_total"], 0);
        p.store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_provider_with_its_own_slots_gets_its_own_pool_row() {
        let dir = tempfile::tempdir().unwrap();
        let p = probes(dir.path(), cfg(&[("SC_MAX_CONCURRENT_DOWNLOADS", "3")]));
        {
            let mut guard = p.registry.write().unwrap();
            guard.register(Arc::new(
                aulos_provider::fake::FakeProvider::new().with_own_slots(Some(3)),
            ));
        }
        let c = queue_component(&p);
        let slots = &c.detail["slots"];
        assert!(slots.get("fake").is_some(), "{slots:?}");
        assert_eq!(slots["fake"]["total"], 3);
        assert_eq!(slots["fake"]["used"], 0);
        p.store.close().await.unwrap();
    }

    #[test]
    fn the_events_component_degrades_only_when_something_was_dropped() {
        let clean = events_component(0, 0);
        assert_eq!(clean.status, ComponentStatus::Ok);
        assert_eq!(clean.detail["dropped"]["hooks"], 0);
        assert_eq!(clean.detail["dropped"]["telegram"], 0);

        let lossy = events_component(4, 0);
        assert_eq!(
            lossy.status,
            ComponentStatus::Degraded,
            "a skipped hook run is user-visible"
        );
        assert_eq!(lossy.detail["dropped"]["hooks"], 4);

        // The Telegram half is measured now, not hard-coded: a dropped notification degrades too.
        let tg = events_component(0, 2);
        assert_eq!(tg.status, ComponentStatus::Degraded);
        assert_eq!(tg.detail["dropped"]["telegram"], 2);
        assert_eq!(tg.detail["dropped"]["hooks"], 0);
    }

    #[test]
    fn the_telegram_component_is_disabled_when_the_bot_is_off() {
        assert_eq!(
            telegram_component(None).status,
            ComponentStatus::Disabled,
            "a bot nobody configured is not a failure"
        );
        let h = TelegramHealth {
            enabled: true,
            chats: 2,
            boards: 1,
            watched_jobs: 3,
            edits_throttled_total: 11,
        };
        let c = telegram_component(Some(&h));
        assert_eq!(c.status, ComponentStatus::Ok);
        assert_eq!(c.detail["chats"], 2);
        assert_eq!(c.detail["edits_throttled_total"], 11);
    }

    #[tokio::test]
    async fn subscription_health_answers_from_the_manager_and_none_when_it_is_gone() {
        let (handle, mut rx) = SubscriptionsHandle::channel(4);
        let responder = tokio::spawn(async move {
            if let Some(SubCmd::Health { ack }) = rx.recv().await {
                let _ = ack.send(aulos_core::subscription::SubsHealth {
                    total: 7,
                    failing: 1,
                    next_due_in_s: Some(412),
                });
            }
            rx
        });
        let c = subscriptions(&handle).await.expect("the manager answered");
        assert_eq!(c.status, ComponentStatus::Degraded, "one feed is failing");
        assert_eq!(c.detail["total"], 7);
        assert_eq!(c.detail["next_due_in_s"], 412);
        let rx = responder.await.unwrap();
        drop(rx);

        // A gone manager reports nothing rather than a wrong zero.
        let (dead, dead_rx) = SubscriptionsHandle::channel(1);
        drop(dead_rx);
        assert!(subscriptions(&dead).await.is_none());
    }

    #[tokio::test]
    async fn one_pass_publishes_the_moving_components_and_reports_change_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = probes(dir.path(), cfg(&[]));
        let registry = HealthRegistry::new();

        assert!(
            publish_once(&p, &registry).await,
            "the first pass writes both components"
        );
        let view = registry.snapshot();
        assert!(view.components.contains_key("store"));
        assert!(view.components.contains_key("queue"));

        // A second pass with nothing moving must not report a change: an unchanging `health`
        // frame per tick would be a frame per second per client forever.
        let again = publish_once(&p, &registry).await;
        let latency_moved = registry.snapshot().components["store"].detail["latency_ms"]
            != view.components["store"].detail["latency_ms"];
        assert!(
            !again || latency_moved,
            "the only field allowed to move is the measured latency"
        );
        p.store.close().await.unwrap();
    }
}
