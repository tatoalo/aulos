//! The command vocabulary, the handle every other crate holds, and the result types
//! (DESIGN §8.1).
//!
//! Everything that mutates queue state is one of these messages. There is no second entry point,
//! no shared lock and no "just read the store and patch it" path: an item's status, its slot, its
//! group's counters and the frame the client sees all move together because they all move inside
//! one [`EngineCmd`].

use std::sync::Arc;

use aulos_core::{
    DownloadRequest, ErrorCode, FileRef, FileSlot, GroupId, ItemId, Kind, PortError, SourceRef,
    WireError,
};
use aulos_provider::{MediaEntry, Outcome, ProviderError, Stage};
use aulos_store::StoreError;
use tokio::sync::{mpsc, oneshot};

/// The ack type shared by `Start`/`Pause`/`Cancel`/`Retry`/`Delete`/`Clear`/`CancelResolve`.
///
/// A type alias, not a struct: there is exactly one actions result shape (DESIGN §8.1).
pub type AckActions = oneshot::Sender<ActionsResult>;

/// The `EngineCmd` channel budget (DESIGN §2.3). The API handler awaits a slot; nothing is dropped.
pub const ENGINE_CHANNEL_CAPACITY: usize = 1_024;

/// How many children one expansion batch inserts (DESIGN §8.4).
pub const CHILD_BATCH: usize = 100;

/// Everything that mutates the queue (DESIGN §8.1).
///
/// The variants below the `// internal` line are sent by the engine's own spawned tasks and by the
/// aggregator; nothing outside this crate constructs them, but they are public because
/// [`EngineHandle`] exposes the two the aggregator needs ([`EngineHandle::stage`] and
/// [`EngineHandle::file`]).
///
/// **v1.0: not implemented, see BRIEF** — DESIGN §8.1's `Watch`, `Unwatch` and `ConnClosed`
/// variants and the `ConnId` they carry are CUT along with the WS watch registry, so no
/// connection→groups map exists anywhere in the process.
#[non_exhaustive]
pub enum EngineCmd {
    /// Validate, insert as `resolving` and ack, then resolve in the background (DESIGN §8.3).
    Add {
        /// One per URL. Capped at `AULOS_MAX_BATCH_URLS`.
        requests: Vec<DownloadRequest>,
        /// Attribution for every item in the batch.
        source: SourceRef,
        /// The ids, the duplicates and the add generation.
        ack: oneshot::Sender<Result<AddOutcome, AddError>>,
    },
    /// The v1 shim's bounded synchronous pre-resolve (DESIGN §11.2).
    ///
    /// Completes when every listed id has left `resolving`; ids already out of `resolving` are
    /// reported immediately. The **caller** owns the deadline (`tokio::time::timeout`), so a slow
    /// resolve cannot pin engine state.
    WaitResolved {
        /// The ids to wait for.
        ids: Vec<ItemId>,
        /// One report per id, in request order.
        ack: oneshot::Sender<Vec<ResolveReport>>,
    },
    /// `queued(!auto_start)` → `queued(auto_start)`.
    Start {
        /// The items.
        ids: Vec<ItemId>,
        /// The result.
        ack: AckActions,
    },
    /// Un-schedule, or park a running job keeping its partial file (DESIGN §8.7).
    Pause {
        /// The items.
        ids: Vec<ItemId>,
        /// The result.
        ack: AckActions,
    },
    /// Cancel, from any non-terminal state, idempotently (DESIGN §8.7).
    Cancel {
        /// The items.
        ids: Vec<ItemId>,
        /// The result.
        ack: AckActions,
    },
    /// `error | canceled` → `queued`, `attempt += 1` (DESIGN §8.8).
    Retry {
        /// The items.
        ids: Vec<ItemId>,
        /// The result.
        ack: AckActions,
    },
    /// Delete rows, and optionally their files (DESIGN §8.10).
    Delete {
        /// The items. A group takes its children with it.
        ids: Vec<ItemId>,
        /// `None` means "use `DELETE_FILE_ON_TRASHCAN`". Singular, everywhere.
        delete_file: Option<bool>,
        /// The result.
        ack: AckActions,
    },
    /// Delete every terminal row, publishing `RemoveReason::Cleared` (DESIGN §8.10).
    ///
    /// Addition to DESIGN §8.1: `RemoveReason::Cleared` has no other producer, and both the v1
    /// `POST <p>delete` "clear completed" call and the v2 clear route need it.
    Clear {
        /// Whether to delete the files too. `None` means `DELETE_FILE_ON_TRASHCAN`.
        delete_file: Option<bool>,
        /// The result.
        ack: AckActions,
    },
    /// v1 `cancel-add`, v2 `cancel-resolve` (DESIGN §8.1).
    CancelResolve {
        /// What to cancel.
        scope: CancelScope,
        /// The result.
        ack: AckActions,
    },
    /// A hook's engine-mediated writeback (DESIGN §13.3).
    ///
    /// This is the only way `aulos-hooks` can touch a row, and it is why `set_size` is not a direct
    /// SQLite write. The engine persists it, updates its item cache, and publishes
    /// `StatusChanged { from == to }` — the generic "this persisted row changed, re-diff it"
    /// signal — so the aggregator emits a `delta` carrying exactly the changed field.
    HookWrite {
        /// The row.
        id: ItemId,
        /// What to write.
        write: HookWrite,
        /// Whether it landed.
        ack: oneshot::Sender<Result<(), PortError>>,
    },
    /// Pre-terminal hooks for this item are done; finalise it (DESIGN §13).
    ///
    /// Deviation from DESIGN §8.1, forced by `aulos_hooks::HookFinalizer` carrying only the id
    /// (see `docs/INTEGRATION-NOTES.md`, WP-11): the outcome is `None` on the wiring path and the
    /// engine pairs the id back up with the [`Outcome`] it parked in `pending_hooks`. A caller
    /// that *does* have the outcome (the engine's own timeout path) passes it.
    HooksFinished {
        /// The row.
        id: ItemId,
        /// The outcome, or `None` to use the parked one.
        outcome: Option<Box<Outcome>>,
    },

