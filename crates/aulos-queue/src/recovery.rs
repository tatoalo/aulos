//! Boot recovery (DESIGN §8.9).
//!
//! Runs after the migrations and the importer and **before** the HTTP listener binds, so the first
//! client sees a consistent snapshot.
//!
//! | Found status | Action |
//! |---|---|
//! | `resolving` | → `queued`, `msg = "Re-queued after restart"` — the resolve task is gone |
//! | `preparing`/`downloading`/`postprocessing` | → `queued`, `BumpAttempt`, `SetSource { kind: "restart" }` |
//! | `queued`, `auto_start = true` | left as-is, pushed to its priority deque ordered by `ord` |
//! | `queued`, `auto_start = false` | left as-is, not scheduled — the legacy `pending` bucket |
//! | terminal | untouched; `clear_after` re-armed; the most recent `AULOS_MEM_DONE_ITEMS` cached |
//! | groups | counters recomputed from children in one `GROUP BY` query |
//!
//! `AULOS_RESTART_POLICY=pause` parks in-flight items as `queued, auto_start = false` instead, for
//! an operator who wants to inspect before resuming.

use std::collections::HashMap;
use std::path::PathBuf;

use aulos_core::{
    FieldUpdate, GroupId, Item, ItemId, Kind, RestartPolicy, SourceKind, SourceRef, Status, UnixMs,
};
use aulos_store::{Durability, WriteOp};

use crate::cmd::EngineError;
use crate::dedupe::DedupeKey;
use crate::engine::Engine;
use crate::groups::GroupAcc;

/// The message a re-queued `resolving` item carries (DESIGN §8.9).
pub const REQUEUED_MSG: &str = "Re-queued after restart";

/// What boot recovery found and did (DESIGN §8.9).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `resolving` rows re-queued.
    pub requeued_resolving: u32,
    /// In-flight rows (`preparing`/`downloading`/`postprocessing`) re-queued or parked.
    pub requeued_running: u32,
    /// `queued(auto_start = true)` rows pushed straight back onto a ready deque.
    pub scheduled: u32,
    /// `queued(auto_start = false)` rows left parked, including the ones this run parked.
    pub parked: u32,
    /// Terminal rows loaded into the done window.
    pub terminal: u32,
    /// How many terminal rows exist in total.
    pub terminal_total: u64,
    /// Groups whose counters were recomputed.
    pub groups: u32,
    /// Terminal rows whose `clear_after` was re-armed.
    pub clear_after_armed: u32,
    /// Orphan `*.part`/`*.ytdl` files (and per-job scratch directories) found in `TEMP_DIR`.
    pub orphan_temp: Vec<PathBuf>,
    /// How many of those were deleted (`AULOS_CLEAN_ORPHAN_TEMP`).
    pub orphan_temp_deleted: u32,
    /// The restart policy that was applied.
    pub policy: &'static str,
}

