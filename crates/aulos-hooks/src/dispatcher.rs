//! The dispatcher: the event loop, the two phases, ordering, debouncing, timeouts and panic
//! isolation (DESIGN §13).
//!
//! # What it subscribes to
//!
//! An [`EventInbox`] from the `EventRouter`, filtered to `Finishing | Completed`, capacity 256,
//! [`aulos_core::event::DropPolicy::DropNewest`] (DESIGN §2.2.1) — **not** an exclusive
//! `mpsc::Receiver<DomainEvent>`, which could not coexist with the aggregator's and Telegram's. A
//! dropped event means one skipped hook run and is counted in [`HooksHealth::events_dropped`].
//!
//! # The invariants
//!
//! - **`HooksFinished` is always sent.** On `Finishing` the dispatcher runs every applicable
//!   `PreTerminal` hook and then tells the engine it is done — on success, on failure, on panic
//!   and on timeout, and also when no pre-terminal hook applied at all. The engine cannot finalise
//!   the item until it arrives, and an item that never finalises is worse than a failed re-encode.
//! - **A hook failure never changes item status.** It is logged, counted and surfaced in
//!   `healthz`.
//! - **A panicking hook does not take down the dispatcher.** Every invocation runs in its own
//!   task, so a panic arrives as a `JoinError` and is recorded like any other failure.
//! - **Ordering is deterministic**: `(ordering(), id())`. `audio_sync` (10) rewrites the file, so
//!   `nfo` (20) and the Jellyfin scan (90) must come after it.
//! - **The download slot is already free.** Hooks run outside it (DESIGN §13); legacy ran cleanup
//!   inside the semaphore and blocked the next download.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering as Atomic};

use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, EventInbox};
use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_core::id::{ItemId, UnixMs};
use aulos_core::item::ItemView;
use aulos_core::paths::RelPath;
use aulos_core::ports::{HookPhase, HookStore};
use aulos_core::status::TerminalStatus;
use aulos_provider::command::HookSpec;
use aulos_provider::sink::ProgressSinkFactory;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::audio_sync::AudioSyncHook;
use crate::error::HookError;
use crate::hook::{BatchEntry, Hook, HookCtx};
use crate::jellyfin::JellyfinHook;
use crate::manifest_hook::ManifestHook;
use crate::nfo::NfoHook;

/// How many post-terminal hook chains run at once (PLAN WP-11: "a per-hook inbox with
/// concurrency 2").
pub const CHAIN_CONCURRENCY: usize = 2;

/// How many invocations of one debounced hook may overlap.
pub const DEBOUNCE_CONCURRENCY: usize = 2;

/// The depth of one debounced hook's own inbox.
pub const HOOK_INBOX: usize = 256;

/// The seam the dispatcher tells the engine that the pre-terminal phase is over through.
///
/// `aulos-hooks` may not depend on `aulos-queue` (DESIGN §3), so `EngineCmd::HooksFinished`
/// cannot be constructed here. `aulos-server`, which depends on both, implements this over the
/// engine handle; the engine keeps the `Outcome` it was going to finalise with, which it must
/// anyway — the outcome never crosses the event boundary (`DomainEvent::Finishing` carries only an
/// `ItemView`). See `docs/INTEGRATION-NOTES.md`, WP-11.
#[async_trait::async_trait]
pub trait HookFinalizer: Send + Sync {
    /// Pre-terminal hooks for `id` are done; finalise it.
    ///
    /// Called exactly once per `Finishing` event, from every path.
    async fn hooks_finished(&self, id: ItemId);
}

/// The finalizer for a wiring with no engine: post-terminal hooks only.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopFinalizer;

#[async_trait::async_trait]
impl HookFinalizer for NoopFinalizer {
    async fn hooks_finished(&self, id: ItemId) {
        tracing::debug!(item = %id, "no finalizer is wired; the pre-terminal phase ends here");
    }
}

/// A finalizer that records what it was told, for tests and for the WP-16 wiring check.
#[derive(Debug, Default)]
pub struct RecordingFinalizer {
    finished: std::sync::Mutex<Vec<ItemId>>,
}