    // ----------------------------------------------------------------- internal
    /// A resolution finished (DESIGN §8.4).
    Resolved {
        /// The row.
        id: ItemId,
        /// What the provider answered.
        result: Result<Vec<MediaEntry>, ProviderError>,
        /// The bookkeeping the engine needs to decide about fall-through and redirects.
        meta: Box<ResolveMeta>,
    },
    /// Insert the next batch of one expansion's children (DESIGN §8.4).
    ///
    /// Addition to DESIGN §8.1: expansion is driven by self-messages rather than one long
    /// `await` inside the `Resolved` handler, so `CancelResolve` can interrupt a 500-child
    /// expansion between batches — which is what "marks the not-yet-created children of every
    /// in-flight expansion cancelled" requires.
    ExpandNext {
        /// The group being expanded.
        group: GroupId,
    },
    /// A provider reported a stage transition, forwarded by the aggregator (DESIGN §15.1).
    Stage {
        /// The row.
        id: ItemId,
        /// The new stage.
        stage: Stage,
        /// Text for `ItemView.msg`. `None` leaves the previous message alone.
        msg: Option<Box<str>>,
    },
    /// A provider produced an auxiliary file, forwarded by the aggregator (DESIGN §15.1).
    ///
    /// Addition to DESIGN §8.1's list, which omits it while §15.1 requires the aggregator to
    /// forward `ProgressMsg::File` "to the engine (persisted)". Without it `WriteOp::PushFile`
    /// would have no caller.
    File {
        /// The row.
        id: ItemId,
        /// Which list.
        slot: FileSlot,
        /// The file.
        file: Box<FileRef>,
    },
    /// A download succeeded.
    Finished {
        /// The row.
        id: ItemId,
        /// What it produced.
        outcome: Box<Outcome>,
    },
    /// A download failed.
    Failed {
        /// The row.
        id: ItemId,
        /// Why.
        err: Box<ProviderError>,
    },
    /// A slot became free; re-run the scheduler.
    SlotFreed,
    /// 1 Hz: `clear_after`, retry backoffs, group drift, the pre-terminal safety net.
    Tick,
    /// Stop: cancel every in-flight job, hand the interrupted rows to the next boot, ack, and
    /// **end the loop** (DESIGN §16.4 steps 5–6).
    ///
    /// This is the only command that terminates [`crate::Engine::run`]. The engine hands a clone
    /// of its own sender to every job task it spawns, so `rx.recv()` cannot return `None` while a
    /// job is alive however many [`EngineHandle`]s the process has dropped — without this the
    /// shutdown has to reach in from outside and abort the task.
    Shutdown {
        /// What was handed back.
        ack: oneshot::Sender<ShutdownReport>,
    },
}

