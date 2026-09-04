//! The per-job watchdogs (DESIGN §8.11).
//!
//! One task per running job, armed with `sleep_until` and re-armed when the job's heartbeat
//! advances — no polling, no lock — and it covers web and subscription downloads, not only
//! Telegram-originated ones (legacy polled every 15 s from inside the bot).
//!
//! | Timer | Default | Action |
//! |---|---|---|
//! | stall | `AULOS_JOB_STALL_SECS` (900) | `Notice{code:"stalled"}`, **once**. Never cancels. |
//! | hard timeout | `AULOS_JOB_TIMEOUT_SECS` (0 = off) | `Notice{code:"job_timeout"}`, then cancel |
//!
//! The Telegram thresholds stay separate (`aulos-telegram`, DESIGN §12.4): the bot's warnings are
//! notifications, the job watchdog is a safety net.
//!
//! # Distinguishing a drop storm from a stall
//!
//! DESIGN §4.7 asks for `last_frame_at` to be bumped "before the drop decision" so a sustained
//! progress-channel drop storm is not mistaken for a stalled download. The drop decision is made
//! by `ProgressSink::progress`, in `aulos-provider`, whose per-item state the engine cannot see —
//! only the factory's process-wide [`aulos_provider::ProgressSinkFactory::dropped`] counter is
//! observable. So the watchdog treats **either** a heartbeat advance **or** an increase in that
//! counter as liveness, which is exactly the discrimination the design asks for. See
//! `docs/INTEGRATION-NOTES.md`, WP-12: an additive per-item beat on `ProgressSink` would let the
//! sink itself record it and make the global counter unnecessary.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::{Clock, DomainEvent, EventSender, ItemId, Level, notice_code};
use aulos_provider::ProgressSinkFactory;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cmd::EngineCmd;

/// One running job's liveness (DESIGN §4.7, §8.11).
#[derive(Debug)]
pub struct JobBeat {
    last_ms: AtomicI64,
    frames: AtomicU64,
}

impl JobBeat {
    /// A beat whose last frame is `now_ms`.
    #[must_use]
    pub fn new(now_ms: i64) -> Self {
        Self {
            last_ms: AtomicI64::new(now_ms),
            frames: AtomicU64::new(0),
        }
    }

