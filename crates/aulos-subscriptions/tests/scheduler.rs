//! The DESIGN §14.2 scheduler: the first-check delay and its jitter, the exponential backoff,
//! bounded check concurrency, and a schedule that survives a restart.
//!
//! Every test here runs on the scripted checker and paused `tokio` time, so a six-hour backoff
//! costs microseconds and nothing waits on wall-clock time.
//!
//! Two conventions make that reliable, and both are worth knowing before adding a test:
//!
//! - **Timing is asserted on the persisted record, not on the wall clock.** `finish_check` stamps
//!   `last_checked` and `next_due` from the same `now`, so `next_due − last_checked` is exactly the
//!   scheduled gap whatever the clock did in between. Asserting "the check fired at +10 s" instead
//!   would be asserting on tokio's auto-advance.
//! - **Most seeds are [`support::parked`].** Paused time auto-advances to the nearest timer
//!   whenever the runtime idles, so an *enabled* subscription re-checks itself every time a test
//!   gives the store's threads a turn. A parked subscription arms no timer, and `SubCmd::Check`
//!   runs it anyway — exactly as legacy's `check_now(ids)` did — so a test drives precisely as many
//!   checks as it means to.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::SubId;
use aulos_core::subscription::SubChanges;
use aulos_subscriptions::CheckFailure;
use support::{Harness, ScriptedAnswer, ScriptedChecker, parked, record, spin, spin_until};

/// One virtual hour: the default `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL`.
const HOUR_MS: i64 = 60 * 60 * 1_000;

fn id(s: &str) -> SubId {
    SubId::parse(s).expect("a valid subscription id")
}

/// DESIGN §14.2: the first check is at `now + AULOS_SUB_FIRST_CHECK_DELAY_SECS + jitter(0..30 s)`,
/// not at boot **+ 60 s** as in legacy — and that schedule is persisted, so `healthz` and the
/// `subscription` frame report it.
#[tokio::test(start_paused = true)]
async fn the_first_check_is_scheduled_shortly_after_boot() {
    let checker = ScriptedChecker::new();
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0) // no jitter offset, so the due time is exactly the configured delay
        .seed(vec![record("01JCFIRST", "https://a.test/@one")])
        .build()
        .await;
    assert_eq!(h.cfg.sub_first_check_delay_secs, 10);

    let r = h.record(&id("01JCFIRST")).await;
    assert_eq!(
        r.next_due,
        Some(support::EPOCH_MS + 10_000),
        "the boot delay, with the jitter pinned to zero"
    );
    // `healthz` reports the persisted schedule, so the boot delay is visible there too. The bound
    // is the interval rather than the delay because paused time auto-advances to the nearest timer
    // whenever this task awaits, and the check may already have run and rescheduled.
    let health = h.health().await;
    assert!(
        health.next_due_in_s.is_some_and(|s| s <= 60 * 60),
        "healthz reports it: {health:?}"
    );
}

/// The jitter window spreads a boot-time stampede: forty feeds land inside
/// `[delay, delay + 30 s]`, at forty different moments rather than all at once.
#[tokio::test(start_paused = true)]
async fn forty_subscriptions_spread_across_the_jitter_window() {
    let checker = ScriptedChecker::new();
    // Parked, so no timer can fire and reschedule a feed while the forty rows are being read: the
    // boot pass computes and persists `first_due` for every subscription, enabled or not.
    let seeded: Vec<_> = (0..40)
        .map(|i| {
            parked(
                &format!("01JCSPREAD{i:02}"),
                &format!("https://a.test/@f{i}"),
            )
        })
        .collect();
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        // The real `RandJitter` — this is the property under test.
        .rand_jitter()
        .seed(seeded)
        .build()
        .await;

    let rows = h.store.subscriptions().await.expect("read");
    assert_eq!(rows.len(), 40);
    let mut dues = Vec::new();
    for r in &rows {
        let due = r.next_due.expect("a scheduled due time") - support::EPOCH_MS;
        assert!(
            (10_000..=40_000).contains(&due),
            "{} is due at +{due} ms, outside the [10 s, 40 s] window",
            r.id.as_str()
        );
        dues.push(due);
    }
    assert!(
        checker.calls().is_empty(),
        "nothing ran: they are all parked"
    );

    let distinct: std::collections::BTreeSet<i64> = dues.iter().copied().collect();
    assert!(
        distinct.len() >= 30,
        "forty feeds collapsed into {} moments: no spread",
        distinct.len()
    );
    let seconds: std::collections::BTreeSet<i64> = dues.iter().map(|d| d / 1_000).collect();
    assert!(
        seconds.len() >= 10,
        "forty feeds landed in only {} distinct seconds",
        seconds.len()
    );
}

