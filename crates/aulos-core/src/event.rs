//! `DomainEvent` and the one-to-N [`EventRouter`] fan-out (DESIGN §2.2.1, §8.1).
//!
//! A `tokio::sync::mpsc::Receiver` has exactly one owner, and a `broadcast` channel cannot express
//! per-subscriber capacity or drop policy (and drops the *oldest* message for a slow reader, which
//! for a `Completed` event means a hook never runs). So the fan-out is an explicit, tiny task that
//! owns the single receiver and pushes into one bounded inbox per subscriber.
//!
//! It lives here — in `aulos-core` — because `DomainEvent` is declared here, which is also why
//! `SubscriptionView`, `ReloadReport` and `HealthView` are `aulos-core` types (DESIGN §3).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::health::HealthView;
use crate::id::{ItemId, SubId};
use crate::item::ItemView;
use crate::reload::ReloadReport;
use crate::status::Status;
use crate::subscription::SubscriptionView;

/// Why a batch of items appeared.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddReason {
    /// A user (or a subscription) added them.
    Created,
    /// A playlist or channel resolved into children.
    Expanded,
    /// A retry re-queued them.
    Retried,
}

/// Why a batch of ids disappeared.
///
/// One event per (reason, batch): a producer that removes ids for two different reasons publishes
/// two events, and the wire carries one `removed` frame per reason (PROTOCOL §5.7).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveReason {
    /// An explicit delete. Wire: `"deleted"`.
    Deleted,
    /// `POST <p>api/v2/items/clear` or the v1 equivalent. Wire: `"cleared"`.
    Cleared,
    /// The `CLEAR_COMPLETED_AFTER` sweeper. Wire: `"auto_cleared"`.
    ///
    /// The variant reads `Expired` because that is what happened to the row; the wire string is
    /// PROTOCOL §5.7's, which is what a client matches on.
    #[serde(rename = "auto_cleared")]
    Expired,
    /// A playlist parent was replaced by its children. Wire: `"group_cascade"`.
    #[serde(rename = "group_cascade")]
    Replaced,
}

impl RemoveReason {
    /// The wire string (PROTOCOL §5.7).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Cleared => "cleared",
            Self::Expired => "auto_cleared",
            Self::Replaced => "group_cascade",
        }
    }
}

impl std::fmt::Display for RemoveReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Severity of a notice. `warn` is spelled `"warning"` on the wire (PROTOCOL §5.8).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Informational.
    Info,
    /// Something the user should know about but that did not fail.
    #[serde(rename = "warning")]
    Warn,
    /// Something failed.
    Error,
}

impl Level {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warning",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The closed, **server-owned** notice code set (PROTOCOL §5.8).
///
/// A client should treat it as an open set with a fallback, but nothing outside this list is ever
/// emitted — a `command` plugin's free-form note is forwarded under
/// [`notice_code::PLUGIN_NOTE`], so a plugin never gets to choose the code.
pub mod notice_code {
    /// The no-progress watchdog fired (`AULOS_JOB_STALL_SECS`).
    pub const STALLED: &str = "stalled";
    /// The hard per-job wall clock fired (`AULOS_JOB_TIMEOUT_SECS`).
    pub const JOB_TIMEOUT: &str = "job_timeout";
    /// The POT sidecar is not answering.
    pub const POT_DOWN: &str = "pot_down";
    /// A provider entered `Degraded` state.
    pub const PROVIDER_DEGRADED: &str = "provider_degraded";
    /// The legacy importer produced a warning.
    pub const IMPORT_WARNING: &str = "import_warning";
    /// A `command` provider printed a note while resolving.
    pub const PLUGIN_NOTE: &str = "plugin_note";

    /// Every code, for the wire-contract test.
    pub const ALL: [&str; 6] = [
        STALLED,
        JOB_TIMEOUT,
        POT_DOWN,
        PROVIDER_DEGRADED,
        IMPORT_WARNING,
        PLUGIN_NOTE,
    ];
}

/// A user-visible out-of-band message: the body of a `notice` frame (PROTOCOL §5.8).
///
/// The same four fields as [`DomainEvent::Notice`], as a named, serialisable type — the aggregator
/// builds one of these per event and `aulos-api` wraps it with `t` and `seq`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Notice {
    /// Severity.
    pub level: Level,
    /// One of [`notice_code`]. Not from the [`crate::error::ErrorCode`] taxonomy: a notice is not
    /// an error envelope.
    pub code: Box<str>,
    /// The item this is about, or `null` for a server-wide notice.
    pub id: Option<ItemId>,
    /// Human text.
    pub message: Box<str>,
}

