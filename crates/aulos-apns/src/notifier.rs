//! [`ApnsNotifier`]: the `aulos_core::event::Notifier` implementation (DESIGN §25.2).
//!
//! # What it does with each event
//!
//! | Event | Action |
//! |---|---|
//! | `StatusChanged` out of `queued`/`resolving` into `preparing`/`downloading` | one Live Activity **push-to-start** per device that offered a start token — top-level items only, and at most once per item |
//! | `StatusChanged` (any) | a Live Activity **update** to every registered activity for that item, throttled per (item, device) |
//! | `Completed` | a Live Activity **end**, then `remove_live_activities_for`; plus one alert per device with `alerts == true`, for a top-level item or a group |
//! | `Removed` | forget the item's cached state |
//!
//! # Three rules that are easy to get wrong
//!
//! - **A group gets one alert; its children get none.** `Completed` for an item with a
//!   `group_id` is dropped on the floor, or a 40-episode season would ring the phone 41 times.
//! - **`canceled` is not worth a notification.** The user is standing at the phone that cancelled
//!   it. Only `finished` and `error` produce an alert — but *every* terminal status ends a Live
//!   Activity, because a cancelled download must not leave a progress ring spinning on the lock
//!   screen.
//! - **A paused item does not start twice.** `queued → downloading → queued → downloading` is an
//!   ordinary pause/resume, so the item ids that have already been started are remembered until
//!   the item completes or is removed.
//!
//! # Nothing here blocks the event inbox on the network
//!
//! `on_event` does in-memory bookkeeping and at most one `DeviceStore` read (cached for
//! [`UPDATE_INTERVAL`], so an item downloading at ten progress frames a second costs one read
//! every two seconds). Every HTTP request runs in a spawned task, bounded by [`PUSH_CONCURRENCY`]
//! simultaneous requests and [`MAX_INFLIGHT_TASKS`] outstanding tasks, each with the client's
//! ten-second per-request timeout.
//!
//! # Two clocks, on purpose
//!
//! Wall-clock time (`aps.timestamp`, `apns-expiration`, `healthz`'s `last_sent_at`) comes from the
//! injected `Clock`. Every *interval* — the throttle window, the cache lifetime, the trailing-edge
//! deadline — is measured with `tokio::time::Instant`, because those deadlines are handed straight
//! to `tokio::time::sleep_until`. Mixing the two would let a frozen test clock hand `sleep_until` a
//! deadline that has already passed and spin the timer task, which is exactly what it did once.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, Notifier};
use aulos_core::id::ItemId;
use aulos_core::item::ItemView;
use aulos_core::ports::{ApnsEnvironment, DeviceRecord, DeviceStore, LiveActivityRecord};
use aulos_core::status::Status;
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{ApnsClient, Outcome, Push, PushKind};
use crate::error::ApnsError;
use crate::health::{ApnsHealthHandle, Counters};
use crate::payload;

/// The notifier's stable id, and the `healthz` component name.
pub const ID: &str = "apns";

/// The minimum spacing between two Live Activity updates for the same (item, device), unless the
/// status itself changed.
///
/// It doubles as the `live_activities_for` cache lifetime: a read fresher than the throttle window
/// could not change any decision, so one constant serves both. Overridable with
/// [`ApnsNotifier::with_update_interval`], which exists so the tests are not two seconds long.
pub const UPDATE_INTERVAL: Duration = Duration::from_secs(2);

/// How long the device-token → bundle-id map is reused.
pub const DEVICE_CACHE_TTL: Duration = Duration::from_secs(30);

/// Simultaneous in-flight requests to Apple.
pub const PUSH_CONCURRENCY: usize = 8;

/// Outstanding push/timer tasks. Past this, a push is dropped and counted rather than queued
/// without bound.
pub const MAX_INFLIGHT_TASKS: usize = 256;

/// What a Live Activity's `apns-topic` appends to the bundle id.
pub const LIVE_ACTIVITY_TOPIC_SUFFIX: &str = ".push-type.liveactivity";

/// `apns-priority` for an alert and for a Live Activity start or end.
pub const PRIORITY_IMMEDIATE: u8 = 10;

