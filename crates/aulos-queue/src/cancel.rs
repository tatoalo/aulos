//! Start, pause, cancel, retry, delete, clear and `cancel-resolve` (DESIGN §8.7, §8.10).
//!
//! Every one of these is idempotent, every one cascades to a group's children, and every one
//! answers with an [`crate::ActionsResult`] naming what it did and what it refused — because a
//! client that asked to cancel four items needs to know which three moved.
//!
//! Cancel is **immediate at the API layer**: the status write and the terminal frame are emitted
//! as soon as the token is cancelled and the status is persisted. The HTTP response does not wait
//! for `SIGKILL`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use aulos_core::{FieldUpdate, GroupId, ItemId, Kind, RemoveReason, Status, WireError};
use aulos_store::{Durability, WriteOp, retry_ops};

use crate::cmd::{ActionsResult, CancelScope, SkipReason};
use crate::engine::{Engine, Settled};

/// The message a paused job carries (DESIGN §8.7).
pub const PAUSED_MSG: &str = "Paused";

impl Engine {
    /// [`crate::EngineCmd::Start`]: `queued(!auto_start)` → `queued(auto_start)`, and **a retry on
    /// a failed or cancelled item** (PROTOCOL §4.2: "on a terminal item it is a `retry`").
    ///
    /// A `finished` item is the one terminal status that is not re-run: it has its file, and
    /// re-downloading it is `retry`'s job to be asked for explicitly. Everything else — the
    /// `network` failure the shipped client offers a Start button on — requeues, which is what
    /// the v1 shim was compensating for in the API layer.
    pub(crate) async fn handle_start(&mut self, ids: Vec<ItemId>) -> ActionsResult {
        let mut result = ActionsResult::default();
        for id in self.expand_targets(ids, &mut result).await {
            let Some(item) = self.row(id).await else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            match item.status {
                // The iOS flow: the app adds with `auto_start: false` and sends `start` in the
                // next breath, while the row is still `resolving` — resolution takes seconds, so
                // this is the *common* ordering rather than a race. Refused as `not_startable`,
                // the item then parked as `queued(auto_start = false)` for ever, because the end
                // of resolution writes the flag and the row still said "no". Flipping it here is
                // what "start this item" means at any point before it is queued (PROTOCOL §4.2).
                Status::Resolving if !item.auto_start => {
                    let at = self.clock.now_ms();
                    if !self
                        .apply(
                            vec![WriteOp::SetAutoStart {
                                id,
                                auto_start: true,
                                at,
                            }],
                            Durability::Batched,
                        )
                        .await
                    {
                        result.skip(id, SkipReason::NotStartable);
                        continue;
                    }
                    self.patch(id, |i| i.auto_start = true);
                    self.publish_changed(id, Status::Resolving, Status::Resolving)
                        .await;
                    self.sync_group_of(id).await;
                    result.applied.push(id);
                }
                // Already on its way to the scheduler: `start` is idempotent.
                Status::Resolving => result.applied.push(id),
                Status::Queued => {
                    if !item.auto_start {
                        let at = self.clock.now_ms();
                        if !self
                            .apply(
                                vec![WriteOp::SetAutoStart {
                                    id,
                                    auto_start: true,
                                    at,
                                }],
                                Durability::Batched,
                            )
                            .await
                        {
                            result.skip(id, SkipReason::NotStartable);
                            continue;
                        }
                        self.patch(id, |i| i.auto_start = true);
                    }
                    // A row that never got as far as a provider cannot be scheduled — the
                    // scheduler drops a provider-less row from its deque — so starting it means
                    // resolving it first. That is the state a restart leaves an interrupted add
                    // in (DESIGN §8.9), and acking `applied` without this would be a lie.
                    if item.provider.is_none() && item.kind == Kind::Item {
                        self.restart_resolution(id).await;
                    } else {
                        // Already on its way: `start` is idempotent. It still re-enqueues, because
                        // "queued with `auto_start`" and "on a ready deque" are two different facts
                        // — a start pressed while a paused job was still being killed leaves the
                        // first without the second until its slot is released
                        // (`Engine::release_job`).
                        self.enqueue(id);
                    }
                    if !item.auto_start {
                        self.publish_changed(id, Status::Queued, Status::Queued)
                            .await;
                        self.sync_group_of(id).await;
                    }
                    result.applied.push(id);
                }
                // PROTOCOL §4.2: on a terminal item `start` *is* `retry`.
                Status::Error | Status::Canceled => {
                    if self.retry_one(id, &item).await {
                        result.applied.push(id);
                    } else {
                        result.skip(id, SkipReason::NotRetryable);
                    }
                }
                s if s.is_terminal() => result.skip(id, SkipReason::AlreadyTerminal),
                _ => result.skip(id, SkipReason::NotStartable),
            }
        }
        self.schedule().await;
        result
    }