impl Notice {
    /// Builds a notice.
    #[must_use]
    pub fn new(
        level: Level,
        code: &'static str,
        id: Option<ItemId>,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            level,
            code: code.into(),
            id,
            message: message.into(),
        }
    }
}

/// Everything that happens in the process which anything outside the queue engine cares about
/// (DESIGN §8.1).
///
/// Every type in a payload is an `aulos-core` type, because this enum is declared here and the
/// dependency graph is strictly downward.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DomainEvent {
    /// New rows exist.
    Added(Vec<Arc<ItemView>>, AddReason),
    /// A persisted row changed.
    ///
    /// `from == to` is legal and is the engine's generic "re-diff this row" signal — used by
    /// `HookWrite` (DESIGN §13.3) and by any write that changes a mutable field without changing
    /// the status. The aggregator diffs against `last_sent`, so nothing needs to say which field
    /// moved.
    StatusChanged {
        /// Which row.
        id: ItemId,
        /// The status before the write.
        from: Status,
        /// The status after the write.
        to: Status,
        /// The complete new view.
        view: Arc<ItemView>,
    },
    /// A row reached a terminal status.
    Completed(Arc<ItemView>),
    /// Rows were removed. One event per (reason, batch).
    Removed {
        /// The ids that went away.
        ids: Vec<ItemId>,
        /// Why.
        reason: RemoveReason,
    },
    /// Terminal work is done but the status has **not** been written yet.
    ///
    /// Delivered to the `hooks` subscriber **only**, so it produces no frame: it exists solely to
    /// give a `PreTerminal` hook its turn before the engine writes the terminal status
    /// (DESIGN §13). The engine finalises on `EngineCmd::HooksFinished`.
    Finishing(Arc<ItemView>),
    /// A subscription was created or updated.
    SubscriptionChanged(Arc<SubscriptionView>),
    /// A subscription was deleted.
    SubscriptionRemoved(SubId),
    /// `YTDL_OPTIONS*` were reloaded. The payload is the exact legacy shape (DESIGN §17.2).
    YtdlOptionsReloaded {
        /// Whether the reload succeeded. On failure the last-good options are kept.
        ok: bool,
        /// The legacy error string, or empty on success.
        msg: Box<str>,
        /// The file mtime as fractional epoch seconds, or `None`.
        update_time: Option<f64>,
    },
    /// `AULOS_PLUGINS_DIR` was re-scanned.
    ProvidersReloaded(Arc<ReloadReport>),
    /// A `healthz` component changed state.
    HealthChanged(Arc<HealthView>),
    /// An out-of-band message for the user (DESIGN §8.1's inline shape).
    Notice {
        /// Severity.
        level: Level,
        /// One of [`notice_code`] — server-owned, so a `&'static str` is the honest type.
        code: &'static str,
        /// The item this is about, or `None` for a server-wide notice.
        id: Option<ItemId>,
        /// Human text.
        message: Box<str>,
    },
}

impl From<Notice> for DomainEvent {
    /// Loses the `'static` guarantee on `code`, so it is only used by producers that already hold
    /// a [`Notice`]. Prefer constructing [`DomainEvent::Notice`] directly.
    fn from(n: Notice) -> Self {
        let code: &'static str = notice_code::ALL
            .iter()
            .find(|c| **c == &*n.code)
            .copied()
            .unwrap_or(notice_code::PLUGIN_NOTE);
        Self::Notice {
            level: n.level,
            code,
            id: n.id,
            message: n.message,
        }
    }
}

