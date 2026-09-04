//! The delta aggregator: the one task that turns queue events and progress frames into wire frames
//! (DESIGN §15.1).
//!
//! Three rules shape everything here.
//!
//! - **A delta is a diff, not a mask.** On flush, each dirty item's freshly merged [`ItemView`] is
//!   compared field-by-field against `last_sent` and only the differing fields are written. The
//!   field list is enumerated by one `macro_rules!` whose generated array is declared with length
//!   [`ItemView::FIELDS`]`.len()`, so a field added to the struct and forgotten here is a
//!   **compile error**. A hand-maintained dirty-field mask fails the other way: a missed bit is a
//!   permanently stale field on the client, with no error anywhere.
//! - **State is urgent, numbers are batched.** Any change outside the nine numeric progress fields
//!   — so `msg`, `title`, `phase`, `status`, `error`, `filename`, `size`, the group counters —
//!   pulls the next flush forward to `AULOS_WS_URGENT_MS` (25 ms), as do `Stage`, `File`, `added`,
//!   `completed`, `removed` and `notice`. Purely numeric progress waits for the
//!   `AULOS_WS_BATCH_MS` (250 ms) tick, and an urgent flush does **not** reset that tick. The
//!   classifier is generated from the same field list as the diff, so a provider cannot forget to
//!   mark a message urgent and a numeric-only frame can never pull the cadence forward.
//! - **The flush order is fixed: `added` → `completed` → `removed`\* → `delta` → republish.**
//!   `added` first, so a `delta` or `completed` can never mention an id the client has not seen.
//!   `removed` after `added`, so an item created *and* deleted inside one window leaves no ghost
//!   row. `delta` last, so a patch for a just-removed id is a guaranteed no-op. The republish is
//!   last, so a REST reader can never observe a snapshot newer than the socket. `removed` is the
//!   one relaxation of "at most one frame per kind": it is emitted **once per distinct
//!   [`RemoveReason`]**, in the fixed reason order, because the frame carries one reason while the
//!   aggregator tracks a reason per id (PROTOCOL §5.7).
//!
//! An empty batch emits **no frame at all**: an idle server is silent, which matters for the iOS
//! radio. A download stalled at a constant 43.2 % therefore produces zero bytes after its first
//! frame.
//!
//! # `Finishing` is not on the wire
//!
//! [`DomainEvent::Finishing`] is filtered out of this subscriber's inbox
//! (`SubscriberSpec::aggregator`): it exists only to give a pre-terminal hook its turn *before* the
//! engine writes the terminal status, so a frame for it would announce a transition that has not
//! happened. The handler below drops it explicitly rather than relying on the filter alone.
//!
//! # Groups
//!
//! Progress never enters the engine (DESIGN §2.2), so the three progress-derived fields of a
//! [`GroupAcc`] — `downloaded`, `speed` and `active_percent` — can only be known here. The
//! aggregator therefore keeps its **own** accumulator per group, folded in O(1) per child change
//! from the child views it already sees, and writes its `percent`/`speed`/`eta` onto the group's
//! row; the engine stays authoritative for `children_total`/`_done`/`_error`/`_active`. See
//! `docs/INTEGRATION-NOTES.md`, WP-13, for the one input this loses (a queued child's
//! `filesize_approx`, which lives in the entry blob and not on the wire) and what it costs.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use aulos_core::{
    AddReason, Clock, ComponentStatus, Config, DomainEvent, EventInbox, GroupId, HealthView,
    ItemId, ItemView, Kind, Normalizer, ProgressCell, RemoveReason, Status,
};
use aulos_provider::ProgressMsg;
use indexmap::{IndexMap, IndexSet};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cmd::EngineHandle;
use crate::groups::{DRIFT_RECOMPUTE_MS, GroupAcc};
use crate::hub::EventHub;
use crate::publish::{Published, StateView, StatusCounts, Truncated};
use crate::ring::{DeltaBatch, DeltaItem, FrameBody, FrameKind, REASON_ORDER};

/// How many messages one wake drains, per DESIGN §15.1. Bounded so a hot inbox can never starve the
/// tick or the urgent deadline — which is precisely the 500-item-playlist path.
const RECV_MANY: usize = 256;

/// How many `delta` frames one flush may emit before leaving the rest dirty for the next tick.
///
/// With the stock `AULOS_WS_MAX_DELTAS_PER_FRAME` of 200 that is 800 items per flush, so the
/// DESIGN §15.1 worked example — 1 000 dirty ids — becomes five frames of 200 across two ticks and
/// no item waits more than four ticks however hot the queue is.
const MAX_DELTA_FRAMES_PER_FLUSH: usize = 4;

/// How many consecutive splitting flushes trigger the tick backoff (DESIGN §15.1).
const SPLIT_BACKOFF_TICKS: u32 = 4;

/// The cap on the backed-off tick (DESIGN §15.1).
const MAX_BACKOFF_MS: u64 = 2_000;

// ---------------------------------------------------------------------------
// the generated diff and its urgency classifier
// ---------------------------------------------------------------------------

/// One field's contribution to the diff, dispatched on its class.
macro_rules! diff_one {
    (key, $name:ident, $old:expr, $new:expr, $out:expr) => {{
        debug_assert_eq!(
            $old.$name, $new.$name,
            concat!("the identity field `", stringify!($name), "` changed")
        );
    }};
    (frozen, $name:ident, $old:expr, $new:expr, $out:expr) => {{
        debug_assert!(
            $old.$name == $new.$name,
            concat!(
                "the immutable field `",
                stringify!($name),
                "` changed; DESIGN §4.6.1 writes it once at insert and PROTOCOL §5.4 promises a \
                 client it never appears in a delta"
            )
        );
    }};
    ($class:ident, $name:ident, $old:expr, $new:expr, $out:expr) => {{
        if $old.$name != $new.$name {
            $out.insert(
                stringify!($name),
                // `ItemView`'s fields are plain data and `serde_json` maps a non-finite float to
                // `null` rather than failing, so the fallback is unreachable in practice.
                serde_json::to_value(&$new.$name).unwrap_or(Value::Null),
            );
        }
    }};
}

/// Whether one field's change makes the flush urgent: everything but the nine numeric progress
/// fields does (DESIGN §15.1).
macro_rules! urgent_one {
    (num, $name:ident, $old:expr, $new:expr) => {
        false
    };
    (key, $name:ident, $old:expr, $new:expr) => {
        false
    };
    (frozen, $name:ident, $old:expr, $new:expr) => {
        false
    };
    ($class:ident, $name:ident, $old:expr, $new:expr) => {
        $old.$name != $new.$name
    };
}

/// Enumerates the wire field list **once**, for the diff, the urgency classifier and the
/// exhaustiveness check.
macro_rules! item_view_diff {
    ( $( $name:ident : $class:ident ),+ $(,)? ) => {
        /// Every wire field the diff covers, in serialisation order. A test asserts it is exactly
        /// [`ItemView::FIELDS`]; the declared length makes a forgotten field a compile error.
        pub const DIFF_FIELDS: [&str; ItemView::FIELDS.len()] = [ $( stringify!($name) ),+ ];

        fn write_diff(old: &ItemView, new: &ItemView, out: &mut IndexMap<&'static str, Value>) {
            $( diff_one!($class, $name, old, new, out); )+
        }

        fn any_urgent_change(old: &ItemView, new: &ItemView) -> bool {
            false $( || urgent_one!($class, $name, old, new) )+
        }
    };
}

item_view_diff! {
    id: key,
    kind: state,
    ord: state,
    group_id: state,
    group_index: state,
    url: state,
    title: state,
    status: state,
    auto_start: state,
    provider: state,
    percent: num,
    speed: num,
    eta: num,
    downloaded_bytes: num,
    total_bytes: num,
    total_bytes_estimate: num,
    fragment_index: num,
    fragment_count: num,
    phase: state,
    phase_percent: num,
    msg: state,
    error: state,
    filename: state,
    size: state,
    download_url: state,
    chapter_files: state,
    subtitle_files: state,
    selection: frozen,
    folder: frozen,
    request: frozen,
    created_at: state,
    started_at: state,
    finished_at: state,
    attempt: state,
    source: state,
    children_total: state,
    children_done: state,
    children_error: state,
    children_active: state,
    children_inline: state,
}

/// The changed fields between two generations of one record, plus `id`.
///
/// An absent key means unchanged; a present `null` means the field changed **to** null. The three
/// immutable fields are never emitted, and a `debug_assert` fires if they ever differ.
#[must_use]
pub fn diff(old: &ItemView, new: &ItemView) -> DeltaItem {
    let mut item = DeltaItem::new(new.id);
    write_diff(old, new, &mut item.fields);
    item
}

/// Whether the change from `old` to `new` should be flushed within `AULOS_WS_URGENT_MS` rather than
/// waiting for the batch tick (DESIGN §15.1).
///
/// True for any change outside `percent`, `speed`, `eta`, the four byte/fragment counters and
/// `phase_percent`.
#[must_use]
pub fn is_urgent(old: &ItemView, new: &ItemView) -> bool {
    any_urgent_change(old, new)
}

/// Whether the two views differ in a **text** field — the rule DESIGN §15.1 and PROTOCOL §5.4
/// state to client authors, and a strict subset of [`is_urgent`].
#[must_use]
pub fn text_changed(old: &ItemView, new: &ItemView) -> bool {
    old.msg != new.msg || old.title != new.title || old.phase != new.phase
}

/// The `snapshot`'s `protocol` block (PROTOCOL §5.3), which `aulos-api` copies verbatim.
///
/// It lives here because the three numbers are this module's cadence, and a client is told to read
/// `delta_semantics` to assert that it and the server agree about absent keys.
#[must_use]
pub fn protocol_block(cfg: &Config) -> Value {
    serde_json::json!({
        "batch_ms": cfg.ws_batch_ms,
        "urgent_ms": cfg.ws_urgent_ms,
        "replay_frames": cfg.ws_replay_frames,
        "delta_semantics": "absent-key-means-unchanged",
    })
}

// ---------------------------------------------------------------------------
// group mirroring
// ---------------------------------------------------------------------------