impl Engine {
    /// Reads the persisted queue, applies the DESIGN §8.9 table and fills the item cache.
    ///
    /// # Errors
    /// [`EngineError::Store`] when the boot query or the recovery write fails. A failure here is
    /// fatal by design: continuing would mean serving a snapshot that does not match the database.
    pub async fn recover(&mut self) -> Result<RecoveryReport, EngineError> {
        let boot = self.store.boot_state().await?;
        let policy = self.cfg.restart_policy;
        let mut report = RecoveryReport {
            policy: match policy {
                RestartPolicy::Resume => "resume",
                RestartPolicy::Pause => "pause",
            },
            terminal_total: self.terminal_item_count().await?,
            ..RecoveryReport::default()
        };
        let now = self.clock.now_ms();
        let mut ops: Vec<WriteOp> = Vec::new();

        // 1. The groups first, so a child's `add_child` finds its accumulator.
        for item in boot.non_terminal.iter().chain(boot.done_window.iter()) {
            if item.kind == Kind::Group {
                let declared = item.children_total.unwrap_or(0);
                self.groups.insert(item.id, GroupAcc::new(declared));
            }
        }

        // 2. Every row into the cache, with the recovery transition applied in memory and one
        //    batched write for the whole lot.
        for item in boot.non_terminal {
            let mut item = item;
            match (item.status, item.auto_start) {
                (Status::Resolving, _) => {
                    report.requeued_resolving += 1;
                    let auto_start = policy == RestartPolicy::Resume && item.auto_start;
                    ops.push(WriteOp::SetStatus {
                        id: item.id,
                        status: Status::Queued,
                        msg: FieldUpdate::Set(REQUEUED_MSG.into()),
                        error: FieldUpdate::Keep,
                        auto_start: Some(auto_start),
                        at: now,
                    });
                    item.status = Status::Queued;
                    item.auto_start = auto_start;
                    item.msg = Some(REQUEUED_MSG.into());
                }
                (s, _) if s.is_running() => {
                    report.requeued_running += 1;
                    let auto_start = policy == RestartPolicy::Resume;
                    let source = SourceRef::bare(SourceKind::Restart);
                    ops.push(WriteOp::SetStatus {
                        id: item.id,
                        status: Status::Queued,
                        msg: FieldUpdate::Set(REQUEUED_MSG.into()),
                        error: FieldUpdate::Keep,
                        auto_start: Some(auto_start),
                        at: now,
                    });
                    ops.push(WriteOp::BumpAttempt { id: item.id });
                    ops.push(WriteOp::SetSource {
                        id: item.id,
                        source: source.clone(),
                    });
                    item.status = Status::Queued;
                    item.auto_start = auto_start;
                    item.msg = Some(REQUEUED_MSG.into());
                    item.attempt = item.attempt.saturating_add(1);
                    item.source = source;
                }
                _ => {}
            }
            if item.status == Status::Queued {
                if item.auto_start {
                    report.scheduled += 1;
                } else {
                    report.parked += 1;
                }
            }
            self.absorb(item);
        }

        // 3. The done window, newest first from the store, and `clear_after` re-arming.
        let window =
            i64::try_from(self.cfg.clear_completed_after.saturating_mul(1_000)).unwrap_or(0);
        for item in boot.done_window {
            let mut item = item;
            report.terminal += 1;
            if window > 0 && item.clear_after.is_none() {
                let at = item.finished_at.unwrap_or(now) + window;
                ops.push(WriteOp::SetClearAfter {
                    id: item.id,
                    at: Some(at),
                });
                item.clear_after = Some(at);
                report.clear_after_armed += 1;
            }
            if let Some(at) = item.clear_after {
                self.next_clear_at = Some(self.next_clear_at.map_or(at, |cur| cur.min(at)));
            }
            self.absorb(item);
        }
        self.next_clear_at = self.next_clear_at.or(boot.next_clear_at);

        if !ops.is_empty() {
            self.store.write(ops, Durability::Sync).await?;
        }

        // 4. Group counters, from the store's own `GROUP BY` plus the loaded children.
        report.groups = self.rebuild_group_counters(&boot.group_counts);

        // 5. Everything schedulable, in `ord` order.
        let mut queued: Vec<(i64, ItemId)> = self
            .items
            .values()
            .filter(|i| i.kind == Kind::Item && i.status == Status::Queued && i.auto_start)
            .map(|i| (i.ord, i.id))
            .collect();
        queued.sort_unstable();
        for (_, id) in queued {
            self.enqueue(id);
        }

        // 6. Stale temp files: logged, not deleted, unless `AULOS_CLEAN_ORPHAN_TEMP`.
        let (found, deleted) = self.scan_orphan_temp();
        report.orphan_temp = found;
        report.orphan_temp_deleted = deleted;

        self.last_drift_ms = now;

        // 7. Seed the realtime side. The aggregator builds the published snapshot from the events
        //    it sees (DESIGN §15.1), so without this the first client after a restart would connect
        //    to an empty snapshot until something happened to change a row. One event carrying the
        //    whole recovered working set, never one per item: the router's inbox is bounded, and a
        //    per-item publish would deadlock a recovery of more than 4 096 rows against a router
        //    that has not been spawned yet.
        let mut views: Vec<ItemId> = self.items.keys().copied().collect();
        views.sort_unstable_by_key(|id| self.items.get(id).map_or(0, |i| i.ord));
        let payload: Vec<std::sync::Arc<aulos_core::ItemView>> = views
            .into_iter()
            .filter_map(|id| self.view_of(id))
            .collect();
        self.publish_added(payload, aulos_core::AddReason::Created)
            .await;

        // 8. Re-enter resolution for what the §8.9 table re-queued out of `resolving`.
        //
        //    Such a row has `provider = NULL` by construction — `handle_add` mints it that way and
        //    only `WriteOp::SetResolved` fills it in, at the *end* of resolution — and nothing
        //    outside `add` and `retry` ever spawns a resolve. Left as `queued`, it is invisible to
        //    the scheduler for the life of the installation: `schedule()` drops a provider-less
        //    row from its deque, `retry` refuses it (`not_retryable`, the status is `queued`), and
        //    every later boot reproduces the same state. Only `delete` cleared it.
        //
        //    Parked rows (`auto_start = false`, which is every row under
        //    `AULOS_RESTART_POLICY=pause`) stay parked, exactly as the table says; `handle_start`
        //    resolves one of those when the user asks for it.
        let unresolved: Vec<ItemId> = {
            let mut ids: Vec<(i64, ItemId)> = self
                .items
                .values()
                .filter(|i| {
                    i.kind == Kind::Item
                        && i.status == Status::Queued
                        && i.auto_start
                        && i.provider.is_none()
                })
                .map(|i| (i.ord, i.id))
                .collect();
            ids.sort_unstable();
            ids.into_iter().map(|(_, id)| id).collect()
        };
        if !unresolved.is_empty() {
            tracing::info!(
                count = unresolved.len(),
                "re-resolving adds that were interrupted mid-resolution"
            );
        }
        for id in unresolved {
            self.restart_resolution(id).await;
        }

        self.schedule().await;
        tracing::info!(
            resolving = report.requeued_resolving,
            running = report.requeued_running,
            scheduled = report.scheduled,
            parked = report.parked,
            terminal = report.terminal,
            groups = report.groups,
            policy = report.policy,
            "boot recovery complete"
        );
        Ok(report)
    }

