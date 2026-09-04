//! The scheduling arithmetic: intervals, jitter, exponential backoff, and what one check reports
//! back to the manager (DESIGN §14.2).
//!
//! Every function here is pure and takes its randomness as a `0.0..=1.0` sample, so the whole
//! backoff curve and the whole jitter window are unit-testable without a clock and without an RNG.

use std::time::Duration;

use aulos_core::id::UnixMs;
use aulos_core::subscription::legacy;

/// The floor a check interval is clamped to, in seconds.
///
/// Legacy used `max(60, minutes * 60)`; keeping the floor means a hand-edited
/// `check_interval_minutes = 0` cannot turn into a busy loop. `check_interval_minutes` is itself
/// clamped to `max(1, n)` on write, so this only ever binds for an imported zero.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// The width of the first-check jitter window, in seconds (DESIGN §14.2).
///
/// 40 channels loaded at boot spread across this window instead of hitting YouTube in the same
/// second.
pub const FIRST_CHECK_JITTER_SECS: u64 = 30;

/// The exponent cap: `interval * 2^min(failures, 8)` (DESIGN §14.2).
pub const MAX_BACKOFF_SHIFT: u32 = 8;

/// The ±fraction applied to every scheduled interval.
pub const JITTER_FRACTION: f64 = 0.10;

/// A `0.0..=1.0` sample source, so a test can pin the jitter.
pub trait Jitter: Send + Sync + std::fmt::Debug {
    /// A sample in `0.0..=1.0`.
    fn sample(&self) -> f64;
}

/// The production source: `rand`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandJitter;

impl Jitter for RandJitter {
    fn sample(&self) -> f64 {
        rand::random::<f64>()
    }
}

/// A constant sample. `FixedJitter(0.5)` is "no offset" for a ±window and "half" for a 0..w one.
#[derive(Clone, Copy, Debug)]
pub struct FixedJitter(pub f64);

impl Jitter for FixedJitter {
    fn sample(&self) -> f64 {
        self.0.clamp(0.0, 1.0)
    }
}

/// The scheduling knobs one manager runs with (`AULOS_SUB_*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timing {
    /// `AULOS_SUB_BACKOFF_MAX_SECS` — the ceiling a failing feed converges to (6 h).
    pub backoff_max_secs: u64,
    /// `AULOS_SUB_FIRST_CHECK_DELAY_SECS` — how long after boot the first check may fire (10 s).
    pub first_check_delay_secs: u64,
    /// `AULOS_SUB_CHECK_TIMEOUT_SECS` — the per-check deadline.
    pub check_timeout_secs: u64,
}

impl Timing {
    /// Reads the four knobs off the effective config.
    #[must_use]
    pub const fn from_config(cfg: &aulos_core::config::Config) -> Self {
        Self {
            backoff_max_secs: cfg.sub_backoff_max_secs,
            first_check_delay_secs: cfg.sub_first_check_delay_secs,
            check_timeout_secs: cfg.sub_check_timeout_secs,
        }
    }

    /// The per-check deadline as a [`Duration`], never zero.
    #[must_use]
    pub const fn check_timeout(self) -> Duration {
        Duration::from_secs(if self.check_timeout_secs == 0 {
            1
        } else {
            self.check_timeout_secs
        })
    }

    /// `max(MIN_INTERVAL_SECS, minutes * 60)`.
    #[must_use]
    pub const fn interval_secs(minutes: u32) -> u64 {
        let raw = (minutes as u64).saturating_mul(60);
        if raw < MIN_INTERVAL_SECS {
            MIN_INTERVAL_SECS
        } else {
            raw
        }
    }

    /// `interval * 2^min(failures, 8)`, capped at `backoff_max_secs` (DESIGN §14.2).
    ///
    /// `failures == 0` is a success and returns the plain interval, so the curve a caller sees
    /// after 0, 1, 2 … failures is `interval, 2×, 4×, 8× …` up to the cap.
    #[must_use]
    pub const fn backoff_secs(self, interval_secs: u64, failures: u32) -> u64 {
        let shift = if failures > MAX_BACKOFF_SHIFT {
            MAX_BACKOFF_SHIFT
        } else {
            failures
        };
        let raw = interval_secs.saturating_mul(1_u64 << shift);
        let cap = if self.backoff_max_secs == 0 {
            u64::MAX
        } else {
            self.backoff_max_secs
        };
        if raw > cap { cap } else { raw }
    }

