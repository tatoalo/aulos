//! The trailing debounce with the `max_wait` cap (DESIGN §13.1, §13.4).
//!
//! These run on a **paused** tokio clock, so a 10-minute scenario costs microseconds and never
//! flakes. The window and cap under test are exactly the ones `JellyfinHook::debounce()` returns
//! from `AULOS_JELLYFIN_DEBOUNCE_SECS` / `AULOS_JELLYFIN_MAX_WAIT_SECS` (asserted in
//! `jellyfin::tests::the_debounce_is_the_configured_window_and_cap`), and the same code path
//! serves a community `[[hook]]`'s `debounce_ms` / `max_wait_ms`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_hooks::hook::Hook;
use aulos_hooks::{HookDispatcher, JellyfinHook};
use common::{FakeStore, ItemBuilder, ScriptHook, config, events, log, settle, sink};

const WINDOW: Duration = Duration::from_secs(30);
const CAP: Duration = Duration::from_secs(300);

/// Advances the paused clock and lets every woken task run.
async fn tick(by: Duration) {
    tokio::time::advance(by).await;
    settle().await;
}

/// PLAN WP-11: "20 completions inside 30 s ⇒ **one** request."
#[tokio::test(start_paused = true)]
async fn twenty_completions_inside_the_window_are_one_invocation() {
    let hook = Arc::new(ScriptHook::new("jellyfin", 90, log()).debounced(WINDOW, CAP));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let health = dispatcher.health_handle();
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    for i in 0..20 {
        ev.completed(&ItemBuilder::finished(&format!("Clip {i}")).view())
            .await;
        tick(Duration::from_secs(1)).await;
    }
    assert_eq!(hook.runs(), 0, "nothing has fired 20 s in");
    assert_eq!(
        health.health().stat("jellyfin").map(|s| s.pending),
        Some(true),
        "healthz shows the armed window"
    );

    // The last event extended the window to t = 20 + 30.
    tick(Duration::from_secs(29)).await;
    assert_eq!(hook.runs(), 0, "the window is still open");
    tick(Duration::from_secs(2)).await;
    assert_eq!(hook.runs(), 1, "one invocation for twenty completions");
    assert_eq!(
        hook.batches()[0].len(),
        20,
        "and it carries every coalesced event"
    );
    assert_eq!(
        hook.batches()[0][0],
        "Clip 0",
        "in arrival order, so {{titles_json}}[0] is the first completion"
    );
    assert_eq!(
        health.health().stat("jellyfin").map(|s| s.pending),
        Some(false),
        "and the window is disarmed again"
    );

    drop(ev.tx);
    task.await.expect("clean stop");
}

/// PLAN WP-11: "a completion every 25 s for 10 minutes ⇒ a request every 300 s (the `max_wait`
/// cap)". Without the cap a long playlist would be invisible in Jellyfin for hours.
#[tokio::test(start_paused = true)]
async fn a_steady_stream_still_fires_every_max_wait() {
    let hook = Arc::new(ScriptHook::new("jellyfin", 90, log()).debounced(WINDOW, CAP));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    let mut fired_at: Vec<u64> = Vec::new();
    let mut last = 0;
    for step in 0..24_u64 {
        let now = step * 25;
        ev.completed(&ItemBuilder::finished(&format!("Clip {step}")).view())
            .await;
        settle().await;
        tick(Duration::from_secs(25)).await;
        if hook.runs() > last {
            last = hook.runs();
            fired_at.push(now + 25);
        }
    }
    assert_eq!(
        fired_at,
        [300, 600],
        "the cap fires every 300 s, never the 30 s window"
    );
    assert_eq!(hook.runs(), 2);
    let sizes: Vec<usize> = hook.batches().iter().map(Vec::len).collect();
    assert_eq!(sizes.iter().sum::<usize>(), 24, "no completion was lost");

    drop(ev.tx);
    task.await.expect("clean stop");
}

/// The trailing edge fires after the last completion, not on the first one.
#[tokio::test(start_paused = true)]
async fn the_trailing_edge_fires_after_the_last_completion() {
    let hook = Arc::new(ScriptHook::new("jellyfin", 90, log()).debounced(WINDOW, CAP));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    ev.completed(&ItemBuilder::finished("First").view()).await;
    settle().await;
    tick(Duration::from_secs(29)).await;
    assert_eq!(hook.runs(), 0, "no leading edge");
    tick(Duration::from_secs(2)).await;
    assert_eq!(hook.runs(), 1);
    assert_eq!(hook.batches(), [["First".to_owned()]]);

    // A second, unrelated completion much later starts a fresh window.
    ev.completed(&ItemBuilder::finished("Second").view()).await;
    settle().await;
    tick(Duration::from_secs(31)).await;
    assert_eq!(hook.runs(), 2);
    assert_eq!(hook.batches()[1], ["Second".to_owned()]);

    drop(ev.tx);
    task.await.expect("clean stop");
}

/// An undebounced hook fires per event, which is what `debounce_ms = 0` means.
#[tokio::test(start_paused = true)]
async fn an_undebounced_hook_fires_once_per_event() {
    let hook = Arc::new(ScriptHook::new("ntfy", 50, log()));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    for i in 0..5 {
        ev.completed(&ItemBuilder::finished(&format!("Clip {i}")).view())
            .await;
        settle().await;
    }
    assert_eq!(hook.runs(), 5);
    assert!(hook.batches().iter().all(|b| b.len() == 1));

    drop(ev.tx);
    task.await.expect("clean stop");
}

/// Shutdown flushes the trailing batch rather than dropping it, unless the shutdown grace has
/// already expired (DESIGN §16.4).
#[tokio::test(start_paused = true)]
async fn shutdown_flushes_the_pending_batch() {
    let hook = Arc::new(ScriptHook::new("jellyfin", 90, log()).debounced(WINDOW, CAP));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    ev.completed(&ItemBuilder::finished("Tail").view()).await;
    settle().await;
    assert_eq!(hook.runs(), 0);

    drop(ev.tx);
    task.await.expect("clean stop");
    assert_eq!(hook.runs(), 1, "the tail fired on the way out");
}

/// With the grace already expired, the tail is dropped with a WARN instead of blocking exit.
#[tokio::test(start_paused = true)]
async fn a_cancelled_shutdown_drops_the_pending_batch() {
    let hook = Arc::new(ScriptHook::new("jellyfin", 90, log()).debounced(WINDOW, CAP));
    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(aulos_core::clock::FakeClock::default()),
    );
    let cancel = dispatcher.cancel_token();
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    ev.completed(&ItemBuilder::finished("Tail").view()).await;
    settle().await;

    cancel.cancel();
    drop(ev.tx);
    task.await.expect("clean stop");
    assert_eq!(hook.runs(), 0, "the grace expired, so the tail was dropped");
}

/// The default Jellyfin window and cap are the values these tests exercise, so the scenarios above
/// describe the shipped configuration and not just the harness.
#[test]
fn the_jellyfin_defaults_are_the_window_and_cap_under_test() {
    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", "http://jf.test"),
        ("JELLYFIN_API_KEY", "k"),
    ]);
    let hook = JellyfinHook::new(&cfg);
    assert_eq!(hook.debounce().window, WINDOW);
    assert_eq!(hook.debounce().max_wait, CAP);
}