impl DomainEvent {
    /// The discriminant, for filtering and for metric labels.
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        match self {
            Self::Added(..) => EventKind::Added,
            Self::StatusChanged { .. } => EventKind::StatusChanged,
            Self::Completed(_) => EventKind::Completed,
            Self::Removed { .. } => EventKind::Removed,
            Self::Finishing(_) => EventKind::Finishing,
            Self::SubscriptionChanged(_) => EventKind::SubscriptionChanged,
            Self::SubscriptionRemoved(_) => EventKind::SubscriptionRemoved,
            Self::YtdlOptionsReloaded { .. } => EventKind::YtdlOptionsReloaded,
            Self::ProvidersReloaded(_) => EventKind::ProvidersReloaded,
            Self::HealthChanged(_) => EventKind::HealthChanged,
            Self::Notice { .. } => EventKind::Notice,
        }
    }

    /// The wire projection of a [`DomainEvent::Notice`], or `None` for any other variant.
    #[must_use]
    pub fn as_notice(&self) -> Option<Notice> {
        match self {
            Self::Notice {
                level,
                code,
                id,
                message,
            } => Some(Notice {
                level: *level,
                code: (*code).into(),
                id: *id,
                message: message.clone(),
            }),
            _ => None,
        }
    }
}

/// One bit per [`DomainEvent`] variant, so an [`EventFilter`] is a `u16` test.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum EventKind {
    /// [`DomainEvent::Added`].
    Added = 1 << 0,
    /// [`DomainEvent::StatusChanged`].
    StatusChanged = 1 << 1,
    /// [`DomainEvent::Completed`].
    Completed = 1 << 2,
    /// [`DomainEvent::Removed`].
    Removed = 1 << 3,
    /// [`DomainEvent::Finishing`].
    Finishing = 1 << 4,
    /// [`DomainEvent::SubscriptionChanged`].
    SubscriptionChanged = 1 << 5,
    /// [`DomainEvent::SubscriptionRemoved`].
    SubscriptionRemoved = 1 << 6,
    /// [`DomainEvent::YtdlOptionsReloaded`].
    YtdlOptionsReloaded = 1 << 7,
    /// [`DomainEvent::ProvidersReloaded`].
    ProvidersReloaded = 1 << 8,
    /// [`DomainEvent::HealthChanged`].
    HealthChanged = 1 << 9,
    /// [`DomainEvent::Notice`].
    Notice = 1 << 10,
}

impl EventKind {
    /// The bit this discriminant occupies.
    #[must_use]
    pub const fn bit(self) -> u16 {
        self as u16
    }

    /// A stable name, for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::StatusChanged => "status_changed",
            Self::Completed => "completed",
            Self::Removed => "removed",
            Self::Finishing => "finishing",
            Self::SubscriptionChanged => "subscription_changed",
            Self::SubscriptionRemoved => "subscription_removed",
            Self::YtdlOptionsReloaded => "ytdl_options_reloaded",
            Self::ProvidersReloaded => "providers_reloaded",
            Self::HealthChanged => "health_changed",
            Self::Notice => "notice",
        }
    }

    /// Every discriminant.
    pub const ALL: [Self; 11] = [
        Self::Added,
        Self::StatusChanged,
        Self::Completed,
        Self::Removed,
        Self::Finishing,
        Self::SubscriptionChanged,
        Self::SubscriptionRemoved,
        Self::YtdlOptionsReloaded,
        Self::ProvidersReloaded,
        Self::HealthChanged,
        Self::Notice,
    ];
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A bitset over [`DomainEvent`] discriminants.
///
/// A filtered-out event is **not** counted as dropped. Filtering is what keeps the hook inbox from
/// being flooded by progress-driven `StatusChanged`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EventFilter(u16);

impl EventFilter {
    /// Nothing passes.
    #[must_use]
    pub const fn none() -> Self {
        Self(0)
    }

    /// Everything passes, including [`EventKind::Finishing`].
    #[must_use]
    pub const fn all() -> Self {
        let mut bits = 0u16;
        let mut i = 0;
        while i < EventKind::ALL.len() {
            bits |= EventKind::ALL[i].bit();
            i += 1;
        }
        Self(bits)
    }

    /// A filter over an explicit list.
    #[must_use]
    pub const fn of(kinds: &[EventKind]) -> Self {
        let mut bits = 0u16;
        let mut i = 0;
        while i < kinds.len() {
            bits |= kinds[i].bit();
            i += 1;
        }
        Self(bits)
    }