    /// How many terminal **item** rows the database holds — the aggregator's `done_total` seed.
    ///
    /// `BootState::done_total` counts every terminal row, groups included, but PROTOCOL §5.3 keeps
    /// a group in `items` and the aggregator never puts one in its done bucket: it neither counts
    /// a terminal group in nor counts one out. Seeding from the unfiltered total therefore left
    /// `done_total` permanently one-per-finished-playlist too high, so `truncated.done` stayed
    /// true — "page the rest with `GET api/v2/items?status=finished`" — even after a `clear`
    /// removed literally everything.
    async fn terminal_item_count(&self) -> Result<u64, EngineError> {
        let mut filter = aulos_store::ItemFilter::terminal().with_kind(Kind::Item);
        // Only the `COUNT(*)` is wanted; the rows come from `boot_state`'s own window.
        filter.limit = Some(1);
        Ok(self.store.items(filter).await?.total)
    }

    /// Puts a recovered row into the cache and its indexes without re-publishing anything.
    ///
    /// A resolved row owns **two** dedupe keys at runtime and both are reinstated here: the
    /// `media_id`-derived `canonical_key` resolution produced, and the URL-derived one the item
    /// was added under. Registering only the first would let a fresh `POST` of the very same URL
    /// through after a restart — `DedupeKey::for_url` computes the URL-derived value — which is
    /// exactly the legacy bug DESIGN §8.5 closes, reopened across the restart boot recovery
    /// exists for.
    fn absorb(&mut self, item: Item) {
        let id = item.id;
        let terminal = item.status.is_terminal();
        if !terminal
            && item.kind == Kind::Item
            && let Some(provider) = item.provider.clone()
        {
            let selection = item.request.selection.clone();
            let by_url = DedupeKey::for_url(&provider, &item.url, selection.clone());
            let by_media = DedupeKey::new(item.canonical_key.clone(), selection);
            if by_url != by_media {
                self.dedupe.insert(by_url, id);
            }
            self.dedupe.insert(by_media, id);
        }
        if let Some(group) = item.group_id
            && let Some(acc) = self.groups.get_mut(&group)
        {
            acc.add_child(item.status, crate::entry::size_hint(&item));
        }
        self.items.insert(id, std::sync::Arc::new(item));
        if terminal {
            self.done_order.push_back(id);
        }
    }