/// The timer path itself: an enabled subscription checks itself with nobody asking.
#[tokio::test(start_paused = true)]
async fn an_enabled_subscription_checks_itself_when_it_comes_due() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0)
        .seed(vec![record("01JCTIMER", "https://a.test/@t")])
        .build()
        .await;

    tokio::time::advance(Duration::from_secs(11)).await;
    spin_until("the scheduled check to fire", || {
        !checker.calls().is_empty()
    })
    .await;
    assert_eq!(checker.calls()[0], "https://a.test/@t");
    let r = h
        .until(&id("01JCTIMER"), "the check to be recorded", |r| {
            r.last_checked.is_some()
        })
        .await;
    assert_eq!(r.consecutive_failures, 0);
}

/// DESIGN §14.2: `last_checked` is updated on **every** check, and a failure multiplies the
/// interval up to the cap. Legacy left `last_checked` alone and hot-retried every 60 s forever.
#[tokio::test(start_paused = true)]
async fn the_backoff_curve_doubles_updates_last_checked_and_a_success_resets_it() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Fail(CheckFailure::Provider("boom".into())));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.5)
        .seed(vec![parked("01JCDEAD", "https://a.test/@dead")])
        .build()
        .await;
    assert_eq!(h.cfg.sub_backoff_max_secs, 21_600, "6 h");
    let sub = id("01JCDEAD");

    let mut previous_checked = None;
    for (failures, want_gap_ms) in [
        (1_u32, 2 * HOUR_MS),
        (2, 4 * HOUR_MS),
        (3, 6 * HOUR_MS), // capped at AULOS_SUB_BACKOFF_MAX_SECS
        (4, 6 * HOUR_MS),
        (5, 6 * HOUR_MS),
    ] {
        h.check(vec![sub.clone()]).await;
        let r = h
            .until(&sub, &format!("failure {failures}"), |r| {
                r.consecutive_failures == failures
            })
            .await;
        assert_eq!(r.error.as_deref(), Some("boom"));
        let checked = r.last_checked.expect("last_checked is always written");
        assert_ne!(
            Some(checked),
            previous_checked,
            "last_checked must move on every check, failure included"
        );
        previous_checked = Some(checked);
        assert_eq!(
            r.next_due.expect("next_due") - checked,
            want_gap_ms,
            "the gap after {failures} failure(s)"
        );
        // Time has to move, or `last_checked` cannot change.
        tokio::time::advance(Duration::from_secs(1)).await;
    }

    // A success resets the counter and the interval.
    checker.default_answer(ScriptedAnswer::Ok(vec!["v1".into()]));
    h.check(vec![sub.clone()]).await;
    let r = h
        .until(&sub, "the recovery", |r| r.consecutive_failures == 0)
        .await;
    assert_eq!(r.error, None);
    assert_eq!(
        r.next_due.expect("next_due") - r.last_checked.expect("last_checked"),
        HOUR_MS,
        "back to the plain interval"
    );
}

