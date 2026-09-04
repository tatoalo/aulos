//! The engine: one task, owned state, no `Mutex` (DESIGN §8.2).
//!
//! This module holds the state, the loop, the item cache, the view projection, the scheduler and
//! the 1 Hz tick. The command handlers live next to the behaviour they implement: [`crate::add`],
//! [`crate::resolve`], [`crate::run`], [`crate::cancel`], [`crate::clear`] and
//! [`crate::recovery`] are all `impl Engine` blocks.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::{
    AddReason, Clock, Config, DomainEvent, EventSender, FieldUpdate, GroupId, Item, ItemId,
    ItemView, Kind, RemoveReason, Status, StatusEdge, UnixMs, ViewExtras, YtdlOptions,
    can_transition,
};
use aulos_provider::{MediaEntry, Outcome, ProgressSinkFactory, Provider, ProviderId, Registry};
use aulos_store::{Durability, Store, StoreError, WriteOp};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::cmd::{ENGINE_CHANNEL_CAPACITY, EngineCmd, EngineHandle, HookWrite, ResolveReport};
use crate::groups::{DRIFT_RECOMPUTE_MS, GroupAcc};
use crate::priority::Priority;
use crate::slots::{Slot, Slots};
use crate::watchdog::Heartbeats;

/// The engine's own safety net for the pre-terminal hook phase (DESIGN §13).
///
/// DESIGN §13 bounds a non-answering pre-terminal hook by "the dispatcher's own per-hook timeout,
/// after which the engine finalises anyway and logs at WARN". There is no configuration knob for
/// it, and the one pre-terminal hook that exists (`audio_sync`) bounds its own ffmpeg run at
/// 1 800 s, so this sits comfortably above that.
pub const PRE_TERMINAL_TIMEOUT_MS: i64 = 2_400_000;

/// Whether an item has any applicable `HookPhase::PreTerminal` hook (DESIGN §13).
///
/// The engine cannot evaluate `aulos_hooks::Hook::applies` — `aulos-queue` must not depend on
/// `aulos-hooks` (DESIGN §3) — so this is the seam. `aulos-server` implements it over the
/// dispatcher's hook list; see `docs/INTEGRATION-NOTES.md`, WP-12.
///
/// The returned string is the label the engine writes into `msg` while the phase runs
/// (`"Re-encoding audio"` for `audio_sync`).
pub trait PreTerminalHooks: Send + Sync {
    /// The label of the first applicable pre-terminal hook, or `None` when there is none.
    fn label_for(&self, view: &ItemView) -> Option<Box<str>>;
}

/// The default: no pre-terminal hook, so every terminal transition finalises in one step.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPreTerminalHooks;

impl PreTerminalHooks for NoPreTerminalHooks {
    fn label_for(&self, _view: &ItemView) -> Option<Box<str>> {
        None
    }
}

/// One in-flight resolution.
pub(crate) struct ResolveSlot {
    pub(crate) handle: JoinHandle<()>,
    pub(crate) cancel: CancellationToken,
    pub(crate) generation: u64,
}

/// One running download.
///
/// The job's `JoinHandle` is deliberately **not** kept: cancellation is cooperative, and aborting
/// the task would orphan the process group it is responsible for killing. Cancelling the token is
/// the only shutdown path.
pub(crate) struct RunSlot {
    pub(crate) cancel: CancellationToken,
    pub(crate) watchdog: Option<JoinHandle<()>>,
    /// The permit, dropped as soon as the provider is done — not when the hooks are (DESIGN §13).
    pub(crate) slot: Option<Slot>,
    /// Set once the engine has written this job's outcome itself (a cancel or a pause), so the
    /// task's own `Finished`/`Failed` is ignored instead of overwriting it.
    pub(crate) settled: bool,
}

/// One playlist expansion, mid-flight (DESIGN §8.4).
pub(crate) struct Expansion {
    pub(crate) generation: u64,
    /// The cancel epoch this expansion was started under (see [`Engine::cancel_epoch`]).
    pub(crate) epoch: u64,
    pub(crate) remaining: VecDeque<MediaEntry>,
    pub(crate) next_index: u32,
    pub(crate) provider: ProviderId,
    pub(crate) first_batch: bool,
}

/// One outstanding [`EngineCmd::WaitResolved`] (DESIGN §11.2).
pub(crate) struct PendingWait {
    pub(crate) order: Vec<ItemId>,
    pub(crate) reports: HashMap<ItemId, ResolveReport>,
    pub(crate) ack: oneshot::Sender<Vec<ResolveReport>>,
}

/// An item whose terminal write is waiting on its pre-terminal hooks (DESIGN §13).
pub(crate) struct PendingHooks {
    pub(crate) outcome: Box<Outcome>,
    pub(crate) deadline_ms: UnixMs,
}

/// A retry the backoff has not released yet (DESIGN §8.8).
pub(crate) struct PendingRetry {
    pub(crate) id: ItemId,
    pub(crate) at_ms: UnixMs,
}