/// What one [`EngineCmd::Shutdown`] did (DESIGN §16.4 step 6).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ShutdownReport {
    /// How many rows were still resolving or running and were handed to the next boot.
    pub interrupted: usize,
    /// Whether that write reached the database. `false` means the next boot re-queues them from
    /// their stored `downloading` status instead, which DESIGN §8.9 also handles.
    pub persisted: bool,
}

/// The `msg` an interrupted row carries into the next boot (DESIGN §16.4 step 6).
pub const SHUTDOWN_MSG: &str = "Interrupted by shutdown";

impl EngineCmd {
    /// A stable name for logs and for the command-coverage test.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::WaitResolved { .. } => "wait_resolved",
            Self::Start { .. } => "start",
            Self::Pause { .. } => "pause",
            Self::Cancel { .. } => "cancel",
            Self::Retry { .. } => "retry",
            Self::Delete { .. } => "delete",
            Self::Clear { .. } => "clear",
            Self::CancelResolve { .. } => "cancel_resolve",
            Self::HookWrite { .. } => "hook_write",
            Self::HooksFinished { .. } => "hooks_finished",
            Self::Resolved { .. } => "resolved",
            Self::ExpandNext { .. } => "expand_next",
            Self::Stage { .. } => "stage",
            Self::File { .. } => "file",
            Self::Finished { .. } => "finished",
            Self::Failed { .. } => "failed",
            Self::SlotFreed => "slot_freed",
            Self::Tick => "tick",
            Self::Shutdown { .. } => "shutdown",
        }
    }
}

impl std::fmt::Debug for EngineCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineCmd")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// What the engine needs to remember about a resolution while it is in flight (DESIGN §8.4, §6.4).
#[derive(Clone, Debug)]
pub struct ResolveMeta {
    /// The add generation this work belongs to (DESIGN §8.1, [`CancelScope`]).
    ///
    /// One per `Add`, so `CancelScope::Generation(n)` isolates a single add.
    pub generation: u64,
    /// The cancel epoch the attempt was spawned under, so a result that a [`CancelScope::All`]
    /// has already condemned is dropped instead of applied. Distinct from `generation`: a later
    /// add must not condemn an earlier one's in-flight resolution.
    pub epoch: u64,
    /// How many redirects deep this resolution is, capped at `AULOS_RESOLVE_MAX_DEPTH`.
    pub depth: u32,
    /// Whether the one documented runner-up fall-through has already been used (DESIGN §6.4).
    pub fell_through: bool,
    /// The provider that ran.
    pub provider: aulos_core::ProviderId,
    /// The runner-up, if any, and whether it was `Ready`.
    pub runner_up: Option<aulos_core::ProviderId>,
    /// The URLs already visited, so the legacy `already`-URL recursion guard is kept.
    pub seen: Vec<Box<str>>,
}

/// The two writes a hook is allowed to make (DESIGN §13.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookWrite {
    /// The produced file was rewritten and is now this many bytes.
    Size(u64),
    /// The NFO hook has consumed the provider entry.
    DropEntryBlob,
}

/// What `POST api/v2/downloads` acknowledges (DESIGN §8.3, PROTOCOL §4.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AddOutcome {
    /// The minted ids, in request order, minus any deduped request.
    pub ids: Vec<ItemId>,
    /// The requests that matched a live item instead of creating one.
    pub duplicates: Vec<Duplicate>,
    /// The add generation, so a v2 caller can later send `CancelScope::Generation`.
    pub generation: u64,
}

