//! `CLEAR_COMPLETED_AFTER`: the auto-clear sweeper (DESIGN §8.10).
//!
//! `clear_after` is **persisted** when an item reaches `finished` or `error`, so the timer survives
//! a restart — legacy lost it. The sweeper runs on the 1 Hz `Tick` plus the engine's
//! `sleep_until(min(clear_after))` fast path, and it queries SQLite rather than the item cache, so
//! auto-clear also covers items that have aged out of the in-memory done window.

use aulos_core::{ItemId, RemoveReason, UnixMs};

use crate::engine::Engine;

impl Engine {
    /// Deletes every row whose `clear_after` has passed (DESIGN §8.10).
    pub(crate) async fn sweep_clears(&mut self) {
        if self.cfg.clear_completed_after == 0 {
            self.next_clear_at = None;
            return;
        }
        let now = self.clock.now_ms();
        if self.next_clear_at.is_some_and(|at| at > now) {
            return;
        }
        let due: Vec<ItemId> = match self.store.due_clears(now).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "the auto-clear sweep could not read the queue");
                self.next_clear_at = Some(now + 1_000);
                return;
            }
        };
        if !due.is_empty() {
            tracing::info!(count = due.len(), "auto-clearing completed items");
            let remove_files = self.cfg.delete_file_on_trashcan;
            self.remove_rows(&due, remove_files, RemoveReason::Expired)
                .await;
        }
        self.next_clear_at = Some(self.next_armed_clear(now));
    }

    /// The earliest `clear_after` still armed among the cached rows, or a bounded re-check.
    ///
    /// A row that has aged out of the memory window has no cached `clear_after`, so the fallback
    /// is one window's worth of delay: every such row was armed before the rows still in the
    /// window, so the sweep that just ran has already taken them.
    fn next_armed_clear(&self, now: UnixMs) -> UnixMs {
        let soonest = self
            .items
            .values()
            .filter_map(|i| i.clear_after)
            .filter(|at| *at > now)
            .min();
        match soonest {
            Some(at) => at,
            None => {
                let window = i64::try_from(self.cfg.clear_completed_after.saturating_mul(1_000))
                    .unwrap_or(60_000);
                now + window.max(1_000)
            }
        }
    }
}
