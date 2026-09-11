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
//! # Only the items the phone asked for (DESIGN §12.6, §25.2)
//!
//! The operator's rule is that a download reports on the channel that added it: an add from the
//! iOS app pushes, an add from Telegram is Telegram's to announce, and a `curl` add reports
//! nowhere. The gate is `view.source.kind == Ios`, which is one check because a playlist child
//! inherits its parent's source and `source` is written once at the add and never rewritten
//! (DESIGN §4.4). `APNS_PUSH_ALL=true` turns the gate off and restores the old fan-out.
//!
//! It gates the two pushes that fan out to **devices** — the push-to-start, and the alert. The
//! Live Activity **update** and **end** are addressed to a registration the app made for that one
//! item, so they follow the registration instead:
//!
//! - an activity only exists because the app asked for one, so pushing it is never a fan-out the
//!   operator's rule was aimed at, and refusing to is a ring frozen at its first frame;
//! - the update path is also the only thing that keeps the record cache warm, and the terminal
//!   `end` falls back on that cache when the store cannot be read — gating it would leave the
//!   fallback permanently empty for exactly the non-iOS items the ungated end exists to protect;
//! - and the knob can be flipped while an activity is live, which is the case that made the end
//!   unconditional in the first place: an activity that is never ended leaves a progress ring
//!   spinning on the lock screen with nothing to close it (§25.2).
//!
//! For the same reason the completion **alert** fires for an item that had a registration even
//! when its `source` is not `ios`: the phone was showing that download.
//!
//! # Which device, not just whether (DESIGN §25.2, decision 42)
//!
//! The gate above says the phone hears about the item; `source.ref` says *which* phone. An iOS add
//! carries the adding install there (`X-Aulos-Install`, PROTOCOL §1.3) and a registration carries
//! its own in `install_id` (§4.8), so the alert and the push-to-start go to the install the
//! download was started from. A `source.ref` of `null` — an app build that predates the header —
//! is every alerting device, which is the fan-out that shipped before the key existed. The update
//! and the end are unfiltered here too: they address a registration for that one item.
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
use aulos_core::ports::{
    ApnsEnvironment, DeviceRecord, DeviceStore, LiveActivityRecord, ProgressReader,
};
use aulos_core::source::{SourceKind, SourceRef};
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

