//! The command loop that owns every subscription mutation (DESIGN §14.1, §14.3).
//!
//! [`Manager`] holds the receiving half of the `mpsc<SubCmd>` whose sender is
//! [`aulos_core::subscription::SubscriptionsHandle`] — the split that keeps `aulos-api` from
//! depending on this crate at all (DESIGN §3).
//!
//! It is a single task with owned state and no `Mutex`, for the same reason the queue engine is:
//! the url index, the in-flight `pending_urls` set, the `checking` flags and the persisted record
//! all move together because they all move inside one command. The two things that must **not**
//! block the loop — resolving a feed on subscribe, and running a scheduled check — happen on other
//! tasks and come back as messages.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, EventSender};
use aulos_core::id::{ItemId, SubId, UnixMs};
use aulos_core::paths::RelDir;
use aulos_core::selection::Selection;
use aulos_core::subscription::{
    SubChanges, SubCmd, SubError, SubsHealth, SubscriptionRecord, SubscriptionView,
};
use aulos_store::{Durability, Store, WriteOp};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use url::Url;

use crate::check::{Feed, FeedChecker};
use crate::model::{CheckFailure, CheckReport, Jitter, RandJitter, Timing};
use crate::scheduler::{CheckMsg, SubTask, TaskParams};

/// How many `CheckMsg`/probe results may queue up before a task waits.
///
/// A task that cannot report is a task that is not checking, which is the correct backpressure:
/// the manager is either persisting or handling a command, and both are short.
const INTERNAL_CAPACITY: usize = 64;

/// Everything the manager needs from the rest of the process.
pub struct SubDeps {
    /// Persisted records and seen sets.
    pub store: Store,
    /// The effective configuration.
    pub cfg: Arc<Config>,
    /// Time, so every timestamp and every timer is testable.
    pub clock: Arc<dyn Clock>,
    /// The scheduler is an event **producer only** (DESIGN §2.2.1): it holds a sender and
    /// registers no `EventInbox`.
    pub events: EventSender,
    /// How a feed is resolved and queued.
    pub checker: Arc<dyn FeedChecker>,
    /// Where the ±10 % interval jitter comes from.
    pub jitter: Arc<dyn Jitter>,
}

impl SubDeps {
    /// The production wiring: [`RandJitter`].
    #[must_use]
    pub fn new(
        store: Store,
        cfg: Arc<Config>,
        clock: Arc<dyn Clock>,
        events: EventSender,
        checker: Arc<dyn FeedChecker>,
    ) -> Self {
        Self {
            store,
            cfg,
            clock,
            events,
            checker,
            jitter: Arc::new(RandJitter),
        }
    }

    /// Replaces the jitter source. A test pins it; production does not call this.
    #[must_use]
    pub fn with_jitter(mut self, jitter: Arc<dyn Jitter>) -> Self {
        self.jitter = jitter;
        self
    }
}

/// One live subscription: the persisted record, its task's schedule channel, and whether a check
/// is running right now.
struct Slot {
    record: Arc<SubscriptionRecord>,
    params: watch::Sender<TaskParams>,
    abort: AbortHandle,
    checking: bool,
}

/// What `subscribe` needs. Every field except `url`, `selection` and `folder` currently comes from
/// the effective config, because [`SubCmd::Add`] carries only those three (see
/// `docs/INTEGRATION-NOTES.md`, WP-16).
#[derive(Clone, Debug)]
struct NewSubscription {
    url: Box<str>,
    selection: Selection,
    folder: Option<RelDir>,
    check_interval_minutes: u32,
    chapter_template: Box<str>,
    playlist_item_limit: u32,
}

/// A finished subscribe probe, on its way back to the loop.
struct Probed {
    req: NewSubscription,
    result: Box<Result<Feed, CheckFailure>>,
    ack: oneshot::Sender<Result<Box<SubscriptionView>, SubError>>,
}