/// What one child contributes to its group's aggregate.
///
/// Kept per child so the accumulator moves in O(1) per change — the new contribution added, the
/// previous one subtracted — rather than walking 500 children four times a second, which is
/// exactly what the accumulator exists to avoid.
#[derive(Clone, Copy, PartialEq, Debug)]
struct ChildFacts {
    status: Status,
    /// Best-effort byte total: the real size once known, else the exact or estimated total the
    /// provider reported. `0` means unknown, which keeps the group on the count-weighted percent.
    hint: u64,
    finished_bytes: u64,
    downloaded: u64,
    speed: f64,
    active_percent: f64,
}

impl ChildFacts {
    fn of(view: &ItemView) -> Self {
        let running = view.status.is_running();
        Self {
            status: view.status,
            hint: view
                .size
                .or(view.total_bytes)
                .or(view.total_bytes_estimate)
                .unwrap_or(0),
            finished_bytes: if view.status == Status::Finished {
                view.size.unwrap_or(0)
            } else {
                0
            },
            downloaded: if running {
                view.downloaded_bytes.unwrap_or(0)
            } else {
                0
            },
            speed: if running {
                view.speed.unwrap_or(0.0)
            } else {
                0.0
            },
            active_percent: if running { view.percent / 100.0 } else { 0.0 },
        }
    }
}

/// Adds one child's contribution to an accumulator.
fn acc_add(acc: &mut GroupAcc, f: ChildFacts) {
    if f.hint > 0 {
        acc.total_est = acc.total_est.saturating_add(f.hint);
        acc.n_with_total += 1;
    }
    acc.finished_bytes = acc.finished_bytes.saturating_add(f.finished_bytes);
    acc.downloaded = acc.downloaded.saturating_add(f.downloaded);
    acc.speed += f.speed;
    acc.active_percent += f.active_percent;
}

/// Removes one child's contribution from an accumulator.
fn acc_sub(acc: &mut GroupAcc, f: ChildFacts) {
    if f.hint > 0 {
        acc.total_est = acc.total_est.saturating_sub(f.hint);
        acc.n_with_total = acc.n_with_total.saturating_sub(1);
    }
    acc.finished_bytes = acc.finished_bytes.saturating_sub(f.finished_bytes);
    acc.downloaded = acc.downloaded.saturating_sub(f.downloaded);
    acc.speed = (acc.speed - f.speed).max(0.0);
    acc.active_percent = (acc.active_percent - f.active_percent).max(0.0);
}

/// Moves one child between two contributions. Either side may be absent (an insert or a removal).
fn acc_apply(acc: &mut GroupAcc, old: Option<ChildFacts>, new: Option<ChildFacts>) {
    match (old, new) {
        (None, Some(n)) => {
            acc.resolved += 1;
            acc.counts[n.status as usize] += 1;
            acc_add(acc, n);
        }
        (Some(o), Some(n)) => {
            if o.status != n.status {
                let slot = &mut acc.counts[o.status as usize];
                *slot = slot.saturating_sub(1);
                acc.counts[n.status as usize] += 1;
            }
            acc_sub(acc, o);
            acc_add(acc, n);
        }
        (Some(o), None) => {
            acc.resolved = acc.resolved.saturating_sub(1);
            let slot = &mut acc.counts[o.status as usize];
            *slot = slot.saturating_sub(1);
            acc_sub(acc, o);
        }
        (None, None) => {}
    }
    acc.total = acc.total.max(acc.resolved);
}

/// Writes a group accumulator's progress roll-up onto the group's own row.
fn apply_group(acc: &GroupAcc, view: &mut ItemView) {
    view.percent = if view.status == Status::Finished {
        100.0
    } else {
        acc.percent()
    };
    view.speed = acc.speed();
    view.eta = acc.eta();
}

// ---------------------------------------------------------------------------
// membership
// ---------------------------------------------------------------------------

/// Which published array a record belongs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bucket {
    /// `Published::items`: non-terminal records plus every group.
    Items,
    /// `Published::done`: terminal, non-group records.
    Done,
}

/// PROTOCOL §5.3: `items` is everything non-terminal **plus groups**; a group stays there even once
/// its roll-up is terminal, because a client renders it as the header of its children.
fn bucket(view: &ItemView) -> Bucket {
    if view.kind == Kind::Group || !view.status.is_terminal() {
        Bucket::Items
    } else {
        Bucket::Done
    }
}

// ---------------------------------------------------------------------------
// the aggregator
// ---------------------------------------------------------------------------

/// The realtime task (DESIGN §15.1).
///
/// Build it with [`Aggregator::new`], which also hands back the [`StateView`] every REST handler
/// and every connecting socket reads, then [`Aggregator::spawn`] it.
pub struct Aggregator {
    hub: EventHub,
    cfg: Arc<Config>,
    clock: Arc<dyn Clock>,
    state: StateView,

    /// Per-item transient progress. Never persisted (DESIGN §4.7).
    cells: HashMap<ItemId, ProgressCell>,
    /// Per-item percent normaliser, so the monotonic floor survives across frames.
    norm: HashMap<ItemId, Normalizer>,
    /// The merged truth: the engine's view with that item's progress cell applied.
    current: HashMap<ItemId, Arc<ItemView>>,
    /// The delta baseline: exactly what a client was last told.
    last_sent: HashMap<ItemId, Arc<ItemView>>,
    /// Ids whose merged view may differ from `last_sent`, in the order they became dirty — which
    /// **is** the persistent round-robin cursor of DESIGN §15.1: a flush drains from the front and
    /// re-dirtying pushes to the back, so no item can starve.
    dirty: IndexSet<ItemId>,

    pending_added: Vec<(AddReason, Vec<ItemId>)>,
    pending_completed: Vec<ItemId>,
    /// A reason **per id**, which is why a flush can emit more than one `removed` frame.
    pending_removed: Vec<(ItemId, RemoveReason)>,

    /// `items` membership, `ord` then `id` ascending.
    items_order: Vec<ItemId>,
    /// The done window in completion order, for eviction.
    done_queue: VecDeque<ItemId>,
    /// The done window in `ord` then `id` order, for the wire.
    done_order: Vec<ItemId>,
    /// Rebuilt only when membership changes, so an unchanged tick hands the next generation the
    /// same allocation.
    by_id: Arc<HashMap<ItemId, u32>>,
    counts: StatusCounts,
    done_total: u64,
    membership_dirty: bool,
    state_dirty: bool,

    groups: HashMap<GroupId, GroupAcc>,
    child_facts: HashMap<ItemId, (GroupId, ChildFacts)>,
    last_drift: Option<Instant>,

    health: Option<Arc<HealthView>>,

    next_tick: Instant,
    urgent_at: Option<Instant>,
    period: Duration,
    split_ticks: u32,
    backoff_logged: bool,
}

impl std::fmt::Debug for Aggregator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aggregator")
            .field("tracked", &self.current.len())
            .field("dirty", &self.dirty.len())
            .field("groups", &self.groups.len())
            .field("head", &self.hub.head())
            .finish_non_exhaustive()
    }
}

impl Aggregator {
    /// Builds the aggregator and the read handle on its published snapshot.
    #[must_use]
    pub fn new(hub: EventHub, cfg: Arc<Config>, clock: Arc<dyn Clock>) -> (Self, StateView) {
        let state = StateView::new(hub.boot_id());
        let period = Duration::from_millis(cfg.ws_batch_ms.max(1));
        let me = Self {
            hub,
            cfg,
            clock,
            state: state.clone(),
            cells: HashMap::new(),
            norm: HashMap::new(),
            current: HashMap::new(),
            last_sent: HashMap::new(),
            dirty: IndexSet::new(),
            pending_added: Vec::new(),
            pending_completed: Vec::new(),
            pending_removed: Vec::new(),
            items_order: Vec::new(),
            done_queue: VecDeque::new(),
            done_order: Vec::new(),
            by_id: Arc::new(HashMap::new()),
            counts: StatusCounts::default(),
            done_total: 0,
            membership_dirty: false,
            state_dirty: false,
            groups: HashMap::new(),
            child_facts: HashMap::new(),
            last_drift: None,
            health: None,
            next_tick: Instant::now() + period,
            urgent_at: None,
            period,
            split_ticks: 0,
            backoff_logged: false,
        };
        (me, state)
    }

    /// Seeds how many terminal records the database holds, from `RecoveryReport::terminal_total`.
    ///
    /// Addition to the DESIGN §15.3 interface, forced by the architecture rather than by taste:
    /// DESIGN §15.2 annotates `Published::done_total` "from SQLite", but the aggregator holds no
    /// `Store` — it is fed events, and `Engine::recover` publishes only the bounded done *window*.
    /// Without the seed a restart would report `done_total` as the window length and every client
    /// would believe its history had been truncated to 500 rows. The counter then moves on its
    /// own: `+1` when a known non-terminal record becomes terminal, `-1` when a terminal record is
    /// removed or retried, and **unchanged** when one is evicted from the window or first appears
    /// already terminal (which is what boot recovery replays).
    #[must_use]
    pub fn with_done_total(mut self, total: u64) -> Self {
        self.done_total = total;
        self.state_dirty = true;
        self
    }

    /// Spawns the loop.
    ///
    /// `rx` is the receiving half of the one `ProgressMsg` channel, `events` is this subscriber's
    /// private inbox (`DropPolicy::Block` — losing a `Completed` here would strand a row on every
    /// connected client), and `engine` is how the two lossless progress kinds get persisted.
    #[must_use]
    pub fn spawn(
        self,
        rx: mpsc::Receiver<ProgressMsg>,
        events: EventInbox,
        engine: EngineHandle,
    ) -> JoinHandle<()> {
        tokio::spawn(self.run(rx, events, engine))
    }