    /// `secs ± JITTER_FRACTION`, as milliseconds. `sample = 0.5` is exactly `secs`.
    #[must_use]
    pub fn jittered_ms(secs: u64, sample: f64) -> i64 {
        let base = secs as f64 * 1_000.0;
        let spread = base * JITTER_FRACTION;
        // `sample` maps 0.0 → −spread, 0.5 → 0, 1.0 → +spread.
        let offset = (sample.clamp(0.0, 1.0) - 0.5) * 2.0 * spread;
        let ms = (base + offset).max(0.0);
        // `as` on an out-of-range f64 saturates in Rust, which is the answer we want anyway.
        ms as i64
    }

    /// When the next check is due after a check that ended with `failures` consecutive failures.
    #[must_use]
    pub fn next_due(
        self,
        now: UnixMs,
        interval_minutes: u32,
        failures: u32,
        sample: f64,
    ) -> UnixMs {
        let interval = Self::interval_secs(interval_minutes);
        let secs = self.backoff_secs(interval, failures);
        now.saturating_add(Self::jittered_ms(secs, sample))
    }

    /// The boot / spawn due time: `now + first_check_delay + jitter(0..30 s)`, or the persisted
    /// `next_due` when that is later (DESIGN §14.2).
    #[must_use]
    pub fn first_due(self, now: UnixMs, persisted: Option<UnixMs>, sample: f64) -> UnixMs {
        let jitter_ms =
            (sample.clamp(0.0, 1.0) * (FIRST_CHECK_JITTER_SECS as f64) * 1_000.0) as i64;
        let soon = now
            .saturating_add(
                i64::try_from(self.first_check_delay_secs.saturating_mul(1_000))
                    .unwrap_or(i64::MAX),
            )
            .saturating_add(jitter_ms);
        match persisted {
            Some(due) if due > soon => due,
            _ => soon,
        }
    }
}

/// What one successful check produced.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CheckReport {
    /// The media ids that were queued (or already existed) and are therefore now seen.
    pub queued: Vec<Box<str>>,
    /// How many entries the check considered new before queueing.
    pub new_total: usize,
    /// One message per entry that failed validation. **Not** marked seen, so they retry (parity).
    pub errors: Vec<Box<str>>,
}

impl CheckReport {
    /// The legacy `error` projection: the first three messages, `"; "`-joined, or `None`.
    #[must_use]
    pub fn error_text(&self) -> Option<Box<str>> {
        if self.errors.is_empty() {
            return None;
        }
        let joined = self
            .errors
            .iter()
            .take(3)
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        Some(joined.into_boxed_str())
    }
}

/// Why a check did not complete. Every variant **counts as a failure** for backoff purposes.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum CheckFailure {
    /// The URL resolved to a single video, or to nothing at all (DESIGN §14.3 step 4).
    ///
    /// Legacy set the same message but hot-retried it every 60 s forever; here it backs off.
    #[error("{}", legacy::VIDEO_ONLY)]
    VideoOnly,
    /// Nothing in the registry claims this URL.
    #[error("no provider matches {0}")]
    NoProvider(Box<str>),
    /// The provider failed. The message is the provider's own, as legacy showed it.
    #[error("{0}")]
    Provider(Box<str>),
    /// The check did not finish inside `AULOS_SUB_CHECK_TIMEOUT_SECS`.
    #[error("the subscription check timed out after {0}s")]
    Timeout(u64),
    /// The store could not be written.
    #[error("store: {0}")]
    Store(Box<str>),
    /// The queue engine could not accept the batch.
    #[error("{0}")]
    Engine(Box<str>),
}