/// The subscription manager (DESIGN §14).
pub struct Manager {
    deps: SubDeps,
    rx: mpsc::Receiver<SubCmd>,
    checks_tx: mpsc::Sender<CheckMsg>,
    checks_rx: mpsc::Receiver<CheckMsg>,
    probes_tx: mpsc::Sender<Probed>,
    probes_rx: mpsc::Receiver<Probed>,
    subs: HashMap<SubId, Slot>,
    /// Insertion order, so `list` matches legacy's `list(self._subs.values())`.
    order: Vec<SubId>,
    url_index: HashMap<Box<str>, SubId>,
    /// URLs with a resolution in flight — the legacy `_pending_urls` guard.
    pending_urls: HashSet<Box<str>>,
    tasks: JoinSet<()>,
    slots: Arc<Semaphore>,
    timing: Timing,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("subscriptions", &self.subs.len())
            .field("pending_urls", &self.pending_urls.len())
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// Builds a manager over the receiving half of a
    /// [`aulos_core::subscription::SubscriptionsHandle`]'s channel.
    #[must_use]
    pub fn new(deps: SubDeps, rx: mpsc::Receiver<SubCmd>) -> Self {
        let (checks_tx, checks_rx) = mpsc::channel(INTERNAL_CAPACITY);
        let (probes_tx, probes_rx) = mpsc::channel(INTERNAL_CAPACITY);
        let permits = usize::try_from(deps.cfg.sub_check_concurrency.max(1)).unwrap_or(1);
        let timing = Timing::from_config(&deps.cfg);
        Self {
            deps,
            rx,
            checks_tx,
            checks_rx,
            probes_tx,
            probes_rx,
            subs: HashMap::new(),
            order: Vec::new(),
            url_index: HashMap::new(),
            pending_urls: HashSet::new(),
            tasks: JoinSet::new(),
            slots: Arc::new(Semaphore::new(permits)),
            timing,
        }
    }

    /// Loads every persisted subscription and spawns its task (DESIGN §14.2).
    ///
    /// The first check of each is at `now + AULOS_SUB_FIRST_CHECK_DELAY_SECS + jitter(0..30 s)`,
    /// or at the persisted `next_due` when that is later — so a restart keeps the schedule and 40
    /// channels do not hit their sources in the same second.
    ///
    /// # Errors
    /// [`aulos_store::StoreError`] when the table cannot be read.
    pub async fn load(&mut self) -> Result<usize, aulos_store::StoreError> {
        let records = self.deps.store.subscriptions().await?;
        let now = self.deps.clock.now_ms();
        let mut ops = Vec::new();
        for mut record in records {
            let due = self
                .timing
                .first_due(now, record.next_due, self.deps.jitter.sample());
            // The persisted `next_due` is what `healthz` and the `subscription` frame report, so
            // the boot delay is written back rather than kept only in the task's timer.
            if record.next_due != Some(due) {
                record.next_due = Some(due);
                ops.push(WriteOp::UpsertSubscription(Box::new(record.clone())));
            }
            self.insert(Arc::new(record), due);
        }
        if !ops.is_empty()
            && let Err(e) = self.deps.store.write(ops, Durability::Batched).await
        {
            tracing::warn!("could not persist the boot schedule: {e}");
        }
        tracing::info!(count = self.subs.len(), "subscriptions loaded");
        Ok(self.subs.len())
    }