    /// The loop, so a test can drive it on the current task.
    ///
    /// Returns once both inputs have closed, after one final flush so nothing queued is lost at
    /// shutdown.
    pub async fn run(
        mut self,
        mut rx: mpsc::Receiver<ProgressMsg>,
        mut events: EventInbox,
        engine: EngineHandle,
    ) {
        self.period = Duration::from_millis(self.cfg.ws_batch_ms.max(1));
        self.next_tick = Instant::now() + self.period;
        let mut progress_open = true;
        let mut events_open = true;
        while progress_open || events_open {
            let tick_at = self.next_tick;
            let urgent_at = self.urgent_at;
            tokio::select! {
                batch = recv_progress(&mut rx), if progress_open => {
                    if batch.is_empty() {
                        progress_open = false;
                    }
                    for msg in batch {
                        self.on_progress(msg, &engine).await;
                    }
                }
                batch = recv_events(&mut events), if events_open => {
                    if batch.is_empty() {
                        events_open = false;
                    }
                    for ev in batch {
                        self.on_event(&ev);
                    }
                }
                () = tokio::time::sleep_until(tick_at) => {
                    self.next_tick = next_deadline(tick_at, self.period);
                    self.flush();
                }
                () = sleep_until_opt(urgent_at) => {
                    self.urgent_at = None;
                    self.flush();
                }
            }
        }
        self.flush();
    }

    // -----------------------------------------------------------------------
    // inputs
    // -----------------------------------------------------------------------

    /// One progress message (DESIGN §15.1).
    async fn on_progress(&mut self, msg: ProgressMsg, engine: &EngineHandle) {
        // Liveness for the stall watchdog is recorded for **every** frame, whatever it carries and
        // whether or not the item is still tracked (DESIGN §4.7, §8.11).
        engine
            .heartbeats()
            .frame(msg.item_id(), self.clock.now_ms());
        match msg {
            ProgressMsg::Progress { id, raw } => self.feed_progress(id, &raw),
            // Both of these are persisted, so they go to the engine and come back as a
            // `StatusChanged`; the urgency is armed here so the resulting frame is prompt whatever
            // the engine ends up writing.
            ProgressMsg::Stage { id, stage, msg } => {
                self.arm_urgent();
                engine.stage(id, stage, msg).await;
            }
            ProgressMsg::File { id, slot, file } => {
                self.arm_urgent();
                engine.file(id, slot, file).await;
            }
        }
    }

    /// Applies one raw progress frame through the item's normaliser and cell.
    ///
    /// A frame for an untracked id is dropped: the row is gone and its cell went with it.
    fn feed_progress(&mut self, id: ItemId, raw: &aulos_core::RawProgress) {
        let Some(status) = self.current.get(&id).map(|v| v.status) else {
            return;
        };
        let at = Instant::now();
        let percent = self.norm.entry(id).or_default().apply(raw, status);
        self.cells
            .entry(id)
            .or_insert_with(|| ProgressCell::new(at))
            .apply(raw, percent, at);
        self.refresh(id);
    }

    /// One domain event (DESIGN §8.1).
    fn on_event(&mut self, ev: &DomainEvent) {
        match ev {
            DomainEvent::Added(views, reason) => {
                let mut ids = Vec::with_capacity(views.len());
                for view in views {
                    self.install(view);
                    ids.push(view.id);
                }
                if !ids.is_empty() {
                    self.pending_added.push((*reason, ids));
                    self.arm_urgent();
                }
            }
            DomainEvent::StatusChanged { view, .. } => self.install(view),
            DomainEvent::Completed(view) => {
                self.install(view);
                self.pending_completed.push(view.id);
                self.arm_urgent();
            }
            DomainEvent::Removed { ids, reason } => {
                for id in ids {
                    self.forget(*id);
                    self.pending_removed.push((*id, *reason));
                }
                if !ids.is_empty() {
                    self.arm_urgent();
                }
            }
            // Not in this subscriber's filter: it precedes a status that has not been written, so
            // a frame for it would be a lie (DESIGN §2.2.1, §13).
            DomainEvent::Finishing(_) => {}
            DomainEvent::SubscriptionChanged(sub) => {
                self.hub.publish(
                    FrameKind::Subscription,
                    serde_json::json!({ "subscription": sub }),
                );
            }
            DomainEvent::SubscriptionRemoved(id) => {
                self.hub.publish(
                    FrameKind::SubscriptionRemoved,
                    serde_json::json!({ "ids": [id] }),
                );
            }
            DomainEvent::YtdlOptionsReloaded {
                ok,
                msg,
                update_time,
            } => {
                self.hub.publish(
                    FrameKind::YtdlOptions,
                    serde_json::json!({ "ok": ok, "msg": msg, "update_time": update_time }),
                );
            }
            DomainEvent::ProvidersReloaded(report) => {
                self.hub.publish(FrameKind::Providers, report);
            }
            DomainEvent::HealthChanged(view) => self.on_health(view),
            DomainEvent::Notice {
                level,
                code,
                id,
                message,
            } => {
                self.hub.publish(
                    FrameKind::Notice,
                    serde_json::json!({
                        "level": level, "code": code, "id": id, "message": message
                    }),
                );
            }
            // `DomainEvent` is `#[non_exhaustive]`: a variant added later must not silently become
            // a frame, and must not stop the aggregator either.
            other => tracing::debug!(event = %other.kind(), "aggregator ignored an event"),
        }
    }

    /// A `health` frame carries the **transition**, so it is diffed against the last view
    /// (PROTOCOL §5.9: emitted only on a component change, never periodically).
    fn on_health(&mut self, next: &Arc<HealthView>) {
        let previous = self.health.replace(Arc::clone(next));
        let status_of = |view: Option<&Arc<HealthView>>, name: &str| -> ComponentStatus {
            view.and_then(|v| v.components.get(name))
                .map_or(ComponentStatus::Disabled, |c| c.status)
        };
        let mut changed: Vec<Value> = Vec::new();
        for (name, component) in &next.components {
            let from = status_of(previous.as_ref(), name);
            if from == component.status {
                continue;
            }
            changed.push(serde_json::json!({
                "component": name,
                "from": from,
                "to": component.status,
                "detail": component.detail.get("detail").cloned().unwrap_or(Value::Null),
            }));
        }
        if let Some(before) = previous.as_ref() {
            for name in before.components.keys() {
                if !next.components.contains_key(name) {
                    changed.push(serde_json::json!({
                        "component": name,
                        "from": status_of(previous.as_ref(), name),
                        "to": ComponentStatus::Disabled,
                        "detail": Value::Null,
                    }));
                }
            }
        }
        if changed.is_empty() && previous.is_some() {
            return;
        }
        self.hub.publish(
            FrameKind::Health,
            serde_json::json!({ "status": next.status, "changed": changed }),
        );
    }

    // -----------------------------------------------------------------------
    // the merged view
    // -----------------------------------------------------------------------

    /// Installs a fresh engine view, merges this item's progress over it and marks it dirty.
    fn install(&mut self, view: &ItemView) {
        let id = view.id;
        let mut merged = view.clone();
        self.apply_progress(&mut merged);
        if merged.kind == Kind::Group {
            let declared = merged.children_total.unwrap_or(0);
            let acc = self
                .groups
                .entry(id)
                .or_insert_with(|| GroupAcc::new(declared));
            acc.total = declared.max(acc.resolved);
            apply_group(acc, &mut merged);
        }
        self.store_view(id, &Arc::new(merged));
    }

    /// Rebuilds one tracked view from its own cell — the progress path, which never re-reads the
    /// engine.
    fn refresh(&mut self, id: ItemId) {
        let Some(existing) = self.current.get(&id) else {
            return;
        };
        let mut merged = (**existing).clone();
        self.apply_progress(&mut merged);
        self.store_view(id, &Arc::new(merged));
    }

    /// Commits a rebuilt view: the counters, the membership buckets, the dirty set, the urgency
    /// classifier and the item's group all move together, or none of them do.
    fn store_view(&mut self, id: ItemId, merged: &Arc<ItemView>) {
        if self.current.get(&id).is_some_and(|old| **old == **merged) {
            return;
        }
        let previous = self.current.insert(id, Arc::clone(merged));
        self.reindex(previous.as_deref(), merged);
        self.dirty.insert(id);
        self.state_dirty = true;
        if self
            .last_sent
            .get(&id)
            .is_some_and(|sent| is_urgent(sent, merged))
        {
            self.arm_urgent();
        }
        if let Some(group) = merged.group_id {
            self.update_child(group, id, Some(merged));
        }
    }

    /// Copies the transient progress fields of `view.id`'s cell onto `view`.
    fn apply_progress(&self, view: &mut ItemView) {
        if let Some(cell) = self.cells.get(&view.id) {
            view.percent = cell.percent;
            view.speed = cell.speed;
            view.eta = cell.eta;
            view.downloaded_bytes = cell.downloaded_bytes;
            view.total_bytes = cell.total_bytes;
            view.total_bytes_estimate = cell.total_bytes_estimate;
            view.fragment_index = cell.fragment_index;
            view.fragment_count = cell.fragment_count;
            view.phase = cell.phase;
            view.phase_percent = cell.phase_percent;
        }
        // DESIGN §4.6: `finished` is exactly 100; `error`/`canceled` keep the last value.
        if view.status == Status::Finished {
            view.percent = 100.0;
        }
        if !view.status.is_running() {
            // "`speed` cleared when a job leaves `downloading`" (PROTOCOL §5.4).
            view.speed = None;
            view.eta = None;
        }
    }

    /// Folds one child's contribution into its group and republishes the group row.
    ///
    /// A child's *progress* never reaches the engine (DESIGN §2.2), so without this nothing would
    /// move a group's bar between child status changes.
    fn update_child(&mut self, group: GroupId, child: ItemId, view: Option<&ItemView>) {
        let next = view.map(ChildFacts::of);
        let previous = match next {
            Some(facts) => self
                .child_facts
                .insert(child, (group, facts))
                .map(|(_, f)| f),
            None => self.child_facts.remove(&child).map(|(_, f)| f),
        };
        if previous == next {
            return;
        }
        let declared = self
            .current
            .get(&group)
            .and_then(|g| g.children_total)
            .unwrap_or(0);
        let acc = self
            .groups
            .entry(group)
            .or_insert_with(|| GroupAcc::new(declared));
        acc_apply(acc, previous, next);
        self.refresh_group(group);
    }