    /// Adds a discriminant.
    #[must_use]
    pub const fn with(self, kind: EventKind) -> Self {
        Self(self.0 | kind.bit())
    }

    /// Removes a discriminant.
    #[must_use]
    pub const fn without(self, kind: EventKind) -> Self {
        Self(self.0 & !kind.bit())
    }

    /// Whether this discriminant passes.
    #[must_use]
    pub const fn allows(self, kind: EventKind) -> bool {
        self.0 & kind.bit() != 0
    }

    /// The aggregator's filter: everything **except** `Finishing`, which is not on the wire.
    ///
    /// A `Finishing` event would otherwise produce a frame for a state transition that has not
    /// happened yet (DESIGN §2.2.1).
    #[must_use]
    pub const fn aggregator() -> Self {
        Self::all().without(EventKind::Finishing)
    }

    /// The hook dispatcher's filter: `Finishing | Completed`.
    #[must_use]
    pub const fn hooks() -> Self {
        Self::of(&[EventKind::Finishing, EventKind::Completed])
    }

    /// The Telegram actor's filter: `Added | StatusChanged | Completed | Removed | Notice`.
    #[must_use]
    pub const fn telegram() -> Self {
        Self::of(&[
            EventKind::Added,
            EventKind::StatusChanged,
            EventKind::Completed,
            EventKind::Removed,
            EventKind::Notice,
        ])
    }
}

/// What the router does when a subscriber's inbox is full.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropPolicy {
    /// The router awaits this subscriber.
    ///
    /// Applying backpressure to every other subscriber is acceptable because losing one of these
    /// events is a correctness bug. A `Block` subscriber must never `await` anything unbounded
    /// between `recv()` calls; the aggregator satisfies that (it only touches memory).
    Block,
    /// `try_send`; on a full inbox the **newest** event is dropped and `dropped()` increments.
    ///
    /// Newest, not oldest, because the oldest event may be the `Completed` that arms a hook.
    DropNewest,
}

/// How a subscriber wants to be fed. Registered once, at wiring time, before [`EventRouter::spawn`].
#[derive(Clone, Copy, Debug)]
pub struct SubscriberSpec {
    /// Stable name; used as the metric label and in logs.
    pub name: &'static str,
    /// This subscriber's own inbox depth.
    pub capacity: usize,
    /// What to do when it is full.
    pub policy: DropPolicy,
    /// Which discriminants it wants.
    pub filter: EventFilter,
}

impl SubscriberSpec {
    /// The `aggregator` subscriber of DESIGN §2.2.1: 1024, `Block`, everything but `Finishing`.
    #[must_use]
    pub const fn aggregator() -> Self {
        Self {
            name: "aggregator",
            capacity: 1024,
            policy: DropPolicy::Block,
            filter: EventFilter::aggregator(),
        }
    }

    /// The `hooks` subscriber: 256, `DropNewest`, `Finishing | Completed`.
    #[must_use]
    pub const fn hooks() -> Self {
        Self {
            name: "hooks",
            capacity: 256,
            policy: DropPolicy::DropNewest,
            filter: EventFilter::hooks(),
        }
    }

    /// The `telegram` subscriber: 512, `DropNewest`, the five wire-ish discriminants.
    #[must_use]
    pub const fn telegram() -> Self {
        Self {
            name: "telegram",
            capacity: 512,
            policy: DropPolicy::DropNewest,
            filter: EventFilter::telegram(),
        }
    }
}

/// Cheap-to-clone producer handle. Held by the engine, the scheduler, the watchers and `healthz`.
#[derive(Clone, Debug)]
pub struct EventSender(mpsc::Sender<DomainEvent>);

impl EventSender {
    /// `send().await` — never dropped, never reordered. Bounded at the router's capacity.
    ///
    /// A closed channel (the router task is gone, i.e. shutdown) is logged at DEBUG and ignored:
    /// a producer racing shutdown is not an error worth propagating to every call site.
    pub async fn publish(&self, ev: DomainEvent) {
        let kind = ev.kind();
        if self.0.send(ev).await.is_err() {
            tracing::debug!(event = %kind, "event router is gone; event dropped at shutdown");
        }
    }