/// One request that matched a live item (DESIGN §8.5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Duplicate {
    /// The URL the caller sent.
    pub url: Arc<str>,
    /// The live item it matched.
    pub existing_id: ItemId,
}

/// One entry per id handed to [`EngineCmd::WaitResolved`] (DESIGN §8.1, §11.2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResolveReport {
    /// The id that was waited on.
    pub id: ItemId,
    /// What it turned out to be. A playlist reports `Kind::Group`.
    pub kind: Kind,
    /// `Ok` when resolution succeeded, otherwise the item's terminal error.
    pub outcome: Result<(), WireError>,
}

/// The result of every action command (DESIGN §8.1).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ActionsResult {
    /// The ids the action was applied to, including cascaded group children.
    pub applied: Vec<ItemId>,
    /// The ids it was not, each with a reason.
    pub skipped: Vec<Skipped>,
}

impl ActionsResult {
    /// Whether nothing was applied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }

    /// Records a skip.
    pub fn skip(&mut self, id: ItemId, reason: SkipReason) {
        self.skipped.push(Skipped { id, reason });
    }
}

/// One id an action did not apply to (DESIGN §8.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub struct Skipped {
    /// The id.
    pub id: ItemId,
    /// Why.
    pub reason: SkipReason,
}

/// Why an action did not apply (DESIGN §8.1, PROTOCOL §4.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// No such id.
    NotFound,
    /// The item is already `finished`/`error`/`canceled`.
    AlreadyTerminal,
    /// It cannot be cancelled from this state.
    NotCancelable,
    /// It cannot be started from this state.
    NotStartable,
    /// It cannot be retried from this state.
    NotRetryable,
    /// It cannot be paused from this state (`resolving`, or terminal).
    NotPausable,
}

impl SkipReason {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::AlreadyTerminal => "already_terminal",
            Self::NotCancelable => "not_cancelable",
            Self::NotStartable => "not_startable",
            Self::NotRetryable => "not_retryable",
            Self::NotPausable => "not_pausable",
        }
    }

    /// Every value.
    pub const ALL: [Self; 6] = [
        Self::NotFound,
        Self::AlreadyTerminal,
        Self::NotCancelable,
        Self::NotStartable,
        Self::NotRetryable,
        Self::NotPausable,
    ];
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The five actions `POST api/v2/items/actions` accepts (DESIGN §8.7).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Schedule a `queued(auto_start = false)` item.
    Start,
    /// Un-schedule, or park a running job keeping its partial file.
    Pause,
    /// Cancel.
    Cancel,
    /// Re-queue a terminal item.
    Retry,
    /// Delete the row, and optionally the files.
    Delete,
}

impl Action {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Cancel => "cancel",
            Self::Retry => "retry",
            Self::Delete => "delete",
        }
    }

    /// Every action, in the order DESIGN §8.7 lists them.
    pub const ALL: [Self; 5] = [
        Self::Start,
        Self::Pause,
        Self::Cancel,
        Self::Retry,
        Self::Delete,
    ];
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a `cancel-add` cancels (DESIGN §8.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelScope {
    /// Bump `add_generation`, abort **every** in-flight resolve, and mark the not-yet-created
    /// children of every in-flight expansion cancelled. What v1 `POST <p>cancel-add` sends —
    /// legacy's `cancel_add()` took no body, so a v1 caller has no generation to pass.
    All,
    /// Only resolutions and expansions belonging to this [`AddOutcome::generation`].
    Generation(u64),
}

/// Why an add failed (DESIGN §8.3).
///
/// One variant per HTTP status, with the per-field detail carried as [`WireError`]s so the API
/// layer maps `errors[0].code` straight onto the DESIGN §8.3 table:
/// `validation_failed` → 400, `unknown_preset` → 400, `overrides_disabled` → 400,
/// `folder_invalid` → 400, `unsupported_url` → 400, `conflict` → 409,
/// `payload_too_large` → 413.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum AddError {
    /// Request `index` failed validation. Every failing field is reported, in legacy check order.
    #[error("request {index} is invalid: {}", first_message(errors))]
    Invalid {
        /// Which request in the batch.
        index: usize,
        /// One entry per failing field, each with its own code and the verbatim legacy message.
        errors: Vec<WireError>,
    },
    /// `AULOS_DEDUPE_MODE=strict` and request `index` duplicates a live item.
    #[error("request {index} duplicates item {existing_id}")]
    Duplicate {
        /// Which request in the batch.
        index: usize,
        /// The live item it matched.
        existing_id: ItemId,
    },
    /// More than `AULOS_MAX_BATCH_URLS` requests in one call.
    #[error("{got} urls exceeds the {max} per-batch limit")]
    TooManyUrls {
        /// The configured cap.
        max: u32,
        /// How many were sent.
        got: usize,
    },
    /// The store or the engine could not accept the add.
    #[error("{0}")]
    Unavailable(Box<str>),
}