/// How often a *progressing* item's Live Activity is refreshed from the progress reader, even
/// though no event arrived (DESIGN §25.4).
///
/// A backgrounded app receives nothing but pushes, so without this the island freezes at whatever
/// the last status change carried. Five seconds is twelve pushes a minute per activity, which is
/// inside what Apple budgets for a `liveactivity` push type at priority 5, and slow enough that a
/// download that is not moving costs no push at all: the timer re-reads, sees an identical view
/// and sends nothing. Overridable with [`ApnsNotifier::with_progress_interval`] for the tests.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

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
            cfg.apns_push_all,
        )))
    }

    /// Builds the notifier around an already-configured client. This is what the tests use.
    ///
    /// `push_all` is `APNS_PUSH_ALL`: `false` (the default) pushes only for items whose
    /// `source.kind` is `ios`.
    #[must_use]
    pub fn with_client(
        client: ApnsClient,
        default_topic: &str,
        store: Arc<dyn DeviceStore>,
        clock: Arc<dyn Clock>,
        push_all: bool,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                client,
                store,
                clock,
                default_topic: Arc::from(default_topic),
                push_all,
                counters: Counters::new(),
                state: Mutex::new(State::default()),
                permits: Semaphore::new(PUSH_CONCURRENCY),
                tasks: Arc::new(watch::Sender::new(0)),
                cancel: CancellationToken::new(),
                update_interval: UPDATE_INTERVAL,
                progress: None,
                progress_interval: PROGRESS_INTERVAL,
            }),
        }
    }

    /// Injects the progress reader the Live Activity update path pulls its numbers from
    /// (DESIGN §15.1, §25.4).
    ///
    /// Without it the notifier is exactly what it was: every `content-state` is built from the
    /// event's own view, whose progress cell the engine leaves `None`, and the island only moves
    /// when the status word does. With it, an update carries the published percent/speed/eta and
    /// a progressing item keeps being refreshed on the [`PROGRESS_INTERVAL`] cadence while the app
    /// is in the background.
    ///
    /// # Panics
    /// If the notifier has already been shared — call it immediately after construction.
    #[must_use]
    pub fn with_progress(mut self, reader: Arc<dyn ProgressReader>) -> Self {
        let shared = Arc::get_mut(&mut self.shared).unwrap_or_else(|| {
            panic!("with_progress must be called before the notifier is shared")
        });
        shared.progress = Some(reader);
        self
    }

    /// Replaces the progress cadence. Only the tests call this, for the same reason
    /// [`Self::with_update_interval`] exists.
    ///
    /// # Panics
    /// If the notifier has already been shared — call it immediately after construction.
    #[must_use]
    pub fn with_progress_interval(mut self, interval: Duration) -> Self {
        let shared = Arc::get_mut(&mut self.shared).unwrap_or_else(|| {
            panic!("with_progress_interval must be called before the notifier is shared")
        });
        shared.progress_interval = interval;
        self
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

    /// Only what the phone asked for: `source.kind == "ios"`, unless `APNS_PUSH_ALL` is on
    /// (DESIGN §12.6, §25.2).
    ///
    /// This is the predicate for the two pushes that fan out to *devices*: the Live Activity
    /// push-to-start, and the completion alert for an item nothing else on the phone is tracking.
    /// The Live Activity **update** and **end** are deliberately not gated on it — both address a
    /// registration the app made for this very item, so the item's origin is not the question, and
    /// a caller that filtered event *delivery* on this method would strand exactly those two: a
    /// ring frozen at its first frame, and then one nothing ever closes. [`Self::on_event`] is the
    /// authority; this stays advisory (logs, and a router that only wants a hint).
    fn interested(&self, item: &ItemView) -> bool {
        self.shared.pushes_for(item)
    }

    async fn on_event(&self, ev: &DomainEvent) {
        match ev {
            DomainEvent::StatusChanged { from, to, view, .. } => {
                // The start push is the only thing on this path that fans out to devices rather
                // than to a registration the app already made, so it is the only thing the source
                // gate covers here (DESIGN §25.2).
                //
                // The latch itself lives in `push_to_start`, which is where it is known that a
                // start will actually be attempted: latching here would burn the item's one
                // chance on a full task set or an unreadable device table, and `is_start_edge`
                // never fires twice for one download.
                if self.shared.pushes_for(view)
                    && view.group_id.is_none()
                    && is_start_edge(*from, *to)
                    && !self.shared.already_started(view.id)
                {
                    let shared = Arc::clone(&self.shared);
                    let view = Arc::clone(view);
                    self.shared
                        .spawn(async move { shared.push_to_start(view).await });
                }
                // Ungated, exactly like the end (DESIGN §25.2). Two reasons, and the second is the
                // one that used to be a bug: an update can only ever reach an activity the app
                // registered for *this* item, and this call is the only thing that keeps the
                // record cache warm — the cache the terminal path falls back on when
                // `live_activities_for` fails. Gating it here left that fallback permanently empty
                // for precisely the non-iOS items the ungated end exists to protect.
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

/// The two statuses whose numbers move on their own, and which therefore deserve a pulled refresh.
///
/// `preparing` is deliberately out: nothing has a percent yet, so re-reading it every five seconds
/// would push an identical `0 %` frame. `queued` is a paused or waiting item — the island should
/// say so and then stop costing pushes.
#[must_use]
pub const fn is_progressing(status: Status) -> bool {
    matches!(status, Status::Downloading | Status::Postprocessing)
}

/// Copies the transient progress fields of `from` onto `onto`, leaving every durable field alone.
///
/// The list is `ItemView`'s own transient set — exactly what the aggregator's `apply_progress`
/// merges out of a `ProgressCell` — so a field added there and forgotten here shows up as a number
/// that never moves on the island rather than as a compile error. That is the reason it is written
/// out field by field instead of cloning the published view: `status`, `msg`, `error` and the
/// group roll-up must stay the caller's.
fn copy_progress(from: &ItemView, onto: &mut ItemView) {
    onto.percent = from.percent;
    onto.speed = from.speed;
    onto.eta = from.eta;
    onto.downloaded_bytes = from.downloaded_bytes;
    onto.total_bytes = from.total_bytes;
    onto.total_bytes_estimate = from.total_bytes_estimate;
    onto.fragment_index = from.fragment_index;
    onto.fragment_count = from.fragment_count;
    onto.phase = from.phase;
    onto.phase_percent = from.phase_percent;
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
    /// `APNS_PUSH_ALL`: `false` restricts alerts and Live Activity starts/updates to items the
    /// iOS app added (DESIGN §25.2).
    push_all: bool,
    counters: Arc<Counters>,
    state: Mutex<State>,
    permits: Semaphore,
    /// The outstanding-task count, as a watch so [`ApnsNotifier::quiesce`] cannot miss the last
    /// decrement. Behind an `Arc` so a [`TaskSlot`] can hold the counter alone.
    tasks: Arc<watch::Sender<usize>>,
    cancel: CancellationToken,
    /// The Live Activity throttle window; also the registration cache lifetime.
    update_interval: Duration,
    /// Where the live percent/speed/eta come from (DESIGN §15.1). `None` leaves the notifier on
    /// the event's own view, which is what it was before this port existed.
    progress: Option<Arc<dyn ProgressReader>>,
    /// How often a progressing item is re-read from [`Shared::progress`].
    progress_interval: Duration,
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
    /// update token → what the last update to it carried: when it went out, the status word, and
    /// which [`Track::pending_seq`] it was.
    ///
    /// The sequence number is what makes the trailing edge terminate. "Up to date" has to be a
    /// fact about the *view* a registration received, not about a clock: with two registrations
    /// whose windows are offset by less than [`Shared::update_interval`], a purely time-based
    /// guard re-throttles whichever one was just stamped, so `next` is `Some` on every pass and
    /// the timer re-pushes the same content-state to Apple, alternately, forever.
    last_sent: HashMap<Box<str>, Delivered>,
    /// The newest view that has not been delivered to every registration yet — the trailing edge.
    pending: Option<Arc<ItemView>>,
    /// Bumped every time `pending` is replaced, so a registration can say which view it has.
    pending_seq: u64,
    /// Whether a trailing-edge timer task owns this item.
    timer_armed: bool,
}

/// What one registration was last sent.
#[derive(Clone, Copy, Debug)]
struct Delivered {
    /// When it went out, on the interval clock.
    at: Instant,
    /// The status word it carried.
    status: Status,
    /// The [`Track::pending_seq`] it carried.
    seq: u64,
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
    // -- the source gate ----------------------------------------------------

    /// Whether this item is one the phone should hear about *unprompted* — the gate on the two
    /// pushes that fan out to devices, the push-to-start and the alert (DESIGN §12.6, §25.2).
    ///
    /// One test on `source.kind` covers a playlist too: the engine copies the parent's `SourceRef`
    /// onto every child, and nothing ever rewrites it (DESIGN §4.4, §8.8, §8.9). A Live Activity
    /// update or end is not asked about here: it goes to a registration the app made for that one
    /// item, so the registration is the permission.
    fn pushes_for(&self, view: &ItemView) -> bool {
        self.push_all || view.source.kind == SourceKind::Ios
    }

    /// Whether *this device* is one of the ones [`Self::pushes_for`] meant (DESIGN §25.2).
    ///
    /// `pushes_for` answers "does the phone hear about this item at all"; this answers "which
    /// phone". An iOS add carries the adding install in `source.ref` (`X-Aulos-Install`, PROTOCOL
    /// §1.3) and a registration carries its own in `install_id` (§4.8), so the household's iPad
    /// stays quiet for a download started on the iPhone.
    ///
    /// Three things are deliberately *not* a match failure, and each of them is the fan-out that
    /// shipped before this key existed:
    ///
    /// - `APNS_PUSH_ALL=true` — the escape hatch means every device, full stop;
    /// - a non-`ios` origin — `ref` is then a chat id or a subscription id, which no device has,
    ///   and the item only reached here through the alert-if-tracked rule or the knob;
    /// - `source.ref == None` — an app build that predates the header. Its adds are indistinct, so
    ///   every alerting device hears about them, exactly as before.
    ///
    /// A device with no `install_id` therefore hears about every *legacy* item and about none of
    /// the items a newer build added — which is right: the moment one install identifies itself,
    /// an unidentified registration is some other install.
    fn install_matches(&self, source: &SourceRef, device: &DeviceRecord) -> bool {
        if self.push_all || source.kind != SourceKind::Ios {
            return true;
        }
        match source.reference.as_deref() {
            None => true,
            Some(install) => device.install_id.as_deref() == Some(install),
        }
    }

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

    /// Whether the item's start push has already been claimed. The cheap pre-check that keeps a
    /// pause/resume from costing a store read; [`Self::mark_started`] is the authority.
    fn already_started(&self, id: ItemId) -> bool {
        self.lock_state().started.contains(&id)
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

    // -- the progress reader -------------------------------------------------

    /// The event's view with the freshest published progress merged over it (DESIGN §15.1).
    ///
    /// Only the transient numbers are taken. `status` stays the event's, because the event *is*
    /// the authority on the transition it announces and the aggregator may not have applied it
    /// yet — taking the snapshot's status wholesale here would push `downloading` over an item
    /// that has just entered `postprocessing`.
    ///
    /// A group is left alone in practice as well as in principle: the engine already writes real
    /// roll-up numbers onto a group view, and the snapshot computes them the same way, so the
    /// merge is a no-op rather than a regression.
    fn freshen(&self, view: &Arc<ItemView>) -> Arc<ItemView> {
        let Some(reader) = &self.progress else {
            return Arc::clone(view);
        };
        if !is_progressing(view.status) {
            return Arc::clone(view);
        }
        // A terminal snapshot row belongs to a download that is already over; its numbers are the
        // final ones and merging them under a `downloading` status would read as a finished bar.
        let Some(live) = reader.view(view.id).filter(|v| !v.status.is_terminal()) else {
            return Arc::clone(view);
        };
        let mut merged = (**view).clone();
        copy_progress(&live, &mut merged);
        Arc::new(merged)
    }

    /// The trailing-edge timer's own source of a new view, when nothing was pushed to it.
    ///
    /// `None` when there is no reader, when the item is not progressing any more (which is what
    /// lets the cadence terminate) or when the published view is byte-for-byte what the item
    /// already has pending — a stalled download must not cost a push every five seconds.
    fn refreshed(&self, id: ItemId, track: &Track) -> Option<Arc<ItemView>> {
        let reader = self.progress.as_ref()?;
        if !track
            .pending
            .as_ref()
            .is_some_and(|p| is_progressing(p.status))
        {
            return None;
        }
        reader
            .view(id)
            .filter(|live| track.pending.as_ref().is_none_or(|p| **p != **live))
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
        let incoming = match incoming {
            Some(v) => Some(self.freshen(&v)),
            None if from_timer => self.refreshed(id, track),
            None => None,
        };
        if let Some(v) = incoming {
            track.pending = Some(v);
            track.pending_seq = track.pending_seq.wrapping_add(1);
        }
        let Some(view) = track.pending.clone() else {
            if from_timer {
                track.timer_armed = false;
            }
            return Step::default();
        };

        let now = Instant::now();
        let seq = track.pending_seq;
        let mut targets = Vec::new();
        let mut next: Option<Instant> = None;
        for r in &track.records {
            match track.last_sent.get(&r.update_token) {
                // This registration already has *this* view. Neither a target nor a reason to
                // come back: without this arm the loop below never converges.
                Some(sent) if sent.seq == seq => {}
                // Throttled: same status, and the two seconds are not up.
                Some(sent)
                    if sent.status == view.status && now < sent.at + self.update_interval =>
                {
                    let due = sent.at + self.update_interval;
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
            track.last_sent.insert(
                t.token.clone(),
                Delivered {
                    at: now,
                    status: view.status,
                    seq,
                },
            );
        }

        // A progressing item with a live activity on it is never "done": the numbers keep moving
        // and the app is not there to ask for them, so the timer comes back on the progress
        // cadence to pull the next frame (DESIGN §25.4). It terminates the moment the item leaves
        // the progressing statuses, or when `Completed`/`Removed` forgets the track entirely.
        let keep_pulling =
            self.progress.is_some() && !track.records.is_empty() && is_progressing(view.status);
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
            None if keep_pulling => {
                let due = now + self.progress_interval;
                if from_timer || !track.timer_armed {
                    track.timer_armed = true;
                    Some(due)
                } else {
                    None
                }
            }
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
        // The same install match the alert uses: a start is a fan-out to devices, so it goes to
        // the install that added the item and not to the iPad in the next room (§25.2).
        let targets: Vec<&DeviceRecord> = devices
            .iter()
            .filter(|d| {
                d.live_activity_start_token.is_some() && self.install_matches(&view.source, d)
            })
            .collect();
        if targets.is_empty() {
            // Nobody can receive a start yet. Leave the latch open so a device that registers its
            // push-to-start token later still gets an activity on the next start edge.
            return;
        }
        if !self.mark_started(view.id) {
            return; // a sibling task claimed it first.
        }
        let now = self.now_secs();
        let payload = payload::live_activity_start(&view, now);
        for d in targets {
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
        // Unconditional, even for an item this notifier does not alert on: `APNS_PUSH_ALL` can be
        // flipped while an activity is live, and nothing else ever closes one (§25.2).
        let tracked = self.end_live_activities(&view).await;

        // The alert obeys the source gate — a Telegram or web add is announced by whoever asked
        // for it, not by the phone (DESIGN §12.6) — with one addition: an item the app was holding
        // a Live Activity for *is* the phone's to announce, whatever `source` says. A registration
        // exists only because the app asked for one, and `source` is written once at the add and
        // never rewritten (§4.4), so it is the registration, not the row, that knows the phone is
        // watching this download.
        if !self.pushes_for(&view) && !tracked {
            return;
        }
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
        // `alerts` is the device's own switch; `install_matches` is the routing key — the phone
        // that added this download, not every phone in the house (§25.2).
        for d in devices
            .iter()
            .filter(|d| d.alerts && self.install_matches(&view.source, d))
        {
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
    ///
    /// Returns whether the item had any registration at all — which is what tells the caller the
    /// phone was watching this download even when its `source` says somebody else added it.
    async fn end_live_activities(self: &Arc<Self>, view: &Arc<ItemView>) -> bool {
        let id = view.id;
        // A failed read is not "this item had no registrations". DESIGN §25.2 makes the end
        // unconditional, nothing retries a `Completed`, and the only other sweep
        // (`WriteOp::DeleteItems`) fires on removal — so a transient store error here would leave
        // a progress ring spinning on the lock screen forever. Fall back to the cached
        // registrations, which are at most one throttle window old.
        let (records, read_failed) = match self.store.live_activities_for(id).await {
            Ok(r) => (r, false),
            Err(e) => {
                tracing::warn!(item = %id, error = %e, "APNs: live activities unreadable");
                self.counters
                    .set_last_error(&format!("live activities unreadable: {e}"));
                (self.cached_records(id), true)
            }
        };
        let mut cleared = true;
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
        }
        if (!records.is_empty() || read_failed)
            && let Err(e) = self.store.remove_live_activities_for(id).await
        {
            cleared = false;
            tracing::warn!(item = %id, error = %e, "APNs: live activities could not be cleared");
            self.counters
                .set_last_error(&format!("live activities not cleared: {e}"));
        }
        // Forgetting the item discards the notifier's only memory that it still owes this id an
        // end. Keep the track when the read failed *and* the delete failed too, so a later
        // `Removed` still sweeps it.
        if cleared || !read_failed {
            self.forget(&[id]);
        }
        !records.is_empty()
    }

    /// The registrations this notifier last read for an item. The fallback the terminal path uses
    /// when the store cannot be read.
    fn cached_records(&self, id: ItemId) -> Vec<LiveActivityRecord> {
        self.lock_state()
            .items
            .get(&id)
            .map(|t| t.records.clone())
            .unwrap_or_default()
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
        // The decrement is a `Drop` guard, not a statement after the `await`: a push that panics
        // would otherwise leak its slot for the life of the process, and 256 of those wedge every
        // future push behind "the task set is full" and make `quiesce` never observe zero.
        let slot = TaskSlot(Arc::clone(&self.tasks));
        tokio::spawn(async move {
            let _slot = slot;
            fut.await;
        });
    }
}

/// One accepted place in the bounded task set, released on drop — panic or not.
struct TaskSlot(Arc<watch::Sender<usize>>);

impl Drop for TaskSlot {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
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

    #[tokio::test]
    async fn a_panicking_task_still_releases_its_slot() {
        // The decrement used to be a statement after `fut.await`, so a panic inside a push leaked
        // one of the 256 slots for the life of the process. 256 of those and every later push is
        // rejected with "the task set is full" and `quiesce` never observes zero.
        let tasks = Arc::new(watch::Sender::new(1usize));
        let slot = TaskSlot(Arc::clone(&tasks));
        let joined = tokio::spawn(async move {
            let _slot = slot;
            panic!("a push blew up");
        });
        assert!(
            joined.await.is_err(),
            "the task must actually have panicked"
        );
        assert_eq!(*tasks.borrow(), 0, "the slot goes back even on a panic");
    }

    #[test]
    fn the_live_activity_topic_is_the_bundle_id_plus_apples_suffix() {
        assert_eq!(
            &*live_activity_topic("com.tatoalo.aulos"),
            "com.tatoalo.aulos.push-type.liveactivity"
        );
    }
}
