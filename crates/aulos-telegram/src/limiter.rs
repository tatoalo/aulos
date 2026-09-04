//! The rate-limit budget of DESIGN §12.4 — the part that breaks naive implementations.
//!
//! | Limit | Reality | What is done about it |
//! |---|---|---|
//! | per-chat message/edit rate | ~1 msg/s sustained; `editMessageText` counts against it | one interval per chat: 1 edit per `AULOS_TELEGRAM_EDIT_INTERVAL_MS` (3000) |
//! | global bot rate | ~30 msg/s | one `governor` GCRA limiter, 20/s, burst 5 |
//! | `message is not modified` (400) | Telegram rejects an unchanged edit | compared against `last_rendered` by the caller and skipped entirely |
//! | `429` with `retry_after` | must be respected or throttling escalates | sleep `d + 250 ms`, **double** that chat's effective interval up to 30 s, halve it back after 3 consecutive successes |
//!
//! `teloxide::adaptors::Throttle` is deliberately **not** stacked on top: two independent limiters
//! on the least critical path in the system is redundant machinery, and this layer is the one that
//! knows about the per-chat interval and the `RetryAfter` escalation.
//!
//! The per-chat half is hand-rolled rather than a second `governor` limiter for a reason a comment
//! is worth: a GCRA `Quota` is immutable, and the whole point of the `429` rule is that a chat's
//! effective interval **changes at runtime**. The global half, whose quota never changes, is
//! `governor`.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::Clock;
use governor::{Quota, RateLimiter};
use tokio::time::Instant;

/// The global cap, in messages per second (DESIGN §12.4).
pub const GLOBAL_PER_SEC: u32 = 20;
/// The global burst allowance.
pub const GLOBAL_BURST: u32 = 5;
/// The ceiling a chat's effective interval escalates to.
pub const MAX_EFFECTIVE_MS: u64 = 30_000;
/// The pad added to Telegram's `retry_after` before the next attempt.
pub const RETRY_AFTER_PAD_MS: u64 = 250;
/// How many consecutive successes halve an escalated interval back down.
pub const SUCCESSES_TO_HALVE: u32 = 3;
/// How many attempts a transient network failure gets before the edit is dropped.
pub const NETWORK_ATTEMPTS: u32 = 3;

/// A `governor` clock over the injected [`Clock`], so the global GCRA limiter is driven by the
/// same time source as the per-chat half.
///
/// Without this the global limiter would read the machine's real clock while the per-chat
/// intervals read a `FakeClock`, and a test that means to exercise the per-chat rule would trip
/// the global burst instead — which is exactly the kind of "the test passes for the wrong reason"
/// this layer exists to prevent. `governor` already implements its `Reference` trait for
/// [`Duration`], so the whole adapter is one method.
#[derive(Clone)]
struct InjectedClock {
    clock: Arc<dyn Clock>,
    origin: Instant,
}

impl governor::clock::Clock for InjectedClock {
    type Instant = Duration;

    fn now(&self) -> Duration {
        self.clock.instant().saturating_duration_since(self.origin)
    }
}

/// The global GCRA limiter type.
type Global = RateLimiter<
    governor::state::NotKeyed,
    governor::state::InMemoryState,
    InjectedClock,
    governor::middleware::NoOpMiddleware<Duration>,
>;

/// One chat's own budget.
#[derive(Clone, Copy, Debug)]
struct ChatLimit {
    /// When the next edit may go out.
    next_allowed: Instant,
    /// The current interval, doubled by a `429` and halved back by successes.
    effective_ms: u64,
    /// Consecutive successes since the last `429`.
    successes: u32,
}

/// Why an edit was not allowed through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Denied {
    /// This chat's own interval has not elapsed.
    ChatInterval,
    /// The global bot budget is exhausted.
    Global,
}

/// The per-chat and global budget.
pub struct Limiter {
    base_interval_ms: u64,
    chats: HashMap<i64, ChatLimit>,
    global: Global,
    clock: Arc<dyn Clock>,
    throttled: u64,
}

impl std::fmt::Debug for Limiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Limiter")
            .field("base_interval_ms", &self.base_interval_ms)
            .field("chats", &self.chats.len())
            .field("throttled", &self.throttled)
            .finish_non_exhaustive()
    }
}

impl Limiter {
    /// A limiter with the DESIGN §12.4 global quota and `base_interval_ms` per chat.
    #[must_use]
    pub fn new(base_interval_ms: u64, clock: Arc<dyn Clock>) -> Self {
        Self::with_global(base_interval_ms, GLOBAL_PER_SEC, GLOBAL_BURST, clock)
    }