    /// [`crate::EngineCmd::Pause`] (DESIGN §8.7).
    pub(crate) async fn handle_pause(&mut self, ids: Vec<ItemId>) -> ActionsResult {
        let mut result = ActionsResult::default();
        for id in self.expand_targets(ids, &mut result).await {
            let Some(item) = self.row(id).await else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            match item.status {
                Status::Queued if item.auto_start => {
                    let at = self.clock.now_ms();
                    if !self
                        .apply(
                            vec![WriteOp::SetAutoStart {
                                id,
                                auto_start: false,
                                at,
                            }],
                            Durability::Batched,
                        )
                        .await
                    {
                        result.skip(id, SkipReason::NotPausable);
                        continue;
                    }
                    self.patch(id, |i| i.auto_start = false);
                    self.unqueue(id);
                    self.retries.retain(|r| r.id != id);
                    self.publish_changed(id, Status::Queued, Status::Queued)
                        .await;
                    result.applied.push(id);
                }
                // Already parked: pausing an already-paused item is idempotent.
                Status::Queued => result.applied.push(id),
                s if s.is_running() => {
                    self.park_running(id).await;
                    result.applied.push(id);
                }
                // Resolution is seconds long and cancelling it would lose the record's identity
                // for the client, so `resolving` is not pausable — nor is a terminal item. Use
                // cancel.
                _ => result.skip(id, SkipReason::NotPausable),
            }
            self.sync_group_of(id).await;
        }
        self.schedule().await;
        result
    }

    /// Kills a running job and parks it as `queued(auto_start = false)` (DESIGN §8.7).
    ///
    /// The same `killpg` sequence as cancel, but the partial file is **kept** and `attempt` is
    /// unchanged, so `start` re-runs the job and yt-dlp resumes from the `.part`.
    ///
    /// `postprocessing` is a running status ([`Status::is_running`]), so this is also the path a
    /// pause takes during the **pre-terminal hook** window (DESIGN §13), when the download is over
    /// and the row is waiting on `HooksFinished`. The pending entry has to go with it, exactly as
    /// [`Engine::cancel_one`] drops it: the outcome it holds belongs to a run the user has just
    /// parked, so leaving it there would let `Engine::expire_pending_hooks` finalise a *stale*
    /// outcome onto a row that is queued or downloading again — and would make
    /// `Engine::handle_stage` refuse every frame of the restarted run, which watches its
    /// awaiting-hooks arm.
    async fn park_running(&mut self, id: ItemId) {
        if let Some(slot) = self.running.get_mut(&id) {
            slot.settled = Some(Settled::Paused);
            slot.cancel.cancel();
            drop(slot.slot.take());
            if let Some(w) = slot.watchdog.take() {
                w.abort();
            }
        }
        self.beats.disarm(id);
        self.pending_hooks.remove(&id);
        // An SC job's partials go anyway: its m3u8 token is dead (DESIGN §8.7, §8.9).
        self.cleanup_partials(id, false);
        self.write_status(
            id,
            Status::Queued,
            FieldUpdate::Set(PAUSED_MSG.into()),
            FieldUpdate::Keep,
            Some(false),
        )
        .await;
        self.unqueue(id);
    }

