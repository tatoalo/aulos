//! Time, behind a trait, so every scheduler and watchdog is testable without sleeping.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::Instant;

use crate::id::UnixMs;

/// Wall-clock and monotonic time.
///
/// [`Self::instant`] returns a [`tokio::time::Instant`] rather than a [`std::time::Instant`] so
/// that a test using `tokio::time::pause()` controls both the timers and the values the
/// aggregator's stall watchdog compares (DESIGN §4.7).
pub trait Clock: Send + Sync {
    /// Unix milliseconds. Every timestamp on the wire uses this unit.
    fn now_ms(&self) -> UnixMs;
    /// A monotonic instant, for deadlines and elapsed-time comparisons.
    fn instant(&self) -> Instant;
}

/// The production clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> UnixMs {
        // `duration_since` only fails for a pre-1970 clock; a negative offset is the honest answer
        // there, and it keeps this method infallible.
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(e) => -(i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX)),
        }
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }
}

/// A manually advanced clock for tests.
///
/// Both halves move together: [`Self::advance`] adds to the wall clock **and** the monotonic
/// instant, so a test can assert a debounce, a backoff and a stall watchdog without a real sleep.
#[derive(Debug)]
pub struct FakeClock {
    now_ms: AtomicI64,
    base: Mutex<Instant>,
}

impl FakeClock {
    /// A clock reading `now_ms`, with its monotonic half anchored at the current instant.
    #[must_use]
    pub fn new(now_ms: UnixMs) -> Self {
        Self {
            now_ms: AtomicI64::new(now_ms),
            base: Mutex::new(Instant::now()),
        }
    }

    /// Moves both halves forward.
    ///
    /// # Panics
    /// If another thread panicked while holding the internal lock.
    pub fn advance(&self, by: Duration) {
        let millis = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
        self.now_ms.fetch_add(millis, Ordering::SeqCst);
        let mut base = self
            .base
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *base += by;
    }

    /// Sets the wall-clock half without touching the monotonic half.
    pub fn set_now_ms(&self, now_ms: UnixMs) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }
}

/// `2026-03-04T00:00:00Z` in Unix milliseconds — [`FakeClock::default`]'s epoch.
pub const DEFAULT_FAKE_EPOCH_MS: UnixMs = 1_772_582_400_000;

impl Default for FakeClock {
    /// `2026-03-04T00:00:00Z`, a fixed instant so snapshots are stable.
    ///
    /// The date is arbitrary — only its fixedness matters — but it is load-bearing for every
    /// `insta` snapshot stamped from this clock, so it must not move. The doc comment used to
    /// claim September while the value said March; `the_default_epoch_is_the_date_it_claims`
    /// keeps the two in step.
    fn default() -> Self {
        Self::new(DEFAULT_FAKE_EPOCH_MS)
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> UnixMs {
        self.now_ms.load(Ordering::SeqCst)
    }

    fn instant(&self) -> Instant {
        *self
            .base
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_after_2020() {
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }

    #[tokio::test]
    async fn fake_clock_advances_both_halves() {
        let c = FakeClock::new(1_000);
        let t0 = c.instant();
        c.advance(Duration::from_secs(5));
        assert_eq!(c.now_ms(), 6_000);
        assert_eq!(c.instant().duration_since(t0), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn fake_clock_is_usable_as_a_trait_object() {
        let c: std::sync::Arc<dyn Clock> = std::sync::Arc::new(FakeClock::default());
        assert_eq!(c.now_ms(), DEFAULT_FAKE_EPOCH_MS);
    }

    /// The doc comment and the constant must agree: snapshots across four crates are stamped from
    /// this epoch, and one that reads March under a comment promising September sends the next
    /// reader looking for a bug that is not there.
    #[test]
    fn the_default_epoch_is_the_date_it_claims() {
        // 2026-03-04T00:00:00Z. Days since the Unix epoch × 86_400_000.
        let days = 20_516_i64;
        assert_eq!(DEFAULT_FAKE_EPOCH_MS, days * 86_400_000);
        assert_eq!(FakeClock::default().now_ms(), DEFAULT_FAKE_EPOCH_MS);
    }
}