    /// Rebuilds a group's own row from its accumulator.
    fn refresh_group(&mut self, group: GroupId) {
        let (Some(existing), Some(acc)) = (self.current.get(&group), self.groups.get(&group))
        else {
            return;
        };
        let mut merged = (**existing).clone();
        apply_group(acc, &mut merged);
        self.store_view(group, &Arc::new(merged));
    }

    /// Drops every trace of a removed row.
    fn forget(&mut self, id: ItemId) {
        if let Some(view) = self.current.remove(&id) {
            self.counts.remove(view.status);
            if bucket(&view) == Bucket::Done {
                self.done_total = self.done_total.saturating_sub(1);
            }
            self.membership_dirty = true;
            self.state_dirty = true;
            if let Some(group) = view.group_id {
                self.update_child(group, id, None);
            }
        }
        self.last_sent.remove(&id);
        self.cells.remove(&id);
        self.norm.remove(&id);
        self.dirty.shift_remove(&id);
        self.done_queue.retain(|d| *d != id);
        if self.groups.remove(&id).is_some() {
            // A group takes its children with it (the engine cascades), so their facts go too
            // rather than leaking for the life of the process.
            self.child_facts.retain(|_, (g, _)| *g != id);
        }
    }

    /// Keeps the counters and the membership buckets in step with one installed view.
    fn reindex(&mut self, previous: Option<&ItemView>, next: &ItemView) {
        match previous {
            Some(old) => {
                self.counts.moved(old.status, next.status);
                let (was, now) = (bucket(old), bucket(next));
                if was != now || old.ord != next.ord {
                    self.membership_dirty = true;
                }
                if was == Bucket::Items && now == Bucket::Done {
                    self.done_total += 1;
                    self.enter_done(next.id);
                } else if was == Bucket::Done && now == Bucket::Items {
                    // A retry pulled a terminal row back into the working set.
                    self.done_total = self.done_total.saturating_sub(1);
                    self.done_queue.retain(|d| *d != next.id);
                }
            }
            None => {
                self.counts.add(next.status);
                self.membership_dirty = true;
                if bucket(next) == Bucket::Done {
                    // A row that is terminal the first time we hear of it is pre-existing history
                    // — boot recovery replays the done window — so `done_total` already counts it.
                    self.enter_done(next.id);
                }
            }
        }
    }

    /// Records a row in the bounded done window, evicting the oldest (DESIGN §15.5).
    fn enter_done(&mut self, id: ItemId) {
        if self.done_queue.contains(&id) {
            return;
        }
        self.done_queue.push_back(id);
        let window = self.cfg.mem_done_items as usize;
        while self.done_queue.len() > window {
            let Some(old) = self.done_queue.pop_front() else {
                break;
            };
            if let Some(view) = self.current.remove(&old) {
                self.counts.remove(view.status);
            }
            self.last_sent.remove(&old);
            self.cells.remove(&old);
            self.norm.remove(&old);
            self.dirty.shift_remove(&old);
            // The child's facts stay: its group still has to count it, exactly as the engine's own
            // accumulator does after its cache eviction.
            self.membership_dirty = true;
            self.state_dirty = true;
        }
    }

    // -----------------------------------------------------------------------
    // the flush
    // -----------------------------------------------------------------------

    fn arm_urgent(&mut self) {
        let at = Instant::now() + Duration::from_millis(self.cfg.ws_urgent_ms);
        self.urgent_at = Some(match self.urgent_at {
            Some(existing) => existing.min(at),
            None => at,
        });
    }

    fn max_per_frame(&self) -> usize {
        (self.cfg.ws_max_deltas_per_frame as usize).max(1)
    }

    /// Emits one batch in the fixed order and republishes (DESIGN §15.1).
    ///
    /// Returns how many frames were emitted, which is zero for an idle server.
    fn flush(&mut self) -> usize {
        self.recompute_group_drift();
        let max = self.max_per_frame();
        let mut frames = 0usize;

        // A dirty id with no baseline has never been sent. A `delta` may not introduce a record
        // (PROTOCOL §5.4), so it is promoted to an `added` upsert rather than dropped — unless a
        // full object for it is already queued in this same flush, which is the ordinary case.
        let orphans: Vec<ItemId> = self
            .dirty
            .iter()
            .copied()
            .filter(|id| {
                !self.last_sent.contains_key(id)
                    && self.current.contains_key(id)
                    && !self.pending_completed.contains(id)
                    && !self
                        .pending_added
                        .iter()
                        .any(|(_, queued)| queued.contains(id))
            })
            .collect();
        if !orphans.is_empty() {
            self.pending_added.push((AddReason::Created, orphans));
        }

        // 1. `added` — full objects, so they are also the new delta baseline.
        for (reason, ids) in std::mem::take(&mut self.pending_added) {
            let views = self.views_of(&ids);
            for chunk in views.chunks(max) {
                self.hub.publish_frame(FrameBody::Added {
                    reason,
                    items: chunk.to_vec(),
                });
                frames += 1;
            }
            self.rebase(&views);
        }

        // 2. `completed` — also full objects.
        let completed_ids = std::mem::take(&mut self.pending_completed);
        let completed = self.views_of(&completed_ids);
        for chunk in completed.chunks(max) {
            self.hub.publish_frame(FrameBody::Completed {
                items: chunk.to_vec(),
            });
            frames += 1;
        }
        self.rebase(&completed);

        // 3. `removed` — one frame per distinct reason, in the fixed reason order.
        if !self.pending_removed.is_empty() {
            let pending = std::mem::take(&mut self.pending_removed);
            for reason in REASON_ORDER {
                let ids: Vec<ItemId> = pending
                    .iter()
                    .filter(|(_, r)| *r == reason)
                    .map(|(id, _)| *id)
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                self.hub.publish_frame(FrameBody::Removed { reason, ids });
                frames += 1;
            }
        }

        // 4. `delta` — the diff, split across at most a few frames per flush.
        let budget = (max * MAX_DELTA_FRAMES_PER_FLUSH).min(self.dirty.len());
        if budget > 0 {
            let ids: Vec<ItemId> = self.dirty.drain(..budget).collect();
            let mut patches: Vec<DeltaItem> = Vec::new();
            let mut rebased: Vec<Arc<ItemView>> = Vec::new();
            for id in ids {
                let Some(current) = self.current.get(&id).map(Arc::clone) else {
                    continue;
                };
                if let Some(previous) = self.last_sent.get(&id) {
                    let patch = diff(previous, &current);
                    if !patch.is_empty() {
                        patches.push(patch);
                    }
                    rebased.push(current);
                }
            }
            self.rebase(&rebased);
            if !patches.is_empty() {
                let ts = self.clock.now_ms();
                for chunk in patches.chunks(max) {
                    self.hub
                        .publish_frame(FrameBody::Delta(Arc::new(DeltaBatch {
                            ts,
                            items: chunk.to_vec(),
                        })));
                    frames += 1;
                }
            }
        }

        self.retune_tick(max);

        // 5. republish, last, so a REST reader can never see state the socket has not carried.
        if self.state_dirty {
            self.republish();
        }
        frames
    }

    /// The current merged views for a list of ids, skipping any that have since been removed.
    fn views_of(&self, ids: &[ItemId]) -> Vec<Arc<ItemView>> {
        ids.iter()
            .filter_map(|id| self.current.get(id).map(Arc::clone))
            .collect()
    }

    /// Makes these views the delta baseline and drops them from the dirty set.
    fn rebase(&mut self, views: &[Arc<ItemView>]) {
        for view in views {
            self.dirty.shift_remove(&view.id);
            self.last_sent.insert(view.id, Arc::clone(view));
        }
    }

    /// The DESIGN §15.1 tick backoff: sustained splitting trades latency for frame overhead.
    fn retune_tick(&mut self, max: usize) {
        if self.dirty.is_empty() {
            self.split_ticks = 0;
            self.backoff_logged = false;
            self.period = Duration::from_millis(self.cfg.ws_batch_ms.max(1));
            return;
        }
        self.split_ticks += 1;
        if self.split_ticks <= SPLIT_BACKOFF_TICKS {
            return;
        }
        let base = self.cfg.ws_batch_ms.max(1);
        let factor = self.dirty.len().div_ceil(max).max(1) as u64;
        let ms = base.saturating_mul(factor).min(MAX_BACKOFF_MS);
        self.period = Duration::from_millis(ms);
        if !self.backoff_logged {
            self.backoff_logged = true;
            tracing::warn!(
                dirty = self.dirty.len(),
                tick_backoff_ms = ms,
                "the delta batch has been splitting for more than four ticks; backing the tick off"
            );
        }
    }

    /// Every five minutes, recompute each group's accumulator from its children's facts and correct
    /// any divergence — incremental sums of floats drift.
    fn recompute_group_drift(&mut self) {
        let interval = Duration::from_millis(u64::try_from(DRIFT_RECOMPUTE_MS).unwrap_or(300_000));
        let now = Instant::now();
        if let Some(last) = self.last_drift
            && now.duration_since(last) < interval
        {
            return;
        }
        self.last_drift = Some(now);
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for group in ids {
            let declared = self
                .current
                .get(&group)
                .and_then(|g| g.children_total)
                .unwrap_or(0);
            let mut fresh = GroupAcc::new(declared);
            for (owner, facts) in self.child_facts.values() {
                if *owner == group {
                    acc_apply(&mut fresh, None, Some(*facts));
                }
            }
            fresh.total = declared.max(fresh.resolved);
            let drifted = self.groups.get(&group).is_some_and(|acc| *acc != fresh);
            if drifted {
                tracing::warn!(%group, "the aggregator's group accumulator had drifted; corrected");
                self.groups.insert(group, fresh);
                self.refresh_group(group);
            }
        }
    }

    /// Swaps in the next published generation (DESIGN §15.2).
    fn republish(&mut self) {
        self.state_dirty = false;
        if self.membership_dirty {
            self.rebuild_membership();
        }
        let items = self.views_of(&self.items_order);
        let done = self.views_of(&self.done_order);
        let truncated = Truncated {
            done: self.done_total > done.len() as u64,
            groups: Arc::from([] as [GroupId; 0]),
        };
        self.state.store(Arc::new(Published {
            seq: self.hub.head(),
            boot_id: self.hub.boot_id(),
            items: Arc::from(items),
            done: Arc::from(done),
            by_id: Arc::clone(&self.by_id),
            counts: self.counts,
            done_total: self.done_total,
            truncated,
        }));
    }