/// The message of the first [`WireError`] in a list, for [`AddError`]'s `Display`.
fn first_message(errors: &[WireError]) -> &str {
    errors.first().map_or("no detail", |e| &e.message)
}

impl AddError {
    /// The wire code this failure answers with (DESIGN §8.3).
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Invalid { errors, .. } => errors
                .first()
                .map_or(ErrorCode::ValidationFailed, |e| e.code),
            Self::Duplicate { .. } => ErrorCode::Conflict,
            Self::TooManyUrls { .. } => ErrorCode::PayloadTooLarge,
            Self::Unavailable(_) => ErrorCode::StateUnavailable,
        }
    }

    /// A single-field validation failure.
    #[must_use]
    pub fn field(index: usize, code: ErrorCode, field: &str, message: impl Into<Arc<str>>) -> Self {
        Self::Invalid {
            index,
            errors: vec![WireError::field(code, field, message)],
        }
    }
}

/// Anything the engine itself can fail with.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The store reported a failure.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The engine task has stopped.
    #[error("the queue engine is unavailable")]
    Closed,
}

/// The cheap-to-clone handle every other crate holds (DESIGN §8.1).
///
/// Every method is one message plus one `oneshot`. A closed channel is reported as
/// [`EngineError::Closed`] or, for the ports, as [`PortError::Unavailable`]: a caller racing
/// shutdown gets an honest error rather than a hang.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    tx: mpsc::Sender<EngineCmd>,
    beats: crate::watchdog::Heartbeats,
}

impl EngineHandle {
    /// Wraps a sender. Used by [`crate::Engine::new`]; nothing else needs it.
    #[must_use]
    pub(crate) fn new(tx: mpsc::Sender<EngineCmd>, beats: crate::watchdog::Heartbeats) -> Self {
        Self { tx, beats }
    }

    /// The per-job liveness table the stall watchdog reads (DESIGN §8.11).
    ///
    /// The aggregator (WP-13) calls [`crate::watchdog::Heartbeats::frame`] on **every** progress
    /// frame it receives, which is what keeps a genuinely stalled job distinguishable from a busy
    /// one.
    #[must_use]
    pub fn heartbeats(&self) -> &crate::watchdog::Heartbeats {
        &self.beats
    }

    /// Whether the engine task is still running.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Validates and inserts a batch, then resolves in the background (DESIGN §8.3).
    ///
    /// # Errors
    /// [`AddError`] for a rejected batch; [`AddError::Unavailable`] when the engine has stopped.
    pub async fn add(
        &self,
        requests: Vec<DownloadRequest>,
        source: SourceRef,
    ) -> Result<AddOutcome, AddError> {
        let (ack, reply) = oneshot::channel();
        self.tx
            .send(EngineCmd::Add {
                requests,
                source,
                ack,
            })
            .await
            .map_err(|_| AddError::Unavailable("the queue engine is unavailable".into()))?;
        reply
            .await
            .map_err(|_| AddError::Unavailable("the queue engine is unavailable".into()))?
    }

