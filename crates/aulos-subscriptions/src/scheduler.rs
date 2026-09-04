//! The per-subscription task: one timer, one permit, one check (DESIGN §14.2).
//!
//! Every subscription gets its own `tokio` task in the manager's `JoinSet`, so a 5-minute
//! subscription fires at 5 minutes rather than at the next multiple of the legacy 60 s tick, and
//! shutdown is one `abort_all()`.
//!
//! The task owns **no** state. Its schedule arrives on a [`tokio::sync::watch`] channel the
//! manager republishes on every change, and its result leaves on an mpsc back to the manager. That
//! is what keeps the manager the single writer: a task cannot persist, cannot publish an event and
//! cannot decide its own next due time.
//!
//! ```text
//!   manager ──watch<TaskParams>──► task ──Semaphore permit──► FeedChecker::check
//!      ▲                                                              │
//!      └──────────────── mpsc<CheckMsg> ◄─────────────────────────────┘
//! ```

use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::Clock;
use aulos_core::id::{SubId, UnixMs};
use aulos_core::subscription::SubscriptionRecord;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use crate::check::FeedChecker;
use crate::model::{CheckFailure, CheckReport, Timing};

/// Everything a task needs to decide *when* and *what* to check.
///
/// Republished by the manager after every mutation, so an `update` that changes the interval, a
/// `check` that pulls the due time forward and a completed check all reach the task the same way.
#[derive(Clone, Debug)]
pub struct TaskParams {
    /// `false` parks the task on the watch channel (DESIGN §14.2).
    pub enabled: bool,
    /// Unix ms of the next check.
    pub next_due: UnixMs,
    /// The record the check runs against.
    pub record: Arc<SubscriptionRecord>,
}

impl TaskParams {
    /// The params for a freshly-loaded record.
    #[must_use]
    pub fn new(record: Arc<SubscriptionRecord>, next_due: UnixMs) -> Self {
        Self {
            enabled: record.enabled,
            next_due,
            record,
        }
    }
}

/// What a task reports back to the manager.
#[derive(Debug)]
pub enum CheckMsg {
    /// A permit was acquired and the check has begun — the manager flips `checking: true`.
    Started(SubId),
    /// The check finished. The manager persists, recomputes `next_due` and publishes.
    Done {
        /// Which subscription.
        id: SubId,
        /// What happened.
        result: Box<Result<CheckReport, CheckFailure>>,
    },
}

/// One subscription's timer task.
pub struct SubTask {
    id: SubId,
    params: watch::Receiver<TaskParams>,
    slots: Arc<Semaphore>,
    checker: Arc<dyn FeedChecker>,
    clock: Arc<dyn Clock>,
    timing: Timing,
    out: mpsc::Sender<CheckMsg>,
}

impl std::fmt::Debug for SubTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubTask")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SubTask {
    /// Assembles a task. Nothing runs until [`Self::run`] is polled.
    #[must_use]
    pub fn new(
        id: SubId,
        params: watch::Receiver<TaskParams>,
        slots: Arc<Semaphore>,
        checker: Arc<dyn FeedChecker>,
        clock: Arc<dyn Clock>,
        timing: Timing,
        out: mpsc::Sender<CheckMsg>,
    ) -> Self {
        Self {
            id,
            params,
            slots,
            checker,
            clock,
            timing,
            out,
        }
    }

    /// Sleep, acquire a permit, check, report; repeat until the manager drops the channel.
    ///
    /// The loop never decides anything: it waits for `next_due`, and after reporting it waits for
    /// the manager to republish. A `watch` change at any point restarts the wait with the new
    /// schedule, which is how `enabled = false` parks it and how `check` wakes it.
    pub async fn run(mut self) {
        loop {
            let params = self.params.borrow_and_update().clone();

            if !params.enabled {
                tracing::debug!(subscription = self.id.as_str(), "subscription parked");
                if self.params.changed().await.is_err() {
                    return;
                }
                continue;
            }

            let delay = until(params.next_due, self.clock.now_ms());
            tokio::select! {
                biased;
                changed = self.params.changed() => {
                    if changed.is_err() { return; }
                    continue;
                }
                () = tokio::time::sleep(delay) => {}
            }

            let Ok(permit) = Arc::clone(&self.slots).acquire_owned().await else {
                return; // the manager closed the semaphore: shutdown
            };
            if self
                .out
                .send(CheckMsg::Started(self.id.clone()))
                .await
                .is_err()
            {
                return;
            }

            let window = self.timing.check_timeout();
            let result =
                match tokio::time::timeout(window, self.checker.check(&params.record)).await {
                    Ok(r) => r,
                    Err(_) => Err(CheckFailure::Timeout(window.as_secs())),
                };
            drop(permit);

            if self
                .out
                .send(CheckMsg::Done {
                    id: self.id.clone(),
                    result: Box::new(result),
                })
                .await
                .is_err()
            {
                return;
            }

            // The manager owns `next_due`, so wait for it to republish rather than guessing. This
            // is also what stops a fast-failing feed from spinning.
            if self.params.changed().await.is_err() {
                return;
            }
        }
    }

    /// Spawns [`Self::run`] onto the current runtime.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(self.run())
    }
}

/// How long until `due`, never negative.
#[must_use]
pub fn until(due: UnixMs, now: UnixMs) -> Duration {
    Duration::from_millis(due.saturating_sub(now).max(0).unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_past_due_time_is_no_wait_at_all() {
        assert_eq!(until(1_000, 5_000), Duration::ZERO);
        assert_eq!(until(5_000, 5_000), Duration::ZERO);
        assert_eq!(until(5_500, 5_000), Duration::from_millis(500));
        // `saturating_sub` clamps at `i64::MAX`, so the wait is never longer than that.
        assert_eq!(
            until(i64::MAX, i64::MIN),
            Duration::from_millis(i64::MAX.unsigned_abs())
        );
    }

    #[test]
    fn task_params_take_enabled_from_the_record() {
        use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};

        let mut record = SubscriptionRecord::new(
            SubId::parse("01JC").expect("id"),
            "C",
            url::Url::parse("https://a.test/@c").expect("url"),
            Selection::new(
                DownloadType::Video,
                Codec::Auto,
                FormatId::parse("any").expect("format"),
                QualityId::parse("best").expect("quality"),
            ),
        );
        record.enabled = false;
        let p = TaskParams::new(Arc::new(record), 42);
        assert!(!p.enabled);
        assert_eq!(p.next_due, 42);
    }
}