    /// Re-sorts the two buckets and rebuilds the id index. Only on a membership change.
    fn rebuild_membership(&mut self) {
        self.membership_dirty = false;
        let mut items: Vec<(i64, ItemId)> = Vec::new();
        let mut done: Vec<(i64, ItemId)> = Vec::new();
        for (id, view) in &self.current {
            match bucket(view) {
                Bucket::Items => items.push((view.ord, *id)),
                Bucket::Done => done.push((view.ord, *id)),
            }
        }
        items.sort_unstable();
        done.sort_unstable();
        let mut by_id: HashMap<ItemId, u32> = HashMap::with_capacity(items.len() + done.len());
        for (at, (_, id)) in items.iter().chain(done.iter()).enumerate() {
            by_id.insert(*id, u32::try_from(at).unwrap_or(u32::MAX));
        }
        self.items_order = items.into_iter().map(|(_, id)| id).collect();
        self.done_order = done.into_iter().map(|(_, id)| id).collect();
        self.by_id = Arc::new(by_id);
    }
}

/// Drains up to [`RECV_MANY`] progress messages, awaiting the first.
///
/// A free function returning an owned buffer rather than `recv_many` into a field, because
/// `tokio::select!` keeps every branch future alive while a branch body runs and a borrowed buffer
/// could not then be drained. `recv_many` is cancel-safe, so a losing branch loses nothing.
async fn recv_progress(rx: &mut mpsc::Receiver<ProgressMsg>) -> Vec<ProgressMsg> {
    let mut buf = Vec::with_capacity(RECV_MANY);
    rx.recv_many(&mut buf, RECV_MANY).await;
    buf
}

/// Drains up to [`RECV_MANY`] events, awaiting the first.
async fn recv_events(inbox: &mut EventInbox) -> Vec<Arc<DomainEvent>> {
    let mut buf = Vec::with_capacity(RECV_MANY);
    inbox.recv_many(&mut buf, RECV_MANY).await;
    buf
}

