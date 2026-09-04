//! The Jellyfin refresh against `wiremock`: the two request shapes, the one-shot fallback, the
//! retry budget, all four legacy message strings and the precondition path (DESIGN §13.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::FakeClock;
use aulos_core::health::ComponentStatus;
use aulos_core::status::TerminalStatus;
use aulos_hooks::hook::{BatchEntry, Hook};
use aulos_hooks::{HookDispatcher, HookError, HookRunner, JellyfinHook};
use common::{FakeStore, ItemBuilder, config, events, settle, sink, until};
use wiremock::matchers::{body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// No backoff: the three attempts of DESIGN §13.1 must not cost a test ten real seconds.
const NO_BACKOFF: [Duration; 2] = [Duration::ZERO, Duration::ZERO];

fn runner(cfg: &Arc<aulos_core::config::Config>) -> (HookRunner, Arc<FakeStore>) {
    let store = FakeStore::new();
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        Arc::clone(cfg),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<_>,
        factory,
    );
    (runner, store)
}

async fn run_once(
    hook: &JellyfinHook,
    cfg: &Arc<aulos_core::config::Config>,
) -> Result<(), HookError> {
    let (runner, _store) = runner(cfg);
    let item = ItemBuilder::finished("Clip");
    let view = item.view();
    let batch = vec![BatchEntry::from_view(&view, TerminalStatus::Finished)];
    runner.run(hook, &view, &batch).await
}

#[tokio::test]
async fn the_global_refresh_is_the_legacy_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .and(header("accept", "application/json"))
        .and(header("authorization", "MediaBrowser Token=\"secret\""))
        .and(body_string(""))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_once(&hook, &cfg).await.expect("a 204 is a success");
    server.verify().await;
}

#[tokio::test]
async fn the_targeted_refresh_matches_the_design_query_string() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Items/lib-42/Refresh"))
        .and(query_param("metadataRefreshMode", "FullRefresh"))
        .and(query_param("imageRefreshMode", "Default"))
        .and(query_param("replaceAllMetadata", "false"))
        .and(query_param("replaceAllImages", "false"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
        ("JELLYFIN_LIBRARY_ID", "lib-42"),
        ("JELLYFIN_METADATA_REFRESH_MODE", "FullRefresh"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_once(&hook, &cfg).await.expect("a 200 is a success");
    server.verify().await;
}

/// A mistyped `JELLYFIN_LIBRARY_ID` must not silently disable sync (DESIGN §13.1).
#[tokio::test]
async fn a_rejected_targeted_refresh_falls_back_to_the_global_one_exactly_once() {
    for status in [400_u16, 404] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/Items/typo/Refresh"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/Library/Refresh"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cfg = config(&[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", &server.uri()),
            ("JELLYFIN_API_KEY", "secret"),
            ("JELLYFIN_LIBRARY_ID", "typo"),
        ]);
        let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
        run_once(&hook, &cfg)
            .await
            .unwrap_or_else(|e| panic!("the fallback must succeed for {status}: {e}"));
        server.verify().await;
    }
}

/// A 4xx that is not 400/404 is final: no fallback, no retry.
#[tokio::test]
async fn a_401_on_the_targeted_refresh_is_final() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Items/lib/Refresh"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "wrong"),
        ("JELLYFIN_LIBRARY_ID", "lib"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    let e = run_once(&hook, &cfg).await.expect_err("401 fails");
    assert_eq!(
        e.to_string(),
        "Jellyfin refresh failed with HTTP 401: ",
        "the legacy shape, with an empty body as its details"
    );
    server.verify().await;
}

/// PLAN WP-11: "a 500 retries twice and then gives up."
#[tokio::test]
async fn a_500_is_attempted_three_times_and_then_gives_up() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string(r#"{"message":"Internal Server Error"}"#),
        )
        .expect(3)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    let e = run_once(&hook, &cfg).await.expect_err("three 500s give up");
    assert_eq!(
        e.to_string(),
        "Jellyfin refresh failed with HTTP 500: Internal Server Error",
        "details prefer the JSON message field, exactly as legacy did"
    );
    server.verify().await;
}

/// A 5xx that recovers on the second attempt is a success, and only two requests are made.
#[tokio::test]
async fn a_transient_503_recovers_on_the_next_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_once(&hook, &cfg).await.expect("the retry succeeds");
    server.verify().await;
}