    /// The command loop. Returns when the handle is dropped.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => break,
                },
                Some(msg) = self.checks_rx.recv() => self.on_check(msg).await,
                Some(p) = self.probes_rx.recv() => self.on_probed(p).await,
            }
        }
        tracing::info!("subscription manager stopping");
        self.slots.close();
        self.tasks.abort_all();
    }

    /// Spawns [`Self::run`].
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }

    // -----------------------------------------------------------------------
    // commands
    // -----------------------------------------------------------------------

    async fn on_cmd(&mut self, cmd: SubCmd) {
        match cmd {
            SubCmd::Add {
                url,
                selection,
                folder,
                ack,
            } => {
                let req = NewSubscription {
                    url,
                    selection: *selection,
                    folder,
                    check_interval_minutes: self
                        .deps
                        .cfg
                        .subscription_default_check_interval
                        .max(1),
                    chapter_template: self.deps.cfg.default_chapter_template().into(),
                    playlist_item_limit: self.deps.cfg.default_option_playlist_item_limit,
                };
                self.begin_subscribe(req, ack);
            }
            SubCmd::Update { id, changes, ack } => {
                let out = self.update(&id, &changes).await;
                let _ = ack.send(out);
            }
            SubCmd::Delete { ids, ack } => {
                let out = self.delete(ids).await;
                let _ = ack.send(out);
            }
            SubCmd::Check { ids, ack } => {
                let _ = ack.send(Ok(self.check_now(ids)));
            }
            SubCmd::List { ack } => {
                let _ = ack.send(Ok(self.views()));
            }
            SubCmd::Health { ack } => {
                let _ = ack.send(self.health());
            }
            other => tracing::warn!("unhandled SubCmd variant: {other:?}"),
        }
    }

    /// The legacy `add_subscription` guards, then an off-loop probe (DESIGN §14.3 step 9).
    fn begin_subscribe(
        &mut self,
        mut req: NewSubscription,
        ack: oneshot::Sender<Result<Box<SubscriptionView>, SubError>>,
    ) {
        // Legacy `_normalize_url`: `.strip()`, and that trimmed form is the uniqueness key.
        req.url = req.url.trim().into();
        if req.url.is_empty() {
            let _ = ack.send(Err(SubError::MissingUrl));
            return;
        }
        if self.url_index.contains_key(&req.url) || self.pending_urls.contains(&req.url) {
            let _ = ack.send(Err(SubError::AlreadySubscribed));
            return;
        }
        let Ok(parsed) = Url::parse(&req.url) else {
            let _ = ack.send(Err(SubError::CouldNotResolve));
            return;
        };

        self.pending_urls.insert(req.url.clone());
        let checker = Arc::clone(&self.deps.checker);
        let tx = self.probes_tx.clone();
        let window = self.timing.check_timeout();
        self.tasks.spawn(async move {
            let result = match tokio::time::timeout(window, checker.probe(&parsed)).await {
                Ok(r) => r,
                Err(_) => Err(CheckFailure::Timeout(window.as_secs())),
            };
            let _ = tx
                .send(Probed {
                    req,
                    result: Box::new(result),
                    ack,
                })
                .await;
        });
    }

    /// Finalises a subscribe: name, backfill suppression, one transaction, one event.
    async fn on_probed(&mut self, p: Probed) {
        let Probed { req, result, ack } = p;
        self.pending_urls.remove(&req.url);

        let feed = match *result {
            Ok(feed) => feed,
            Err(failure) => {
                let _ = ack.send(Err(subscribe_error(&failure)));
                return;
            }
        };
        // Lost the race against another subscribe of the same URL.
        if self.url_index.contains_key(&req.url) {
            let _ = ack.send(Err(SubError::AlreadySubscribed));
            return;
        }
        let Ok(parsed) = Url::parse(&req.url) else {
            let _ = ack.send(Err(SubError::CouldNotResolve));
            return;
        };

        let now = self.deps.clock.now_ms();
        // DESIGN §14.3 step 8: every currently visible media id is marked seen **without**
        // queueing, except an upcoming premiere — which stays unseen so it is queued when the
        // stream starts.
        let backfill = feed.backfill_ids();
        let name = feed.name.unwrap_or_else(|| req.url.clone());

        let mut record = SubscriptionRecord::new(SubId::new(), name, parsed, req.selection);
        record.check_interval_minutes = req.check_interval_minutes.max(1);
        record.folder = req.folder;
        record.chapter_template = req.chapter_template;
        record.playlist_item_limit = req.playlist_item_limit;
        record.last_checked = Some(now);
        record.next_due = Some(self.timing.next_due(
            now,
            record.check_interval_minutes,
            0,
            self.deps.jitter.sample(),
        ));
        record.seen_count = u32::try_from(backfill.len()).unwrap_or(u32::MAX);

        let mut ops = vec![WriteOp::UpsertSubscription(Box::new(record.clone()))];
        if !backfill.is_empty() {
            ops.push(WriteOp::MarkSeen {
                sub: record.id.clone(),
                ids: backfill,
                at: now,
            });
            ops.push(WriteOp::PruneSeen {
                sub: record.id.clone(),
                keep: self.deps.cfg.subscription_max_seen_ids,
            });
        }
        if let Err(e) = self.deps.store.write(ops, Durability::Sync).await {
            tracing::error!("could not persist the new subscription: {e}");
            let _ = ack.send(Err(SubError::Other(e.to_string().into_boxed_str())));
            return;
        }

        let due = record.next_due.unwrap_or(now);
        let record = Arc::new(record);
        self.url_index.insert(req.url, record.id.clone());
        self.insert(Arc::clone(&record), due);
        let view = record.to_view(false);
        self.publish_changed(&view).await;
        tracing::info!(
            subscription = record.id.as_str(),
            name = &*record.name,
            seen = record.seen_count,
            "subscription added"
        );
        let _ = ack.send(Ok(Box::new(view)));
    }

    /// Legacy `update_subscription`: only `enabled`, `check_interval_minutes` and `name`
    /// (DESIGN §14.3 step 10).
    async fn update(
        &mut self,
        id: &SubId,
        changes: &SubChanges,
    ) -> Result<Box<SubscriptionView>, SubError> {
        let slot = self
            .subs
            .get(id)
            .ok_or_else(|| SubError::NotFound(id.clone()))?;
        let was_enabled = slot.record.enabled;
        let old_interval = slot.record.check_interval_minutes;
        let mut record = (*slot.record).clone();

        if let Some(enabled) = changes.enabled {
            record.enabled = enabled;
        }
        if let Some(minutes) = changes.check_interval_minutes {
            record.check_interval_minutes = minutes.max(1);
        }
        // Legacy: `if "name" in changes and changes["name"]` — an empty name is ignored.
        if let Some(name) = changes.name.as_deref().filter(|n| !n.trim().is_empty()) {
            record.name = name.into();
        }

        let now = self.deps.clock.now_ms();
        if record.check_interval_minutes != old_interval {
            // A shortened interval must take effect now, not at the old due time.
            let base = record.last_checked.unwrap_or(now);
            record.next_due = Some(
                self.timing
                    .next_due(
                        base,
                        record.check_interval_minutes,
                        record.consecutive_failures,
                        self.deps.jitter.sample(),
                    )
                    .max(now),
            );
        }

        self.deps
            .store
            .write(
                vec![WriteOp::UpsertSubscription(Box::new(record.clone()))],
                Durability::Sync,
            )
            .await
            .map_err(|e| SubError::Other(e.to_string().into_boxed_str()))?;

        if changes.enabled.is_some() && record.enabled != was_enabled {
            tracing::info!(
                "Subscription {} {}",
                record.name,
                if record.enabled { "resumed" } else { "paused" }
            );
        }

        let due = record.next_due.unwrap_or(now);
        let record = Arc::new(record);
        let checking = self.subs.get(id).is_some_and(|s| s.checking);
        if let Some(slot) = self.subs.get_mut(id) {
            slot.record = Arc::clone(&record);
            let _ = slot.params.send(TaskParams::new(Arc::clone(&record), due));
        }
        let view = record.to_view(checking);
        self.publish_changed(&view).await;
        Ok(Box::new(view))
    }

    async fn delete(&mut self, ids: Vec<SubId>) -> Result<Vec<SubId>, SubError> {
        let mut removed = Vec::new();
        for id in ids {
            if let Some(slot) = self.subs.remove(&id) {
                slot.abort.abort();
                self.order.retain(|k| k != &id);
                self.url_index.retain(|_, v| v != &id);
                removed.push(id);
            }
        }
        if removed.is_empty() {
            return Ok(removed);
        }
        self.deps
            .store
            .write(
                vec![WriteOp::DeleteSubscriptions(removed.clone())],
                Durability::Sync,
            )
            .await
            .map_err(|e| SubError::Other(e.to_string().into_boxed_str()))?;
        for id in &removed {
            self.deps
                .events
                .publish(DomainEvent::SubscriptionRemoved(id.clone()))
                .await;
        }
        tracing::info!(count = removed.len(), "subscriptions deleted");
        Ok(removed)
    }

    /// `POST <p>subscriptions/check`: nudge the timers and answer immediately (BRIEF §12).
    fn check_now(&mut self, ids: Vec<SubId>) -> aulos_core::subscription::CheckJob {
        let now = self.deps.clock.now_ms();
        let targets: Vec<SubId> = if ids.is_empty() {
            self.order
                .iter()
                .filter(|id| self.subs.get(*id).is_some_and(|s| s.record.enabled))
                .cloned()
                .collect()
        } else {
            ids.into_iter()
                .filter(|id| self.subs.contains_key(id))
                .collect()
        };
        for id in &targets {
            if let Some(slot) = self.subs.get(id) {
                // `enabled: true` for the duration of a manual check, so an explicitly requested
                // check of a paused subscription still runs — legacy's `check_now(ids)` did too.
                let _ = slot.params.send(TaskParams {
                    enabled: true,
                    next_due: now,
                    record: Arc::clone(&slot.record),
                });
            }
        }
        tracing::info!(
            "Manual subscription check requested for {} subscription(s)",
            targets.len()
        );
        aulos_core::subscription::CheckJob {
            job_id: ItemId::new().to_string().into_boxed_str(),
            subscriptions: targets,
        }
    }

    fn views(&self) -> Vec<SubscriptionView> {
        self.order
            .iter()
            .filter_map(|id| self.subs.get(id))
            .map(|s| s.record.to_view(s.checking))
            .collect()
    }

    fn health(&self) -> SubsHealth {
        let now = self.deps.clock.now_ms();
        let next = self
            .subs
            .values()
            .filter(|s| s.record.enabled)
            .filter_map(|s| s.record.next_due)
            .min()
            .map(|due| (due - now).max(0) / 1_000);
        SubsHealth {
            total: u32::try_from(self.subs.len()).unwrap_or(u32::MAX),
            failing: u32::try_from(
                self.subs
                    .values()
                    .filter(|s| s.record.consecutive_failures > 0)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            next_due_in_s: next,
        }
    }

    // -----------------------------------------------------------------------
    // check results
    // -----------------------------------------------------------------------

    async fn on_check(&mut self, msg: CheckMsg) {
        match msg {
            CheckMsg::Started(id) => {
                if let Some(slot) = self.subs.get_mut(&id) {
                    slot.checking = true;
                    let view = slot.record.to_view(true);
                    self.publish_changed(&view).await;
                }
            }
            CheckMsg::Done { id, result } => self.finish_check(&id, *result).await,
        }
    }

    /// The DESIGN §14.2 tail of one check: always update `last_checked`, apply the backoff on
    /// failure, persist one row plus only the new seen ids, then publish.
    async fn finish_check(&mut self, id: &SubId, result: Result<CheckReport, CheckFailure>) {
        let Some(slot) = self.subs.get(id) else {
            return; // deleted while the check was running
        };
        let now = self.deps.clock.now_ms();
        let mut record = (*slot.record).clone();
        // The legacy bug this fixes: a failure left `last_checked` untouched, so a broken feed was
        // re-extracted every 60 s forever.
        record.last_checked = Some(now);

        let mut ops = Vec::with_capacity(3);
        match result {
            Ok(report) => {
                record.consecutive_failures = 0;
                record.error = report.error_text();
                if !report.queued.is_empty() {
                    ops.push(WriteOp::MarkSeen {
                        sub: id.clone(),
                        ids: report.queued,
                        at: now,
                    });
                    ops.push(WriteOp::PruneSeen {
                        sub: id.clone(),
                        keep: self.deps.cfg.subscription_max_seen_ids,
                    });
                }
            }
            Err(failure) => {
                record.consecutive_failures = record.consecutive_failures.saturating_add(1);
                record.error = Some(failure.error_text());
                tracing::warn!(
                    subscription = id.as_str(),
                    name = &*record.name,
                    failures = record.consecutive_failures,
                    "subscription check failed: {failure}"
                );
            }
        }
        record.next_due = Some(self.timing.next_due(
            now,
            record.check_interval_minutes,
            record.consecutive_failures,
            self.deps.jitter.sample(),
        ));

        ops.insert(0, WriteOp::UpsertSubscription(Box::new(record.clone())));
        if let Err(e) = self.deps.store.write(ops, Durability::Batched).await {
            tracing::error!(subscription = id.as_str(), "could not persist a check: {e}");
        }
        // `seen_count` is a `COUNT(*)` over `subscription_seen`, so the store is the only place
        // that knows it after a `MarkSeen`/`PruneSeen` pair.
        if let Ok(Some(fresh)) = self.deps.store.subscription(id).await {
            record.seen_count = fresh.seen_count;
        }

        let due = record.next_due.unwrap_or(now);
        let record = Arc::new(record);
        if let Some(slot) = self.subs.get_mut(id) {
            slot.checking = false;
            slot.record = Arc::clone(&record);
            let _ = slot.params.send(TaskParams::new(Arc::clone(&record), due));
        }
        let view = record.to_view(false);
        self.publish_changed(&view).await;
    }

    // -----------------------------------------------------------------------
    // plumbing
    // -----------------------------------------------------------------------

    /// Registers a record and spawns its timer task.
    fn insert(&mut self, record: Arc<SubscriptionRecord>, due: UnixMs) {
        let id = record.id.clone();
        self.url_index
            .insert(record.url.as_str().trim().into(), id.clone());
        let (params_tx, params_rx) = watch::channel(TaskParams::new(Arc::clone(&record), due));
        let task = SubTask::new(
            id.clone(),
            params_rx,
            Arc::clone(&self.slots),
            Arc::clone(&self.deps.checker),
            Arc::clone(&self.deps.clock),
            self.timing,
            self.checks_tx.clone(),
        );
        let abort = self.tasks.spawn(task.run());
        if !self.subs.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.subs.insert(
            id,
            Slot {
                record,
                params: params_tx,
                abort,
                checking: false,
            },
        );
    }

    async fn publish_changed(&self, view: &SubscriptionView) {
        self.deps
            .events
            .publish(DomainEvent::SubscriptionChanged(Arc::new(view.clone())))
            .await;
    }
}