/// `apns-priority` for a Live Activity update: throttleable by iOS, which is what Apple asks for.
pub const PRIORITY_THROTTLED: u8 = 5;

// ---------------------------------------------------------------------------
// The notifier
// ---------------------------------------------------------------------------

/// The APNs notifier. Register it as an `EventRouter` subscriber like any other.
#[derive(Debug)]
pub struct ApnsNotifier {
    shared: Arc<Shared>,
}

impl ApnsNotifier {
    /// Builds the notifier from config.
    ///
    /// - `Ok(None)` — `APNS_ENABLED=false`. Publish [`crate::ApnsHealth::disabled`] and wire
    ///   nothing.
    /// - `Err(e)` — `APNS_ENABLED=true` but the server is misconfigured. DESIGN §25.6: the binary
    ///   logs an ERROR, publishes [`crate::ApnsHealth::misconfigured`] and **keeps running**; it
    ///   does not refuse to start.
    /// - `Ok(Some(n))` — subscribe it.
    ///
    /// # Errors
    /// Everything [`ApnsClient::from_config`] reports: an empty or unreadable `APNS_KEY_FILE`, a
    /// blank `APNS_KEY_ID`/`APNS_TEAM_ID`, a `.p8` that is not an ES256 key, or an
    /// `APNS_BASE_URL_OVERRIDE` that does not parse.
    pub fn new(
        cfg: &Config,
        store: Arc<dyn DeviceStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<Self>, ApnsError> {
        if !cfg.apns_enabled {
            return Ok(None);
        }
        let client = ApnsClient::from_config(cfg, Arc::clone(&clock))?;
        Ok(Some(Self::with_client(
            client,
            &cfg.apns_topic,
            store,
            clock,
        )))
    }

    /// Builds the notifier around an already-configured client. This is what the tests use.
    #[must_use]
    pub fn with_client(
        client: ApnsClient,
        default_topic: &str,
        store: Arc<dyn DeviceStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                client,
                store,
                clock,
                default_topic: Arc::from(default_topic),
                counters: Counters::new(),
                state: Mutex::new(State::default()),
                permits: Semaphore::new(PUSH_CONCURRENCY),
                tasks: watch::Sender::new(0),
                cancel: CancellationToken::new(),
                update_interval: UPDATE_INTERVAL,
            }),
        }
    }

    /// Replaces the Live Activity throttle window (and with it the registration cache lifetime).
    ///
    /// Only the tests call this: a suite that had to wait out the real two seconds for every
    /// trailing-edge assertion would be slow enough that nobody would run it.
    ///
    /// # Panics
    /// If the notifier has already been shared — call it immediately after construction.
    #[must_use]
    pub fn with_update_interval(mut self, interval: Duration) -> Self {
        let shared = Arc::get_mut(&mut self.shared).unwrap_or_else(|| {
            panic!("with_update_interval must be called before the notifier is shared")
        });
        shared.update_interval = interval;
        self
    }

    /// The `healthz` handle. Take it at wiring time: the notifier itself is handed to the
    /// subscriber loop.
    #[must_use]
    pub fn health_handle(&self) -> ApnsHealthHandle {
        ApnsHealthHandle::new(
            Arc::clone(&self.shared.counters),
            Arc::clone(&self.shared.store),
        )
    }

    /// Waits until every spawned push and trailing-edge timer has finished.
    ///
    /// The test seam, and half of [`Self::shutdown`].
    pub async fn quiesce(&self) {
        // A `watch` rather than a `Notify`: `notify_waiters` only wakes receivers that are already
        // registered, and the window between reading the counter and registering is exactly where
        // the last task finishes. A watch carries the value, so there is nothing to miss.
        let mut rx = self.shared.tasks.subscribe();
        loop {
            if *rx.borrow_and_update() == 0 {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Cancels the in-flight pushes and timers, then waits for them.
    pub async fn shutdown(&self) {
        self.shared.cancel.cancel();
        self.quiesce().await;
    }

    /// How many spawned tasks are outstanding. For tests and for logs.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        *self.shared.tasks.borrow()
    }
}

#[async_trait::async_trait]
impl Notifier for ApnsNotifier {
    fn id(&self) -> &'static str {
        ID
    }

    /// Every item. A device registration is per user, not per source, so there is nothing to
    /// filter on (DESIGN §12.6).
    fn interested(&self, _item: &ItemView) -> bool {
        true
    }

    async fn on_event(&self, ev: &DomainEvent) {
        match ev {
            DomainEvent::StatusChanged { from, to, view, .. } => {
                if view.group_id.is_none()
                    && is_start_edge(*from, *to)
                    && self.shared.mark_started(view.id)
                {
                    let shared = Arc::clone(&self.shared);
                    let view = Arc::clone(view);
                    self.shared
                        .spawn(async move { shared.push_to_start(view).await });
                }
                self.shared.live_activity_update(Arc::clone(view)).await;
            }
            DomainEvent::Completed(view) => {
                let shared = Arc::clone(&self.shared);
                let view = Arc::clone(view);
                self.shared
                    .spawn(async move { shared.complete(view).await });
            }
            DomainEvent::Removed { ids, .. } => self.shared.forget(ids),
            _ => {}
        }
    }
}