    /// Recomputes the group counters from the store's `GROUP BY` plus the cached children.
    ///
    /// The store's [`aulos_store::GroupCounts`] carries four numbers — children, done, error and
    /// active — while [`GroupAcc`] needs all eight status counters. The non-terminal children are
    /// *all* cached by construction, so their statuses come from the cache; `done` and `error` come
    /// from the query; and `canceled` is the remainder. That is exactly enough to reproduce the
    /// counters without loading every child of every group.
    fn rebuild_group_counters(
        &mut self,
        counts: &HashMap<GroupId, aulos_store::GroupCounts>,
    ) -> u32 {
        let groups: Vec<GroupId> = self.groups.keys().copied().collect();
        let mut done = 0;
        for group in groups {
            let declared = self
                .items
                .get(&group)
                .and_then(|i| i.children_total)
                .unwrap_or(0);
            let mut acc = GroupAcc::new(declared);
            let mut non_terminal = 0u32;
            let cached: Vec<(Status, Option<u64>)> = self
                .items
                .values()
                .filter(|i| i.group_id == Some(group))
                .map(|i| (i.status, crate::entry::size_hint(i)))
                .collect();
            for (status, hint) in cached {
                if status.is_terminal() {
                    continue;
                }
                non_terminal += 1;
                acc.add_child(status, hint);
            }
            if let Some(row) = counts.get(&group) {
                for _ in 0..row.done {
                    acc.add_child(Status::Finished, None);
                }
                for _ in 0..row.error {
                    acc.add_child(Status::Error, None);
                }
                let canceled = row
                    .children
                    .saturating_sub(row.done)
                    .saturating_sub(row.error)
                    .saturating_sub(non_terminal);
                for _ in 0..canceled {
                    acc.add_child(Status::Canceled, None);
                }
                acc.total = acc.total.max(row.children).max(declared);
            }
            self.groups.insert(group, acc);
            done += 1;
        }
        done
    }

    /// `*.part`/`*.ytdl` files and per-job scratch directories in `TEMP_DIR` whose owning item no
    /// longer exists (DESIGN §8.9).
    ///
    /// Logged, not deleted, unless `AULOS_CLEAN_ORPHAN_TEMP=true`: deleting user data on boot by
    /// default is not acceptable, and yt-dlp resumes HTTP downloads from `.part`, so leaving them
    /// is also the faster choice.
    fn scan_orphan_temp(&self) -> (Vec<PathBuf>, u32) {
        let temp = &self.cfg.paths.temp;
        let Ok(entries) = std::fs::read_dir(temp) else {
            return (Vec::new(), 0);
        };
        let mut found = Vec::new();
        let mut deleted = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let orphan = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                // A per-job scratch directory is named after its item.
                name.parse::<ItemId>()
                    .is_ok_and(|id| !self.items.contains_key(&id))
            } else {
                (name.ends_with(".part") || name.ends_with(".ytdl")) && !self.owns_temp_file(&name)
            };
            if !orphan {
                continue;
            }
            found.push(path.clone());
            if !self.cfg.clean_orphan_temp {
                tracing::warn!(path = %path.display(), "orphan temp file left in place");
                continue;
            }
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match removed {
                Ok(()) => deleted += 1,
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "cannot remove"),
            }
        }
        (found, deleted)
    }

    /// Whether some live item claims this temp file name.
    fn owns_temp_file(&self, name: &str) -> bool {
        let stem = name.trim_end_matches(".part").trim_end_matches(".ytdl");
        self.items.values().any(|i| {
            i.filename
                .as_ref()
                .is_some_and(|f| f.as_str() == stem || name.starts_with(f.as_str()))
        })
    }

    /// The earliest armed `clear_after`, for tests and for `healthz`.
    #[must_use]
    pub fn next_clear_at(&self) -> Option<UnixMs> {
        self.next_clear_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_requeue_message_is_the_design_string() {
        assert_eq!(REQUEUED_MSG, "Re-queued after restart");
    }

    #[test]
    fn the_report_defaults_to_nothing_found() {
        let r = RecoveryReport::default();
        assert_eq!(r.requeued_resolving, 0);
        assert!(r.orphan_temp.is_empty());
        assert_eq!(r.policy, "");
    }
}