/// DESIGN §14.2: `AULOS_SUB_CHECK_CONCURRENCY` permits, and a slow feed occupies only its own
/// permit — the other nine complete while it is still running.
#[tokio::test(start_paused = true)]
async fn ten_due_subscriptions_run_at_most_the_configured_concurrency() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let gate = checker.gate("https://a.test/@slow");

    let mut seeded = vec![parked("01JCSLOW", "https://a.test/@slow")];
    let mut ids = vec![id("01JCSLOW")];
    for i in 0..9 {
        seeded.push(parked(
            &format!("01JCFAST{i}"),
            &format!("https://a.test/@fast{i}"),
        ));
        ids.push(id(&format!("01JCFAST{i}")));
    }
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0)
        .env("AULOS_SUB_CHECK_CONCURRENCY", "2")
        .seed(seeded)
        .build()
        .await;
    assert_eq!(h.cfg.sub_check_concurrency, 2);

    // Everything becomes due at once.
    h.check(ids).await;
    spin_until("the nine fast feeds to finish", || {
        checker.completed().len() >= 9
    })
    .await;

    assert!(
        checker.peak_concurrency() <= 2,
        "peak concurrency was {}",
        checker.peak_concurrency()
    );
    let completed = checker.completed();
    assert_eq!(completed.len(), 9, "the gated one has not finished");
    assert!(
        !completed.iter().any(|c| c.ends_with("@slow")),
        "the gated one must still be holding its permit"
    );
    assert!(
        checker.calls().iter().any(|c| c.ends_with("@slow")),
        "…and it must have started: it holds one of the two permits"
    );

    // Release it; nothing was stuck.
    gate.notify_waiters();
    let r = h
        .until(&id("01JCSLOW"), "the gated check", |r| {
            r.last_checked.is_some()
        })
        .await;
    assert_eq!(r.consecutive_failures, 0);
    assert!(checker.peak_concurrency() <= 2);
}

/// A per-check deadline: a check that never answers is a failure, so the subscription backs off
/// instead of pinning its permit forever.
#[tokio::test(start_paused = true)]
async fn a_check_that_overruns_its_timeout_is_a_failure() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let _gate = checker.gate("https://a.test/@hang");
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.5)
        .env("AULOS_SUB_CHECK_TIMEOUT_SECS", "30")
        .seed(vec![parked("01JCHANG", "https://a.test/@hang")])
        .build()
        .await;
    let sub = id("01JCHANG");

    h.check(vec![sub.clone()]).await;
    let r = h
        .until(&sub, "the timeout", |r| r.consecutive_failures >= 1)
        .await;
    assert_eq!(r.consecutive_failures, 1);
    assert_eq!(
        r.error.as_deref(),
        Some("the subscription check timed out after 30s")
    );
    assert_eq!(
        r.next_due.expect("next_due") - r.last_checked.expect("last_checked"),
        2 * HOUR_MS
    );
}

/// DESIGN §14.2: `next_due` and `consecutive_failures` are persisted, so a restart keeps the
/// schedule instead of resetting it.
#[tokio::test(start_paused = true)]
async fn next_due_and_consecutive_failures_survive_a_restart() {
    let sub = id("01JCRESTART");

    // The first process. Everything it owns — the manager task, its per-subscription tasks and its
    // store handle — is dropped at the end of this block; only the directory survives.
    let (dir, want_next) = {
        let checker = ScriptedChecker::new();
        checker.default_answer(ScriptedAnswer::Fail(CheckFailure::Provider("nope".into())));
        let h = Harness::builder()
            .scripted(Arc::clone(&checker))
            .jitter(0.5)
            .seed(vec![parked("01JCRESTART", "https://a.test/@r")])
            .build()
            .await;

        h.check(vec![sub.clone()]).await;
        h.until(&sub, "one failure", |r| r.consecutive_failures == 1)
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        h.check(vec![sub.clone()]).await;
        let before = h
            .until(&sub, "two failures", |r| r.consecutive_failures == 2)
            .await;

        let want_next = before.next_due.expect("next_due");
        assert_eq!(
            want_next - before.last_checked.expect("last_checked"),
            4 * HOUR_MS
        );
        (h.dir, want_next)
    };

    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.5)
        .reusing(dir)
        .build()
        .await;

    let after = h.record(&sub).await;
    assert_eq!(after.consecutive_failures, 2, "the counter is persisted");
    assert_eq!(after.next_due, Some(want_next), "the schedule is persisted");
    assert_eq!(after.error.as_deref(), Some("nope"));
    assert!(!after.enabled, "and so is `enabled`");
    assert!(
        checker.calls().is_empty(),
        "a persisted future due time must win over the boot delay"
    );
}