/// The one transition that starts a Live Activity: leaving the waiting room for real work.
///
/// `postprocessing` is deliberately not a start edge — an item can only reach it through
/// `downloading`, so a start there would mean the earlier edge was missed, and starting an
/// activity that is about to end is worse than not starting one.
#[must_use]
pub const fn is_start_edge(from: Status, to: Status) -> bool {
    matches!(from, Status::Queued | Status::Resolving)
        && matches!(to, Status::Preparing | Status::Downloading)
}

/// The Live Activity `apns-topic` for a bundle id.
#[must_use]
pub fn live_activity_topic(bundle_id: &str) -> Arc<str> {
    Arc::from(format!("{bundle_id}{LIVE_ACTIVITY_TOPIC_SUFFIX}"))
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything a spawned push task needs, behind one `Arc`.
struct Shared {
    client: ApnsClient,
    store: Arc<dyn DeviceStore>,
    clock: Arc<dyn Clock>,
    /// `APNS_TOPIC`: the bundle id used when a device did not report its own.
    default_topic: Arc<str>,
    counters: Arc<Counters>,
    state: Mutex<State>,
    permits: Semaphore,
    /// The outstanding-task count, as a watch so [`ApnsNotifier::quiesce`] cannot miss the last
    /// decrement.
    tasks: watch::Sender<usize>,
    cancel: CancellationToken,
    /// The Live Activity throttle window; also the registration cache lifetime.
    update_interval: Duration,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsNotifier")
            .field("client", &self.client)
            .field("tasks", &*self.tasks.borrow())
            .finish_non_exhaustive()
    }
}

/// The notifier's memory.
#[derive(Debug, Default)]
struct State {
    /// Items whose Live Activity has already been started, so a pause/resume does not start a
    /// second one. Forgotten on `Completed` and `Removed`.
    started: HashSet<ItemId>,
    /// Per-item Live Activity bookkeeping.
    items: HashMap<ItemId, Track>,
    /// device token → bundle id, so a Live Activity push can name its `apns-topic` without a
    /// second store read per push.
    bundle_ids: HashMap<Box<str>, Arc<str>>,
    /// When `bundle_ids` was last filled.
    bundle_ids_at: Option<Instant>,
}

/// One item's Live Activity state.
#[derive(Debug, Default)]
struct Track {
    /// The registrations, as of `fetched_at`.
    records: Vec<LiveActivityRecord>,
    /// When `records` was read, or `None` when it has never been.
    fetched_at: Option<Instant>,
    /// update token → (when the last update went out, what status it carried).
    last_sent: HashMap<Box<str>, (Instant, Status)>,
    /// The newest view that has not been delivered to every registration yet — the trailing edge.
    pending: Option<Arc<ItemView>>,
    /// Whether a trailing-edge timer task owns this item.
    timer_armed: bool,
}

/// One addressee of a Live Activity push.
#[derive(Clone, Debug)]
struct Target {
    /// The token the push goes to (an activity update token).
    token: Box<str>,
    /// The device that owns it, so a dead token prunes the right row.
    device_token: Box<str>,
    /// Which gateway.
    env: ApnsEnvironment,
    /// `apns-topic`.
    topic: Arc<str>,
}