    /// Records a frame. Called on **every** frame the aggregator receives.
    pub fn frame(&self, now_ms: i64) {
        self.last_ms.store(now_ms, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// When the last frame arrived, in unix milliseconds.
    #[must_use]
    pub fn last_ms(&self) -> i64 {
        self.last_ms.load(Ordering::Relaxed)
    }

    /// How many frames have been recorded.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// The per-job liveness table (DESIGN §8.11).
///
/// Copy-on-write over an `ArcSwap`, so there is no `Mutex` anywhere on the progress path: the
/// engine is the only writer of the *table* (arm/disarm, at most a few times per job) and the
/// aggregator only ever writes through an `Arc<JobBeat>` it already holds. The table is bounded by
/// `MAX_CONCURRENT_DOWNLOADS` plus the per-provider pools, so cloning it per arm is a few dozen
/// pointer copies.
#[derive(Clone, Debug, Default)]
pub struct Heartbeats {
    table: Arc<ArcSwap<HashMap<ItemId, Arc<JobBeat>>>>,
}

impl Heartbeats {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms a beat for `id`, replacing any previous one, and hands it back.
    #[must_use]
    pub fn arm(&self, id: ItemId, now_ms: i64) -> Arc<JobBeat> {
        let beat = Arc::new(JobBeat::new(now_ms));
        let mut next = (**self.table.load()).clone();
        next.insert(id, Arc::clone(&beat));
        self.table.store(Arc::new(next));
        beat
    }

    /// Drops `id`'s beat.
    pub fn disarm(&self, id: ItemId) {
        if !self.table.load().contains_key(&id) {
            return;
        }
        let mut next = (**self.table.load()).clone();
        next.remove(&id);
        self.table.store(Arc::new(next));
    }

    /// `id`'s beat, if it has one.
    #[must_use]
    pub fn get(&self, id: ItemId) -> Option<Arc<JobBeat>> {
        self.table.load().get(&id).cloned()
    }

    /// Records a frame for `id`. **The aggregator's entry point** — call it on every progress
    /// frame received, whatever the frame carries.
    pub fn frame(&self, id: ItemId, now_ms: i64) {
        if let Some(beat) = self.get(id) {
            beat.frame(now_ms);
        }
    }

    /// How many jobs are armed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.load().len()
    }

    /// Whether no job is armed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What one watchdog task needs (DESIGN §8.11).
pub(crate) struct Watchdog {
    pub(crate) id: ItemId,
    pub(crate) beat: Arc<JobBeat>,
    pub(crate) stall: Option<Duration>,
    pub(crate) hard: Option<Duration>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) events: EventSender,
    pub(crate) sink: ProgressSinkFactory,
    pub(crate) tx: mpsc::Sender<EngineCmd>,
    pub(crate) cancel: CancellationToken,
}

impl Watchdog {
    /// Runs until the job's token is cancelled, or until the hard timeout has fired.
    ///
    /// The stall notice is emitted at most **once** per job and never cancels. The hard timeout
    /// emits its notice and then cancels the job through the ordinary [`EngineCmd::Cancel`] path,
    /// so the status write, the partial cleanup and the terminal frame all happen in one place.
    pub(crate) async fn run(self) {
        if self.stall.is_none() && self.hard.is_none() {
            return;
        }
        let started_ms = self.clock.now_ms();
        // The last sign of life: a progress frame, or a drop storm at the sink. Both re-arm the
        // stall timer, which is what makes a saturated progress channel distinguishable from a
        // stalled download (DESIGN §4.7, and the module docs).
        let mut alive_ms = started_ms;
        let mut stall_fired = false;
        let mut last_dropped = self.sink.dropped();

        loop {
            let now = self.clock.now_ms();
            let mut wake: Option<Duration> = None;
            if let Some(stall) = self.stall.filter(|_| !stall_fired) {
                wake = Some(remaining(alive_ms.max(self.beat.last_ms()), now, stall));
            }
            if let Some(hard) = self.hard {
                let until_hard = remaining(started_ms, now, hard);
                wake = Some(wake.map_or(until_hard, |w| w.min(until_hard)));
            }
            // Nothing left to wait for: the stall notice has fired and there is no hard timeout.
            let Some(wake) = wake else {
                return;
            };

            tokio::select! {
                () = self.cancel.cancelled() => return,
                () = tokio::time::sleep(wake) => {}
            }

            let now = self.clock.now_ms();
            let dropped = self.sink.dropped();
            if dropped > last_dropped {
                alive_ms = now;
            }
            last_dropped = dropped;

            if let Some(hard) = self.hard
                && elapsed(started_ms, now) >= hard
            {
                let secs = hard.as_secs();
                self.events
                    .publish(DomainEvent::Notice {
                        level: Level::Error,
                        code: notice_code::JOB_TIMEOUT,
                        id: Some(self.id),
                        message: format!("Job exceeded the {secs}s hard timeout; cancelling")
                            .into_boxed_str(),
                    })
                    .await;
                let (ack, _reply) = tokio::sync::oneshot::channel();
                let _ = self
                    .tx
                    .send(EngineCmd::Cancel {
                        ids: vec![self.id],
                        ack,
                    })
                    .await;
                return;
            }

            if let Some(stall) = self.stall.filter(|_| !stall_fired)
                && elapsed(alive_ms.max(self.beat.last_ms()), now) >= stall
            {
                stall_fired = true;
                let secs = stall.as_secs();
                self.events
                    .publish(DomainEvent::Notice {
                        level: Level::Warn,
                        code: notice_code::STALLED,
                        id: Some(self.id),
                        message: format!("No progress for {secs}s").into_boxed_str(),
                    })
                    .await;
            }
        }
    }
}

/// How long until `since + window`, never below one millisecond so the loop cannot spin.
fn remaining(since_ms: i64, now_ms: i64, window: Duration) -> Duration {
    let done = elapsed(since_ms, now_ms);
    window
        .checked_sub(done)
        .unwrap_or(Duration::ZERO)
        .max(Duration::from_millis(1))
}

/// `now - since`, clamped at zero — a clock that went backwards is not a stall.
fn elapsed(since_ms: i64, now_ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(now_ms.saturating_sub(since_ms)).unwrap_or(0))
}

/// Spawns a watchdog, or returns `None` when both timers are off.
pub(crate) fn spawn(w: Watchdog) -> Option<JoinHandle<()>> {
    if w.stall.is_none() && w.hard.is_none() {
        return None;
    }
    Some(tokio::spawn(w.run()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arming_and_disarming_is_copy_on_write() {
        let beats = Heartbeats::new();
        assert!(beats.is_empty());
        let a = ItemId::new();
        let b = ItemId::new();
        let beat = beats.arm(a, 1_000);
        let _ = beats.arm(b, 1_000);
        assert_eq!(beats.len(), 2);

        beats.frame(a, 2_000);
        assert_eq!(beat.last_ms(), 2_000);
        assert_eq!(beat.frames(), 1);

        beats.disarm(a);
        assert_eq!(beats.len(), 1);
        assert!(beats.get(a).is_none());
        // The handle a task already holds keeps working after a disarm.
        beat.frame(3_000);
        assert_eq!(beat.last_ms(), 3_000);
        // Disarming twice, and disarming an unknown id, are both no-ops.
        beats.disarm(a);
        beats.disarm(ItemId::new());
        assert_eq!(beats.len(), 1);
    }

    #[test]
    fn a_frame_for_an_unarmed_item_is_dropped_silently() {
        let beats = Heartbeats::new();
        beats.frame(ItemId::new(), 5);
        assert!(beats.is_empty());
    }

    #[test]
    fn remaining_never_returns_zero() {
        assert_eq!(
            remaining(0, 10_000, Duration::from_secs(5)),
            Duration::from_millis(1)
        );
        assert_eq!(
            remaining(0, 1_000, Duration::from_secs(5)),
            Duration::from_secs(4)
        );
        assert_eq!(elapsed(1_000, 500), Duration::ZERO, "a backwards clock");
    }

    // -----------------------------------------------------------------------
    // The two timers. Both run on paused time, so a 900-second stall costs microseconds, and both
    // read the same `FakeClock` the engine hands the real watchdog.
    // -----------------------------------------------------------------------

    use std::sync::atomic::AtomicBool;

    use aulos_core::{EventFilter, EventInbox, EventRouter, FakeClock, SubscriberSpec};

    /// A watchdog plus everything needed to observe it.
    struct Rig {
        beat: Arc<JobBeat>,
        clock: Arc<FakeClock>,
        inbox: EventInbox,
        cmds: mpsc::Receiver<EngineCmd>,
        sink: ProgressSinkFactory,
        /// Held, never read: a live receiver is what makes a full channel *drop* frames rather
        /// than report them closed, and only a drop increments the factory's counter.
        _progress_rx: mpsc::Receiver<aulos_provider::ProgressMsg>,
        cancel: CancellationToken,
        id: ItemId,
        handle: JoinHandle<()>,
        _router: JoinHandle<()>,
        _sender: aulos_core::EventSender,
    }

    fn rig(stall: Option<Duration>, hard: Option<Duration>) -> Rig {
        let id = ItemId::new();
        let clock = Arc::new(FakeClock::default());
        let beat = Arc::new(JobBeat::new(clock.now_ms()));
        let (mut router, sender) = EventRouter::new(64);
        let inbox = router.subscribe(SubscriberSpec {
            name: "test",
            capacity: 64,
            policy: aulos_core::DropPolicy::Block,
            filter: EventFilter::all(),
        });
        let router_task = router.spawn();
        let (tx, cmds) = mpsc::channel(16);
        // A one-slot progress channel, so a burst of frames is dropped and the factory's counter
        // climbs — which is the only per-process signal a drop storm produces.
        let (progress, progress_rx) = mpsc::channel(1);
        let sink = ProgressSinkFactory::new(progress);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(
            Watchdog {
                id,
                beat: Arc::clone(&beat),
                stall,
                hard,
                clock: Arc::clone(&clock) as Arc<dyn Clock>,
                events: sender.clone(),
                sink: sink.clone(),
                tx,
                cancel: cancel.clone(),
            }
            .run(),
        );
        Rig {
            beat,
            clock,
            inbox,
            cmds,
            sink,
            _progress_rx: progress_rx,
            cancel,
            id,
            handle,
            _router: router_task,
            _sender: sender,
        }
    }

    /// Lets every spawned task reach its first `await`.
    ///
    /// Load-bearing before the first [`advance`]: a task `tokio::spawn` has not polled yet has no
    /// timer registered, so advancing paused time past its deadline would simply not wake it — it
    /// would arm its sleep from the *advanced* clock instead.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// Moves both clocks together, then lets every task run.
    async fn advance(clock: &FakeClock, by: Duration) {
        clock.advance(by);
        tokio::time::advance(by).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_genuinely_stalled_job_trips_the_stall_watchdog_exactly_once() {
        let mut r = rig(Some(Duration::from_secs(900)), None);
        settle().await;
        advance(&r.clock, Duration::from_secs(400)).await;
        assert!(is_empty(&mut r.inbox).await, "not yet: 400 s of 900");

        advance(&r.clock, Duration::from_secs(600)).await;
        let notice = next_notice(&mut r.inbox).await;
        assert_eq!(&*notice.code, notice_code::STALLED);
        assert_eq!(notice.id, Some(r.id));
        assert_eq!(notice.level, Level::Warn);
        assert_eq!(&*notice.message, "No progress for 900s");

        // Never twice, and never a cancel.
        advance(&r.clock, Duration::from_secs(2_000)).await;
        assert!(
            is_empty(&mut r.inbox).await,
            "the stall notice is emitted once"
        );
        assert!(r.cmds.try_recv().is_err(), "a stall never cancels");
        r.cancel.cancel();
        let _ = r.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_re_arms_the_stall_watchdog() {
        let mut r = rig(Some(Duration::from_secs(900)), None);
        settle().await;
        for _ in 0..4 {
            advance(&r.clock, Duration::from_secs(800)).await;
            r.beat.frame(r.clock.now_ms());
        }
        advance(&r.clock, Duration::from_secs(800)).await;
        assert!(
            is_empty(&mut r.inbox).await,
            "3 200 s of progress must not read as a stall"
        );
        r.cancel.cancel();
        let _ = r.handle.await;
    }

    /// A drop storm: the progress channel is saturated, so every frame the provider reports is
    /// dropped at the sink and never reaches the aggregator — the job is busy, and DESIGN §4.7
    /// requires the watchdog to tell the two apart.
    #[tokio::test(start_paused = true)]
    async fn a_drop_storm_does_not_trip_the_stall_watchdog() {
        let mut r = rig(Some(Duration::from_secs(900)), None);
        settle().await;
        let sink = r.sink.for_item(r.id);
        let stop = Arc::new(AtomicBool::new(false));
        let dropper = tokio::spawn({
            let stop = Arc::clone(&stop);
            async move {
                while !stop.load(Ordering::Relaxed) {
                    sink.progress(aulos_core::RawProgress::default());
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        });

        for _ in 0..40 {
            advance(&r.clock, Duration::from_secs(100)).await;
        }
        assert!(
            r.sink.dropped() > 0,
            "the storm must actually be dropping frames"
        );
        assert!(
            is_empty(&mut r.inbox).await,
            "4 000 s of dropped frames is a busy job, not a stalled one"
        );

        stop.store(true, Ordering::Relaxed);
        r.cancel.cancel();
        let _ = dropper.await;
        let _ = r.handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_hard_timeout_notices_and_then_cancels() {
        let mut r = rig(None, Some(Duration::from_secs(60)));
        settle().await;
        advance(&r.clock, Duration::from_secs(30)).await;
        assert!(r.cmds.try_recv().is_err(), "not yet");

        advance(&r.clock, Duration::from_secs(40)).await;
        let notice = next_notice(&mut r.inbox).await;
        assert_eq!(&*notice.code, notice_code::JOB_TIMEOUT);
        assert_eq!(notice.level, Level::Error);
        match r.cmds.try_recv() {
            Ok(EngineCmd::Cancel { ids, .. }) => assert_eq!(ids, vec![r.id]),
            other => panic!("expected a cancel, got {other:?}"),
        }
        // And the watchdog stops after cancelling.
        let _ = tokio::time::timeout(Duration::from_secs(1), r.handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_watchdog_with_both_timers_off_does_not_even_spawn() {
        let id = ItemId::new();
        let clock = Arc::new(FakeClock::default());
        let (_router, sender) = EventRouter::new(4);
        let (tx, _rx) = mpsc::channel(4);
        let (progress, _held) = mpsc::channel(4);
        assert!(
            super::spawn(Watchdog {
                id,
                beat: Arc::new(JobBeat::new(0)),
                stall: None,
                hard: None,
                clock: clock as Arc<dyn Clock>,
                events: sender,
                sink: ProgressSinkFactory::new(progress),
                tx,
                cancel: CancellationToken::new(),
            })
            .is_none()
        );
    }

    /// Whether the inbox is empty right now. `EventInbox` exposes only `recv`, so a one-tick
    /// timeout is how a paused-time test asks.
    async fn is_empty(inbox: &mut EventInbox) -> bool {
        tokio::time::timeout(Duration::from_millis(1), inbox.recv())
            .await
            .is_err()
    }

    /// The next notice, or a failure — never a hang, so a watchdog that stops publishing shows up
    /// as a red test rather than a stuck suite.
    async fn next_notice(inbox: &mut EventInbox) -> aulos_core::Notice {
        tokio::time::timeout(Duration::from_secs(30), inbox.recv())
            .await
            .expect("the watchdog published nothing")
            .expect("the router is alive")
            .as_notice()
            .expect("a notice")
    }
}