/// The legacy error mapping for `POST <p>subscribe` (DESIGN §14.3 steps 4 and 9).
fn subscribe_error(failure: &CheckFailure) -> SubError {
    match failure {
        CheckFailure::VideoOnly => SubError::VideoOnly,
        CheckFailure::NoProvider(_) => SubError::CouldNotResolve,
        // Legacy surfaced the yt-dlp message verbatim for a `YoutubeDLError`.
        other => SubError::Other(other.error_text()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_video_only_probe_is_the_legacy_single_video_message() {
        assert_eq!(
            subscribe_error(&CheckFailure::VideoOnly),
            SubError::VideoOnly
        );
        assert_eq!(
            subscribe_error(&CheckFailure::VideoOnly).to_string(),
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );
    }

    #[test]
    fn no_provider_is_could_not_resolve() {
        assert_eq!(
            subscribe_error(&CheckFailure::NoProvider("https://x.test".into())),
            SubError::CouldNotResolve
        );
        assert_eq!(
            subscribe_error(&CheckFailure::NoProvider("x".into())).to_string(),
            "Could not resolve URL"
        );
    }

    #[test]
    fn a_provider_failure_surfaces_its_own_message() {
        let e = subscribe_error(&CheckFailure::Provider("ERROR: Sign in to confirm".into()));
        assert_eq!(e.to_string(), "ERROR: Sign in to confirm");
    }
}