/// The queue engine (DESIGN §8.2).
///
/// Construct with [`Engine::new`], optionally call [`Engine::recover`], then [`Engine::spawn`].
pub struct Engine {
    pub(crate) store: Store,
    pub(crate) registry: Arc<RwLock<Registry>>,
    pub(crate) cfg: Arc<Config>,
    pub(crate) ytdl: Arc<ArcSwap<YtdlOptions>>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) events: EventSender,
    pub(crate) sink: ProgressSinkFactory,
    pub(crate) pre_terminal: Arc<dyn PreTerminalHooks>,
    pub(crate) pre_terminal_timeout_ms: i64,
    pub(crate) beats: Heartbeats,

    rx: Option<mpsc::Receiver<EngineCmd>>,
    pub(crate) tx: mpsc::Sender<EngineCmd>,

    /// Every non-terminal item plus the most recent `AULOS_MEM_DONE_ITEMS` terminal ones
    /// (DESIGN §8.7): `schedule()` and the published snapshot never go through the store actor.
    pub(crate) items: HashMap<ItemId, Arc<Item>>,
    pub(crate) done_order: VecDeque<ItemId>,
    pub(crate) done_total: u64,

    pub(crate) ready: [VecDeque<ItemId>; Priority::COUNT],
    pub(crate) resolving: HashMap<ItemId, ResolveSlot>,
    pub(crate) running: HashMap<ItemId, RunSlot>,
    pub(crate) groups: HashMap<GroupId, GroupAcc>,
    pub(crate) expansions: HashMap<GroupId, Expansion>,
    pub(crate) slots: Slots,
    pub(crate) dedupe: HashMap<crate::dedupe::DedupeKey, ItemId>,
    /// The generation stamped on the **next** add (DESIGN §8.1, [`crate::CancelScope`]).
    ///
    /// One generation per `Add`, so `CancelScope::Generation(n)` isolates a single add. It is
    /// *not* the "has a blanket cancel happened" counter — that is [`Engine::cancel_epoch`],
    /// which two racing adds must not share (the WP-14 request in `docs/INTEGRATION-NOTES.md`).
    pub(crate) add_generation: u64,
    /// Bumped by [`crate::CancelScope::All`] only, and compared against the epoch a resolution or
    /// an expansion was started under to drop work a blanket cancel has already condemned.
    pub(crate) cancel_epoch: u64,
    pub(crate) waits: Vec<PendingWait>,
    pub(crate) pending_hooks: HashMap<ItemId, PendingHooks>,
    /// Items whose `size` a hook has rewritten through the port, so the terminal write keeps the
    /// hook's value instead of the provider's (DESIGN §13.3).
    pub(crate) hook_sized: std::collections::HashSet<ItemId>,
    pub(crate) retries: Vec<PendingRetry>,
    pub(crate) next_clear_at: Option<UnixMs>,
    pub(crate) last_drift_ms: UnixMs,
    pub(crate) shutdown: CancellationToken,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("cached", &self.items.len())
            .field(
                "ready",
                &self.ready.iter().map(VecDeque::len).collect::<Vec<_>>(),
            )
            .field("resolving", &self.resolving.len())
            .field("running", &self.running.len())
            .field("groups", &self.groups.len())
            .field("generation", &self.add_generation)
            .field("cancel_epoch", &self.cancel_epoch)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Builds the engine and its handle (DESIGN §8.2).
    ///
    /// `progress` is the sending half of the one `ProgressMsg` channel (DESIGN §2.3); the engine
    /// hands each job a per-item [`aulos_provider::ProgressSink`] built from it, and never reads
    /// the channel itself — progress does not enter the engine.
    #[must_use]
    pub fn new(
        store: Store,
        registry: Arc<RwLock<Registry>>,
        cfg: Arc<Config>,
        ytdl: Arc<ArcSwap<YtdlOptions>>,
        clock: Arc<dyn Clock>,
        events: EventSender,
        progress: mpsc::Sender<aulos_provider::ProgressMsg>,
    ) -> (Self, EngineHandle) {
        let (tx, rx) = mpsc::channel(ENGINE_CHANNEL_CAPACITY);
        let beats = Heartbeats::new();
        let handle = EngineHandle::new(tx.clone(), beats.clone());
        let slots = Slots::from_config(&cfg);
        let engine = Self {
            store,
            registry,
            cfg,
            ytdl,
            clock,
            events,
            sink: ProgressSinkFactory::new(progress),
            pre_terminal: Arc::new(NoPreTerminalHooks),
            pre_terminal_timeout_ms: PRE_TERMINAL_TIMEOUT_MS,
            beats,
            rx: Some(rx),
            tx,
            items: HashMap::new(),
            done_order: VecDeque::new(),
            done_total: 0,
            ready: std::array::from_fn(|_| VecDeque::new()),
            resolving: HashMap::new(),
            running: HashMap::new(),
            groups: HashMap::new(),
            expansions: HashMap::new(),
            slots,
            dedupe: HashMap::new(),
            add_generation: 0,
            cancel_epoch: 0,
            waits: Vec::new(),
            pending_hooks: HashMap::new(),
            hook_sized: std::collections::HashSet::new(),
            retries: Vec::new(),
            next_clear_at: None,
            last_drift_ms: 0,
            shutdown: CancellationToken::new(),
        };
        (engine, handle)
    }

    /// Wires the pre-terminal hook gate (DESIGN §13).
    ///
    /// Without it every terminal transition finalises in one step, which is correct for a wiring
    /// with no `PreTerminal` hook.
    #[must_use]
    pub fn with_pre_terminal(mut self, hooks: Arc<dyn PreTerminalHooks>) -> Self {
        self.pre_terminal = hooks;
        self
    }

    /// Overrides the pre-terminal safety net. For tests, and for an operator who ships a
    /// slower hook than `audio_sync`.
    #[must_use]
    pub const fn with_pre_terminal_timeout_ms(mut self, ms: i64) -> Self {
        self.pre_terminal_timeout_ms = ms;
        self
    }

    /// Uses an externally owned cancellation token, so the DESIGN §16.4 shutdown grace reaches
    /// every running job.
    #[must_use]
    pub fn with_shutdown(mut self, token: CancellationToken) -> Self {
        self.shutdown = token;
        self
    }

    /// The token every job's [`aulos_provider::DownloadCtx::cancel`] is a child of.
    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Spawns the event loop.
    #[must_use]
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// The event loop, so a test can drive it on the current task.
    ///
    /// Returns once every [`EngineHandle`] has been dropped and the inbox has drained. On the way
    /// out it cancels every in-flight resolution and download: the jobs' own tasks then observe
    /// their tokens and kill their process groups.
    pub async fn run(mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => break,
                },
                _ = tick.tick() => self.tick().await,
                () = tokio::time::sleep_until(deadline) => self.tick().await,
            }
        }

        for (_, slot) in self.resolving.drain() {
            slot.cancel.cancel();
            slot.handle.abort();
        }
        for (_, slot) in self.running.drain() {
            slot.cancel.cancel();
            if let Some(w) = slot.watchdog {
                w.abort();
            }
        }
        self.slots.close();
        tracing::debug!("the queue engine has stopped");
    }

    /// The next `sleep_until` target: the earliest armed retry, clear or hook deadline.
    ///
    /// One second is the floor rather than the ceiling — the 1 Hz `Tick` covers everything this
    /// misses, and a far-future instant keeps the branch from firing when nothing is armed.
    fn next_deadline(&self) -> Instant {
        let now = self.clock.now_ms();
        let mut soonest: Option<i64> = None;
        for r in &self.retries {
            soonest = Some(soonest.map_or(r.at_ms, |s: i64| s.min(r.at_ms)));
        }
        if let Some(at) = self.next_clear_at {
            soonest = Some(soonest.map_or(at, |s: i64| s.min(at)));
        }
        let base = self.clock.instant();
        match soonest {
            Some(at) => {
                let delay = u64::try_from(at.saturating_sub(now)).unwrap_or(0);
                base + Duration::from_millis(delay)
            }
            None => base + Duration::from_secs(3_600),
        }
    }

    /// Dispatches one command.
    async fn handle(&mut self, cmd: EngineCmd) {
        match cmd {
            EngineCmd::Add {
                requests,
                source,
                ack,
            } => self.handle_add(requests, source, ack).await,
            EngineCmd::WaitResolved { ids, ack } => self.handle_wait_resolved(ids, ack),
            EngineCmd::Start { ids, ack } => {
                let r = self.handle_start(ids).await;
                let _ = ack.send(r);
            }
            EngineCmd::Pause { ids, ack } => {
                let r = self.handle_pause(ids).await;
                let _ = ack.send(r);
            }
            EngineCmd::Cancel { ids, ack } => {
                let r = self.handle_cancel(ids).await;
                let _ = ack.send(r);
            }
            EngineCmd::Retry { ids, ack } => {
                let r = self.handle_retry(ids).await;
                let _ = ack.send(r);
            }
            EngineCmd::Delete {
                ids,
                delete_file,
                ack,
            } => {
                let r = self.handle_delete(ids, delete_file).await;
                let _ = ack.send(r);
            }
            EngineCmd::Clear { delete_file, ack } => {
                let r = self.handle_clear(delete_file).await;
                let _ = ack.send(r);
            }
            EngineCmd::CancelResolve { scope, ack } => {
                let r = self.handle_cancel_resolve(scope).await;
                let _ = ack.send(r);
            }
            EngineCmd::HookWrite { id, write, ack } => {
                let r = self.handle_hook_write(id, write).await;
                let _ = ack.send(r);
            }
            EngineCmd::HooksFinished { id, outcome } => {
                self.handle_hooks_finished(id, outcome).await;
            }
            EngineCmd::Resolved { id, result, meta } => {
                self.handle_resolved(id, result, *meta).await;
            }
            EngineCmd::ExpandNext { group } => self.handle_expand_next(group).await,
            EngineCmd::Stage { id, stage, msg } => self.handle_stage(id, stage, msg).await,
            EngineCmd::File { id, slot, file } => self.handle_file(id, slot, *file).await,
            EngineCmd::Finished { id, outcome } => self.handle_finished(id, outcome).await,
            EngineCmd::Failed { id, err } => self.handle_failed(id, *err).await,
            EngineCmd::SlotFreed => self.schedule().await,
            EngineCmd::Tick => self.tick().await,
        }
    }

    // -----------------------------------------------------------------------
    // the 1 Hz tick
    // -----------------------------------------------------------------------

    /// `clear_after`, released retries, the pre-terminal safety net and the group drift recompute.
    async fn tick(&mut self) {
        self.release_retries().await;
        self.sweep_clears().await;
        self.expire_pending_hooks().await;
        self.recompute_group_drift();
    }

    /// Pushes every retry whose backoff has expired into its ready deque (DESIGN §8.8).
    async fn release_retries(&mut self) {
        let now = self.clock.now_ms();
        let mut due = Vec::new();
        self.retries.retain(|r| {
            if r.at_ms <= now {
                due.push(r.id);
                false
            } else {
                true
            }
        });
        if due.is_empty() {
            return;
        }
        for id in due {
            let Some(item) = self.items.get(&id) else {
                continue;
            };
            if item.status != Status::Queued || !item.auto_start {
                continue;
            }
            let msg = FieldUpdate::Set(Box::<str>::from("Retrying"));
            self.write_status(id, Status::Queued, msg, FieldUpdate::Keep, None)
                .await;
            self.enqueue(id);
        }
        self.schedule().await;
    }

    /// Finalises any item whose pre-terminal hooks never answered (DESIGN §13).
    async fn expire_pending_hooks(&mut self) {
        let now = self.clock.now_ms();
        let expired: Vec<ItemId> = self
            .pending_hooks
            .iter()
            .filter(|(_, p)| p.deadline_ms <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::warn!(
                item = %id,
                "the pre-terminal hook phase did not answer in time; finalising anyway"
            );
            self.handle_hooks_finished(id, None).await;
        }
    }

    /// Recomputes every group's accumulator from its children every five minutes (DESIGN §8.6).
    fn recompute_group_drift(&mut self) {
        let now = self.clock.now_ms();
        if now.saturating_sub(self.last_drift_ms) < DRIFT_RECOMPUTE_MS {
            return;
        }
        self.last_drift_ms = now;
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for id in ids {
            self.recompute_group(id);
        }
    }

    /// Recomputes one group's accumulator, correcting and reporting any drift.
    pub(crate) fn recompute_group(&mut self, id: GroupId) {
        let declared = self
            .items
            .get(&id)
            .and_then(|i| i.children_total)
            .unwrap_or(0);
        let fresh = GroupAcc::recomputed(
            declared,
            self.items
                .values()
                .filter(|i| i.group_id == Some(id))
                .map(Arc::as_ref),
            crate::entry::size_hint,
        );
        if let Some(acc) = self.groups.get_mut(&id)
            && acc.correct(&fresh)
        {
            tracing::warn!(group = %id, "group accumulator had drifted; corrected");
        }
    }

    // -----------------------------------------------------------------------
    // the item cache
    // -----------------------------------------------------------------------

    /// The cached row, if the engine is holding it.
    #[must_use]
    pub(crate) fn cached(&self, id: ItemId) -> Option<Arc<Item>> {
        self.items.get(&id).cloned()
    }

    /// Mutates a cached row in place and returns the new `Arc`.
    pub(crate) fn patch(&mut self, id: ItemId, f: impl FnOnce(&mut Item)) -> Option<Arc<Item>> {
        let slot = self.items.get_mut(&id)?;
        f(Arc::make_mut(slot));
        Some(Arc::clone(slot))
    }

    /// Inserts a freshly created row into the cache and, when it is a group child, into its
    /// group's accumulator.
    pub(crate) fn cache_insert(&mut self, item: Item) -> Arc<Item> {
        let id = item.id;
        if let Some(group) = item.group_id {
            let hint = crate::entry::size_hint(&item);
            if let Some(acc) = self.groups.get_mut(&group) {
                acc.add_child(item.status, hint);
            }
        }
        let arc = Arc::new(item);
        self.items.insert(id, Arc::clone(&arc));
        if arc.status.is_terminal() {
            self.mark_done(id);
        }
        arc
    }

    /// Records a terminal row in the bounded done window, evicting the oldest.
    pub(crate) fn mark_done(&mut self, id: ItemId) {
        if self.done_order.contains(&id) {
            return;
        }
        self.done_order.push_back(id);
        self.done_total += 1;
        let window = self.cfg.mem_done_items as usize;
        while self.done_order.len() > window {
            if let Some(old) = self.done_order.pop_front() {
                // Never evict a row something still points at: a group whose children are cached
                // needs its own row for the roll-up.
                if self.groups.contains_key(&old) {
                    continue;
                }
                self.items.remove(&old);
            }
        }
    }

    /// Drops a row from every index it appears in.
    pub(crate) fn forget(&mut self, id: ItemId) {
        if let Some(item) = self.items.remove(&id) {
            self.drop_dedupe(&item);
        }
        self.done_order.retain(|d| *d != id);
        self.groups.remove(&id);
        self.expansions.remove(&id);
        self.retries.retain(|r| r.id != id);
        self.pending_hooks.remove(&id);
        self.hook_sized.remove(&id);
        self.beats.disarm(id);
        for deque in &mut self.ready {
            deque.retain(|q| *q != id);
        }
    }

    /// Removes every dedupe entry pointing at an item.
    ///
    /// A value scan rather than a keyed remove, because a resolved item owns **two** keys — the
    /// URL-derived one it was added under and the `media_id`-derived one resolution produced — and
    /// both have to go. The scan is bounded by the live queue, which is what the index holds.
    pub(crate) fn drop_dedupe(&mut self, item: &Item) {
        let id = item.id;
        self.dedupe.retain(|_, v| *v != id);
    }

    // -----------------------------------------------------------------------
    // views and events
    // -----------------------------------------------------------------------

    /// Projects a row onto the wire shape (DESIGN §4.6).
    ///
    /// The progress cell is deliberately `None`: progress lives in the aggregator, which merges
    /// its own cells over this view before diffing (DESIGN §15.1). `download_url` is `None` for
    /// the same reason it is `None` in [`ViewExtras`]'s own documentation — it needs
    /// `PUBLIC_HOST_URL` and percent-encoding, which are `aulos-api`'s.
    #[must_use]
    pub(crate) fn view(&self, item: &Item) -> Arc<ItemView> {
        let mut extras = ViewExtras::default();
        let acc = (item.kind == Kind::Group)
            .then(|| self.groups.get(&item.id))
            .flatten();
        if let Some(acc) = acc {
            extras.children_done = Some(acc.done());
            extras.children_error = Some(acc.error());
            extras.children_active = Some(acc.active());
            // v1.0: not implemented, see BRIEF — `AULOS_SNAPSHOT_GROUP_INLINE` and the WS `watch`
            // frame are CUT, so the snapshot always carries every non-terminal child.
            extras.children_inline = Some(true);
        }
        let mut view = ItemView::from_item(item, None, &extras);
        if let Some(acc) = acc {
            if item.status != Status::Finished {
                view.percent = acc.percent();
            }
            view.speed = acc.speed();
            view.eta = acc.eta();
        }
        Arc::new(view)
    }

    /// The view of a cached row.
    #[must_use]
    pub(crate) fn view_of(&self, id: ItemId) -> Option<Arc<ItemView>> {
        self.items.get(&id).map(|i| self.view(i))
    }

    /// Publishes `Added` (DESIGN §8.1).
    pub(crate) async fn publish_added(&self, views: Vec<Arc<ItemView>>, reason: AddReason) {
        if views.is_empty() {
            return;
        }
        self.events.publish(DomainEvent::Added(views, reason)).await;
    }

    /// Publishes `StatusChanged`. `from == to` is the generic "re-diff this row" signal.
    pub(crate) async fn publish_changed(&self, id: ItemId, from: Status, to: Status) {
        if let Some(view) = self.view_of(id) {
            self.events
                .publish(DomainEvent::StatusChanged { id, from, to, view })
                .await;
        }
    }

    /// Publishes `Completed` for a row that has just become terminal.
    pub(crate) async fn publish_completed(&self, id: ItemId) {
        if let Some(view) = self.view_of(id) {
            self.events.publish(DomainEvent::Completed(view)).await;
        }
    }

    /// Publishes `Removed` for one reason.
    pub(crate) async fn publish_removed(&self, ids: Vec<ItemId>, reason: RemoveReason) {
        if ids.is_empty() {
            return;
        }
        self.events
            .publish(DomainEvent::Removed { ids, reason })
            .await;
    }

    // -----------------------------------------------------------------------
    // writes
    // -----------------------------------------------------------------------

    /// Applies a batch, logging and reporting a failure rather than propagating it.
    ///
    /// A write failure leaves the cache ahead of the database for one row. That is survivable —
    /// the next boot re-reads the row and DESIGN §8.9 re-queues whatever was in flight — and it is
    /// strictly better than an engine that stops handling commands because one `UPDATE` failed.
    pub(crate) async fn apply(&self, ops: Vec<WriteOp>, durability: Durability) -> bool {
        if ops.is_empty() {
            return true;
        }
        let names: Vec<&'static str> = ops.iter().map(WriteOp::name).collect();
        match self.store.write(ops, durability).await {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(ops = ?names, error = %e, "a queue write failed");
                false
            }
        }
    }

    /// The one status-write path: guards the transition, persists it, patches the cache, keeps the
    /// group counters in step and publishes.
    ///
    /// `can_transition` is used as a real guard, not a `debug_assert`: a release build refuses an
    /// illegal write instead of corrupting the queue (DESIGN §4.2).
    pub(crate) async fn write_status(
        &mut self,
        id: ItemId,
        status: Status,
        msg: FieldUpdate<Box<str>>,
        error: FieldUpdate<aulos_core::WireError>,
        auto_start: Option<bool>,
    ) -> bool {
        let Some(item) = self.cached(id) else {
            tracing::debug!(item = %id, "status write for an unknown item");
            return false;
        };
        let from = item.status;
        let to_edge = StatusEdge {
            status,
            auto_start: auto_start.unwrap_or(item.auto_start),
        };
        let from_edge = StatusEdge {
            status: from,
            auto_start: item.auto_start,
        };
        let chain = status_chain(from_edge, to_edge);
        if chain.is_empty() {
            tracing::warn!(item = %id, %from, to = %status, "refused an illegal transition");
            return false;
        }

        let at = self.clock.now_ms();
        let last = chain.len() - 1;
        let ops: Vec<WriteOp> = chain
            .iter()
            .enumerate()
            .map(|(i, hop)| WriteOp::SetStatus {
                id,
                status: *hop,
                // The intermediate hops of a chained transition exist so the persisted `status`
                // column is legal at every step; only the last one carries the patch.
                msg: if i == last {
                    msg.clone()
                } else {
                    FieldUpdate::Keep
                },
                error: if i == last {
                    error.clone()
                } else {
                    FieldUpdate::Keep
                },
                auto_start: if i == last { auto_start } else { None },
                at,
            })
            .collect();
        if !self.apply(ops, Durability::Batched).await {
            return false;
        }

        let touched_preparing = chain.contains(&Status::Preparing);
        self.patch(id, |item| {
            item.status = status;
            if let Some(flag) = auto_start {
                item.auto_start = flag;
            }
            msg.apply_to(&mut item.msg);
            error.apply_to(&mut item.error);
            if touched_preparing && item.started_at.is_none() {
                item.started_at = Some(at);
            }
            if status.is_terminal() {
                item.finished_at = Some(at);
            } else if from.is_terminal() {
                item.finished_at = None;
            }
        });
        if status.is_terminal() {
            self.mark_done(id);
        }
        self.on_child_status(id, from, status).await;
        self.publish_changed(id, from, status).await;
        true
    }

    /// Keeps a group's accumulator and persisted roll-up in step with one of its children.
    pub(crate) async fn on_child_status(&mut self, child: ItemId, from: Status, to: Status) {
        let Some(group) = self.items.get(&child).and_then(|i| i.group_id) else {
            return;
        };
        if let Some(acc) = self.groups.get_mut(&group) {
            acc.on_child_status(from, to);
            if to == Status::Finished {
                let size = self.items.get(&child).and_then(|i| i.size);
                let hint = self
                    .items
                    .get(&child)
                    .and_then(|i| crate::entry::size_hint_excluding_size(i));
                if let Some(acc) = self.groups.get_mut(&group) {
                    acc.on_child_finished(size, hint);
                }
            }
        }
        self.sync_group_status(group).await;
    }

    /// Writes and publishes a group's rolled-up status when it has actually changed (DESIGN §8.6).
    pub(crate) async fn sync_group_status(&mut self, group: GroupId) {
        let Some(acc) = self.groups.get(&group) else {
            return;
        };
        let rolled = acc.status();
        let Some(item) = self.cached(group) else {
            return;
        };
        if item.status == rolled {
            // Still republish so the aggregator re-diffs the group's counters and percent.
            self.publish_changed(group, item.status, item.status).await;
            return;
        }
        let at = self.clock.now_ms();
        let ok = self
            .apply(
                vec![WriteOp::SetStatus {
                    id: group,
                    status: rolled,
                    msg: FieldUpdate::Keep,
                    error: FieldUpdate::Keep,
                    auto_start: None,
                    at,
                }],
                Durability::Batched,
            )
            .await;
        if !ok {
            return;
        }
        let from = item.status;
        self.patch(group, |g| {
            g.status = rolled;
            if rolled.is_terminal() {
                g.finished_at = Some(at);
            } else if from.is_terminal() {
                g.finished_at = None;
            }
        });
        if rolled.is_terminal() {
            self.mark_done(group);
            self.publish_completed(group).await;
        } else {
            self.publish_changed(group, from, rolled).await;
        }
    }

    // -----------------------------------------------------------------------
    // scheduling
    // -----------------------------------------------------------------------

    /// Pushes an id into its priority deque, keeping `ord` order (DESIGN §8.2).
    pub(crate) fn enqueue(&mut self, id: ItemId) {
        let Some(item) = self.items.get(&id) else {
            return;
        };
        if item.kind == Kind::Group {
            return;
        }
        let prio = Priority::of(item.source.kind, item.group_id.is_some());
        let ord = item.ord;
        let deque = &mut self.ready[prio.index()];
        if deque.contains(&id) {
            return;
        }
        // The deques are append-mostly and already `ord`-ordered, so this is a tail insert in the
        // overwhelming majority of cases.
        let at = deque
            .iter()
            .position(|other| self.items.get(other).is_some_and(|o| o.ord > ord));
        match at {
            Some(i) => deque.insert(i, id),
            None => deque.push_back(id),
        }
    }

    /// The DESIGN §8.7 scheduler: four priority classes, a bounded lookahead, `own_slots` bypass.
    pub(crate) async fn schedule(&mut self) {
        let lookahead = self.cfg.sched_lookahead.max(1) as usize;
        for prio in Priority::ALL {
            let mut scanned = 0usize;
            let mut cursor = 0usize;
            loop {
                if scanned >= lookahead {
                    break;
                }
                let Some(&id) = self.ready[prio.index()].get(cursor) else {
                    break;
                };
                let Some(item) = self.cached(id) else {
                    self.ready[prio.index()].remove(cursor);
                    continue;
                };
                if item.kind == Kind::Group
                    || item.status != Status::Queued
                    || !item.auto_start
                    || self.running.contains_key(&id)
                    || self.retries.iter().any(|r| r.id == id)
                {
                    self.ready[prio.index()].remove(cursor);
                    continue;
                }
                let Some(provider_id) = item.provider.clone() else {
                    self.ready[prio.index()].remove(cursor);
                    continue;
                };
                let Some(selected) = self.provider_of(&provider_id) else {
                    self.ready[prio.index()].remove(cursor);
                    self.fail_missing_provider(id, &provider_id).await;
                    continue;
                };
                if let Some(reason) = selected.degraded {
                    self.ready[prio.index()].remove(cursor);
                    self.fail_degraded(id, &provider_id, &reason).await;
                    continue;
                }
                match self.slots.try_acquire(&provider_id, selected.own_slots) {
                    Some(slot) => {
                        self.ready[prio.index()].remove(cursor);
                        self.start_job(&item, selected.provider, slot).await;
                    }
                    None => {
                        scanned += 1;
                        cursor += 1;
                    }
                }
            }
        }
    }

    /// A provider looked up out of the registry, plus the two things the scheduler needs.
    pub(crate) fn provider_of(&self, id: &ProviderId) -> Option<SelectedProvider> {
        let registry = self.registry.read().unwrap_or_else(PoisonError::into_inner);
        let provider = Arc::clone(registry.by_id(id)?);
        let own_slots = provider.own_slots();
        let degraded = registry
            .state_of(id)
            .and_then(|s| s.reason().map(Box::<str>::from));
        Some(SelectedProvider {
            provider,
            own_slots,
            degraded,
        })
    }

    /// Terminates an item whose provider is no longer registered.
    async fn fail_missing_provider(&mut self, id: ItemId, provider: &ProviderId) {
        let err = aulos_core::WireError::new(
            aulos_core::ErrorCode::ProviderDegraded,
            format!("provider {provider} is no longer registered"),
        );
        self.terminate(id, Status::Error, FieldUpdate::Set(err))
            .await;
    }

    /// Terminates an item routed to a degraded provider (DESIGN §6.4): no fall-through.
    async fn fail_degraded(&mut self, id: ItemId, provider: &ProviderId, reason: &str) {
        let err = aulos_core::WireError::new(aulos_core::ErrorCode::ProviderDegraded, reason)
            .with_provider(provider.as_arc(), None);
        self.terminate(id, Status::Error, FieldUpdate::Set(err))
            .await;
    }

    /// Writes a terminal status, arms `clear_after` and publishes `Completed` (DESIGN §8.10).
    pub(crate) async fn terminate(
        &mut self,
        id: ItemId,
        status: Status,
        error: FieldUpdate<aulos_core::WireError>,
    ) -> bool {
        if !self
            .write_status(id, status, FieldUpdate::Keep, error, None)
            .await
        {
            return false;
        }
        if let Some(item) = self.cached(id) {
            self.drop_dedupe(&item);
        }
        self.arm_clear_after(id, status).await;
        self.publish_completed(id).await;
        true
    }

    /// Persists `clear_after` for a terminal row (DESIGN §8.10).
    ///
    /// Only `finished` and `error` are armed: a cancelled row is the user's own doing and vanishing
    /// it behind their back is not the same thing as tidying up after a completed download.
    pub(crate) async fn arm_clear_after(&mut self, id: ItemId, status: Status) {
        let window = self.cfg.clear_completed_after;
        if window == 0 || !matches!(status, Status::Finished | Status::Error) {
            return;
        }
        let at = self.clock.now_ms() + i64::try_from(window.saturating_mul(1_000)).unwrap_or(0);
        if !self
            .apply(
                vec![WriteOp::SetClearAfter { id, at: Some(at) }],
                Durability::Batched,
            )
            .await
        {
            return;
        }
        self.patch(id, |i| i.clear_after = Some(at));
        self.next_clear_at = Some(self.next_clear_at.map_or(at, |cur| cur.min(at)));
    }

    // -----------------------------------------------------------------------
    // WaitResolved
    // -----------------------------------------------------------------------

    /// Answers immediately for every id already out of `resolving`, and parks the rest
    /// (DESIGN §11.2).
    fn handle_wait_resolved(&mut self, ids: Vec<ItemId>, ack: oneshot::Sender<Vec<ResolveReport>>) {
        let mut wait = PendingWait {
            order: ids,
            reports: HashMap::new(),
            ack,
        };
        for id in wait.order.clone() {
            if let Some(report) = self.settled_report(id) {
                wait.reports.insert(id, report);
            }
        }
        if wait.reports.len() == wait.order.len() {
            let reports = collect_reports(&wait);
            let _ = wait.ack.send(reports);
            return;
        }
        self.waits.push(wait);
    }

    /// The report for an id that is no longer resolving, or `None` while it still is.
    fn settled_report(&self, id: ItemId) -> Option<ResolveReport> {
        if self.resolving.contains_key(&id) {
            return None;
        }
        let Some(item) = self.items.get(&id) else {
            // Unknown ids are reported as gone rather than waited on forever.
            return Some(ResolveReport {
                id,
                kind: Kind::Item,
                outcome: Err(aulos_core::WireError::new(
                    aulos_core::ErrorCode::NotFound,
                    "item not found",
                )),
            });
        };
        if item.status == Status::Resolving {
            return None;
        }
        Some(ResolveReport {
            id,
            kind: item.kind,
            outcome: match (item.status, item.error.clone()) {
                (Status::Error, Some(e)) => Err(e),
                (Status::Error, None) => Err(aulos_core::WireError::new(
                    aulos_core::ErrorCode::Internal,
                    "resolution failed",
                )),
                _ => Ok(()),
            },
        })
    }

    /// Notifies every waiter that `id` has left `resolving` (DESIGN §11.2).
    ///
    /// A caller that dropped its receiver is pruned here, so a waiter cannot leak.
    pub(crate) fn notify_resolved(&mut self, id: ItemId) {
        let Some(report) = self.settled_report(id) else {
            return;
        };
        let mut done = Vec::new();
        for (i, wait) in self.waits.iter_mut().enumerate() {
            if wait.ack.is_closed() {
                done.push(i);
                continue;
            }
            if !wait.order.contains(&id) {
                continue;
            }
            wait.reports.insert(id, report.clone());
            if wait.reports.len() == wait.order.len() {
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            let wait = self.waits.swap_remove(i);
            if !wait.ack.is_closed() {
                let reports = collect_reports(&wait);
                let _ = wait.ack.send(reports);
            }
        }
    }

    // -----------------------------------------------------------------------
    // hook writes
    // -----------------------------------------------------------------------

    /// One of the two engine-mediated hook writes (DESIGN §13.3).
    async fn handle_hook_write(
        &mut self,
        id: ItemId,
        write: HookWrite,
    ) -> Result<(), aulos_core::PortError> {
        if !self.items.contains_key(&id) {
            return Err(aulos_core::PortError::NotFound(id));
        }
        let op = match write {
            HookWrite::Size(size) => WriteOp::SetSize { id, size },
            HookWrite::DropEntryBlob => WriteOp::DropEntryBlob { id },
        };
        self.store
            .write(vec![op], Durability::Batched)
            .await
            .map_err(map_port_error)?;
        match write {
            HookWrite::Size(size) => {
                self.patch(id, |i| i.size = Some(size));
                self.hook_sized.insert(id);
            }
            HookWrite::DropEntryBlob => {
                self.patch(id, |i| i.entry = None);
            }
        }
        // The generic "this persisted row changed, re-diff it" signal, so the aggregator emits a
        // `delta` carrying exactly the changed field (DESIGN §8.1, §13.3).
        let status = self
            .items
            .get(&id)
            .map_or(Status::Postprocessing, |i| i.status);
        self.publish_changed(id, status, status).await;
        Ok(())
    }

    /// The pre-terminal handshake's second half (DESIGN §13).
    async fn handle_hooks_finished(&mut self, id: ItemId, outcome: Option<Box<Outcome>>) {
        let parked = self.pending_hooks.remove(&id);
        let Some(outcome) = outcome.or_else(|| parked.map(|p| p.outcome)) else {
            tracing::debug!(item = %id, "hooks_finished for an item that was not waiting");
            return;
        };
        self.finalise_success(id, &outcome).await;
    }
}