impl CheckFailure {
    /// The text that lands in `SubscriptionRecord::error`.
    #[must_use]
    pub fn error_text(&self) -> Box<str> {
        self.to_string().into_boxed_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Timing = Timing {
        backoff_max_secs: 21_600,
        first_check_delay_secs: 10,
        check_timeout_secs: 300,
    };

    #[test]
    fn the_interval_floor_is_sixty_seconds() {
        assert_eq!(Timing::interval_secs(0), 60);
        assert_eq!(Timing::interval_secs(1), 60);
        assert_eq!(Timing::interval_secs(5), 300);
        assert_eq!(Timing::interval_secs(60), 3_600);
        // No overflow on an absurd imported value.
        assert!(Timing::interval_secs(u32::MAX) > 0);
    }

    /// DESIGN §14.2: `min(interval * 2^min(failures, 8), AULOS_SUB_BACKOFF_MAX_SECS)`.
    #[test]
    fn the_backoff_curve_doubles_and_then_caps() {
        let interval = 60; // a 1-minute feed, so the whole curve fits under the 6 h cap
        let seen: Vec<u64> = (0..=10).map(|f| T.backoff_secs(interval, f)).collect();
        assert_eq!(
            seen,
            vec![
                60,     // success
                120,    // 1 failure
                240,    // 2
                480,    // 3
                960,    // 4
                1_920,  // 5
                3_840,  // 6
                7_680,  // 7
                15_360, // 8 — the exponent cap
                15_360, // 9 — no further growth
                15_360, // 10
            ]
        );
    }

    #[test]
    fn the_backoff_cap_is_six_hours_for_an_hourly_feed() {
        let interval = 3_600;
        assert_eq!(T.backoff_secs(interval, 1), 7_200);
        assert_eq!(T.backoff_secs(interval, 2), 14_400);
        assert_eq!(T.backoff_secs(interval, 3), 21_600, "capped, not 28800");
        assert_eq!(T.backoff_secs(interval, 8), 21_600);
        assert_eq!(T.backoff_secs(interval, u32::MAX), 21_600);
    }

    #[test]
    fn a_zero_cap_means_uncapped_rather_than_zero() {
        let t = Timing {
            backoff_max_secs: 0,
            ..T
        };
        assert_eq!(t.backoff_secs(60, 3), 480);
    }

    #[test]
    fn jitter_is_plus_or_minus_ten_percent_and_centred() {
        assert_eq!(Timing::jittered_ms(100, 0.5), 100_000);
        assert_eq!(Timing::jittered_ms(100, 0.0), 90_000);
        assert_eq!(Timing::jittered_ms(100, 1.0), 110_000);
        // Out-of-range samples are clamped rather than trusted.
        assert_eq!(Timing::jittered_ms(100, -3.0), 90_000);
        assert_eq!(Timing::jittered_ms(100, 9.0), 110_000);
        assert_eq!(Timing::jittered_ms(0, 0.0), 0);
    }

    #[test]
    fn next_due_applies_the_backoff_then_the_jitter() {
        let now = 1_000_000;
        assert_eq!(T.next_due(now, 1, 0, 0.5), now + 60_000);
        assert_eq!(T.next_due(now, 1, 1, 0.5), now + 120_000);
        let low = T.next_due(now, 1, 1, 0.0);
        let high = T.next_due(now, 1, 1, 1.0);
        assert_eq!((low, high), (now + 108_000, now + 132_000));
    }

    #[test]
    fn the_first_check_is_soon_after_boot_and_jittered() {
        let now = 5_000_000;
        assert_eq!(T.first_due(now, None, 0.0), now + 10_000);
        assert_eq!(T.first_due(now, None, 1.0), now + 40_000);
        assert_eq!(T.first_due(now, None, 0.5), now + 25_000);
    }

    /// A persisted `next_due` in the future wins — that is what "a restart keeps the schedule"
    /// means. One in the past does not, or a box that was off for a week would stampede.
    #[test]
    fn a_later_persisted_due_time_wins() {
        let now = 5_000_000;
        let later = now + 3_600_000;
        assert_eq!(T.first_due(now, Some(later), 0.0), later);
        assert_eq!(T.first_due(now, Some(now - 999_999), 0.0), now + 10_000);
    }

    /// DESIGN §14.2: jitter spreads 40 channels so they do not hit YouTube in the same second.
    #[test]
    fn forty_subscriptions_spread_across_the_jitter_window() {
        let now = 0;
        let dues: Vec<i64> = (0..40)
            .map(|i| T.first_due(now, None, f64::from(i) / 39.0))
            .collect();
        let min = *dues.iter().min().unwrap();
        let max = *dues.iter().max().unwrap();
        assert_eq!(min, 10_000);
        assert_eq!(max, 40_000);
        let distinct: std::collections::BTreeSet<i64> = dues.iter().copied().collect();
        assert!(
            distinct.len() >= 30,
            "40 samples collapsed into {} slots",
            distinct.len()
        );
        // No two land in the same second for more than a couple of channels.
        let seconds: std::collections::BTreeSet<i64> = dues.iter().map(|d| d / 1_000).collect();
        assert!(
            seconds.len() >= 25,
            "only {} distinct seconds",
            seconds.len()
        );
    }

    #[test]
    fn the_report_joins_the_first_three_messages() {
        let mut r = CheckReport::default();
        assert_eq!(r.error_text(), None);
        r.errors = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert_eq!(r.error_text().as_deref(), Some("a; b; c"));
    }

    #[test]
    fn the_single_video_failure_keeps_the_legacy_message() {
        assert_eq!(
            CheckFailure::VideoOnly.to_string(),
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );
        assert_eq!(
            CheckFailure::Timeout(300).error_text().as_ref(),
            "the subscription check timed out after 300s"
        );
    }

    #[test]
    fn fixed_jitter_clamps_and_rand_jitter_stays_in_range() {
        assert_eq!(FixedJitter(2.0).sample(), 1.0);
        assert_eq!(FixedJitter(-2.0).sample(), 0.0);
        for _ in 0..64 {
            let s = RandJitter.sample();
            assert!((0.0..=1.0).contains(&s), "{s}");
        }
    }
}
