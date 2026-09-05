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
use crate::engine::Engine;

/// The message a paused job carries (DESIGN §8.7).
pub const PAUSED_MSG: &str = "Paused";

impl Engine {
    /// [`crate::EngineCmd::Start`]: `queued(!auto_start)` → `queued(auto_start)`.
    pub(crate) async fn handle_start(&mut self, ids: Vec<ItemId>) -> ActionsResult {
        let mut result = ActionsResult::default();
        for id in self.expand_targets(ids, &mut result) {
            let Some(item) = self.cached(id) else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            match item.status {
                Status::Queued if !item.auto_start => {
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
                    self.enqueue(id);
                    self.publish_changed(id, Status::Queued, Status::Queued)
                        .await;
                    self.sync_group_of(id).await;
                    result.applied.push(id);
                }
                // Already on its way: `start` is idempotent. It still re-enqueues, because
                // "queued with `auto_start`" and "on a ready deque" are two different facts —
                // a start pressed while a paused job was still being killed leaves the first
                // without the second until its slot is released (`Engine::release_job`).
                Status::Queued => {
                    self.enqueue(id);
                    result.applied.push(id);
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
        for id in self.expand_targets(ids, &mut result) {
            let Some(item) = self.cached(id) else {
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
    async fn park_running(&mut self, id: ItemId) {
        if let Some(slot) = self.running.get_mut(&id) {
            slot.settled = true;
            slot.cancel.cancel();
            drop(slot.slot.take());
            if let Some(w) = slot.watchdog.take() {
                w.abort();
            }
        }
        self.beats.disarm(id);
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
        for id in self.expand_targets(ids, &mut result) {
            let Some(item) = self.cached(id) else {
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
            slot.settled = true;
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
        self.cleanup_partials(id, true);
        self.notify_resolved(id);
    }

    /// [`crate::EngineCmd::Retry`] (DESIGN §8.8).
    pub(crate) async fn handle_retry(&mut self, ids: Vec<ItemId>) -> ActionsResult {
        let mut result = ActionsResult::default();
        for id in self.expand_targets(ids, &mut result) {
            let Some(item) = self.cached(id) else {
                result.skip(id, SkipReason::NotFound);
                continue;
            };
            if !matches!(item.status, Status::Error | Status::Canceled) {
                result.skip(id, SkipReason::NotRetryable);
                continue;
            }
            let at = self.clock.now_ms();
            let source = aulos_core::SourceRef::bare(aulos_core::SourceKind::Retry);
            if !self
                .apply(retry_ops(id, at, source.clone()), Durability::Batched)
                .await
            {
                result.skip(id, SkipReason::NotRetryable);
                continue;
            }
            let from = item.status;
            self.patch(id, |i| {
                i.status = Status::Queued;
                i.auto_start = true;
                i.msg = None;
                i.error = None;
                i.attempt = i.attempt.saturating_add(1);
                i.source = source.clone();
                i.finished_at = None;
            });
            // The row leaves the done window; the published terminal total is the aggregator's
            // and follows the `terminal → queued` view it is about to see.
            self.done_order.retain(|d| *d != id);
            self.on_child_status(id, from, Status::Queued).await;
            self.publish_changed(id, from, Status::Queued).await;
            // A retried item that was never resolved has to resolve again before it can run.
            if item.provider.is_none() {
                self.write_status(
                    id,
                    Status::Resolving,
                    FieldUpdate::Keep,
                    FieldUpdate::Clear,
                    None,
                )
                .await;
                // A retry belongs to no add, so it gets a generation of its own rather than
                // borrowing the last add's — only `CancelScope::All` can condemn it.
                self.add_generation += 1;
                let generation = self.add_generation;
                self.spawn_resolve(id, generation, None).await;
            } else {
                self.enqueue(id);
            }
            result.applied.push(id);
        }
        self.schedule().await;
        result
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
        for id in self.expand_targets(ids, &mut result) {
            if self.cached(id).is_none() {
                result.skip(id, SkipReason::NotFound);
                continue;
            }
            // Deleting a group cancels any active child first, then relies on ON DELETE CASCADE.
            if !self.cached(id).is_some_and(|i| i.status.is_terminal()) {
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
            let paths: Vec<PathBuf> = ids.iter().flat_map(|id| self.files_of(*id)).collect();
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

    /// Every path an item produced (DESIGN §8.10).
    ///
    /// `filename`, **every** `chapter_files`/`subtitle_files` entry, the StreamingCommunity
    /// `.info.json` and `.nfo` siblings, and the scratch directory. Legacy orphaned all of those.
    fn files_of(&self, id: ItemId) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let Some(item) = self.cached(id) else {
            return paths;
        };
        let dir = self.out_dir_for(&item);
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
        paths.push(self.tmp_dir_for(id));
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

        // The not-yet-created children of an in-flight expansion are cancelled by never being
        // created; the group and the children that do exist are cancelled outright.
        for group in expanding {
            self.expansions.remove(&group);
            let children: Vec<ItemId> = self
                .items
                .values()
                .filter(|i| i.group_id == Some(group) && !i.status.is_terminal())
                .map(|i| i.id)
                .collect();
            for child in children {
                self.cancel_one(child).await;
                result.applied.push(child);
            }
            self.sync_group_status(group).await;
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

    /// Expands group ids into themselves plus their non-terminal children, and reports unknown
    /// ids as [`SkipReason::NotFound`] (DESIGN §8.7: "a group cancel cascades").
    fn expand_targets(&self, ids: Vec<ItemId>, result: &mut ActionsResult) -> Vec<ItemId> {
        let mut out: Vec<ItemId> = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(item) = self.items.get(&id) else {
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