    /// For sync contexts: `Drop` guards, signal handlers.
    ///
    /// # Errors
    /// [`TryPublishError::Full`] or [`TryPublishError::Closed`].
    pub fn try_publish(&self, ev: DomainEvent) -> Result<(), TryPublishError> {
        self.0.try_send(ev).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => TryPublishError::Full,
            mpsc::error::TrySendError::Closed(_) => TryPublishError::Closed,
        })
    }

    /// Whether the router is still running.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.0.is_closed()
    }
}

/// [`EventSender::try_publish`] failures.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum TryPublishError {
    /// The router inbox is full. The caller decides whether to drop or to spawn.
    #[error("the event router inbox is full")]
    Full,
    /// The router task has stopped.
    #[error("the event router has shut down")]
    Closed,
}

/// One subscriber's private inbox.
///
/// This is what `HookDispatcher::spawn`, `TelegramActor::spawn` and `Aggregator::spawn` take —
/// **not** a raw `mpsc::Receiver<DomainEvent>`.
#[derive(Debug)]
pub struct EventInbox {
    /// The subscriber's stable name.
    pub name: &'static str,
    rx: mpsc::Receiver<Arc<DomainEvent>>,
    dropped: Arc<AtomicU64>,
}

impl EventInbox {
    /// The next event, or `None` once every [`EventSender`] is gone and the inbox has drained.
    pub async fn recv(&mut self) -> Option<Arc<DomainEvent>> {
        self.rx.recv().await
    }

    /// Drains up to `max` events into `buf`, awaiting the first one. Returns how many were added.
    pub async fn recv_many(&mut self, buf: &mut Vec<Arc<DomainEvent>>, max: usize) -> usize {
        self.rx.recv_many(buf, max).await
    }