    /// A limiter with an explicit global quota, so a test can make the global cap bind.
    #[must_use]
    pub fn with_global(
        base_interval_ms: u64,
        per_sec: u32,
        burst: u32,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let per_sec = NonZeroU32::new(per_sec.max(1)).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(burst.max(1)).unwrap_or(NonZeroU32::MIN);
        let governor_clock = InjectedClock {
            clock: Arc::clone(&clock),
            origin: clock.instant(),
        };
        Self {
            base_interval_ms: base_interval_ms.max(1),
            chats: HashMap::new(),
            global: RateLimiter::direct_with_clock(
                Quota::per_second(per_sec).allow_burst(burst),
                governor_clock,
            ),
            clock,
            throttled: 0,
        }
    }

    /// Whether an edit for `chat` may go out **now**, consuming one global cell if so.
    ///
    /// # Errors
    /// [`Denied`] with the reason, so a caller can log which budget bound.
    pub fn acquire(&mut self, chat: i64) -> Result<(), Denied> {
        let now = self.clock.instant();
        let base = self.base_interval_ms;
        let limit = self.chats.entry(chat).or_insert(ChatLimit {
            next_allowed: now,
            effective_ms: base,
            successes: 0,
        });
        if now < limit.next_allowed {
            self.throttled += 1;
            return Err(Denied::ChatInterval);
        }
        if self.global.check().is_err() {
            self.throttled += 1;
            return Err(Denied::Global);
        }
        Ok(())
    }

    /// Records a successful send for `chat`: arms the next interval and, after
    /// [`SUCCESSES_TO_HALVE`] in a row, halves an escalated interval back towards the base.
    pub fn record_sent(&mut self, chat: i64) {
        let now = self.clock.instant();
        let base = self.base_interval_ms;
        let limit = self.chats.entry(chat).or_insert(ChatLimit {
            next_allowed: now,
            effective_ms: base,
            successes: 0,
        });
        limit.successes = limit.successes.saturating_add(1);
        if limit.successes >= SUCCESSES_TO_HALVE && limit.effective_ms > base {
            limit.effective_ms = (limit.effective_ms / 2).max(base);
            limit.successes = 0;
        }
        limit.next_allowed = now + Duration::from_millis(limit.effective_ms);
    }

    /// Records a `429` for `chat` and returns how long the caller must sleep.
    ///
    /// The chat's effective interval **doubles**, up to [`MAX_EFFECTIVE_MS`], and the success
    /// streak resets — so a bot that has been told to back off does not immediately try again at
    /// the old rate.
    pub fn record_retry_after(&mut self, chat: i64, retry_after: Duration) -> Duration {
        let now = self.clock.instant();
        let base = self.base_interval_ms;
        let limit = self.chats.entry(chat).or_insert(ChatLimit {
            next_allowed: now,
            effective_ms: base,
            successes: 0,
        });
        limit.effective_ms = limit
            .effective_ms
            .saturating_mul(2)
            .min(MAX_EFFECTIVE_MS)
            .max(base);
        limit.successes = 0;
        let sleep = retry_after + Duration::from_millis(RETRY_AFTER_PAD_MS);
        limit.next_allowed = now + sleep.max(Duration::from_millis(limit.effective_ms));
        self.throttled += 1;
        sleep
    }

    /// This chat's current interval, in milliseconds.
    #[must_use]
    pub fn effective_interval_ms(&self, chat: i64) -> u64 {
        self.chats
            .get(&chat)
            .map_or(self.base_interval_ms, |c| c.effective_ms)
    }

    /// The configured base interval.
    #[must_use]
    pub const fn base_interval_ms(&self) -> u64 {
        self.base_interval_ms
    }

    /// `edits_throttled_total` — what `healthz` exports so over-budget behaviour is observable
    /// (DESIGN §12.4).
    #[must_use]
    pub const fn throttled_total(&self) -> u64 {
        self.throttled
    }

    /// Drops a chat's budget. Called when its board is retired.
    pub fn forget(&mut self, chat: i64) {
        self.chats.remove(&chat);
    }

    /// How many chats have a budget.
    #[must_use]
    pub fn tracked_chats(&self) -> usize {
        self.chats.len()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use aulos_core::clock::FakeClock;

    use super::*;

    fn limiter(interval_ms: u64) -> (Limiter, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::default());
        (
            Limiter::new(interval_ms, Arc::clone(&clock) as Arc<dyn Clock>),
            clock,
        )
    }

    #[tokio::test]
    async fn a_chat_gets_one_edit_per_interval() {
        let (mut l, clock) = limiter(3_000);
        assert_eq!(l.acquire(7), Ok(()));
        l.record_sent(7);

        assert_eq!(l.acquire(7), Err(Denied::ChatInterval), "too soon");
        clock.advance(Duration::from_millis(2_999));
        assert_eq!(l.acquire(7), Err(Denied::ChatInterval), "still too soon");
        clock.advance(Duration::from_millis(1));
        assert_eq!(l.acquire(7), Ok(()), "the interval has elapsed");
        assert_eq!(l.throttled_total(), 2);
    }