/// The fourth legacy message: a transport failure, produced by pointing the hook at a port that
/// is not listening. No network access is involved.
#[tokio::test]
async fn a_transport_failure_uses_the_legacy_message_shape() {
    // Bind and immediately drop, so the port is almost certainly free and refuses connections.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &format!("http://127.0.0.1:{port}")),
        ("JELLYFIN_API_KEY", "secret"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    let e = run_once(&hook, &cfg)
        .await
        .expect_err("nothing is listening");
    let message = e.to_string();
    assert!(
        message.starts_with("Jellyfin refresh request failed: "),
        "{message}"
    );
}

/// PLAN WP-11: "`JELLYFIN_SYNC_ENABLED=true` with a blank `JELLYFIN_URL` produces exactly one boot
/// WARN, a `degraded` component carrying `JELLYFIN_URL is required`, and **zero** HTTP requests
/// across 20 completions."
#[tokio::test]
async fn a_blank_url_is_degraded_and_silent_across_twenty_completions() {
    let server = MockServer::start().await;
    // Any request at all is a failure: nothing is mounted, and wiremock reports unmatched calls.
    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", ""),
        ("JELLYFIN_API_KEY", "secret"),
    ]);
    // The boot WARN is emitted by the constructor, which runs exactly once per process.
    let hook = Arc::new(JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF));
    assert_eq!(hook.precondition(), Some("JELLYFIN_URL is required"));

    let dispatcher = HookDispatcher::with_hooks(
        Arc::clone(&cfg),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(FakeClock::default()),
    );
    let health = dispatcher.health_handle();
    let component = health
        .health()
        .component("jellyfin")
        .cloned()
        .expect("jellyfin");
    assert_eq!(component.status, ComponentStatus::Degraded);
    assert_eq!(component.detail["detail"], "JELLYFIN_URL is required");

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    for i in 0..20 {
        ev.completed(&ItemBuilder::finished(&format!("Clip {i}")).view())
            .await;
    }
    settle().await;
    drop(ev.tx);
    task.await.expect("clean stop");

    assert_eq!(
        health.health().stat("jellyfin").map(|s| s.runs_total),
        Some(0),
        "applies() returned false, so nothing ran"
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "and no request was made"
    );
}

/// A blank key is the same story with the other message.
#[tokio::test]
async fn a_blank_key_is_degraded_with_the_other_message() {
    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", "http://jf.invalid"),
        ("JELLYFIN_API_KEY", ""),
    ]);
    let hook = JellyfinHook::new(&cfg);
    assert_eq!(hook.precondition(), Some("JELLYFIN_API_KEY is required"));
    // A direct call still reports the legacy string verbatim.
    let e = run_once(&hook, &cfg)
        .await
        .expect_err("a precondition fails");
    assert_eq!(e.to_string(), "JELLYFIN_API_KEY is required");
}

/// A debounced batch produces one request for many completions, end to end through the dispatcher
/// (the timing itself is `tests/debounce.rs`; this is the HTTP half).
#[tokio::test]
async fn twenty_completions_produce_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
        // A window short enough for real time, with the cap ten times it, as DESIGN §13.4's
        // default relationship has it.
        ("AULOS_JELLYFIN_DEBOUNCE_SECS", "1"),
        ("AULOS_JELLYFIN_MAX_WAIT_SECS", "10"),
    ]);
    let hook = Arc::new(JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF));
    let dispatcher = HookDispatcher::with_hooks(
        Arc::clone(&cfg),
        vec![Arc::clone(&hook) as Arc<dyn Hook>],
        Arc::new(FakeClock::default()),
    );
    let health = dispatcher.health_handle();
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    for i in 0..20 {
        ev.completed(&ItemBuilder::finished(&format!("Clip {i}")).view())
            .await;
    }
    assert!(
        until(|| health
            .health()
            .stat("jellyfin")
            .is_some_and(|s| s.runs_total == 1))
        .await,
        "{:?}",
        health.health()
    );
    let stat = health.health().stat("jellyfin").cloned().expect("jellyfin");
    assert_eq!(stat.failures_total, 0);
    assert!(
        stat.last_success_at.is_some(),
        "healthz records the success"
    );
    assert_eq!(stat.health.status, ComponentStatus::Ok);

    drop(ev.tx);
    task.await.expect("clean stop");
    let requests: Vec<Request> = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "one scan for twenty completions");
    server.verify().await;
}