    /// Events dropped for **this** subscriber since start. Always `0` for a [`DropPolicy::Block`]
    /// subscriber.
    ///
    /// `aulos_event_dropped_total{subscriber} > 0` is a WARN-level signal in `healthz`: it means a
    /// hook or a Telegram notification was silently skipped.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The same counter [`EventInbox::dropped`] reads, as a handle that outlives the inbox.
    ///
    /// [`EventInbox::dropped`] needs `&self`, and every consumer in the server — `Aggregator::
    /// spawn`, `HookDispatcher::spawn`, `TelegramActor::spawn` — takes the inbox **by value**. So
    /// the health publisher has to take the counter *before* handing the inbox over, or it can
    /// only ever report the boot value. Take this at wiring time and read it forever:
    ///
    /// ```no_run
    /// # use aulos_core::event::{EventRouter, SubscriberSpec};
    /// # let (mut router, _tx) = EventRouter::new(64);
    /// let inbox = router.subscribe(SubscriberSpec::telegram());
    /// let dropped = inbox.dropped_handle();
    /// // `inbox` is consumed here by the subscriber's `spawn`; `dropped` still reads the counter.
    /// assert_eq!(dropped.load(std::sync::atomic::Ordering::Relaxed), 0);
    /// ```
    #[must_use]
    pub fn dropped_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

/// One registered subscriber, from the router's point of view.
#[derive(Debug)]
struct Subscriber {
    name: &'static str,
    tx: mpsc::Sender<Arc<DomainEvent>>,
    policy: DropPolicy,
    filter: EventFilter,
    dropped: Arc<AtomicU64>,
}

/// The one-to-N fan-out task (DESIGN §2.2.1).
///
/// # Guarantees
///
/// | Property | Statement |
/// |---|---|
/// | Ordering | Per subscriber, strict FIFO in publish order. All subscribers observe the same *relative* order of the events they receive. There is no cross-subscriber synchronisation: A may be 40 events ahead of B. |
/// | Cost | One `Arc` clone per subscriber, never the payload. A 500-child `Added` is one allocation total. |
/// | Filtering | A subscriber receives only the discriminants in its filter; a filtered-out event is not counted as dropped. |
/// | Shutdown | Dropping every [`EventSender`] closes the inbox chain; each subscriber drains what it has and then sees `None`. |
#[derive(Debug)]
pub struct EventRouter {
    rx: mpsc::Receiver<DomainEvent>,
    subscribers: Vec<Subscriber>,
}

impl EventRouter {
    /// Creates the router and its producer handle. `capacity` is the shared inbound depth (4096
    /// in production, DESIGN §2.3).
    #[must_use]
    pub fn new(capacity: usize) -> (Self, EventSender) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                rx,
                subscribers: Vec::new(),
            },
            EventSender(tx),
        )
    }

    /// Registers a subscriber and hands back its private inbox.
    ///
    /// Registration happens once, at wiring time (DESIGN §16.1 steps 11–14), **before**
    /// [`Self::spawn`]. Because `spawn` consumes `self`, registering afterwards is not
    /// expressible: the borrow checker enforces what DESIGN §2.2.1 calls a programmer error.
    pub fn subscribe(&mut self, spec: SubscriberSpec) -> EventInbox {
        let (tx, rx) = mpsc::channel(spec.capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        self.subscribers.push(Subscriber {
            name: spec.name,
            tx,
            policy: spec.policy,
            filter: spec.filter,
            dropped: Arc::clone(&dropped),
        });
        EventInbox {
            name: spec.name,
            rx,
            dropped,
        }
    }

    /// The registered subscriber names, in registration order.
    #[must_use]
    pub fn subscriber_names(&self) -> Vec<&'static str> {
        self.subscribers.iter().map(|s| s.name).collect()
    }

    /// Runs the fan-out until every [`EventSender`] is dropped.
    ///
    /// Exposed as well as [`Self::spawn`] so a test can drive it on the current task.
    pub async fn run(mut self) {
        while let Some(ev) = self.rx.recv().await {
            let kind = ev.kind();
            let shared = Arc::new(ev);
            for sub in &self.subscribers {
                if !sub.filter.allows(kind) {
                    continue;
                }
                match sub.policy {
                    DropPolicy::Block => {
                        if sub.tx.send(Arc::clone(&shared)).await.is_err() {
                            tracing::debug!(
                                subscriber = sub.name,
                                event = %kind,
                                "subscriber inbox closed"
                            );
                        }
                    }
                    DropPolicy::DropNewest => {
                        if sub.tx.try_send(Arc::clone(&shared)).is_err() {
                            let n = sub.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(
                                subscriber = sub.name,
                                event = %kind,
                                dropped_total = n,
                                "event inbox full; dropped the newest event"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Spawns [`Self::run`] on the current runtime.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }
}

/// The notification seam (DESIGN §12.6).
///
/// It lives next to [`DomainEvent`] and [`EventRouter`] so a future APNs notifier can be a crate
/// of its own, registered as one more subscriber, without depending on `aulos-telegram` or
/// forcing any existing crate to change. `aulos-telegram` is the first implementation and
/// re-exports this trait; BRIEF's "out of scope" list keeps APNs itself for later.
#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    /// A stable id, for logs and for `healthz`.
    fn id(&self) -> &'static str;

    /// Whether this notifier wants to hear about `item`.
    fn interested(&self, item: &ItemView) -> bool;

    /// Handle one event. Must not block.
    async fn on_event(&self, ev: &DomainEvent);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn notice(msg: &str) -> DomainEvent {
        DomainEvent::Notice {
            level: Level::Info,
            code: notice_code::PLUGIN_NOTE,
            id: None,
            message: msg.into(),
        }
    }

    fn text(e: &DomainEvent) -> String {
        e.as_notice()
            .map(|n| n.message.to_string())
            .unwrap_or_default()
    }

    fn health() -> DomainEvent {
        DomainEvent::HealthChanged(Arc::new(HealthView::empty()))
    }

    fn reloaded() -> DomainEvent {
        DomainEvent::YtdlOptionsReloaded {
            ok: true,
            msg: "".into(),
            update_time: None,
        }
    }

    #[test]
    fn filters_are_bitsets_over_the_discriminants() {
        assert!(EventFilter::all().allows(EventKind::Finishing));
        assert!(!EventFilter::aggregator().allows(EventKind::Finishing));
        for k in EventKind::ALL {
            if k != EventKind::Finishing {
                assert!(EventFilter::aggregator().allows(k), "{k}");
            }
        }
        assert!(EventFilter::hooks().allows(EventKind::Completed));
        assert!(!EventFilter::hooks().allows(EventKind::Added));
        assert!(!EventFilter::none().allows(EventKind::Added));
        // Every bit is distinct.
        let mut seen = 0u16;
        for k in EventKind::ALL {
            assert_eq!(seen & k.bit(), 0, "{k} reuses a bit");
            seen |= k.bit();
        }
    }

    #[tokio::test]
    async fn three_subscribers_get_only_their_discriminants_in_publish_order() {
        let (mut router, tx) = EventRouter::new(16);
        let mut all = router.subscribe(SubscriberSpec {
            name: "all",
            capacity: 16,
            policy: DropPolicy::Block,
            filter: EventFilter::all(),
        });
        let mut hooks = router.subscribe(SubscriberSpec::hooks());
        let mut notices = router.subscribe(SubscriberSpec {
            name: "notices",
            capacity: 16,
            policy: DropPolicy::Block,
            filter: EventFilter::of(&[EventKind::Notice]),
        });
        assert_eq!(router.subscriber_names(), ["all", "hooks", "notices"]);
        let task = router.spawn();

        tx.publish(notice("one")).await;
        tx.publish(reloaded()).await;
        tx.publish(health()).await;
        tx.publish(notice("two")).await;
        drop(tx);
        task.await.unwrap();

        let mut got = Vec::new();
        while let Some(e) = all.recv().await {
            got.push(e.kind());
        }
        assert_eq!(
            got,
            [
                EventKind::Notice,
                EventKind::YtdlOptionsReloaded,
                EventKind::HealthChanged,
                EventKind::Notice
            ]
        );

        assert!(hooks.recv().await.is_none(), "hooks wanted none of these");
        assert_eq!(hooks.dropped(), 0, "a filtered-out event is not a drop");

        let mut texts = Vec::new();
        while let Some(e) = notices.recv().await {
            assert_eq!(e.kind(), EventKind::Notice);
            texts.push(text(&e));
        }
        assert_eq!(texts, ["one", "two"]);
    }

    #[tokio::test]
    async fn a_full_drop_newest_inbox_drops_the_newest_and_leaves_others_alone() {
        let (mut router, tx) = EventRouter::new(64);
        let mut small = router.subscribe(SubscriberSpec {
            name: "small",
            capacity: 2,
            policy: DropPolicy::DropNewest,
            filter: EventFilter::all(),
        });
        let mut big = router.subscribe(SubscriberSpec {
            name: "big",
            capacity: 64,
            policy: DropPolicy::Block,
            filter: EventFilter::all(),
        });
        let task = router.spawn();

        for i in 0..5 {
            tx.publish(notice(&i.to_string())).await;
        }
        drop(tx);
        task.await.unwrap();

        // The two OLDEST survived; the newest three were dropped.
        let mut kept = Vec::new();
        while let Some(e) = small.recv().await {
            kept.push(text(&e));
        }
        assert_eq!(kept, ["0", "1"]);
        assert_eq!(small.dropped(), 3);

        let mut all = Vec::new();
        while let Some(e) = big.recv().await {
            all.push(text(&e));
        }
        assert_eq!(
            all,
            ["0", "1", "2", "3", "4"],
            "the other subscriber is unaffected"
        );
        assert_eq!(big.dropped(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_block_inbox_applies_backpressure_to_the_producer() {
        let (mut router, tx) = EventRouter::new(1);
        let mut blocker = router.subscribe(SubscriberSpec {
            name: "blocker",
            capacity: 1,
            policy: DropPolicy::Block,
            filter: EventFilter::all(),
        });
        let task = router.spawn();

        // Fill the inbox (1) and the router's in-flight slot without reading anything.
        tx.publish(notice("a")).await;
        tokio::task::yield_now().await;

        let producer = tokio::spawn({
            let tx = tx.clone();
            async move {
                for i in 0..8 {
                    tx.publish(notice(&i.to_string())).await;
                }
            }
        });

        // With nobody draining, the producer cannot finish: capacity is 1 + 1.
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        assert!(!producer.is_finished(), "the producer must be blocked");

        // Draining releases it.
        let mut n = 0;
        while n < 9 {
            if blocker.recv().await.is_some() {
                n += 1;
            } else {
                break;
            }
        }
        producer.await.unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(n, 9);
        assert_eq!(blocker.dropped(), 0, "Block never drops");
    }

    #[tokio::test]
    async fn dropping_every_sender_terminates_every_inbox_after_it_drains() {
        let (mut router, tx) = EventRouter::new(8);
        let mut a = router.subscribe(SubscriberSpec {
            name: "a",
            capacity: 8,
            policy: DropPolicy::Block,
            filter: EventFilter::all(),
        });
        let clone = tx.clone();
        let task = router.spawn();

        tx.publish(notice("x")).await;
        drop(tx);
        assert!(clone.is_open());
        clone.publish(notice("y")).await;
        drop(clone);
        task.await.unwrap();

        assert!(a.recv().await.is_some());
        assert!(a.recv().await.is_some());
        assert!(a.recv().await.is_none(), "closed after draining");
    }

    #[test]
    fn try_publish_reports_full_and_closed() {
        let (router, tx) = EventRouter::new(1);
        assert!(tx.try_publish(notice("a")).is_ok());
        assert_eq!(tx.try_publish(notice("b")), Err(TryPublishError::Full));
        drop(router);
        assert_eq!(tx.try_publish(notice("c")), Err(TryPublishError::Closed));
    }

    #[test]
    fn the_documented_subscriber_specs_match_design_2_2_1() {
        let agg = SubscriberSpec::aggregator();
        assert_eq!((agg.name, agg.capacity), ("aggregator", 1024));
        assert_eq!(agg.policy, DropPolicy::Block);
        let hooks = SubscriberSpec::hooks();
        assert_eq!((hooks.name, hooks.capacity), ("hooks", 256));
        assert_eq!(hooks.policy, DropPolicy::DropNewest);
        let tg = SubscriberSpec::telegram();
        assert_eq!((tg.name, tg.capacity), ("telegram", 512));
        assert_eq!(tg.policy, DropPolicy::DropNewest);
    }

    #[test]
    fn notice_and_reason_enums_serialise_snake_case() {
        // PROTOCOL §5.7's four strings, in PROTOCOL's order. The last two variants are named for
        // what happened to the row; the wire strings are the ones a client matches on, and
        // `as_str` and serde must not drift apart.
        for (reason, wire) in [
            (RemoveReason::Deleted, "deleted"),
            (RemoveReason::Cleared, "cleared"),
            (RemoveReason::Expired, "auto_cleared"),
            (RemoveReason::Replaced, "group_cascade"),
        ] {
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(reason.as_str(), wire);
            assert_eq!(reason.to_string(), wire);
            assert_eq!(
                serde_json::from_str::<RemoveReason>(&format!("\"{wire}\"")).unwrap(),
                reason,
                "and it round-trips"
            );
        }
        assert_eq!(
            serde_json::to_string(&AddReason::Expanded).unwrap(),
            "\"expanded\""
        );
        assert_eq!(
            serde_json::to_string(&Level::Warn).unwrap(),
            "\"warning\"",
            "PROTOCOL §5.8 spells it `warning`"
        );
        assert_eq!(serde_json::to_string(&Level::Info).unwrap(), "\"info\"");
        assert_eq!(
            serde_json::from_str::<Level>("\"warning\"").unwrap(),
            Level::Warn
        );
        // A notice code comes from the server-owned set and is deliberately not an ErrorCode.
        let n = Notice::new(Level::Error, notice_code::PROVIDER_DEGRADED, None, "boom");
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["code"], "provider_degraded");
        assert_eq!(v["level"], "error");
        assert_eq!(v["id"], serde_json::Value::Null);
        assert_ne!(v["code"], serde_json::json!(ErrorCode::Internal));
        assert_eq!(
            DomainEvent::from(n.clone()).as_notice().unwrap(),
            n,
            "the round trip through the event keeps the code"
        );
    }
}