/// Which row a `TokenInvalid` outcome should delete.
#[derive(Clone, Debug)]
enum TokenRole {
    /// A device token or a push-to-start token: the whole device goes.
    Device(Box<str>),
    /// A Live Activity update token: only that registration goes.
    Activity {
        /// The owning device.
        device_token: Box<str>,
        /// The item the activity tracks.
        item: ItemId,
    },
}

/// What one pass over an item's Live Activity state decided.
#[derive(Default)]
struct Step {
    /// Push this view to these targets now.
    send: Option<(Arc<ItemView>, Vec<Target>)>,
    /// The caller must (re)arm the trailing-edge timer for this instant.
    arm: Option<Instant>,
}

impl Shared {
    // -- state helpers ------------------------------------------------------

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `true` when this call is the one that claimed the item's start push.
    fn mark_started(&self, id: ItemId) -> bool {
        self.lock_state().started.insert(id)
    }

    /// Drops every trace of these items.
    fn forget(&self, ids: &[ItemId]) {
        let mut st = self.lock_state();
        for id in ids {
            st.started.remove(id);
            st.items.remove(id);
        }
        let total = live_activity_count(&st);
        self.counters.set_live_activities(total);
    }

    fn now_secs(&self) -> i64 {
        self.clock.now_ms().div_euclid(1_000)
    }

    // -- device / bundle-id cache -------------------------------------------

    fn cache_bundle_ids(&self, devices: &[DeviceRecord]) {
        let mut st = self.lock_state();
        st.bundle_ids = devices
            .iter()
            .map(|d| (d.token.clone(), Arc::from(&*d.bundle_id)))
            .collect();
        st.bundle_ids_at = Some(Instant::now());
    }

    /// Whether the topic map cannot name every device behind `id`'s registrations.
    fn bundle_ids_stale(&self, id: ItemId) -> bool {
        let st = self.lock_state();
        let expired = st
            .bundle_ids_at
            .is_none_or(|at| Instant::now() >= at + DEVICE_CACHE_TTL);
        if expired {
            return true;
        }
        st.items.get(&id).is_some_and(|t| {
            t.records
                .iter()
                .any(|r| !st.bundle_ids.contains_key(&r.device_token))
        })
    }

    async fn refresh_devices(&self) {
        match self.store.devices().await {
            Ok(devices) => self.cache_bundle_ids(&devices),
            Err(e) => {
                tracing::warn!(error = %e, "APNs: the device registrations could not be read");
                self.counters
                    .set_last_error(&format!("device registrations unreadable: {e}"));
            }
        }
    }

    // -- the Live Activity update path --------------------------------------

    /// One `StatusChanged`, as far as Live Activity updates are concerned.
    async fn live_activity_update(self: &Arc<Self>, view: Arc<ItemView>) {
        let id = view.id;
        if self.records_stale(id) {
            match self.store.live_activities_for(id).await {
                Ok(records) => self.store_records(id, records),
                Err(e) => {
                    tracing::warn!(item = %id, error = %e, "APNs: live activities unreadable");
                    self.counters
                        .set_last_error(&format!("live activities unreadable: {e}"));
                    return;
                }
            }
        }
        if self.bundle_ids_stale(id) {
            self.refresh_devices().await;
        }
        let step = self.step(id, Some(view), false);
        let arm = step.arm;
        self.dispatch_updates(id, step);
        if let Some(deadline) = arm {
            self.arm_timer(id, deadline);
        }
    }

    fn records_stale(&self, id: ItemId) -> bool {
        let st = self.lock_state();
        st.items
            .get(&id)
            .and_then(|t| t.fetched_at)
            .is_none_or(|at| Instant::now() >= at + self.update_interval)
    }

    fn store_records(&self, id: ItemId, records: Vec<LiveActivityRecord>) {
        let mut st = self.lock_state();
        let track = st.items.entry(id).or_default();
        // A token that is gone must not keep a throttle slot alive forever.
        let live: HashSet<Box<str>> = records.iter().map(|r| r.update_token.clone()).collect();
        track.last_sent.retain(|token, _| live.contains(token));
        track.records = records;
        track.fetched_at = Some(Instant::now());
        let total = live_activity_count(&st);
        self.counters.set_live_activities(total);
    }