/// `enabled = false` parks the task; an update wakes it (DESIGN §14.2).
#[tokio::test(start_paused = true)]
async fn a_disabled_subscription_is_parked_until_it_is_re_enabled() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0)
        .seed(vec![parked("01JCPARK", "https://a.test/@p")])
        .build()
        .await;

    spin().await;
    assert!(checker.calls().is_empty(), "a paused feed is never checked");

    h.update(
        &id("01JCPARK"),
        SubChanges {
            enabled: Some(true),
            ..SubChanges::default()
        },
    )
    .await
    .expect("re-enabled");

    tokio::time::advance(Duration::from_secs(11)).await;
    spin_until("the resumed feed to check itself", || {
        !checker.calls().is_empty()
    })
    .await;
}

/// An explicit `check` of a paused subscription still runs — legacy's `check_now(ids)` did too —
/// and it goes straight back to being parked afterwards.
#[tokio::test(start_paused = true)]
async fn an_explicit_check_of_a_paused_subscription_runs_once() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0)
        .seed(vec![parked("01JCPAUSED", "https://a.test/@q")])
        .build()
        .await;
    let sub = id("01JCPAUSED");

    let job = h.check(vec![sub.clone()]).await;
    assert_eq!(job.subscriptions, vec![sub.clone()]);
    h.until(&sub, "the one-off check", |r| r.last_checked.is_some())
        .await;
    assert_eq!(checker.calls().len(), 1);

    tokio::time::advance(Duration::from_secs(3 * 60 * 60)).await;
    spin().await;
    assert_eq!(checker.calls().len(), 1, "parked again, not repeating");
    assert!(!h.record(&sub).await.enabled);
}

/// `check` with no ids targets every **enabled** subscription, and answers with a job handle
/// before any of them has run (BRIEF §12).
#[tokio::test(start_paused = true)]
async fn check_with_no_ids_targets_every_enabled_subscription_and_returns_immediately() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let _gate = checker.gate("https://a.test/@e1");
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0)
        .seed(vec![
            record("01JCON1", "https://a.test/@e1"),
            record("01JCON2", "https://a.test/@e2"),
            parked("01JCOFF", "https://a.test/@off"),
        ])
        .build()
        .await;

    // The gated feed cannot have finished, yet the call has already returned.
    let job = h.check(Vec::new()).await;
    assert_eq!(job.job_id.len(), 26, "a ULID: {}", job.job_id);
    assert_eq!(job.subscriptions.len(), 2, "the parked one is excluded");
    assert!(!job.subscriptions.iter().any(|i| i.as_str() == "01JCOFF"));
    assert!(
        !checker.completed().iter().any(|c| c.ends_with("@e1")),
        "the gated check has not finished, so `check` did not await it"
    );

    spin_until("the ungated feed to be checked", || {
        checker.completed().iter().any(|c| c.ends_with("@e2"))
    })
    .await;
    assert!(
        !checker.calls().iter().any(|c| c.ends_with("@off")),
        "a parked subscription is never checked by an id-less request"
    );
}