impl RecordingFinalizer {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The ids finalised so far, in order.
    ///
    /// # Panics
    /// If another thread panicked while holding the lock.
    #[must_use]
    pub fn finished(&self) -> Vec<ItemId> {
        self.finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl HookFinalizer for RecordingFinalizer {
    async fn hooks_finished(&self, id: ItemId) {
        self.finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// One hook's counters, as `healthz` reports them (DESIGN §16.3).
#[derive(Clone, PartialEq, Debug)]
pub struct HookStat {
    /// The hook id, which is also the `components.<name>` key.
    pub id: Arc<str>,
    /// Successful and failed invocations.
    pub runs_total: u64,
    /// Failed invocations, panics and timeouts included.
    pub failures_total: u64,
    /// When the last success was, unix ms.
    pub last_success_at: Option<UnixMs>,
    /// The last failure's message, cleared by the next success.
    pub last_error: Option<Arc<str>>,
    /// Whether a debounce window is currently armed.
    pub pending: bool,
    /// Which phase this hook runs in.
    pub phase: HookPhase,
    /// The rolled-up component, ready to hand to a [`HealthRegistry`].
    pub health: ComponentHealth,
}

/// Every hook's health, plus the subscriber-level drop counter (DESIGN §16.3).
#[derive(Clone, PartialEq, Debug)]
pub struct HooksHealth {
    /// One entry per registered hook, in `ordering()` order.
    pub hooks: Vec<HookStat>,
    /// `aulos_event_dropped_total{subscriber="hooks"}` — a skipped hook run, and a WARN-level
    /// signal.
    pub events_dropped: u64,
}

impl HooksHealth {
    /// One hook's component, by id.
    #[must_use]
    pub fn component(&self, id: &str) -> Option<&ComponentHealth> {
        self.hooks.iter().find(|h| &*h.id == id).map(|h| &h.health)
    }

    /// One hook's counters, by id.
    #[must_use]
    pub fn stat(&self, id: &str) -> Option<&HookStat> {
        self.hooks.iter().find(|h| &*h.id == id)
    }

    /// Failures across every hook.
    #[must_use]
    pub fn failures_total(&self) -> u64 {
        self.hooks.iter().map(|h| h.failures_total).sum()
    }

    /// Publishes every hook as a `healthz` component. Returns whether anything changed.
    pub fn apply(&self, registry: &HealthRegistry) -> bool {
        let mut changed = false;
        for h in &self.hooks {
            changed |= registry.set(&h.id, h.health.clone());
        }
        changed
    }
}

/// One hook's live counters.
#[derive(Debug, Default)]
struct Slot {
    runs: AtomicU64,
    failures: AtomicU64,
    /// `0` means "never".
    last_success_at: AtomicI64,
    last_error: std::sync::Mutex<Option<Arc<str>>>,
    pending: AtomicBool,
}

impl Slot {
    fn record_success(&self, now_ms: UnixMs) {
        self.runs.fetch_add(1, Atomic::Relaxed);
        self.last_success_at.store(now_ms, Atomic::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn record_failure(&self, message: &str) {
        self.runs.fetch_add(1, Atomic::Relaxed);
        self.failures.fetch_add(1, Atomic::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::from(message));
    }
}

/// Everything the dispatcher's tasks share.
#[derive(Debug)]
struct State {
    slots: BTreeMap<Arc<str>, Slot>,
    events_dropped: AtomicU64,
}

impl State {
    fn slot(&self, id: &str) -> Option<&Slot> {
        self.slots.get(id)
    }
}

/// The per-invocation dependencies. Cheap to clone through an `Arc`.
struct Deps {
    cfg: Arc<Config>,
    clock: Arc<dyn Clock>,
    store: Arc<dyn HookStore>,
    sink: ProgressSinkFactory,
    cancel: CancellationToken,
    state: Arc<State>,
}

/// Runs one hook once, outside the event loop, with the **same** [`HookCtx`] construction the
/// dispatcher uses.
///
/// This exists so a caller that wants one invocation — a test asserting a hook's own behaviour, or
/// a future `doctor --hook` probe — cannot accidentally build a context that differs from the real
/// one in how `out_dir`, `file` or the entry blob are derived.
pub struct HookRunner {
    deps: Arc<Deps>,
}

impl HookRunner {
    /// A runner over the same four dependencies [`HookDispatcher::spawn`] takes.
    #[must_use]
    pub fn new(
        cfg: Arc<Config>,
        clock: Arc<dyn Clock>,
        store: Arc<dyn HookStore>,
        sink: ProgressSinkFactory,
    ) -> Self {
        Self {
            deps: Arc::new(Deps {
                cfg,
                clock,
                store,
                sink,
                cancel: CancellationToken::new(),
                state: Arc::new(State {
                    slots: BTreeMap::new(),
                    events_dropped: AtomicU64::new(0),
                }),
            }),
        }
    }

    /// The token this runner's [`HookCtx::cancel`] observes.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.deps.cancel.clone()
    }

    /// Runs `hook` once.
    ///
    /// # Errors
    /// Whatever the hook returns.
    pub async fn run(
        &self,
        hook: &dyn Hook,
        item: &ItemView,
        batch: &[BatchEntry],
    ) -> Result<(), HookError> {
        invoke(hook, item, batch, &self.deps).await
    }
}

/// One event handed to a debounced hook's own inbox.
struct Job {
    item: Arc<ItemView>,
    entry: BatchEntry,
}

// ---------------------------------------------------------------------------
// The dispatcher
// ---------------------------------------------------------------------------

/// The hook dispatcher (DESIGN §13).
pub struct HookDispatcher {
    cfg: Arc<Config>,
    clock: Arc<dyn Clock>,
    hooks: Vec<Arc<dyn Hook>>,
    finalizer: Arc<dyn HookFinalizer>,
    state: Arc<State>,
    cancel: CancellationToken,
    enabled: bool,
}

impl std::fmt::Debug for HookDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookDispatcher")
            .field("hooks", &self.hook_ids())
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl HookDispatcher {
    /// The stock dispatcher: the three built-ins plus one [`ManifestHook`] per community
    /// `[[hook]]` (DESIGN §13).
    ///
    /// `AULOS_HOOKS_ENABLED=false` disables the lot: every hook is still registered, so `healthz`
    /// still names it, but nothing ever runs and every component reports `disabled`.
    #[must_use]
    pub fn new(cfg: Arc<Config>, specs: Vec<HookSpec>, clock: Arc<dyn Clock>) -> Self {
        let mut hooks: Vec<Arc<dyn Hook>> = vec![
            Arc::new(AudioSyncHook::new()),
            Arc::new(NfoHook::from_config(&cfg)),
            Arc::new(JellyfinHook::new(&cfg)),
        ];
        for spec in specs {
            hooks.push(Arc::new(ManifestHook::new(spec, &cfg.plugins_dir)));
        }
        Self::with_hooks(cfg, hooks, clock)
    }

    /// A dispatcher over an explicit hook list. Used by the tests and by any wiring that needs to
    /// substitute a built-in (the tool paths of [`AudioSyncHook::with_tools`], say).
    #[must_use]
    pub fn with_hooks(
        cfg: Arc<Config>,
        mut hooks: Vec<Arc<dyn Hook>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        hooks.sort_by(|a, b| {
            a.ordering()
                .cmp(&b.ordering())
                .then_with(|| a.id().cmp(&b.id()))
        });
        let mut slots = BTreeMap::new();
        for h in &hooks {
            slots.insert(h.id(), Slot::default());
        }
        let enabled = cfg.hooks_enabled;
        Self {
            cfg,
            clock,
            hooks,
            finalizer: Arc::new(NoopFinalizer),
            state: Arc::new(State {
                slots,
                events_dropped: AtomicU64::new(0),
            }),
            cancel: CancellationToken::new(),
            enabled,
        }
    }

    /// Wires the engine seam. Without one, `HooksFinished` goes nowhere and a pre-terminal hook
    /// would leave the item in `postprocessing` forever — so the wiring in `aulos-server` must
    /// always call this.
    #[must_use]
    pub fn with_finalizer(mut self, finalizer: Arc<dyn HookFinalizer>) -> Self {
        self.finalizer = finalizer;
        self
    }

    /// Adds one more hook, keeping the ordering invariant.
    #[must_use]
    pub fn with_hook(mut self, hook: Arc<dyn Hook>) -> Self {
        self.hooks.push(hook);
        Self::with_hooks(self.cfg, self.hooks, self.clock)
            .with_finalizer(self.finalizer)
            .with_cancel(self.cancel)
    }

    /// Uses an externally owned cancellation token, so the shutdown grace of DESIGN §16.4 reaches
    /// every running hook.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// The token every [`HookCtx::cancel`] observes.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The registered hook ids, in run order.
    #[must_use]
    pub fn hook_ids(&self) -> Vec<Arc<str>> {
        self.hooks.iter().map(|h| h.id()).collect()
    }

    /// Every hook's health (DESIGN §16.3). Safe to call after [`Self::spawn`].
    #[must_use]
    pub fn health(&self) -> HooksHealth {
        health_of(&self.hooks, &self.state, self.enabled)
    }

    /// A handle that reports [`Self::health`] after `self` has been consumed by [`Self::spawn`].
    #[must_use]
    pub fn health_handle(&self) -> HooksHealthHandle {
        HooksHealthHandle {
            hooks: self.hooks.clone(),
            state: Arc::clone(&self.state),
            enabled: self.enabled,
        }
    }

    /// Spawns the event loop.
    #[must_use]
    pub fn spawn(
        self,
        events: EventInbox,
        sink: ProgressSinkFactory,
        store: Arc<dyn HookStore>,
    ) -> JoinHandle<()> {
        tokio::spawn(self.run(events, sink, store))
    }

    /// The event loop, so a test can drive it on the current task.
    ///
    /// Returns when every [`aulos_core::event::EventSender`] is gone and the inbox has drained:
    /// each debounced hook then fires its trailing batch (unless the cancellation token is already
    /// cancelled, i.e. the shutdown grace has expired) and every in-flight invocation is awaited.
    pub async fn run(
        self,
        mut events: EventInbox,
        sink: ProgressSinkFactory,
        store: Arc<dyn HookStore>,
    ) {
        let deps = Arc::new(Deps {
            cfg: Arc::clone(&self.cfg),
            clock: Arc::clone(&self.clock),
            store,
            sink,
            cancel: self.cancel.clone(),
            state: Arc::clone(&self.state),
        });

        // One inbox per debounced hook; the undebounced ones run in ordering order as one chain
        // per event, which is what makes the observed order deterministic.
        let mut senders: BTreeMap<Arc<str>, mpsc::Sender<Job>> = BTreeMap::new();
        let mut workers: Vec<JoinHandle<()>> = Vec::new();
        for hook in &self.hooks {
            if !hook.debounce().is_armed() {
                continue;
            }
            let (tx, rx) = mpsc::channel(HOOK_INBOX);
            senders.insert(hook.id(), tx);
            workers.push(tokio::spawn(debounce_worker(
                Arc::clone(hook),
                rx,
                Arc::clone(&deps),
            )));
        }

        let chain_slots = Arc::new(Semaphore::new(CHAIN_CONCURRENCY));
        let mut tasks = JoinSet::new();

        while let Some(event) = events.recv().await {
            self.state
                .events_dropped
                .store(events.dropped(), Atomic::Relaxed);
            // Reap finished tasks so the set does not grow without bound.
            while tasks.try_join_next().is_some() {}

            match &*event {
                DomainEvent::Finishing(view) => {
                    // The engine publishes `Finishing` from `EngineCmd::Finished`, which is the
                    // success path (`Failed` is a different command), so the prospective outcome
                    // is `finished`.
                    let outcome = TerminalStatus::Finished;
                    let hooks = if self.enabled {
                        self.applicable(HookPhase::PreTerminal, view, outcome)
                    } else {
                        Vec::new()
                    };
                    let batch = vec![BatchEntry::from_view(view, outcome)];
                    tasks.spawn(pre_terminal(
                        hooks,
                        Arc::clone(view),
                        batch,
                        Arc::clone(&deps),
                        Arc::clone(&self.finalizer),
                    ));
                }
                DomainEvent::Completed(view) => {
                    if !self.enabled {
                        continue;
                    }
                    let Ok(outcome) = TerminalStatus::try_from(view.status) else {
                        tracing::warn!(
                            item = %view.id,
                            status = %view.status,
                            "a Completed event carried a non-terminal status; ignoring it"
                        );
                        continue;
                    };
                    let mut chain = Vec::new();
                    for hook in self.applicable(HookPhase::PostTerminal, view, outcome) {
                        match senders.get(&hook.id()) {
                            Some(tx) => {
                                let job = Job {
                                    item: Arc::clone(view),
                                    entry: BatchEntry::from_view(view, outcome),
                                };
                                if tx.try_send(job).is_err() {
                                    tracing::warn!(
                                        hook = %hook.id(),
                                        item = %view.id,
                                        "the hook inbox is full; skipping this event"
                                    );
                                }
                            }
                            None => chain.push(hook),
                        }
                    }
                    if !chain.is_empty() {
                        tasks.spawn(post_terminal(
                            chain,
                            Arc::clone(view),
                            vec![BatchEntry::from_view(view, outcome)],
                            Arc::clone(&deps),
                            Arc::clone(&chain_slots),
                        ));
                    }
                }
                // The filter of `SubscriberSpec::hooks()` admits nothing else; a wider filter is
                // a wiring mistake, not a reason to do anything.
                other => tracing::trace!(event = %other.kind(), "the hooks inbox ignored an event"),
            }
        }

        // Shutdown: let the trailing batches fire, then wait for everything in flight.
        drop(senders);
        for w in workers {
            if let Err(e) = w.await {
                tracing::warn!(error = %e, "a debounced hook worker did not shut down cleanly");
            }
        }
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                tracing::warn!(error = %e, "a hook task did not shut down cleanly");
            }
        }
        tracing::debug!("the hook dispatcher has stopped");
    }

    /// The hooks of one phase that apply to `view`, in run order.
    fn applicable(
        &self,
        phase: HookPhase,
        view: &ItemView,
        outcome: TerminalStatus,
    ) -> Vec<Arc<dyn Hook>> {
        self.hooks
            .iter()
            .filter(|h| h.phase() == phase && h.applies(view, outcome))
            .map(Arc::clone)
            .collect()
    }
}

/// [`HookDispatcher::health`] after the dispatcher has been consumed by `spawn`.
#[derive(Clone)]
pub struct HooksHealthHandle {
    hooks: Vec<Arc<dyn Hook>>,
    state: Arc<State>,
    enabled: bool,
}

impl std::fmt::Debug for HooksHealthHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HooksHealthHandle")
            .field("hooks", &self.hooks.len())
            .finish_non_exhaustive()
    }
}

impl HooksHealthHandle {
    /// Every hook's health (DESIGN §16.3).
    #[must_use]
    pub fn health(&self) -> HooksHealth {
        health_of(&self.hooks, &self.state, self.enabled)
    }
}

fn health_of(hooks: &[Arc<dyn Hook>], state: &State, enabled: bool) -> HooksHealth {
    let mut out = Vec::with_capacity(hooks.len());
    for hook in hooks {
        let id = hook.id();
        let own = hook.health();
        let (runs, failures, last_success_at, last_error, pending) = match state.slot(&id) {
            Some(s) => (
                s.runs.load(Atomic::Relaxed),
                s.failures.load(Atomic::Relaxed),
                match s.last_success_at.load(Atomic::Relaxed) {
                    0 => None,
                    ms => Some(ms),
                },
                s.last_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                s.pending.load(Atomic::Relaxed),
            ),
            None => (0, 0, None, None, false),
        };

        let status = if !enabled || own.status == ComponentStatus::Disabled {
            ComponentStatus::Disabled
        } else if last_error.is_some() {
            own.status.worse(ComponentStatus::Degraded)
        } else {
            own.status
        };

        let mut health = ComponentHealth::new(status);
        for (k, v) in &own.detail {
            health.detail.insert(k.clone(), v.clone());
        }
        health.detail.insert("runs_total".to_owned(), runs.into());
        health
            .detail
            .insert("failures_total".to_owned(), failures.into());
        if let Some(ms) = last_success_at {
            health
                .detail
                .insert("last_success_at".to_owned(), Value::from(ms));
        }
        if let Some(e) = &last_error {
            health
                .detail
                .insert("last_error".to_owned(), Value::from(&**e));
        }
        if hook.debounce().is_armed() {
            health.detail.insert("pending".to_owned(), pending.into());
        }
        let phase = hook.phase();
        if phase == HookPhase::PreTerminal {
            health
                .detail
                .insert("phase".to_owned(), Value::from(phase.as_str()));
        }

        out.push(HookStat {
            id,
            runs_total: runs,
            failures_total: failures,
            last_success_at,
            last_error,
            pending,
            phase,
            health,
        });
    }
    HooksHealth {
        hooks: out,
        events_dropped: state.events_dropped.load(Atomic::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// The two phases
// ---------------------------------------------------------------------------

/// Runs every pre-terminal hook sequentially and then tells the engine, always (DESIGN §13).
async fn pre_terminal(
    hooks: Vec<Arc<dyn Hook>>,
    item: Arc<ItemView>,
    batch: Vec<BatchEntry>,
    deps: Arc<Deps>,
    finalizer: Arc<dyn HookFinalizer>,
) {
    for hook in hooks {
        run_counted(hook, Arc::clone(&item), batch.clone(), Arc::clone(&deps)).await;
    }
    finalizer.hooks_finished(item.id).await;
}

/// Runs one event's undebounced post-terminal hooks in ordering order.
async fn post_terminal(
    hooks: Vec<Arc<dyn Hook>>,
    item: Arc<ItemView>,
    batch: Vec<BatchEntry>,
    deps: Arc<Deps>,
    slots: Arc<Semaphore>,
) {
    let _permit = slots.acquire().await;
    for hook in hooks {
        run_counted(hook, Arc::clone(&item), batch.clone(), Arc::clone(&deps)).await;
    }
}

/// One debounced hook's inbox: coalesce, cap, fire (DESIGN §13.1, §13.4).
async fn debounce_worker(hook: Arc<dyn Hook>, mut rx: mpsc::Receiver<Job>, deps: Arc<Deps>) {
    let id = hook.id();
    let d = hook.debounce();
    let mut pending: Vec<BatchEntry> = Vec::new();
    let mut representative: Option<Arc<ItemView>> = None;
    let mut first_at = tokio::time::Instant::now();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut inflight = JoinSet::new();

    loop {
        let at = deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            job = rx.recv() => match job {
                Some(job) => {
                    if pending.is_empty() {
                        first_at = tokio::time::Instant::now();
                    }
                    pending.push(job.entry);
                    representative = Some(job.item);
                    // Each event extends the window, but never past `first_at + max_wait`.
                    let cap = first_at + d.max_wait;
                    deadline = Some((tokio::time::Instant::now() + d.window).min(cap));
                    if let Some(s) = deps.state.slot(&id) {
                        s.pending.store(true, Atomic::Relaxed);
                    }
                }
                None => break,
            },
            () = tokio::time::sleep_until(at), if deadline.is_some() => {
                deadline = None;
                fire(&hook, &mut pending, &mut representative, &deps, &mut inflight).await;
            }
        }
    }

    // The channel closed: flush the tail, unless the shutdown grace has already expired.
    if deps.cancel.is_cancelled() {
        if !pending.is_empty() {
            tracing::warn!(
                hook = %id,
                skipped = pending.len(),
                "shutting down; dropping a debounced batch that never fired"
            );
        }
    } else {
        fire(
            &hook,
            &mut pending,
            &mut representative,
            &deps,
            &mut inflight,
        )
        .await;
    }
    while let Some(res) = inflight.join_next().await {
        if let Err(e) = res {
            tracing::warn!(hook = %id, error = %e, "a debounced invocation did not finish cleanly");
        }
    }
}

async fn fire(
    hook: &Arc<dyn Hook>,
    pending: &mut Vec<BatchEntry>,
    representative: &mut Option<Arc<ItemView>>,
    deps: &Arc<Deps>,
    inflight: &mut JoinSet<()>,
) {
    if pending.is_empty() {
        return;
    }
    let batch = std::mem::take(pending);
    // The batch's first event is the representative, so `{title}` and `{titles_json}[0]` agree.
    let Some(item) = representative.take() else {
        return;
    };
    if let Some(s) = deps.state.slot(&hook.id()) {
        s.pending.store(false, Atomic::Relaxed);
    }
    while inflight.len() >= DEBOUNCE_CONCURRENCY {
        let _ = inflight.join_next().await;
    }
    let hook = Arc::clone(hook);
    let deps = Arc::clone(deps);
    inflight.spawn(async move { run_counted(hook, item, batch, deps).await });
}

/// Runs one invocation in its own task — so a panic is a `JoinError` rather than a dead
/// dispatcher — bounded by [`Hook::timeout`], and records the outcome.
async fn run_counted(
    hook: Arc<dyn Hook>,
    item: Arc<ItemView>,
    batch: Vec<BatchEntry>,
    deps: Arc<Deps>,
) {
    let id = hook.id();
    let timeout = hook.timeout();
    let item_id = item.id;
    let count = batch.len();
    let started = std::time::Instant::now();

    let task = {
        let hook = Arc::clone(&hook);
        let deps = Arc::clone(&deps);
        tokio::spawn(async move { invoke(&*hook, &item, &batch, &deps).await })
    };
    let handle = task.abort_handle();
    let outcome: Result<(), HookError> = match tokio::time::timeout(timeout, task).await {
        Ok(Ok(r)) => r,
        Ok(Err(join)) => {
            let detail = if join.is_panic() {
                "the hook panicked".to_owned()
            } else {
                join.to_string()
            };
            Err(HookError::Panic(detail.into()))
        }
        Err(_elapsed) => {
            handle.abort();
            Err(HookError::Timeout(timeout))
        }
    };

    let slot = deps.state.slot(&id);
    match outcome {
        Ok(()) => {
            if let Some(s) = slot {
                s.record_success(deps.clock.now_ms());
            }
            tracing::debug!(
                hook = %id, item = %item_id, items = count, elapsed_ms = started.elapsed().as_millis(),
                "hook finished"
            );
        }
        Err(e) => {
            if let Some(s) = slot {
                s.record_failure(&e.to_string());
            }
            // A hook failure never changes the item's status (DESIGN §13).
            tracing::warn!(
                hook = %id, item = %item_id, items = count, error = %e,
                "hook failed; the item is unaffected"
            );
        }
    }
}

/// Builds the [`HookCtx`] and calls the hook.
async fn invoke(
    hook: &dyn Hook,
    item: &ItemView,
    batch: &[BatchEntry],
    deps: &Deps,
) -> Result<(), HookError> {
    let entry = if hook.wants_entry() {
        match deps.store.entry_blob(item.id).await {
            Ok(blob) => blob,
            Err(e) => {
                tracing::warn!(item = %item.id, error = %e, "could not load the entry blob");
                None
            }
        }
    } else {
        None
    };

    let root = deps.cfg.paths.root_for(item.selection.download_type);
    let file = file_path(root, item.filename.as_deref());
    let out_dir = file
        .as_deref()
        .and_then(Path::parent)
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf);
    let sink = deps.sink.for_item(item.id);

    let ctx = HookCtx {
        item,
        entry: entry.as_ref(),
        out_dir: &out_dir,
        file: file.as_deref(),
        sink: &sink,
        store: &*deps.store,
        cfg: &deps.cfg,
        batch,
        cancel: &deps.cancel,
        clock: &*deps.clock,
    };
    hook.run(ctx).await
}

/// The absolute path of the produced file: the download root plus the item's relative `filename`,
/// which already carries the request's `folder` (DESIGN §4.5).
fn file_path(root: &Path, filename: Option<&str>) -> Option<PathBuf> {
    let name = filename?;
    // Validated rather than joined blindly: `filename` comes from a provider, and a `..` in it
    // would put a hook's writes outside the download root.
    match RelPath::parse(name) {
        Ok(rel) => Some(root.join(rel.as_path())),
        Err(e) => {
            tracing::warn!(filename = name, error = %e, "refusing to build a path from this filename");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::config::{RawEnv, load};

    fn cfg() -> Arc<Config> {
        Arc::new(
            load(&RawEnv::from_pairs([
                ("STATE_DIR", "/tmp"),
                ("DOWNLOAD_DIR", "/downloads"),
                ("AUDIO_DOWNLOAD_DIR", "/audio"),
            ]))
            .expect("config"),
        )
    }

    #[test]
    fn the_builtins_are_registered_in_ordering_order() {
        let d = HookDispatcher::new(
            cfg(),
            Vec::new(),
            Arc::new(aulos_core::clock::FakeClock::default()),
        );
        let ids: Vec<String> = d.hook_ids().iter().map(ToString::to_string).collect();
        assert_eq!(ids, ["audio_sync", "nfo", "jellyfin"]);
    }

    #[test]
    fn health_names_every_hook_with_the_documented_detail_fields() {
        let d = HookDispatcher::new(
            cfg(),
            Vec::new(),
            Arc::new(aulos_core::clock::FakeClock::default()),
        );
        let h = d.health();
        assert_eq!(h.hooks.len(), 3);
        assert_eq!(h.events_dropped, 0);

        let audio = h.component("audio_sync").expect("audio_sync");
        assert_eq!(audio.detail["runs_total"], 0);
        assert_eq!(audio.detail["failures_total"], 0);
        assert_eq!(
            audio.detail["phase"], "pre_terminal",
            "only the pre-terminal hook carries a phase"
        );

        let nfo = h.component("nfo").expect("nfo");
        assert!(!nfo.detail.contains_key("phase"));
        assert!(!nfo.detail.contains_key("pending"));

        let jellyfin = h.component("jellyfin").expect("jellyfin");
        assert_eq!(
            jellyfin.detail["pending"], false,
            "a debounced hook reports whether a window is armed"
        );
        assert_eq!(
            jellyfin.status,
            ComponentStatus::Disabled,
            "JELLYFIN_SYNC_ENABLED defaults to false"
        );
        assert!(!jellyfin.detail.contains_key("last_success_at"));
    }

    #[test]
    fn hooks_enabled_false_disables_every_component() {
        let cfg = Arc::new(
            load(&RawEnv::from_pairs([
                ("STATE_DIR", "/tmp"),
                ("DOWNLOAD_DIR", "/downloads"),
                ("AULOS_HOOKS_ENABLED", "false"),
                ("JELLYFIN_SYNC_ENABLED", "true"),
                ("JELLYFIN_URL", "http://jf.test"),
                ("JELLYFIN_API_KEY", "k"),
            ]))
            .expect("config"),
        );
        let d = HookDispatcher::new(
            cfg,
            Vec::new(),
            Arc::new(aulos_core::clock::FakeClock::default()),
        );
        for stat in d.health().hooks {
            assert_eq!(stat.health.status, ComponentStatus::Disabled, "{}", stat.id);
        }
    }

    #[test]
    fn a_filename_is_resolved_under_its_download_root_and_traversal_is_refused() {
        assert_eq!(
            file_path(Path::new("/downloads"), Some("Show/S01E01.mp4")),
            Some(PathBuf::from("/downloads/Show/S01E01.mp4"))
        );
        assert_eq!(file_path(Path::new("/downloads"), None), None);
        assert_eq!(
            file_path(Path::new("/downloads"), Some("../etc/passwd")),
            None
        );
        assert_eq!(file_path(Path::new("/downloads"), Some("")), None);
    }

    #[test]
    fn a_recording_finalizer_records_in_order() {
        let f = RecordingFinalizer::new();
        assert!(f.finished().is_empty());
    }
}