    /// Applies one of the five actions to a list of ids (DESIGN §8.7).
    ///
    /// `delete_file` is only read by [`Action::Delete`]; `None` means `DELETE_FILE_ON_TRASHCAN`.
    /// An unreachable engine answers with every id skipped as [`SkipReason::NotFound`], so a
    /// caller never has to distinguish "gone" from "shutting down" on a best-effort path.
    pub async fn actions(
        &self,
        action: Action,
        ids: Vec<ItemId>,
        delete_file: Option<bool>,
    ) -> ActionsResult {
        let (ack, reply) = oneshot::channel();
        let all = ids.clone();
        let cmd = match action {
            Action::Start => EngineCmd::Start { ids, ack },
            Action::Pause => EngineCmd::Pause { ids, ack },
            Action::Cancel => EngineCmd::Cancel { ids, ack },
            Action::Retry => EngineCmd::Retry { ids, ack },
            Action::Delete => EngineCmd::Delete {
                ids,
                delete_file,
                ack,
            },
        };
        if self.tx.send(cmd).await.is_err() {
            return unavailable(all);
        }
        reply.await.unwrap_or_else(|_| unavailable(all))
    }

    /// Deletes every terminal row (DESIGN §8.10).
    pub async fn clear(&self, delete_file: Option<bool>) -> ActionsResult {
        let (ack, reply) = oneshot::channel();
        if self
            .tx
            .send(EngineCmd::Clear { delete_file, ack })
            .await
            .is_err()
        {
            return ActionsResult::default();
        }
        reply.await.unwrap_or_default()
    }

    /// Aborts in-flight resolutions (DESIGN §8.1).
    pub async fn cancel_resolve(&self, scope: CancelScope) -> ActionsResult {
        let (ack, reply) = oneshot::channel();
        if self
            .tx
            .send(EngineCmd::CancelResolve { scope, ack })
            .await
            .is_err()
        {
            return ActionsResult::default();
        }
        reply.await.unwrap_or_default()
    }

    /// The v1 shim's bounded pre-resolve (DESIGN §11.2). The **caller** applies the timeout.
    ///
    /// Ids already out of `resolving` are reported immediately; the rest are reported as they
    /// transition. An unreachable engine answers with an empty vector.
    pub async fn wait_resolved(&self, ids: Vec<ItemId>) -> Vec<ResolveReport> {
        let (ack, reply) = oneshot::channel();
        if self
            .tx
            .send(EngineCmd::WaitResolved { ids, ack })
            .await
            .is_err()
        {
            return Vec::new();
        }
        reply.await.unwrap_or_default()
    }

    /// One of the two engine-mediated hook writes (DESIGN §13.3).
    ///
    /// # Errors
    /// [`PortError::NotFound`] when the row is gone, [`PortError::Unavailable`] at shutdown,
    /// [`PortError::Store`] when the write failed.
    pub async fn hook_write(&self, id: ItemId, write: HookWrite) -> Result<(), PortError> {
        let (ack, reply) = oneshot::channel();
        self.tx
            .send(EngineCmd::HookWrite { id, write, ack })
            .await
            .map_err(|_| PortError::Unavailable)?;
        reply.await.map_err(|_| PortError::Unavailable)?
    }

    /// Pre-terminal hooks for `id` are done; finalise it (DESIGN §13).
    ///
    /// This is what `aulos-server`'s `HookFinalizer` adapter calls. Sending it for an item that
    /// is not waiting on hooks is a no-op.
    pub async fn hooks_finished(&self, id: ItemId) {
        let _ = self
            .tx
            .send(EngineCmd::HooksFinished { id, outcome: None })
            .await;
    }

    /// Stops the engine: cancels every in-flight job, hands the interrupted rows back to the next
    /// boot with `msg = `[`SHUTDOWN_MSG`], and ends [`crate::Engine::run`] (DESIGN §16.4 steps
    /// 5–6).
    ///
    /// The rows are written by the engine itself, before it stops handling commands, so the
    /// `canceled` a job's own cancellation would otherwise produce is never written at all —
    /// `canceled` is terminal, and a terminal row is one the next boot will not resume.
    ///
    /// Idempotent from the caller's side: a second call, or a call after the engine has already
    /// stopped, reports [`ShutdownReport::default`] rather than failing.
    pub async fn shutdown(&self) -> ShutdownReport {
        let (ack, reply) = oneshot::channel();
        if self.tx.send(EngineCmd::Shutdown { ack }).await.is_err() {
            return ShutdownReport::default();
        }
        reply.await.unwrap_or_default()
    }