/// A registry lookup's result (DESIGN §6.3, §6.4).
pub(crate) struct SelectedProvider {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) own_slots: Option<usize>,
    /// `Some(reason)` when the provider is in `ProviderState::Degraded`.
    pub(crate) degraded: Option<Box<str>>,
}

/// One waiter's reports, in the order the ids were requested.
fn collect_reports(wait: &PendingWait) -> Vec<ResolveReport> {
    wait.order
        .iter()
        .filter_map(|id| wait.reports.get(id).cloned())
        .collect()
}

/// The hops one status write has to take to be legal at every step (DESIGN §4.2).
///
/// A direct edge is one hop. The forward run of the happy path is the only thing that needs more,
/// and it needs it because a provider is not obliged to report every stage: the `fake` provider's
/// default script and any downloader that produces its file in one go hand the engine `Finished`
/// while the row still reads `preparing`, and DESIGN §4.2 has no `Preparing → Finished` edge. The
/// hops are then `Downloading → Postprocessing → Finished`, they all land in **one** transaction,
/// and they produce **one** frame — so the persisted `status` column is legal at every step while
/// the wire never shows a state the item was only in for a microsecond.
///
/// An empty answer means the transition is illegal and must be refused. `Resolving → Preparing`
/// is the case that matters: the only edge out of `resolving` is to `queued`, and inventing a
/// chain through it would let a resolve result overwrite a cancel.
#[must_use]
pub(crate) fn status_chain(from: StatusEdge, to: StatusEdge) -> Vec<Status> {
    if can_transition(from, to) {
        return vec![to.status];
    }
    /// The forward run of DESIGN §4.2's happy path.
    const PATH: [Status; 4] = [
        Status::Preparing,
        Status::Downloading,
        Status::Postprocessing,
        Status::Finished,
    ];
    let position = |s: Status| PATH.iter().position(|p| *p == s);
    let Some(target) = position(to.status) else {
        return Vec::new();
    };
    let start = match position(from.status) {
        // Already on the path: continue from the next hop.
        Some(i) if i < target => i + 1,
        Some(_) => return Vec::new(),
        // Off the path: only `queued` may join it, at the beginning.
        None => 0,
    };
    let hops = PATH[start..=target].to_vec();
    // Prove the chain rather than trusting the table: every hop must be legal from the previous
    // one, so a change to `can_transition` cannot silently make this function lie.
    let mut cursor = from;
    for hop in &hops {
        let next = StatusEdge::scheduled(*hop);
        if !can_transition(cursor, next) {
            return Vec::new();
        }
        cursor = next;
    }
    hops
}

/// A store failure, as the `HookStore` port reports it.
pub(crate) fn map_port_error(e: StoreError) -> aulos_core::PortError {
    match e {
        StoreError::NotFound(id) => aulos_core::PortError::NotFound(id),
        StoreError::Closed => aulos_core::PortError::Unavailable,
        other => aulos_core::PortError::Store(other.to_string().into_boxed_str()),
    }
}