    /// [`crate::EngineCmd::Cancel`] (DESIGN §8.7).
    pub(crate) async fn handle_cancel(&mut self, ids: Vec<ItemId>) -> ActionsResult {
        let mut result = ActionsResult::default();
        for id in self.expand_targets(ids, &mut result).await {
            let Some(item) = self.row(id).await else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            if item.status.is_terminal() {
                // Idempotent: a cancelled item cancels again with an ok ack.
                result.applied.push(id);
                continue;
            }
            self.cancel_one(id).await;
            result.applied.push(id);
            self.sync_group_of(id).await;
        }
        self.schedule().await;
        result
    }

    /// Cancels one non-terminal item, whatever state it is in (DESIGN §8.7).
    pub(crate) async fn cancel_one(&mut self, id: ItemId) {
        if let Some(slot) = self.resolving.remove(&id) {
            slot.cancel.cancel();
            slot.handle.abort();
        }
        if let Some(slot) = self.running.get_mut(&id) {
            slot.settled = Some(Settled::Canceled);
            slot.cancel.cancel();
            drop(slot.slot.take());
            if let Some(w) = slot.watchdog.take() {
                w.abort();
            }
        }
        self.beats.disarm(id);
        self.unqueue(id);
        self.retries.retain(|r| r.id != id);
        self.pending_hooks.remove(&id);
        self.expansions.remove(&id);
        let error = WireError::new(aulos_core::ErrorCode::Canceled, "canceled");
        self.terminate(id, Status::Canceled, FieldUpdate::Set(error))
            .await;
        // Eager, so a cancel of a row with no live process cleans up now. A row that *does* have
        // one is cleaned again in `Engine::release_job` when the task reports back, because until
        // then the process still has the rest of the `killpg` grace to recreate this directory and
        // write another fragment into it (DESIGN §8.7 orders the removal after the kill).
        self.cleanup_partials(id, true);
        self.notify_resolved(id);
    }

    /// [`crate::EngineCmd::Retry`] (DESIGN §8.8).
    pub(crate) async fn handle_retry(&mut self, ids: Vec<ItemId>) -> ActionsResult {
        let mut result = ActionsResult::default();
        for id in self.expand_targets(ids, &mut result).await {
            let Some(item) = self.row(id).await else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            if !matches!(item.status, Status::Error | Status::Canceled) {
                result.skip(id, SkipReason::NotRetryable);
                continue;
            }
            if self.retry_one(id, &item).await {
                result.applied.push(id);
            } else {
                result.skip(id, SkipReason::NotRetryable);
            }
        }
        self.schedule().await;
        result
    }

    /// Requeues one `error`/`canceled` row: the DESIGN §8.8 retry, shared with [`Self::handle_start`]
    /// because PROTOCOL §4.2 defines `start` on a terminal item as a retry.
    ///
    /// Returns whether the row moved — `false` only when the persisted write failed.
    async fn retry_one(&mut self, id: ItemId, item: &std::sync::Arc<aulos_core::Item>) -> bool {
        let at = self.clock.now_ms();
        // `item.source` is deliberately untouched: DESIGN §4.4 makes the origin permanent so
        // per-origin routing survives a retry — a Telegram item still reports to its chat, an iOS
        // item still reaches the phone. `attempt` is what marks it a retry (DESIGN §8.2).
        if !self.apply(retry_ops(id, at), Durability::Batched).await {
            return false;
        }
        // A row the done window had evicted came back from SQLite ([`Engine::row`]) and has to
        // re-enter the working set before anything can patch, publish or schedule it — it is
        // about to be non-terminal, which is exactly what the working set is for. It is *not*
        // re-counted into its group's accumulator: eviction never took it out of one.
        self.items
            .entry(id)
            .or_insert_with(|| std::sync::Arc::clone(item));
        let from = item.status;
        self.patch(id, |i| {
            i.status = Status::Queued;
            i.auto_start = true;
            i.msg = None;
            i.error = None;
            i.attempt = i.attempt.saturating_add(1);
            i.finished_at = None;
        });
        // The row leaves the done window; the published terminal total is the aggregator's
        // and follows the `terminal → queued` view it is about to see.
        self.done_order.retain(|d| *d != id);
        self.on_child_status(id, from, Status::Queued).await;
        self.publish_changed(id, from, Status::Queued).await;
        // A retried item that was never resolved has to resolve again before it can run.
        if item.provider.is_none() && item.kind == Kind::Item {
            self.restart_resolution(id).await;
        } else {
            self.enqueue(id);
        }
        true
    }