    /// Runs the 1 Hz maintenance pass now: released retries, `clear_after`, the pre-terminal
    /// safety net and the group drift recompute (DESIGN §8.6, §8.8, §8.10, §13).
    ///
    /// The engine ticks itself once a second; this exists so a caller that has just moved the
    /// clock — a test with a `FakeClock`, or `aulos-server` after a config reload changed
    /// `CLEAR_COMPLETED_AFTER` — does not have to wait for the next one.
    pub async fn tick(&self) {
        let _ = self.tx.send(EngineCmd::Tick).await;
    }

    /// Forwards a provider stage transition (DESIGN §15.1). Called by the aggregator.
    pub async fn stage(&self, id: ItemId, stage: Stage, msg: Option<Box<str>>) {
        let _ = self.tx.send(EngineCmd::Stage { id, stage, msg }).await;
    }

    /// Forwards a produced auxiliary file (DESIGN §15.1). Called by the aggregator.
    pub async fn file(&self, id: ItemId, slot: FileSlot, file: FileRef) {
        let _ = self
            .tx
            .send(EngineCmd::File {
                id,
                slot,
                file: Box::new(file),
            })
            .await;
    }
}

/// Every id skipped as [`SkipReason::NotFound`] — the answer when the engine is gone.
fn unavailable(ids: Vec<ItemId>) -> ActionsResult {
    ActionsResult {
        applied: Vec::new(),
        skipped: ids
            .into_iter()
            .map(|id| Skipped {
                id,
                reason: SkipReason::NotFound,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_reasons_serialise_snake_case() {
        for r in SkipReason::ALL {
            assert_eq!(
                serde_json::to_string(&r).unwrap(),
                format!("\"{}\"", r.as_str())
            );
            assert_eq!(
                serde_json::from_str::<SkipReason>(&serde_json::to_string(&r).unwrap()).unwrap(),
                r
            );
        }
    }

    #[test]
    fn the_action_set_is_the_five_of_design_8_7() {
        assert_eq!(
            Action::ALL.map(Action::as_str),
            ["start", "pause", "cancel", "retry", "delete"]
        );
        for a in Action::ALL {
            assert_eq!(
                serde_json::to_string(&a).unwrap(),
                format!("\"{}\"", a.as_str())
            );
        }
    }

    #[test]
    fn add_error_codes_follow_the_design_table() {
        assert_eq!(
            AddError::field(0, ErrorCode::FolderInvalid, "folder", "nope").code(),
            ErrorCode::FolderInvalid
        );
        assert_eq!(
            AddError::field(0, ErrorCode::UnknownPreset, "ytdl_options_presets", "nope").code(),
            ErrorCode::UnknownPreset
        );
        assert_eq!(
            AddError::Duplicate {
                index: 1,
                existing_id: ItemId::new()
            }
            .code(),
            ErrorCode::Conflict
        );
        assert_eq!(
            AddError::TooManyUrls { max: 500, got: 501 }.code(),
            ErrorCode::PayloadTooLarge
        );
        assert_eq!(
            AddError::Invalid {
                index: 0,
                errors: Vec::new()
            }
            .code(),
            ErrorCode::ValidationFailed,
            "an empty detail list still answers 400"
        );
    }

    #[test]
    fn an_unreachable_engine_skips_every_id() {
        let a = ItemId::new();
        let r = unavailable(vec![a]);
        assert!(r.is_empty());
        assert_eq!(
            r.skipped,
            [Skipped {
                id: a,
                reason: SkipReason::NotFound
            }]
        );
    }

    #[test]
    fn command_names_are_unique() {
        let (ack, _rx) = oneshot::channel();
        let names = [
            EngineCmd::Tick.name(),
            EngineCmd::SlotFreed.name(),
            EngineCmd::Clear {
                delete_file: None,
                ack,
            }
            .name(),
        ];
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }
}