    /// One pass over an item's throttle state, under one lock.
    ///
    /// `from_timer` says the caller is the trailing-edge task, which already owns
    /// `Track::timer_armed`; that is what stops a second timer from being armed for the same item.
    fn step(&self, id: ItemId, incoming: Option<Arc<ItemView>>, from_timer: bool) -> Step {
        let mut guard = self.lock_state();
        let st = &mut *guard;
        let Some(track) = st.items.get_mut(&id) else {
            return Step::default();
        };
        if let Some(v) = incoming {
            track.pending = Some(v);
        }
        let Some(view) = track.pending.clone() else {
            if from_timer {
                track.timer_armed = false;
            }
            return Step::default();
        };

        let now = Instant::now();
        let mut targets = Vec::new();
        let mut next: Option<Instant> = None;
        for r in &track.records {
            match track.last_sent.get(&r.update_token) {
                // Throttled: same status, and the two seconds are not up.
                Some((at, status))
                    if *status == view.status && now < *at + self.update_interval =>
                {
                    let due = *at + self.update_interval;
                    next = Some(next.map_or(due, |n: Instant| n.min(due)));
                }
                _ => {
                    let topic = st.bundle_ids.get(&r.device_token).map_or_else(
                        || live_activity_topic(&self.default_topic),
                        |b| live_activity_topic(b),
                    );
                    targets.push(Target {
                        token: r.update_token.clone(),
                        device_token: r.device_token.clone(),
                        env: r.environment,
                        topic,
                    });
                }
            }
        }
        for t in &targets {
            track.last_sent.insert(t.token.clone(), (now, view.status));
        }

        let arm = match next {
            // Someone still owes this view an update.
            Some(due) => {
                if from_timer || !track.timer_armed {
                    track.timer_armed = true;
                    Some(due)
                } else {
                    None
                }
            }
            // Everybody is up to date: the trailing edge has been delivered.
            None => {
                track.pending = None;
                if from_timer {
                    track.timer_armed = false;
                }
                None
            }
        };

        Step {
            send: (!targets.is_empty()).then_some((view, targets)),
            arm,
        }
    }

    /// Spawns the pushes a [`Step`] asked for.
    fn dispatch_updates(self: &Arc<Self>, id: ItemId, step: Step) {
        let Some((view, targets)) = step.send else {
            return;
        };
        let now = self.now_secs();
        let payload = payload::live_activity_update(&view, now);
        for t in targets {
            let push = Push {
                kind: PushKind::LiveActivity,
                topic: t.topic,
                priority: PRIORITY_THROTTLED,
                // A progress frame that could not be delivered immediately is worthless by the
                // time a queue drains, and Apple asks providers to say so.
                expiration: 0,
                collapse_id: None,
                payload: payload.clone(),
            };
            self.spawn_push(
                push,
                t.token,
                t.env,
                TokenRole::Activity {
                    device_token: t.device_token,
                    item: id,
                },
            );
        }
    }

    /// The trailing-edge task: sleeps to the next due instant, delivers the newest pending view,
    /// and repeats until every registration is up to date.
    fn arm_timer(self: &Arc<Self>, id: ItemId, deadline: Instant) {
        let shared = Arc::clone(self);
        self.spawn(async move {
            let mut at = deadline;
            loop {
                tokio::select! {
                    () = shared.cancel.cancelled() => {
                        shared.disarm(id);
                        return;
                    }
                    () = tokio::time::sleep_until(at) => {}
                }
                let step = shared.step(id, None, true);
                let next = step.arm;
                shared.dispatch_updates(id, step);
                match next {
                    Some(n) => at = n,
                    None => return,
                }
            }
        });
    }

    /// Releases the timer flag on cancellation, so a restarted notifier is not wedged.
    fn disarm(&self, id: ItemId) {
        if let Some(track) = self.lock_state().items.get_mut(&id) {
            track.timer_armed = false;
        }
    }

    // -- the start path ------------------------------------------------------