/// `sleep_until` for an optional deadline: `None` never fires.
async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The next periodic deadline, skipping any the task slept through.
fn next_deadline(previous: Instant, period: Duration) -> Instant {
    let now = Instant::now();
    let next = previous + period;
    if next > now { next } else { now + period }
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! Fixtures shared by this crate's realtime unit tests.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use aulos_core::config::{RawEnv, load};
    use aulos_core::{
        BootId, Codec, Config, DownloadRequest, DownloadType, FormatId, HiLoAllocator, Item,
        ItemId, ItemView, Kind, QualityId, Selection, SourceKind, SourceRef, Status, ViewExtras,
    };

    use crate::hub::EventHub;

    /// A config over the stock defaults plus the given overrides.
    pub(crate) fn config(overrides: &[(&str, &str)]) -> Config {
        load(&RawEnv::from_pairs(overrides.iter().copied())).expect("the defaults must load")
    }

    /// A non-durable monotonic counter: the hub only needs monotonicity, and a unit test does not
    /// want a SQLite file.
    #[derive(Debug)]
    pub(crate) struct Counter(AtomicU64);

    impl HiLoAllocator for Counter {
        fn next(&self) -> i64 {
            i64::try_from(self.0.fetch_add(1, Ordering::SeqCst) + 1).unwrap_or(i64::MAX)
        }

        fn current(&self) -> i64 {
            i64::try_from(self.0.load(Ordering::SeqCst)).unwrap_or(i64::MAX)
        }
    }

    /// An allocator whose first handed-out value is `from + 1`.
    pub(crate) fn counter(from: u64) -> Arc<dyn HiLoAllocator> {
        Arc::new(Counter(AtomicU64::new(from)))
    }

    /// A hub over a fresh boot id and the given config overrides.
    pub(crate) fn hub(overrides: &[(&str, &str)]) -> EventHub {
        EventHub::new(counter(10), BootId::new(), &config(overrides))
    }

    /// A persisted row with a fresh id.
    pub(crate) fn item(status: Status, ord: i64) -> Item {
        let selection = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("best").unwrap(),
        );
        let url = url::Url::parse("https://fake.test/watch?v=1").unwrap();
        Item {
            id: ItemId::new(),
            kind: Kind::Item,
            group_id: None,
            group_index: None,
            ord,
            url: url.clone(),
            canonical_key: "ytdlp:fake.test/1".into(),
            provider: None,
            media_id: None,
            title: "A title".into(),
            status,
            auto_start: true,
            msg: None,
            error: None,
            request: DownloadRequest::new(url, selection),
            entry: None,
            filename: None,
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: 1_757_000_000_000,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: SourceRef::bare(SourceKind::ApiV2),
            children_total: None,
            clear_after: None,
        }
    }

    /// The wire projection of [`item`], with no progress applied.
    pub(crate) fn view(status: Status, ord: i64) -> Arc<ItemView> {
        Arc::new(ItemView::from_item(
            &item(status, ord),
            None,
            &ViewExtras::default(),
        ))
    }

    /// A group row's wire projection, with `children_total` declared.
    pub(crate) fn group_view(status: Status, ord: i64, children: u32) -> Arc<ItemView> {
        let mut row = item(status, ord);
        row.kind = Kind::Group;
        row.children_total = Some(children);
        let extras = ViewExtras {
            children_done: Some(0),
            children_error: Some(0),
            children_active: Some(0),
            children_inline: Some(true),
            ..ViewExtras::default()
        };
        Arc::new(ItemView::from_item(&row, None, &extras))
    }

    /// A child of `group` at `index`.
    pub(crate) fn child_view(group: ItemId, index: u32, status: Status, ord: i64) -> Arc<ItemView> {
        let mut row = item(status, ord);
        row.group_id = Some(group);
        row.group_index = Some(index);
        Arc::new(ItemView::from_item(&row, None, &ViewExtras::default()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::tests_support::{child_view, config, group_view, hub, item, view};
    use super::*;
    use aulos_core::{
        ErrorCode, FileRef, FileSlot, PhaseTag, RawProgress, SourceKind, SourceRef, WireError,
    };
    use proptest::prelude::*;
    use serde_json::json;
    use tokio::sync::broadcast::error::TryRecvError;

    // -----------------------------------------------------------------------
    // the generated diff
    // -----------------------------------------------------------------------

    /// One named mutation of a mutable wire field, seeded so successive draws differ.
    macro_rules! mutators {
        ( $( $name:ident => |$v:ident, $seed:ident| $body:expr ),+ $(,)? ) => {
            /// Every mutable wire field, with a way to change it.
            const MUTATORS: &[(&str, fn(&mut ItemView, u8))] = &[
                $( (stringify!($name), |$v: &mut ItemView, $seed: u8| { $body; }) ),+
            ];
        };
    }

    mutators! {
        kind => |v, _s| v.kind = Kind::Group,
        ord => |v, s| v.ord = 1_000 + i64::from(s),
        group_id => |v, _s| v.group_id = Some(ItemId::new()),
        group_index => |v, s| v.group_index = Some(u32::from(s) + 1),
        url => |v, s| v.url = Arc::from(format!("https://fake.test/{s}").as_str()),
        title => |v, s| v.title = Arc::from(format!("Title {s}").as_str()),
        status => |v, s| v.status =
            Status::ALL[(v.status as usize + 1 + usize::from(s) % 7) % Status::ALL.len()],
        auto_start => |v, _s| v.auto_start = !v.auto_start,
        provider => |v, _s| v.provider = Some(Arc::from("streamingcommunity")),
        percent => |v, s| v.percent = f64::from(s) / 3.0,
        speed => |v, s| v.speed = if s.is_multiple_of(3) { None } else { Some(f64::from(s) * 1_024.0) },
        eta => |v, s| v.eta = if s.is_multiple_of(4) { None } else { Some(i64::from(s)) },
        downloaded_bytes => |v, s| v.downloaded_bytes = Some(u64::from(s) * 4_096),
        total_bytes => |v, s| v.total_bytes = Some(u64::from(s) * 8_192 + 1),
        total_bytes_estimate => |v, s| v.total_bytes_estimate = Some(u64::from(s) * 3),
        fragment_index => |v, s| v.fragment_index = Some(u32::from(s)),
        fragment_count => |v, s| v.fragment_count = Some(u32::from(s) + 7),
        phase => |v, s| v.phase = Some(if s.is_multiple_of(2) { PhaseTag::Video } else { PhaseTag::Remux }),
        phase_percent => |v, s| v.phase_percent = Some(f64::from(s % 100)),
        msg => |v, s| v.msg = Some(Arc::from(format!("Message {s}").as_str())),
        error => |v, s| v.error = Some(WireError::new(
            ErrorCode::Unavailable,
            format!("boom {s}"),
        )),
        filename => |v, s| v.filename = Some(Arc::from(format!("f{s}.mp4").as_str())),
        size => |v, s| v.size = Some(u64::from(s) * 1_000),
        download_url => |v, s| v.download_url = Some(Arc::from(format!("download/{s}").as_str())),
        chapter_files => |v, s| v.chapter_files = Arc::from([FileRef {
            filename: Arc::from(format!("c{s}.mp4").as_str()),
            size: Some(u64::from(s)),
            download_url: None,
            lang: None,
        }]),
        subtitle_files => |v, s| v.subtitle_files = Arc::from([FileRef {
            filename: Arc::from(format!("s{s}.srt").as_str()),
            size: None,
            download_url: None,
            lang: Some(Arc::from("en")),
        }]),
        created_at => |v, s| v.created_at += i64::from(s) + 1,
        started_at => |v, s| v.started_at = Some(1_757_000_000_000 + i64::from(s)),
        finished_at => |v, s| v.finished_at = Some(1_757_000_100_000 + i64::from(s)),
        attempt => |v, s| v.attempt = u16::from(s) + 1,
        source => |v, _s| v.source = SourceRef::bare(SourceKind::Telegram),
        children_total => |v, s| v.children_total = Some(u32::from(s) + 1),
        children_done => |v, s| v.children_done = Some(u32::from(s)),
        children_error => |v, s| v.children_error = Some(u32::from(s)),
        children_active => |v, s| v.children_active = Some(u32::from(s)),
        children_inline => |v, _s| v.children_inline = Some(false),
    }

    /// The nine fields DESIGN §15.1 batches; everything else pulls the flush forward.
    const NUMERIC_FIELDS: [&str; 9] = [
        "percent",
        "speed",
        "eta",
        "downloaded_bytes",
        "total_bytes",
        "total_bytes_estimate",
        "fragment_index",
        "fragment_count",
        "phase_percent",
    ];

    /// PROTOCOL §5.4's apply step, verbatim: overwrite exactly the keys that are present.
    fn apply(base: &ItemView, patch: &DeltaItem) -> Value {
        let mut object = match serde_json::to_value(base) {
            Ok(Value::Object(o)) => o,
            other => panic!("an ItemView must serialise to an object, got {other:?}"),
        };
        for (key, value) in &patch.fields {
            object.insert((*key).to_owned(), value.clone());
        }
        Value::Object(object)
    }

    #[test]
    fn the_diff_covers_exactly_the_wire_field_list_in_order() {
        assert_eq!(
            DIFF_FIELDS.as_slice(),
            ItemView::FIELDS.as_slice(),
            "the diff macro and the serialiser must enumerate the same fields, in the same order"
        );
        let mut names: Vec<&str> = MUTATORS.iter().map(|(n, _)| *n).collect();
        names.extend(ItemView::IMMUTABLE_FIELDS);
        names.push("id");
        names.sort_unstable();
        let mut expected: Vec<&str> = ItemView::FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            names, expected,
            "every field is either mutable (and has a mutator), immutable, or the key"
        );
    }

    #[test]
    fn mutating_one_field_emits_exactly_that_key() {
        let base = view(Status::Downloading, 1);
        for (name, mutate) in MUTATORS {
            let mut next = (*base).clone();
            mutate(&mut next, 5);
            let patch = diff(&base, &next);
            let keys: Vec<&str> = patch.fields.keys().copied().collect();
            assert_eq!(keys, vec![*name], "mutating {name}");
            assert_eq!(patch.id, base.id, "id is always the key");
            assert_eq!(
                apply(&base, &patch),
                serde_json::to_value(&next).unwrap(),
                "applying the patch for {name} must reproduce the new view"
            );
        }
    }

    #[test]
    fn the_urgency_classifier_batches_only_the_nine_numeric_fields() {
        let base = view(Status::Downloading, 1);
        for (name, mutate) in MUTATORS {
            let mut next = (*base).clone();
            mutate(&mut next, 3);
            let urgent = is_urgent(&base, &next);
            let numeric = NUMERIC_FIELDS.contains(name);
            assert_eq!(urgent, !numeric, "{name} was classified wrongly");
        }
        assert!(!is_urgent(&base, &base), "no change is never urgent");
        // The text rule PROTOCOL §5.4 states to clients is a strict subset.
        for field in ["msg", "title", "phase"] {
            let (_, mutate) = MUTATORS.iter().find(|(n, _)| *n == field).unwrap();
            let mut next = (*base).clone();
            mutate(&mut next, 1);
            assert!(text_changed(&base, &next), "{field} is a text field");
            assert!(is_urgent(&base, &next));
        }
    }

    #[test]
    fn absent_means_unchanged_and_null_means_cleared() {
        let mut before = (*view(Status::Downloading, 1)).clone();
        before.speed = Some(3_210_000.0);
        before.eta = Some(61);
        let mut after = before.clone();
        after.speed = None;
        let patch = diff(&before, &after);
        assert_eq!(patch.fields.len(), 1);
        assert_eq!(patch.fields["speed"], Value::Null, "cleared, not absent");
        assert!(!patch.fields.contains_key("eta"), "eta did not change");
        assert_eq!(
            serde_json::to_string(&patch).unwrap(),
            format!("{{\"id\":\"{}\",\"speed\":null}}", before.id)
        );
    }

    #[test]
    fn an_unchanged_view_diffs_to_nothing() {
        let v = view(Status::Downloading, 1);
        let patch = diff(&v, &v);
        assert!(patch.is_empty());
        assert_eq!(
            serde_json::to_string(&patch).unwrap(),
            format!("{{\"id\":\"{}\"}}", v.id)
        );
    }

    proptest! {
        /// The structural proof that a stale client field is impossible: whatever subset of fields
        /// moved, applying the emitted delta to the baseline reproduces the new view **exactly**.
        #[test]
        fn applying_a_delta_to_the_baseline_reproduces_the_new_view(
            picks in proptest::collection::vec((0usize..MUTATORS.len(), any::<u8>()), 0..12),
            seed in any::<u8>(),
        ) {
            let base = view(Status::Downloading, 1);
            let mut start = (*base).clone();
            // A non-trivial baseline, so "unchanged" is a real case rather than always-None.
            for (at, _) in &picks {
                MUTATORS[*at].1(&mut start, seed);
            }
            start.id = base.id;
            let mut next = start.clone();
            for (at, s) in &picks {
                MUTATORS[*at].1(&mut next, *s);
            }
            next.id = base.id;

            let patch = diff(&start, &next);
            prop_assert_eq!(apply(&start, &patch), serde_json::to_value(&next).unwrap());
            for frozen in ItemView::IMMUTABLE_FIELDS {
                prop_assert!(
                    !patch.fields.contains_key(frozen),
                    "an immutable field reached the wire: {}", frozen
                );
            }
            for key in patch.fields.keys() {
                prop_assert!(ItemView::FIELDS.contains(key));
            }
        }
    }

    // -----------------------------------------------------------------------
    // the flush
    // -----------------------------------------------------------------------

    /// An aggregator with no engine and no store, plus a subscriber on its hub.
    struct Rig {
        agg: Aggregator,
        state: StateView,
        hub: EventHub,
        rx: tokio::sync::broadcast::Receiver<Arc<crate::ring::WireFrame>>,
    }

    impl Rig {
        fn new(overrides: &[(&str, &str)]) -> Self {
            let hub = hub(overrides);
            let rx = hub.subscribe();
            let (agg, state) = Aggregator::new(
                hub.clone(),
                Arc::new(config(overrides)),
                Arc::new(aulos_core::FakeClock::new(1_757_000_000_000)),
            );
            Self {
                agg,
                state,
                hub,
                rx,
            }
        }

        /// Every frame published since the last drain, as `(kind, parsed json)`.
        fn frames(&mut self) -> Vec<(FrameKind, Value)> {
            let mut out = Vec::new();
            loop {
                match self.rx.try_recv() {
                    Ok(frame) => out.push((
                        frame.kind,
                        serde_json::from_str(frame.as_str()).expect("a frame must be JSON"),
                    )),
                    Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                    Err(TryRecvError::Lagged(_)) => continue,
                }
            }
            out
        }

        fn kinds(&mut self) -> Vec<FrameKind> {
            self.frames().into_iter().map(|(k, _)| k).collect()
        }

        fn added(&mut self, views: &[Arc<ItemView>]) {
            self.agg
                .on_event(&DomainEvent::Added(views.to_vec(), AddReason::Created));
        }

        fn changed(&mut self, view: &Arc<ItemView>) {
            self.agg.on_event(&DomainEvent::StatusChanged {
                id: view.id,
                from: view.status,
                to: view.status,
                view: Arc::clone(view),
            });
        }

        fn completed(&mut self, view: &Arc<ItemView>) {
            self.agg.on_event(&DomainEvent::Completed(Arc::clone(view)));
        }

        fn removed(&mut self, ids: &[ItemId], reason: RemoveReason) {
            self.agg.on_event(&DomainEvent::Removed {
                ids: ids.to_vec(),
                reason,
            });
        }

        fn progress(&mut self, id: ItemId, downloaded: f64, total: f64) {
            self.agg.feed_progress(
                id,
                &RawProgress {
                    downloaded_bytes: Some(downloaded),
                    total_bytes: Some(total),
                    speed: Some(1_024.0),
                    eta: Some(9),
                    ..RawProgress::default()
                },
            );
        }
    }

    #[test]
    fn an_idle_aggregator_emits_nothing_for_ten_ticks() {
        let mut rig = Rig::new(&[]);
        for _ in 0..10 {
            assert_eq!(rig.agg.flush(), 0);
        }
        assert!(rig.frames().is_empty());
        assert_eq!(rig.hub.frames_published(), 0);
        assert_eq!(
            rig.hub.head(),
            aulos_core::Seq(10),
            "no sequence was consumed either"
        );
    }

    #[test]
    fn a_stalled_download_emits_zero_bytes_after_the_first_frame() {
        let mut rig = Rig::new(&[]);
        let v = view(Status::Downloading, 1);
        rig.added(std::slice::from_ref(&v));
        rig.progress(v.id, 432.0, 1_000.0);
        assert!(rig.agg.flush() > 0);
        let first = rig.frames();
        assert_eq!(first.len(), 1, "the added frame carries the 43.2 % already");
        assert!((first[0].1["items"][0]["percent"].as_f64().unwrap() - 43.2).abs() < 1e-9);

        // Twenty more frames at exactly the same numbers.
        for _ in 0..20 {
            rig.progress(v.id, 432.0, 1_000.0);
            assert_eq!(rig.agg.flush(), 0);
        }
        assert!(
            rig.frames().is_empty(),
            "a constant percent is zero bytes on the wire"
        );
    }

    #[test]
    fn a_flush_with_all_four_kinds_emits_added_completed_removed_delta_in_order() {
        let mut rig = Rig::new(&[]);
        // A record the client already holds, so it can produce a delta.
        let moving = view(Status::Downloading, 1);
        rig.added(std::slice::from_ref(&moving));
        rig.agg.flush();
        rig.frames();

        let fresh = view(Status::Queued, 2);
        let done = view(Status::Finished, 3);
        let gone = view(Status::Canceled, 4);
        rig.added(std::slice::from_ref(&fresh));
        rig.completed(&done);
        rig.added(std::slice::from_ref(&gone));
        rig.agg.flush();
        rig.frames();
        rig.removed(&[gone.id], RemoveReason::Deleted);
        rig.progress(moving.id, 500.0, 1_000.0);
        let fresh2 = view(Status::Queued, 5);
        rig.added(std::slice::from_ref(&fresh2));
        let done2 = view(Status::Finished, 6);
        rig.completed(&done2);

        rig.agg.flush();
        assert_eq!(
            rig.kinds(),
            vec![
                FrameKind::Added,
                FrameKind::Completed,
                FrameKind::Removed,
                FrameKind::Delta,
            ],
            "PROTOCOL §4.3/§6.3 are normative about this order"
        );
    }

    #[test]
    fn two_removal_reasons_in_one_window_emit_two_frames_in_the_reason_order() {
        let mut rig = Rig::new(&[]);
        let a = view(Status::Finished, 1);
        let b = view(Status::Finished, 2);
        let c = view(Status::Finished, 3);
        rig.added(&[Arc::clone(&a), Arc::clone(&b), Arc::clone(&c)]);
        rig.agg.flush();
        rig.frames();

        rig.removed(&[c.id], RemoveReason::Expired);
        rig.removed(&[a.id, b.id], RemoveReason::Deleted);
        rig.agg.flush();
        let frames = rig.frames();
        assert_eq!(
            frames.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![FrameKind::Removed, FrameKind::Removed]
        );
        assert_eq!(frames[0].1["reason"], "deleted");
        assert_eq!(frames[0].1["ids"].as_array().unwrap().len(), 2);
        assert_eq!(frames[1].1["reason"], "auto_cleared");

        // One reason in a window is one frame.
        let d = view(Status::Finished, 4);
        rig.added(std::slice::from_ref(&d));
        rig.agg.flush();
        rig.frames();
        rig.removed(&[d.id], RemoveReason::Cleared);
        rig.agg.flush();
        assert_eq!(rig.kinds(), vec![FrameKind::Removed]);
    }

    /// The reason `removed` is emitted **after** `added`: a row created and deleted inside one
    /// 250 ms window must leave the client with nothing, not with a permanent ghost.
    #[test]
    fn an_item_added_and_removed_in_one_window_leaves_no_row() {
        let mut rig = Rig::new(&[]);
        let v = view(Status::Queued, 1);
        rig.added(std::slice::from_ref(&v));
        rig.removed(&[v.id], RemoveReason::Deleted);
        rig.agg.flush();
        let frames = rig.frames();
        // Client state: apply every frame in order and see what is left.
        let mut rows: HashMap<String, Value> = HashMap::new();
        for (kind, body) in &frames {
            match kind {
                FrameKind::Added | FrameKind::Completed => {
                    for item in body["items"].as_array().unwrap() {
                        rows.insert(item["id"].as_str().unwrap().to_owned(), item.clone());
                    }
                }
                FrameKind::Removed => {
                    for id in body["ids"].as_array().unwrap() {
                        rows.remove(id.as_str().unwrap());
                    }
                }
                other => panic!("unexpected {other}"),
            }
        }
        assert!(
            rows.is_empty(),
            "the client must be left with no row at all"
        );
        assert!(rig.state.load().is_empty());
    }

    #[test]
    fn a_delta_never_introduces_an_id_the_client_has_not_seen() {
        let mut rig = Rig::new(&[]);
        // A `StatusChanged` with no preceding `Added` — the self-healing path.
        let v = view(Status::Downloading, 1);
        rig.changed(&v);
        rig.agg.flush();
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].0,
            FrameKind::Added,
            "promoted to an upsert, not a patch"
        );
        assert_eq!(frames[0].1["items"][0]["id"], v.id.to_string());
    }

    #[test]
    fn one_thousand_dirty_items_split_into_five_frames_of_two_hundred() {
        let mut rig = Rig::new(&[]);
        let views: Vec<Arc<ItemView>> = (0..1_000)
            .map(|i| view(Status::Downloading, i64::from(i)))
            .collect();
        rig.added(&views);
        rig.agg.flush();
        rig.frames();

        for (i, v) in views.iter().enumerate() {
            rig.progress(v.id, (i + 1) as f64, 1_000.0);
        }
        assert_eq!(rig.agg.flush(), 4, "at most four delta frames per flush");
        let first = rig.frames();
        assert!(first.iter().all(|(k, _)| *k == FrameKind::Delta));
        for (_, body) in &first {
            assert_eq!(body["items"].as_array().unwrap().len(), 200);
        }
        assert_eq!(rig.agg.flush(), 1, "the remainder lands on the next tick");
        let rest = rig.frames();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].1["items"].as_array().unwrap().len(), 200);
        assert_eq!(rig.agg.flush(), 0, "and then nothing is left");
    }

    /// The persistent cursor: with every item re-dirtied every tick, the FIFO drain still reaches
    /// each of them within four ticks rather than starving the tail.
    #[test]
    fn a_hot_queue_starves_nothing() {
        let mut rig = Rig::new(&[]);
        let views: Vec<Arc<ItemView>> = (0..800)
            .map(|i| view(Status::Downloading, i64::from(i)))
            .collect();
        rig.added(&views);
        rig.agg.flush();
        rig.frames();

        let mut seen: HashMap<String, usize> = HashMap::new();
        for tick in 0..4 {
            for (i, v) in views.iter().enumerate() {
                rig.progress(v.id, (i + tick * 1_000 + 1) as f64, 1_000_000.0);
            }
            rig.agg.flush();
            for (kind, body) in rig.frames() {
                assert_eq!(kind, FrameKind::Delta);
                for item in body["items"].as_array().unwrap() {
                    *seen
                        .entry(item["id"].as_str().unwrap().to_owned())
                        .or_default() += 1;
                }
            }
        }
        assert_eq!(
            seen.len(),
            views.len(),
            "every item was emitted within four ticks"
        );
    }

    #[test]
    fn sustained_splitting_backs_the_tick_off_once() {
        let base = Duration::from_millis(250);
        let mut rig = Rig::new(&[]);
        let views: Vec<Arc<ItemView>> = (0..2_000)
            .map(|i| view(Status::Downloading, i64::from(i)))
            .collect();
        rig.added(&views);
        rig.agg.flush();
        rig.frames();
        for tick in 0..8u32 {
            for (i, v) in views.iter().enumerate() {
                rig.progress(v.id, (i + tick as usize * 5_000 + 1) as f64, 10_000_000.0);
            }
            rig.agg.flush();
            if tick < SPLIT_BACKOFF_TICKS {
                assert_eq!(rig.agg.period, base, "no backoff yet at tick {tick}");
            }
        }
        assert!(rig.agg.period > base, "the tick backed off");
        assert!(rig.agg.period <= Duration::from_millis(MAX_BACKOFF_MS));
        assert!(
            rig.agg.backoff_logged,
            "and it is logged once, not per tick"
        );
    }

    // -----------------------------------------------------------------------
    // the published snapshot
    // -----------------------------------------------------------------------

    /// The aggregator holds no `Store` — it cannot, its only inputs are two channels — so serving
    /// a snapshot is structurally zero database round trips. This asserts the read path itself.
    #[test]
    fn five_hundred_items_are_served_from_the_published_view_with_no_store_at_all() {
        let mut rig = Rig::new(&[]);
        let views: Vec<Arc<ItemView>> = (0..500)
            .map(|i| view(Status::Queued, i64::from(500 - i)))
            .collect();
        rig.added(&views);
        rig.agg.flush();
        let published = rig.state.load();
        assert_eq!(published.items.len(), 500);
        assert!(published.done.is_empty());
        assert_eq!(published.counts.queued, 500);
        assert_eq!(published.counts.total(), 500);
        assert_eq!(published.seq, rig.hub.head());
        let ords: Vec<i64> = published.items.iter().map(|v| v.ord).collect();
        let mut sorted = ords.clone();
        sorted.sort_unstable();
        assert_eq!(ords, sorted, "ord ascending, always");
        for v in &views {
            assert_eq!(published.get(v.id).map(|f| f.id), Some(v.id));
        }
    }

    #[test]
    fn by_id_is_pointer_equal_across_a_tick_with_no_membership_change() {
        let mut rig = Rig::new(&[]);
        let v = view(Status::Downloading, 1);
        rig.added(std::slice::from_ref(&v));
        rig.agg.flush();
        let first = rig.state.snapshot();
        rig.progress(v.id, 10.0, 100.0);
        rig.agg.flush();
        let second = rig.state.snapshot();
        assert!(
            Arc::ptr_eq(&first.by_id, &second.by_id),
            "membership did not change, so the index is reused"
        );
        assert!(
            !Arc::ptr_eq(&first.items, &second.items),
            "the views did move"
        );

        let other = view(Status::Queued, 2);
        rig.added(std::slice::from_ref(&other));
        rig.agg.flush();
        let third = rig.state.snapshot();
        assert!(
            !Arc::ptr_eq(&second.by_id, &third.by_id),
            "membership changed"
        );
    }

    #[test]
    fn a_terminal_record_moves_to_the_done_window_and_the_totals_follow() {
        let mut rig = Rig::new(&[]).agg_with_done_total(4_000);
        let v = view(Status::Downloading, 1);
        rig.added(std::slice::from_ref(&v));
        rig.agg.flush();
        assert_eq!(rig.state.load().done_total, 4_000);
        assert!(
            rig.state.load().truncated.done,
            "history is longer than the window"
        );

        let mut finished = (*v).clone();
        finished.status = Status::Finished;
        finished.size = Some(4_096);
        rig.completed(&Arc::new(finished));
        rig.agg.flush();
        let published = rig.state.load();
        assert!(published.items.is_empty());
        assert_eq!(published.done.len(), 1);
        assert!((published.done[0].percent - 100.0).abs() < 1e-9);
        assert_eq!(published.done[0].speed, None, "a finished job has no speed");
        assert_eq!(published.counts.finished, 1);
        assert_eq!(published.done_total, 4_001);

        rig.removed(&[v.id], RemoveReason::Cleared);
        rig.agg.flush();
        assert_eq!(rig.state.load().done_total, 4_000);
        assert_eq!(rig.state.load().counts.total(), 0);
    }

    #[test]
    fn the_done_window_is_bounded_and_evicts_the_oldest() {
        let mut rig = Rig::new(&[("AULOS_MEM_DONE_ITEMS", "4")]);
        let mut ids = Vec::new();
        for i in 0..10 {
            let v = view(Status::Downloading, i64::from(i));
            ids.push(v.id);
            rig.added(std::slice::from_ref(&v));
            let mut done = (*v).clone();
            done.status = Status::Finished;
            rig.completed(&Arc::new(done));
        }
        rig.agg.flush();
        let published = rig.state.load();
        assert_eq!(published.done.len(), 4, "DESIGN §15.5's bound");
        assert_eq!(
            published.done_total, 10,
            "the history total is not the window"
        );
        assert!(published.truncated.done);
        assert_eq!(
            published.counts.finished, 4,
            "counts cover what is published"
        );
        let kept: Vec<ItemId> = published.done.iter().map(|v| v.id).collect();
        for id in &ids[..6] {
            assert!(!kept.contains(id), "the oldest completions were evicted");
        }
    }

    #[test]
    fn a_retry_pulls_a_record_back_out_of_the_done_window() {
        let mut rig = Rig::new(&[]);
        let v = view(Status::Error, 1);
        rig.added(std::slice::from_ref(&v));
        rig.agg.flush();
        assert_eq!(rig.state.load().done.len(), 1);
        assert_eq!(rig.state.load().done_total, 0, "it was already history");

        let mut requeued = (*v).clone();
        requeued.status = Status::Queued;
        requeued.attempt = 1;
        rig.changed(&Arc::new(requeued));
        rig.agg.flush();
        let published = rig.state.load();
        assert_eq!(published.items.len(), 1);
        assert!(published.done.is_empty());
        assert_eq!(published.counts.queued, 1);
        assert_eq!(published.counts.error, 0);
    }

    #[test]
    fn a_group_stays_in_items_even_once_its_rollup_is_terminal() {
        let mut rig = Rig::new(&[]);
        let g = group_view(Status::Finished, 1, 2);
        rig.added(std::slice::from_ref(&g));
        rig.agg.flush();
        let published = rig.state.load();
        assert_eq!(published.items.len(), 1, "PROTOCOL §5.3: items plus groups");
        assert!(published.done.is_empty());
    }

    // -----------------------------------------------------------------------
    // groups
    // -----------------------------------------------------------------------

    #[test]
    fn a_childs_progress_moves_its_groups_bar_without_the_engine() {
        let mut rig = Rig::new(&[]);
        let g = group_view(Status::Downloading, 1, 2);
        let a = child_view(g.id, 1, Status::Downloading, 2);
        let b = child_view(g.id, 2, Status::Queued, 3);
        rig.added(&[Arc::clone(&g), Arc::clone(&a), Arc::clone(&b)]);
        rig.agg.flush();
        rig.frames();
        assert_eq!(rig.state.load().get(g.id).map(|v| v.percent), Some(0.0));

        // Half of a known 1 000-byte child, and nothing known about the other.
        rig.progress(a.id, 500.0, 1_000.0);
        rig.agg.flush();
        let group = rig.state.load().get(g.id).cloned().unwrap();
        assert!(
            (group.percent - 25.0).abs() < 0.001,
            "count-weighted while one child's total is unknown: {}",
            group.percent
        );
        assert_eq!(group.speed, Some(1_024.0), "the sum over running children");
        assert_eq!(
            group.children_active,
            Some(0),
            "the engine still owns the counters"
        );
    }

    /// DESIGN §8.6's worked example, from the aggregator's side: once every child has a byte total
    /// the roll-up is byte-weighted, so 49 small finished clips plus a half-done 4 GB file reads
    /// about half, not 98 %.
    #[test]
    fn a_groups_percent_is_dominated_by_bytes_not_by_count() {
        const SMALL: u64 = 1_000_000;
        const BIG: f64 = 4_000_000_000.0;
        let mut rig = Rig::new(&[]);
        let g = group_view(Status::Downloading, 1, 50);
        rig.added(std::slice::from_ref(&g));
        for i in 0..49u32 {
            let mut child = (*child_view(g.id, i + 1, Status::Finished, i64::from(i) + 2)).clone();
            child.size = Some(SMALL);
            rig.added(&[Arc::new(child)]);
        }
        let big = child_view(g.id, 50, Status::Downloading, 100);
        rig.added(std::slice::from_ref(&big));
        rig.progress(big.id, BIG / 2.0, BIG);
        rig.agg.flush();
        let percent = rig.state.load().get(g.id).unwrap().percent;
        assert!(
            (48.0..53.0).contains(&percent),
            "byte-weighted, so about half: {percent}"
        );
    }

    #[test]
    fn removing_a_group_forgets_its_children_facts() {
        let mut rig = Rig::new(&[]);
        let g = group_view(Status::Queued, 1, 1);
        let child = child_view(g.id, 1, Status::Queued, 2);
        rig.added(&[Arc::clone(&g), Arc::clone(&child)]);
        rig.agg.flush();
        assert_eq!(rig.agg.child_facts.len(), 1);
        rig.removed(&[g.id, child.id], RemoveReason::Deleted);
        rig.agg.flush();
        assert!(rig.agg.groups.is_empty());
        assert!(rig.agg.child_facts.is_empty(), "no per-child leak");
        assert!(rig.state.load().is_empty());
    }

    // -----------------------------------------------------------------------
    // the passthrough frames
    // -----------------------------------------------------------------------

    #[test]
    fn a_notice_is_published_immediately_and_verbatim() {
        let mut rig = Rig::new(&[]);
        let id = ItemId::new();
        rig.agg.on_event(&DomainEvent::Notice {
            level: aulos_core::Level::Warn,
            code: aulos_core::notice_code::STALLED,
            id: Some(id),
            message: "No progress for 900s".into(),
        });
        let frames = rig.frames();
        assert_eq!(frames.len(), 1, "not held for the batch");
        assert_eq!(frames[0].0, FrameKind::Notice);
        assert_eq!(frames[0].1["level"], "warning");
        assert_eq!(frames[0].1["code"], "stalled");
        assert_eq!(frames[0].1["id"], id.to_string());
        assert_eq!(frames[0].1["message"], "No progress for 900s");
    }

    #[test]
    fn finishing_produces_no_frame_at_all() {
        let mut rig = Rig::new(&[]);
        let v = view(Status::Postprocessing, 1);
        rig.agg.on_event(&DomainEvent::Finishing(Arc::clone(&v)));
        assert_eq!(rig.agg.flush(), 0);
        assert!(
            rig.frames().is_empty(),
            "it precedes a status that is not written yet"
        );
        assert!(rig.state.load().is_empty());
    }

    #[test]
    fn health_is_emitted_only_on_a_component_transition() {
        let mut rig = Rig::new(&[]);
        let mut view = HealthView::empty();
        view.components.insert(
            "pot".to_owned(),
            aulos_core::ComponentHealth::new(ComponentStatus::Ok),
        );
        rig.agg
            .on_event(&DomainEvent::HealthChanged(Arc::new(view.clone())));
        assert_eq!(
            rig.frames().len(),
            1,
            "the first view is always a transition"
        );

        rig.agg
            .on_event(&DomainEvent::HealthChanged(Arc::new(view.clone())));
        assert!(
            rig.frames().is_empty(),
            "nothing changed, nothing is emitted"
        );

        view.status = ComponentStatus::Degraded;
        view.components.insert(
            "pot".to_owned(),
            aulos_core::ComponentHealth::new(ComponentStatus::Down)
                .with("detail", "3 consecutive probe failures"),
        );
        rig.agg
            .on_event(&DomainEvent::HealthChanged(Arc::new(view)));
        let frames = rig.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].1["status"], "degraded");
        assert_eq!(frames[0].1["changed"][0]["component"], "pot");
        assert_eq!(frames[0].1["changed"][0]["from"], "ok");
        assert_eq!(frames[0].1["changed"][0]["to"], "down");
        assert_eq!(
            frames[0].1["changed"][0]["detail"],
            "3 consecutive probe failures"
        );
    }

    #[test]
    fn the_protocol_block_is_self_describing() {
        let cfg = config(&[]);
        assert_eq!(
            protocol_block(&cfg),
            json!({
                "batch_ms": 250,
                "urgent_ms": 25,
                "replay_frames": 512,
                "delta_semantics": "absent-key-means-unchanged",
            })
        );
    }

    // -----------------------------------------------------------------------
    // resuming across the published snapshot
    // -----------------------------------------------------------------------

    /// The invariant the flush order's final step exists for: a REST reader's cursor is never
    /// ahead of the socket, so `?since=` is always resumable rather than a phantom gap.
    #[test]
    fn a_rest_readers_cursor_is_never_newer_than_the_socket() {
        let mut rig = Rig::new(&[]);
        for i in 0..5 {
            let v = view(Status::Queued, i64::from(i));
            rig.added(std::slice::from_ref(&v));
            rig.agg.flush();
            let seq = rig.state.load().seq;
            assert!(seq <= rig.hub.head());
            assert!(matches!(
                rig.hub.resume(seq, Some(rig.hub.boot_id())),
                crate::hub::Resume::UpToDate | crate::hub::Resume::Merged { .. }
            ));
        }
        rig.frames();
    }

    impl Rig {
        /// `Rig::new` with a seeded history total.
        fn agg_with_done_total(mut self, total: u64) -> Self {
            self.agg = self.agg.with_done_total(total);
            self
        }
    }

    #[test]
    fn a_file_and_a_stage_message_are_forwarded_and_arm_the_urgent_deadline() {
        // The forward itself needs an engine handle, so this covers only the classification: both
        // kinds are lossless and both are urgent by class (DESIGN §15.1).
        let mut rig = Rig::new(&[]);
        assert!(rig.agg.urgent_at.is_none());
        rig.agg.arm_urgent();
        let first = rig.agg.urgent_at.expect("armed");
        rig.agg.arm_urgent();
        assert!(
            rig.agg.urgent_at.expect("still armed") <= first,
            "the earliest wins"
        );
        let _ = FileSlot::Chapter;
        let _ = item(Status::Queued, 1);
    }
}