    /// Puts a row that has no provider yet back into resolution (DESIGN §8.1, §8.9).
    ///
    /// A row is provider-less until `WriteOp::SetResolved` names one at the *end* of resolution,
    /// so this is the state a retry of an unresolved add is in — and the state boot recovery
    /// leaves an interrupted `resolving` row in. Without it the row is invisible to the scheduler
    /// forever: `schedule()` drops a provider-less row from its deque, `retry` refuses a `queued`
    /// status, and only `delete` clears it.
    pub(crate) async fn restart_resolution(&mut self, id: ItemId) {
        if self.resolving.contains_key(&id) {
            return;
        }
        self.unqueue(id);
        if !self
            .write_status(
                id,
                Status::Resolving,
                FieldUpdate::Keep,
                FieldUpdate::Clear,
                None,
            )
            .await
        {
            return;
        }
        // Resolution restarted this way belongs to no add, so it gets a generation of its own
        // rather than borrowing the last add's — only `CancelScope::All` can condemn it.
        self.add_generation += 1;
        let generation = self.add_generation;
        self.spawn_resolve(id, generation, None).await;
    }

    /// [`crate::EngineCmd::Delete`] (DESIGN §8.10).
    pub(crate) async fn handle_delete(
        &mut self,
        ids: Vec<ItemId>,
        delete_file: Option<bool>,
    ) -> ActionsResult {
        let remove_files = delete_file.unwrap_or(self.cfg.delete_file_on_trashcan);
        let mut result = ActionsResult::default();
        let mut doomed: Vec<ItemId> = Vec::new();
        for id in self.expand_targets(ids, &mut result).await {
            let Some(item) = self.row(id).await else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            // Deleting a group cancels any active child first, then relies on ON DELETE CASCADE.
            if !item.status.is_terminal() {
                self.cancel_one(id).await;
            }
            doomed.push(id);
        }
        if doomed.is_empty() {
            return result;
        }
        self.remove_rows(&doomed, remove_files, RemoveReason::Deleted)
            .await;
        result.applied = doomed;
        self.schedule().await;
        result
    }

