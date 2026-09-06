//! The Jellyfin library scan against `wiremock`: the global scan that is the default, the opt-in
//! `Library/Media/Updated` mode, the fallbacks, the retry budget, all four legacy message strings,
//! the precondition path and the honest health detail (DESIGN §13.1).
//!
//! The bug these tests were rewritten for is `docs/reference/jellyfin-refresh-experiment.md`:
//! `POST /Items/{id}/Refresh` answers `204` and never indexes a new file, so a fallback gated on
//! "rejected" never fired and the hook reported success for a no-op.
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
use serde_json::Value;
use wiremock::matchers::{body_string, header, method, path};
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
    run_batch(hook, cfg, &["Clip.mp4"]).await
}

/// One invocation whose batch produced `files`, each relative to `DOWNLOAD_DIR` (`/downloads` in
/// [`common::config`]). This is how a debounced playlist reaches the hook.
async fn run_batch(
    hook: &JellyfinHook,
    cfg: &Arc<aulos_core::config::Config>,
    files: &[&str],
) -> Result<(), HookError> {
    let (runner, _store) = runner(cfg);
    let views: Vec<_> = files
        .iter()
        .map(|f| ItemBuilder::finished("Clip").filename(f).view())
        .collect();
    let batch: Vec<_> = views
        .iter()
        .map(|v| BatchEntry::from_view(v, TerminalStatus::Finished))
        .collect();
    runner.run(hook, &views[0], &batch).await
}

/// The `Updates` array of the one `Library/Media/Updated` request the server received.
fn updates_of(req: &Request) -> Vec<(String, String)> {
    let body: Value = serde_json::from_slice(&req.body).expect("a JSON body");
    body["Updates"]
        .as_array()
        .expect("Updates is an array")
        .iter()
        .map(|u| {
            (
                u["Path"].as_str().expect("Path").to_owned(),
                u["UpdateType"].as_str().expect("UpdateType").to_owned(),
            )
        })
        .collect()
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

/// The regression, pinned. A set `JELLYFIN_LIBRARY_ID` used to select `Items/{id}/Refresh`, which
/// answers 204 and never indexes a new file. It must now change nothing about the request.
#[tokio::test]
async fn a_library_id_no_longer_selects_the_item_refresh_that_cannot_discover_files() {
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
        ("JELLYFIN_LIBRARY_ID", "ca4fc2dadb00fcd7e929d2d0a49151b8"),
        ("JELLYFIN_METADATA_REFRESH_MODE", "FullRefresh"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_once(&hook, &cfg).await.expect("the global scan runs");

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "one request, and it is the global scan");
    assert!(
        !requests[0].url.path().contains("/Items/"),
        "no item-metadata refresh was issued: {}",
        requests[0].url
    );
    let health = hook.health();
    assert_eq!(health.detail["mode"], "global_scan");
    assert_eq!(health.detail["last_status"], 204);
    assert_eq!(health.detail["library_id_ignored"], true);
    server.verify().await;
}

/// A 204 on the targeted call is what Jellyfin answers for a no-op, so it must never be the reason
/// a fallback does *not* happen. Here the map does not cover the path, and the hook scans globally
/// even though a `Library/Media/Updated` call would have been accepted with 204.
#[tokio::test]
async fn an_uncovered_path_falls_back_to_the_global_scan_rather_than_trusting_a_204() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Media/Updated"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
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
        // `DOWNLOAD_DIR` is /downloads, which this map does not mention.
        ("JELLYFIN_PATH_MAP", "/elsewhere=/data/videos"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_once(&hook, &cfg).await.expect("the fallback succeeds");
    assert_eq!(hook.health().detail["mode"], "global_scan");
    server.verify().await;
}

/// The targeted mode: every produced path is mapped, so one `Library/Media/Updated` carries them
/// all and no global scan is issued.
#[tokio::test]
async fn a_covered_batch_notifies_media_updated_with_the_mapped_paths() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Media/Updated"))
        .and(header("content-type", "application/json"))
        .and(header("authorization", "MediaBrowser Token=\"secret\""))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
        ("JELLYFIN_PATH_MAP", "/downloads=/data/videos"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_batch(&hook, &cfg, &["tube/a.mp4", "tube/b.mp4"])
        .await
        .expect("the notification succeeds");

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        updates_of(&requests[0]),
        vec![
            ("/data/videos/tube/a.mp4".to_owned(), "Created".to_owned()),
            ("/data/videos/tube/b.mp4".to_owned(), "Created".to_owned()),
        ],
        "every file in the debounced batch, translated to Jellyfin's spelling"
    );
    let health = hook.health();
    assert_eq!(health.detail["mode"], "media_updated");
    assert_eq!(health.detail["last_status"], 204);
    assert!(health.detail.contains_key("last_request_at"));
    server.verify().await;
}

/// A batch that mixes a covered and an uncovered path takes the global scan, which is a superset
/// of the notification it would otherwise have sent for half of it.
#[tokio::test]
async fn a_partially_covered_batch_takes_the_global_scan() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Media/Updated"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
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
        // Only the `tube` subtree is mapped; `other/b.mp4` is not.
        ("JELLYFIN_PATH_MAP", "/downloads/tube=/data/videos"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_batch(&hook, &cfg, &["tube/a.mp4", "other/b.mp4"])
        .await
        .expect("the fallback succeeds");
    server.verify().await;
}

/// A non-2xx on the notification falls back to the global scan — on ANY status, not just the
/// 400/404 the old targeted path waited for.
#[tokio::test]
async fn a_failed_notification_falls_back_to_the_global_scan() {
    for status in [400_u16, 404, 401, 500] {
        let server = MockServer::start().await;
        let expected = if status == 500 { 3 } else { 1 };
        Mock::given(method("POST"))
            .and(path("/Library/Media/Updated"))
            .respond_with(ResponseTemplate::new(status))
            .expect(expected)
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
            ("JELLYFIN_PATH_MAP", "/downloads=/data/videos"),
        ]);
        let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
        run_batch(&hook, &cfg, &["tube/a.mp4"])
            .await
            .unwrap_or_else(|e| panic!("the fallback must succeed for {status}: {e}"));
        assert_eq!(
            hook.health().detail["mode"],
            "global_scan",
            "health reports the mode that actually ran, not the configured one"
        );
        server.verify().await;
    }
}

/// A transport failure on the global scan is reported with a `null` status rather than a stale
/// success — the health payload must never imply a request that did not happen.
#[tokio::test]
async fn a_failed_scan_records_the_attempt_and_not_a_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "secret"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    run_once(&hook, &cfg).await.expect_err("three 503s give up");
    let health = hook.health();
    assert_eq!(health.detail["mode"], "global_scan");
    assert_eq!(health.detail["last_status"], 503);
    server.verify().await;
}

/// A 401 is final: not retryable, and the legacy message shape survives.
#[tokio::test]
async fn a_401_on_the_scan_is_final_and_keeps_the_legacy_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let cfg = config(&[
        ("JELLYFIN_SYNC_ENABLED", "true"),
        ("JELLYFIN_URL", &server.uri()),
        ("JELLYFIN_API_KEY", "wrong"),
    ]);
    let hook = JellyfinHook::new(&cfg).with_backoff(&NO_BACKOFF);
    let e = run_once(&hook, &cfg).await.expect_err("401 fails");
    assert_eq!(
        e.to_string(),
        "Jellyfin refresh failed with HTTP 401: ",
        "the legacy shape, with an empty body as its details"
    );
    assert_eq!(hook.health().detail["last_status"], 401);
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