    #[tokio::test]
    async fn chats_have_independent_budgets() {
        let (mut l, _clock) = limiter(3_000);
        assert_eq!(l.acquire(1), Ok(()));
        l.record_sent(1);
        assert_eq!(l.acquire(1), Err(Denied::ChatInterval));
        assert_eq!(l.acquire(2), Ok(()), "a different chat is unaffected");
        assert_eq!(l.tracked_chats(), 2);
        l.forget(1);
        assert_eq!(l.tracked_chats(), 1);
    }

    /// DESIGN §12.4: an injected `RetryAfter(7)` sleeps `d + 250 ms` and **doubles** the interval;
    /// three successes halve it back.
    #[tokio::test]
    async fn a_retry_after_doubles_the_interval_and_successes_halve_it_back() {
        let (mut l, clock) = limiter(3_000);
        assert_eq!(l.effective_interval_ms(7), 3_000);

        let sleep = l.record_retry_after(7, Duration::from_secs(7));
        assert_eq!(
            sleep,
            Duration::from_millis(7_250),
            "the server's retry_after plus the 250 ms pad"
        );
        assert_eq!(l.effective_interval_ms(7), 6_000, "doubled");
        // And the chat is blocked for at least that long.
        clock.advance(Duration::from_millis(7_249));
        assert_eq!(l.acquire(7), Err(Denied::ChatInterval));
        clock.advance(Duration::from_millis(1));
        assert_eq!(l.acquire(7), Ok(()));

        // Three consecutive successes halve it back to the base.
        for i in 1..=3 {
            l.record_sent(7);
            clock.advance(Duration::from_millis(l.effective_interval_ms(7)));
            assert_eq!(l.acquire(7), Ok(()), "success {i}");
        }
        assert_eq!(l.effective_interval_ms(7), 3_000, "back to the base");
    }

    #[tokio::test]
    async fn the_escalation_stops_at_thirty_seconds() {
        let (mut l, _clock) = limiter(3_000);
        let mut seen = Vec::new();
        for _ in 0..8 {
            l.record_retry_after(7, Duration::from_secs(1));
            seen.push(l.effective_interval_ms(7));
        }
        assert_eq!(
            seen,
            vec![
                6_000, 12_000, 24_000, 30_000, 30_000, 30_000, 30_000, 30_000
            ],
            "doubling, capped at MAX_EFFECTIVE_MS"
        );
    }

    #[tokio::test]
    async fn a_success_streak_is_reset_by_a_retry_after() {
        let (mut l, clock) = limiter(1_000);
        l.record_retry_after(7, Duration::from_secs(0));
        assert_eq!(l.effective_interval_ms(7), 2_000);
        // Two successes are not three, so nothing halves.
        for _ in 0..2 {
            clock.advance(Duration::from_millis(3_000));
            assert_eq!(l.acquire(7), Ok(()));
            l.record_sent(7);
        }
        assert_eq!(l.effective_interval_ms(7), 2_000);
        // A `429` in between throws the streak away.
        l.record_retry_after(7, Duration::from_secs(0));
        assert_eq!(l.effective_interval_ms(7), 4_000);
        for _ in 0..2 {
            clock.advance(Duration::from_millis(5_000));
            assert_eq!(l.acquire(7), Ok(()));
            l.record_sent(7);
        }
        assert_eq!(l.effective_interval_ms(7), 4_000, "still two, not three");
    }

    /// DESIGN §12.4: the global cap holds across chats, not just within one.
    #[tokio::test]
    async fn the_global_cap_holds_under_five_chats() {
        let clock = Arc::new(FakeClock::default());
        // One per second, burst 3: the fourth attempt across *any* chats is refused.
        let mut l = Limiter::with_global(1, 1, 3, Arc::clone(&clock) as Arc<dyn Clock>);
        let mut allowed = 0;
        let mut refused = Vec::new();
        for chat in 1..=5 {
            match l.acquire(chat) {
                Ok(()) => {
                    allowed += 1;
                    l.record_sent(chat);
                }
                Err(reason) => refused.push(reason),
            }
        }
        assert_eq!(allowed, 3, "the burst allowance, then the global cap");
        assert_eq!(refused, vec![Denied::Global, Denied::Global]);
        assert_eq!(l.throttled_total(), 2);
    }

    #[tokio::test]
    async fn an_interval_of_zero_is_clamped_to_one_millisecond() {
        let (mut l, _clock) = limiter(0);
        assert_eq!(l.base_interval_ms(), 1);
        assert_eq!(l.acquire(1), Ok(()));
    }
}