    /// [`crate::EngineCmd::Clear`]: every terminal row (DESIGN §8.10).
    pub(crate) async fn handle_clear(&mut self, delete_file: Option<bool>) -> ActionsResult {
        let remove_files = delete_file.unwrap_or(self.cfg.delete_file_on_trashcan);
        let terminal: Vec<ItemId> = self
            .items
            .values()
            .filter(|i| i.status.is_terminal())
            .map(|i| i.id)
            .collect();
        // Rows that have aged out of the memory window are cleared too, which is the whole reason
        // this queries SQLite rather than only the cache (DESIGN §8.10).
        let aged = match self.store.items(aulos_store::ItemFilter::terminal()).await {
            Ok(page) => page.rows,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list terminal rows");
                Vec::new()
            }
        };
        let mut all = terminal;
        for item in aged {
            if !all.contains(&item.id) {
                all.push(item.id);
            }
        }
        if all.is_empty() {
            return ActionsResult::default();
        }
        self.remove_rows(&all, remove_files, RemoveReason::Cleared)
            .await;
        ActionsResult {
            applied: all,
            skipped: Vec::new(),
        }
    }

    /// Deletes rows, optionally their files, and publishes one `Removed` per reason.
    ///
    /// The unlinks run on a blocking pool rather than on the engine task: a bulk clear of a few
    /// thousand rows is tens of thousands of syscalls against a bind-mounted volume, and nothing
    /// here depends on their result — the row is deleted regardless (DESIGN §8.10). The engine
    /// only collects the paths, which costs no syscall at all.
    ///
    /// Every affected group is resynchronised afterwards, because [`Engine::forget`] has just
    /// taken the removed children out of their accumulators.
    pub(crate) async fn remove_rows(
        &mut self,
        ids: &[ItemId],
        remove_files: bool,
        reason: RemoveReason,
    ) {
        // One pass per id, so a caller that named a group and one of its children does not remove
        // — or un-count — the same row twice. The order the caller chose is kept: it is the order
        // the `removed` frame lists.
        let mut seen: HashSet<ItemId> = HashSet::with_capacity(ids.len());
        let ids: Vec<ItemId> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        if remove_files {
            let paths = self.files_to_remove(&ids).await;
            if !paths.is_empty() {
                drop(tokio::task::spawn_blocking(move || {
                    for path in paths {
                        remove_path(&path);
                    }
                }));
            }
        }
        if !self
            .apply(vec![WriteOp::DeleteItems(ids.clone())], Durability::Batched)
            .await
        {
            return;
        }
        let mut groups: Vec<GroupId> = ids
            .iter()
            .filter_map(|id| self.cached(*id).and_then(|i| i.group_id))
            .collect();
        groups.sort_unstable();
        groups.dedup();
        for id in &ids {
            self.forget(*id);
        }
        for group in groups {
            // A group whose children were all removed with it is gone too; `sync_group_status`
            // returns without writing in that case.
            self.sync_group_status(group).await;
        }
        self.publish_removed(ids, reason).await;
    }

    /// Every path the named rows produced, reading SQLite for the ones the done window evicted.
    ///
    /// The fallback is the whole point: a `clear` and the `CLEAR_COMPLETED_AFTER` sweep both take
    /// their id list straight out of SQLite, so most of what they remove was never in the working
    /// set — and a path list built from the cache alone silently skipped every one of them,
    /// deleting the record and leaving the media behind with nothing left to reference it
    /// (PROTOCOL §4.7). One query covers however many rows are missing, and none at all is run
    /// when the window still holds them.
    async fn files_to_remove(&self, ids: &[ItemId]) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = Vec::new();
        let mut missing: HashSet<ItemId> = HashSet::new();
        for id in ids {
            match self.cached(*id) {
                Some(item) => paths.extend(self.files_of(&item)),
                None => {
                    missing.insert(*id);
                }
            }
        }
        if missing.is_empty() {
            return paths;
        }
        match self.store.items(aulos_store::ItemFilter::terminal()).await {
            Ok(page) => {
                for item in page.rows.iter().filter(|i| missing.contains(&i.id)) {
                    paths.extend(self.files_of(item));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the rows a removal is about to unlink");
            }
        }
        paths
    }

    /// Every path an item produced (DESIGN §8.10).
    ///
    /// `filename`, **every** `chapter_files`/`subtitle_files` entry, the StreamingCommunity
    /// `.info.json` and `.nfo` siblings, and the scratch directory. Legacy orphaned all of those.
    fn files_of(&self, item: &aulos_core::Item) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let dir = self.out_dir_for(item);
        if let Some(name) = item.filename.as_ref() {
            let primary = dir.join(name.as_path());
            if let Some(stem) = primary.file_stem() {
                for sibling in [".info.json", ".nfo"] {
                    let mut side = stem.to_os_string();
                    side.push(sibling);
                    paths.push(dir.join(side));
                }
            }
            paths.push(primary);
        }
        for file in item.chapter_files.iter().chain(item.subtitle_files.iter()) {
            paths.push(dir.join(&*file.filename));
        }
        paths.push(self.tmp_dir_for(item.id));
        paths
    }

    /// [`crate::EngineCmd::CancelResolve`] (DESIGN §8.1, §8.4).
    pub(crate) async fn handle_cancel_resolve(&mut self, scope: CancelScope) -> ActionsResult {
        let mut result = ActionsResult::default();
        let matches_scope = |generation: u64| match scope {
            CancelScope::All => true,
            CancelScope::Generation(n) => generation == n,
        };

        if scope == CancelScope::All {
            // Legacy's `cancel_add()` bumped a process-global generation and took no argument, so
            // this is what a v1 caller gets. The epoch — not the per-add generation — is what
            // condemns work that is already in flight.
            self.cancel_epoch += 1;
        }

        let doomed: Vec<ItemId> = self
            .resolving
            .iter()
            .filter(|(_, slot)| matches_scope(slot.generation))
            .map(|(id, _)| *id)
            .collect();
        let expanding: Vec<GroupId> = self
            .expansions
            .iter()
            .filter(|(_, state)| matches_scope(state.generation))
            .map(|(id, _)| *id)
            .collect();

        // PROTOCOL §4.7 and DESIGN §8.1: this stops the *expansion*, nothing else. The
        // not-yet-created children are cancelled by never being created; "items already created
        // keep their state, so follow it with a `delete` if you want them gone" — which is only
        // true if there is something left to delete, so a child that is already downloading is
        // not killed and its partial bytes are not removed. `applied` therefore counts the
        // expansions and resolve tasks stopped, which is what the `canceled` number means.
        for group in expanding {
            self.expansions.remove(&group);
            let created = self.items.values().any(|i| i.group_id == Some(group));
            if created {
                // Re-roll the header off the children that do exist.
                self.sync_group_status(group).await;
            } else {
                // Nothing was ever created, so the aborted expansion *is* the whole group: left
                // alone it would sit `queued` with no children and no expansion to make any,
                // forever.
                self.cancel_one(group).await;
            }
            if !result.applied.contains(&group) {
                result.applied.push(group);
            }
        }
        for id in doomed {
            self.cancel_one(id).await;
            result.applied.push(id);
        }
        self.schedule().await;
        result
    }

    // -----------------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------------

    /// Expands group ids into themselves plus their children, and reports unknown ids as
    /// [`SkipReason::NotFound`] (DESIGN §8.7: "a group cancel cascades").
    ///
    /// Both lookups fall back to SQLite for what the bounded done window has evicted
    /// ([`Engine::row`]): a terminal row the client can still list is a legitimate target, and so
    /// are a group's finished children — the delete that misses them leaves their files orphaned
    /// even though `ON DELETE CASCADE` takes their rows.
    async fn expand_targets(
        &mut self,
        ids: Vec<ItemId>,
        result: &mut ActionsResult,
    ) -> Vec<ItemId> {
        let mut out: Vec<ItemId> = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(item) = self.row(id).await else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            if item.kind == Kind::Group {
                let mut children: Vec<(i64, ItemId)> = self
                    .items
                    .values()
                    .filter(|c| c.group_id == Some(id))
                    .map(|c| (c.ord, c.id))
                    .collect();
                for (ord, child) in self.stored_children(id).await {
                    if !children.iter().any(|(_, c)| *c == child) {
                        children.push((ord, child));
                    }
                }
                children.sort_unstable();
                for (_, child) in children {
                    if !out.contains(&child) {
                        out.push(child);
                    }
                }
            }
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// One group's children as `(ord, id)`, straight from SQLite.
    async fn stored_children(&self, group: GroupId) -> Vec<(i64, ItemId)> {
        let filter =
            aulos_store::ItemFilter::default().with_group(aulos_store::GroupScope::Of(group));
        match self.store.items(filter).await {
            Ok(page) => page.rows.into_iter().map(|c| (c.ord, c.id)).collect(),
            Err(e) => {
                tracing::warn!(group = %group, error = %e, "cannot list a group's children");
                Vec::new()
            }
        }
    }

    /// Removes an id from every ready deque.
    pub(crate) fn unqueue(&mut self, id: ItemId) {
        for deque in &mut self.ready {
            deque.retain(|q| *q != id);
        }
    }

    /// Rolls a child's group up, if it has one.
    pub(crate) async fn sync_group_of(&mut self, id: ItemId) {
        if let Some(group) = self.items.get(&id).and_then(|i| i.group_id) {
            self.sync_group_status(group).await;
        }
    }
}

/// Best-effort removal of one file or directory, with a WARN (DESIGN §8.10).
///
/// Runs on a blocking pool, never on the engine task — see [`Engine::remove_rows`].
fn remove_path(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let removed = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    if let Err(e) = removed {
        tracing::warn!(path = %path.display(), error = %e, "cannot remove");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_message_is_what_the_client_renders() {
        assert_eq!(PAUSED_MSG, "Paused");
    }
}