/// `healthz.components.subscriptions` (DESIGN §16.3): totals, the failing count and the seconds
/// until the next scheduled check.
#[tokio::test(start_paused = true)]
async fn health_reports_totals_failures_and_the_next_due_time() {
    let checker = ScriptedChecker::new();
    checker.script(
        "https://a.test/@bad",
        vec![ScriptedAnswer::Fail(CheckFailure::Provider("x".into()))],
    );
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.5)
        .seed(vec![
            parked("01JCH1", "https://a.test/@good"),
            parked("01JCH2", "https://a.test/@bad"),
        ])
        .build()
        .await;

    let before = h.health().await;
    assert_eq!(before.total, 2);
    assert_eq!(before.failing, 0);
    assert_eq!(
        before.next_due_in_s, None,
        "both are parked, so nothing is scheduled"
    );

    h.check(vec![id("01JCH2")]).await;
    h.until(&id("01JCH2"), "the failure", |r| {
        r.consecutive_failures == 1
    })
    .await;
    let after = h.health().await;
    assert_eq!(after.total, 2);
    assert_eq!(after.failing, 1);

    h.delete(vec![id("01JCH1"), id("01JCH2")]).await;
    let empty = h.health().await;
    assert_eq!(empty.total, 0);
    assert_eq!(empty.failing, 0);
    assert_eq!(empty.next_due_in_s, None);
}

/// Shortening the interval must replace the pending due time, not wait for it.
#[tokio::test(start_paused = true)]
async fn shortening_the_interval_reschedules_the_next_check() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let mut seeded = parked("01JCINT", "https://a.test/@i");
    seeded.next_due = Some(support::EPOCH_MS + 24 * HOUR_MS);
    seeded.last_checked = Some(support::EPOCH_MS);
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.5)
        .seed(vec![seeded])
        .build()
        .await;
    let sub = id("01JCINT");
    assert_eq!(
        h.record(&sub).await.next_due,
        Some(support::EPOCH_MS + 24 * HOUR_MS),
        "the persisted schedule survives the boot pass"
    );

    let updated = h
        .update(
            &sub,
            SubChanges {
                check_interval_minutes: Some(5),
                enabled: Some(true),
                ..SubChanges::default()
            },
        )
        .await
        .expect("updated");
    assert_eq!(updated.check_interval_minutes, 5);
    let due = updated.next_due.expect("next_due");
    assert_eq!(
        due,
        support::EPOCH_MS + 5 * 60 * 1_000,
        "five minutes after the last check, not a day later"
    );

    tokio::time::advance(Duration::from_secs(6 * 60)).await;
    spin_until("the shortened interval to fire", || {
        !checker.calls().is_empty()
    })
    .await;
}

/// Every subscription's task goes away with it: a deleted subscription never checks again.
#[tokio::test(start_paused = true)]
async fn deleting_a_subscription_aborts_its_task() {
    let checker = ScriptedChecker::new();
    checker.default_answer(ScriptedAnswer::Ok(Vec::new()));
    let h = Harness::builder()
        .scripted(Arc::clone(&checker))
        .jitter(0.0)
        .seed(vec![
            record("01JCGONE", "https://a.test/@gone"),
            parked("01JCSTAY", "https://a.test/@stay"),
        ])
        .build()
        .await;

    assert_eq!(h.delete(vec![id("01JCGONE")]).await, vec![id("01JCGONE")]);
    // Whatever it managed to do before the delete is the baseline; nothing may be added after.
    let baseline = checker
        .calls()
        .iter()
        .filter(|c| c.ends_with("@gone"))
        .count();
    for _ in 0..4 {
        tokio::time::advance(Duration::from_secs(3 * 60 * 60)).await;
        spin().await;
    }
    let after = checker
        .calls()
        .iter()
        .filter(|c| c.ends_with("@gone"))
        .count();
    assert_eq!(
        after,
        baseline,
        "the aborted task must not check again: {:?}",
        checker.calls()
    );
    assert_eq!(h.list().await.len(), 1);
    assert!(
        h.store
            .subscription(&id("01JCGONE"))
            .await
            .expect("read")
            .is_none()
    );
}