    async fn push_to_start(self: Arc<Self>, view: Arc<ItemView>) {
        let devices = match self.store.devices().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(item = %view.id, error = %e, "APNs: device registrations unreadable");
                self.counters
                    .set_last_error(&format!("device registrations unreadable: {e}"));
                return;
            }
        };
        self.cache_bundle_ids(&devices);
        let now = self.now_secs();
        let payload = payload::live_activity_start(&view, now);
        for d in &devices {
            let Some(start_token) = d.live_activity_start_token.as_ref() else {
                continue;
            };
            let push = Push {
                kind: PushKind::LiveActivity,
                topic: live_activity_topic(&d.bundle_id),
                priority: PRIORITY_IMMEDIATE,
                expiration: now + payload::ALERT_TTL_SECS,
                collapse_id: None,
                payload: payload.clone(),
            };
            self.spawn_push(
                push,
                start_token.clone(),
                d.environment,
                TokenRole::Device(d.token.clone()),
            );
        }
    }

    // -- the terminal path ---------------------------------------------------

    async fn complete(self: Arc<Self>, view: Arc<ItemView>) {
        self.end_live_activities(&view).await;

        // A child of a group is silent: the group gets the one alert.
        if view.group_id.is_some() {
            return;
        }
        // `canceled` is deliberately not worth a notification.
        if !matches!(view.status, Status::Finished | Status::Error) {
            return;
        }
        let devices = match self.store.devices().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(item = %view.id, error = %e, "APNs: device registrations unreadable");
                self.counters
                    .set_last_error(&format!("device registrations unreadable: {e}"));
                return;
            }
        };
        self.cache_bundle_ids(&devices);
        let now = self.now_secs();
        let payload = payload::alert(&view);
        let collapse: Arc<str> = Arc::from(view.id.to_string());
        for d in devices.iter().filter(|d| d.alerts) {
            let push = Push {
                kind: PushKind::Alert,
                topic: Arc::from(&*d.bundle_id),
                priority: PRIORITY_IMMEDIATE,
                expiration: now + payload::ALERT_TTL_SECS,
                collapse_id: Some(Arc::clone(&collapse)),
                payload: payload.clone(),
            };
            self.spawn_push(
                push,
                d.token.clone(),
                d.environment,
                TokenRole::Device(d.token.clone()),
            );
        }
    }

    /// The `end` push for every registration, then `remove_live_activities_for`.
    async fn end_live_activities(self: &Arc<Self>, view: &Arc<ItemView>) {
        let id = view.id;
        let records = match self.store.live_activities_for(id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(item = %id, error = %e, "APNs: live activities unreadable");
                self.counters
                    .set_last_error(&format!("live activities unreadable: {e}"));
                Vec::new()
            }
        };
        if !records.is_empty() {
            if self.bundle_ids_stale(id) {
                self.refresh_devices().await;
            }
            let now = self.now_secs();
            let payload = payload::live_activity_end(view, now);
            let topics = {
                let st = self.lock_state();
                records
                    .iter()
                    .map(|r| {
                        st.bundle_ids.get(&r.device_token).map_or_else(
                            || live_activity_topic(&self.default_topic),
                            |b| live_activity_topic(b),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            for (r, topic) in records.iter().zip(topics) {
                let push = Push {
                    kind: PushKind::LiveActivity,
                    topic,
                    priority: PRIORITY_IMMEDIATE,
                    expiration: now + payload::ALERT_TTL_SECS,
                    collapse_id: None,
                    payload: payload.clone(),
                };
                self.spawn_push(
                    push,
                    r.update_token.clone(),
                    r.environment,
                    TokenRole::Activity {
                        device_token: r.device_token.clone(),
                        item: id,
                    },
                );
            }
            if let Err(e) = self.store.remove_live_activities_for(id).await {
                tracing::warn!(item = %id, error = %e, "APNs: live activities could not be cleared");
                self.counters
                    .set_last_error(&format!("live activities not cleared: {e}"));
            }
        }
        self.forget(&[id]);
    }

    // -- sending -------------------------------------------------------------

    fn spawn_push(
        self: &Arc<Self>,
        push: Push,
        token: Box<str>,
        env: ApnsEnvironment,
        role: TokenRole,
    ) {
        let shared = Arc::clone(self);
        self.spawn(async move { shared.send_one(&push, &token, env, role).await });
    }

    async fn send_one(&self, push: &Push, token: &str, env: ApnsEnvironment, role: TokenRole) {
        let Ok(_permit) = self.permits.acquire().await else {
            return; // the semaphore is closed: shutting down.
        };
        let sent = tokio::select! {
            () = self.cancel.cancelled() => return,
            r = self.client.send(push, token, env) => r,
        };
        match sent {
            Err(e) => {
                tracing::error!(error = %e, "APNs: the push could not be signed");
                self.counters.failed(&e.to_string(), !e.retryable());
            }
            Ok(outcome) => self.record(&outcome, role).await,
        }
    }

    async fn record(&self, outcome: &Outcome, role: TokenRole) {
        if outcome.delivered() {
            self.counters.delivered(self.clock.now_ms());
            return;
        }
        let reason = outcome.last_error().unwrap_or_default();
        match outcome {
            Outcome::ProviderTokenRejected { .. } => tracing::error!(
                reason = %reason,
                "APNs rejected the provider token twice: check APNS_KEY_ID, APNS_TEAM_ID and APNS_KEY_FILE"
            ),
            Outcome::TokenInvalid { .. } => {
                tracing::info!(reason = %reason, "APNs: pruning a dead token");
            }
            _ => tracing::warn!(reason = %reason, "APNs: the push did not land"),
        }
        self.counters.failed(&reason, outcome.misconfigured());
        if outcome.prunes_token() {
            self.prune(role).await;
        }
    }

    async fn prune(&self, role: TokenRole) {
        let result = match &role {
            TokenRole::Device(token) => self.store.remove_device(token).await,
            TokenRole::Activity { device_token, item } => {
                self.store.remove_live_activity(device_token, *item).await
            }
        };
        match result {
            Ok(()) => {
                self.counters.pruned();
                self.invalidate_caches();
            }
            Err(e) => {
                tracing::warn!(error = %e, "APNs: a dead token could not be pruned");
                self.counters.set_last_error(&format!("prune failed: {e}"));
            }
        }
    }

    /// After a removal the cached registrations and topics are wrong, so the next event re-reads.
    fn invalidate_caches(&self) {
        let mut st = self.lock_state();
        st.bundle_ids_at = None;
        for track in st.items.values_mut() {
            track.fetched_at = None;
        }
    }

    // -- the bounded task set -------------------------------------------------

    fn spawn<F>(self: &Arc<Self>, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut accepted = false;
        self.tasks.send_if_modified(|n| {
            if *n >= MAX_INFLIGHT_TASKS {
                false
            } else {
                *n += 1;
                accepted = true;
                true
            }
        });
        if !accepted {
            tracing::warn!(
                in_flight = MAX_INFLIGHT_TASKS,
                "APNs: the task set is full; the push was dropped"
            );
            self.counters
                .failed("the APNs task set is full; push dropped", false);
            return;
        }
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            fut.await;
            shared.tasks.send_modify(|n| *n = n.saturating_sub(1));
        });
    }
}

/// Every cached Live Activity registration, across items.
fn live_activity_count(st: &State) -> u64 {
    let n: usize = st.items.values().map(|t| t.records.len()).sum();
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_leaving_the_waiting_room_starts_an_activity() {
        assert!(is_start_edge(Status::Queued, Status::Downloading));
        assert!(is_start_edge(Status::Queued, Status::Preparing));
        assert!(is_start_edge(Status::Resolving, Status::Preparing));
        // A progress re-diff (`from == to`) is not an edge.
        assert!(!is_start_edge(Status::Downloading, Status::Downloading));
        // Neither is a pause, nor the move into postprocessing.
        assert!(!is_start_edge(Status::Downloading, Status::Queued));
        assert!(!is_start_edge(Status::Downloading, Status::Postprocessing));
        assert!(!is_start_edge(Status::Queued, Status::Error));
    }

    #[test]
    fn the_live_activity_topic_is_the_bundle_id_plus_apples_suffix() {
        assert_eq!(
            &*live_activity_topic("com.tatoalo.aulos"),
            "com.tatoalo.aulos.push-type.liveactivity"
        );
    }
}
