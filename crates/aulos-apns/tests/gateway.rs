//! The client against a mock gateway (DESIGN §25.5).
//!
//! The mock is an ordinary `wiremock` server over HTTP/1.1. That is not a compromise: reqwest
//! picks the protocol by ALPN, so the same client that speaks HTTP/2 to
//! `api.push.apple.com` speaks HTTP/1.1 to `http://127.0.0.1:…` — which is the whole reason
//! `APNS_BASE_URL_OVERRIDE` exists.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_apns::{
    ApnsClient, ApnsEnvironment, Outcome, PRODUCTION_BASE, Push, PushKind, SANDBOX_BASE,
};
use aulos_core::clock::Clock;
use common::{TEST_BUNDLE_ID, TEST_KEY_ID, TEST_KEY_P8, TEST_TEAM_ID, clock};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "aabbccddeeff00112233445566778899aabbccddeeff001122334455667788ff";

/// A client pointed at `server`, with a millisecond retry ladder so a backoff test is not a
/// twenty-one-second test.
fn client(server: &MockServer) -> ApnsClient {
    let clock: Arc<dyn Clock> = clock();
    ApnsClient::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, TEST_TEAM_ID, clock)
        .expect("client")
        .with_base_url(&server.uri())
        .expect("base url")
        .with_backoff(vec![Duration::from_millis(1); 3])
}

fn alert_push() -> Push {
    Push {
        kind: PushKind::Alert,
        topic: Arc::from(TEST_BUNDLE_ID),
        priority: 10,
        expiration: 1_772_586_000,
        collapse_id: Some(Arc::from("01JABCDEF0123456789ABCDEFG")),
        payload: json!({ "aps": { "alert": { "title": "Download finished" } } }),
    }
}

async fn mount(server: &MockServer, status: u16, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{TOKEN}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_gateway_is_chosen_by_the_devices_environment() {
    let clock: Arc<dyn Clock> = clock();
    let plain =
        ApnsClient::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, TEST_TEAM_ID, clock).expect("client");
    assert_eq!(plain.base_url(ApnsEnvironment::Sandbox), SANDBOX_BASE);
    assert_eq!(plain.base_url(ApnsEnvironment::Production), PRODUCTION_BASE);

    // The override collapses both onto the test server, trailing slash and all.
    let overridden = plain
        .with_base_url("http://127.0.0.1:9/")
        .expect("override");
    assert_eq!(
        overridden.base_url(ApnsEnvironment::Sandbox),
        "http://127.0.0.1:9"
    );
    assert_eq!(
        overridden.base_url(ApnsEnvironment::Production),
        "http://127.0.0.1:9"
    );
}

#[tokio::test]
async fn a_200_delivers_and_carries_every_documented_header() {
    let server = MockServer::start().await;
    mount(&server, 200, json!({})).await;

    let client = client(&server);
    let outcome = client
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(outcome, Outcome::Delivered);

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    let header = |name: &str| {
        req.headers
            .get(name)
            .map(|v| v.to_str().unwrap_or_default().to_owned())
    };
    assert_eq!(req.url.path(), format!("/3/device/{TOKEN}"));
    assert_eq!(header("apns-topic").as_deref(), Some(TEST_BUNDLE_ID));
    assert_eq!(header("apns-push-type").as_deref(), Some("alert"));
    assert_eq!(header("apns-priority").as_deref(), Some("10"));
    assert_eq!(header("apns-expiration").as_deref(), Some("1772586000"));
    assert_eq!(
        header("apns-collapse-id").as_deref(),
        Some("01JABCDEF0123456789ABCDEFG")
    );
    let auth = header("authorization").expect("authorization");
    assert!(auth.starts_with("bearer "), "{auth}");
    assert_eq!(auth["bearer ".len()..].split('.').count(), 3, "a JWT");
}

#[tokio::test]
async fn a_live_activity_push_declares_the_liveactivity_type_and_no_collapse_id() {
    let server = MockServer::start().await;
    mount(&server, 200, json!({})).await;

    let push = Push {
        kind: PushKind::LiveActivity,
        topic: Arc::from(format!("{TEST_BUNDLE_ID}.push-type.liveactivity")),
        priority: 5,
        expiration: 0,
        collapse_id: None,
        payload: json!({ "aps": { "event": "update" } }),
    };
    client(&server)
        .send(&push, TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");

    let requests = server.received_requests().await.expect("requests");
    let req = &requests[0];
    assert_eq!(
        req.headers.get("apns-push-type").unwrap().to_str().unwrap(),
        "liveactivity"
    );
    assert_eq!(
        req.headers.get("apns-topic").unwrap().to_str().unwrap(),
        "com.tatoalo.aulos.push-type.liveactivity"
    );
    assert_eq!(req.headers.get("apns-priority").unwrap(), "5");
    assert!(req.headers.get("apns-collapse-id").is_none());
}

#[tokio::test]
async fn a_410_unregistered_asks_the_caller_to_prune_the_token() {
    let server = MockServer::start().await;
    mount(&server, 410, json!({ "reason": "Unregistered" })).await;

    let outcome = client(&server)
        .send(&alert_push(), TOKEN, ApnsEnvironment::Production)
        .await
        .expect("send");
    assert_eq!(
        outcome,
        Outcome::TokenInvalid {
            status: 410,
            reason: "Unregistered".into()
        }
    );
    assert!(outcome.prunes_token());
    // One request: a dead token is not worth a retry.
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn a_400_bad_device_token_prunes_but_another_400_does_not() {
    for (reason, prunes) in [
        ("BadDeviceToken", true),
        ("DeviceTokenNotForTopic", true),
        ("BadCollapseId", false),
    ] {
        let server = MockServer::start().await;
        mount(&server, 400, json!({ "reason": reason })).await;
        let outcome = client(&server)
            .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
            .await
            .expect("send");
        assert_eq!(outcome.prunes_token(), prunes, "{reason}");
        if !prunes {
            assert!(matches!(outcome, Outcome::Rejected { status: 400, .. }));
        }
    }
}

#[tokio::test]
async fn a_403_expired_provider_token_reminds_the_jwt_and_retries_exactly_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{TOKEN}")))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({ "reason": "ExpiredProviderToken" })),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{TOKEN}")))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(2)
        .mount(&server)
        .await;

    let client = client(&server);
    let outcome = client
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(outcome, Outcome::Delivered);
    assert_eq!(client.provider_token().minted_total(), 2, "one remint");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2, "one retry, not a loop");
    for req in &requests {
        let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
        assert!(auth.starts_with("bearer "), "{auth}");
    }
    // The two bearer strings are byte-identical here, and that is correct rather than a bug:
    // `jsonwebtoken`'s ES256 signatures are deterministic (RFC 6979) and the fake clock has not
    // moved, so re-signing the same claims reproduces the same token. `minted_total` above is what
    // proves a second signature was actually taken; in production `iat` has moved on.
}

#[tokio::test]
async fn a_403_that_survives_the_remint_is_a_misconfiguration() {
    let server = MockServer::start().await;
    mount(&server, 403, json!({ "reason": "InvalidProviderToken" })).await;

    let outcome = client(&server)
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(
        outcome,
        Outcome::ProviderTokenRejected {
            reason: "InvalidProviderToken".into()
        }
    );
    assert!(outcome.misconfigured());
    // Two requests, and then it stops: spinning on a wrong APNS_KEY_ID helps nobody.
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
}

#[tokio::test]
async fn a_403_with_any_other_reason_is_rejected_without_a_remint() {
    let server = MockServer::start().await;
    mount(&server, 403, json!({ "reason": "Forbidden" })).await;

    let client = client(&server);
    let outcome = client
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert!(matches!(outcome, Outcome::Rejected { status: 403, .. }));
    assert_eq!(client.provider_token().minted_total(), 1);
}

#[tokio::test]
async fn a_429_backs_off_and_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{TOKEN}")))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({ "reason": "TooManyRequests" })),
        )
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{TOKEN}")))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(2)
        .mount(&server)
        .await;

    let outcome = client(&server)
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(outcome, Outcome::Delivered);
    assert_eq!(server.received_requests().await.expect("requests").len(), 3);
}

#[tokio::test]
async fn a_5xx_gives_up_after_three_retries() {
    let server = MockServer::start().await;
    mount(&server, 503, json!({ "reason": "ServiceUnavailable" })).await;

    let outcome = client(&server)
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(
        outcome,
        Outcome::GaveUp {
            status: Some(503),
            reason: "ServiceUnavailable".into()
        }
    );
    // The DEFAULT_BACKOFF ladder is three delays, so four requests in total.
    assert_eq!(server.received_requests().await.expect("requests").len(), 4);
    assert!(
        !outcome.prunes_token(),
        "a busy gateway is not a dead token"
    );
    assert!(!outcome.misconfigured());
}

#[tokio::test]
async fn a_body_that_is_not_apples_shape_still_reaches_the_log() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{TOKEN}")))
        .respond_with(ResponseTemplate::new(413).set_body_string("<html>too big</html>"))
        .mount(&server)
        .await;

    let outcome = client(&server)
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(
        outcome,
        Outcome::Rejected {
            status: 413,
            reason: "<html>too big</html>".into()
        }
    );
}

#[tokio::test]
async fn an_unreachable_gateway_gives_up_rather_than_erroring() {
    // Port 1 on the loopback refuses instantly, which is the transport branch of the ladder.
    let clock: Arc<dyn Clock> = clock();
    let client = ApnsClient::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, TEST_TEAM_ID, clock)
        .expect("client")
        .with_base_url("http://127.0.0.1:1")
        .expect("base url")
        .with_backoff(vec![Duration::from_millis(1)]);

    let outcome = client
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("a transport failure is an Outcome, not an Err");
    assert!(matches!(outcome, Outcome::GaveUp { status: None, .. }));

    // And the reason must not carry the device token. reqwest's `Display` appends
    // " for url (...)", the URL is `/3/device/<token>`, and this string becomes `healthz`'s
    // `apns.last_error` — which is served outside the auth layer.
    let reason = outcome.last_error().expect("a reason");
    assert!(!reason.contains(TOKEN), "{reason}");
    assert!(!reason.contains("127.0.0.1"), "{reason}");
    assert!(reason.ends_with("(gave up)"), "{reason}");
}

// ---------------------------------------------------------------------------
// The retry ladder the pushes that matter get (DESIGN §25.5)
// ---------------------------------------------------------------------------

/// A client whose two ladders are distinguishable but both millisecond-fast: three delays for an
/// update, six for a priority-10 push.
fn laddered(server: &MockServer) -> ApnsClient {
    let clock: Arc<dyn Clock> = clock();
    ApnsClient::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, TEST_TEAM_ID, clock)
        .expect("client")
        .with_base_url(&server.uri())
        .expect("base url")
        .with_backoff_ladders(
            vec![Duration::from_millis(1); 3],
            vec![Duration::from_millis(1); 6],
        )
}

fn update_push(expiration: i64) -> Push {
    Push {
        kind: PushKind::LiveActivity,
        topic: Arc::from(format!("{TEST_BUNDLE_ID}.push-type.liveactivity")),
        priority: 5,
        expiration,
        collapse_id: None,
        payload: json!({ "aps": { "event": "update" } }),
    }
}

#[test]
fn the_two_ladders_are_the_documented_ones() {
    // Both APNs attempts on the VPS since boot ended `GaveUp` during a VPN blip that lasted a few
    // minutes: [1s, 4s, 16s] covers 21 s, which is not a blip. The pushes that matter — alerts,
    // and a Live Activity start or end — now cover just over eight minutes.
    assert_eq!(aulos_apns::DEFAULT_BACKOFF.map(|d| d.as_secs()), [1, 4, 16]);
    assert_eq!(
        aulos_apns::IMMEDIATE_BACKOFF.map(|d| d.as_secs()),
        [1, 4, 16, 60, 120, 300]
    );
}

#[tokio::test]
async fn a_priority_ten_push_walks_the_long_ladder() {
    let server = MockServer::start().await;
    mount(&server, 503, json!({ "reason": "ServiceUnavailable" })).await;

    let client = laddered(&server);
    let outcome = client
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert!(matches!(
        outcome,
        Outcome::GaveUp {
            status: Some(503),
            ..
        }
    ));
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        7,
        "six delays, so seven requests"
    );
    assert_eq!(client.counters().snapshot().retried_total, 6);
}

#[tokio::test]
async fn an_update_keeps_the_short_ladder() {
    // A progress frame that could not be delivered is superseded by the next one, so spending five
    // minutes on it would only push a stale percentage onto the lock screen.
    let server = MockServer::start().await;
    mount(&server, 429, json!({ "reason": "TooManyRequests" })).await;

    let client = laddered(&server);
    client
        .send(&update_push(0), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        4,
        "three delays, so four requests"
    );
    assert_eq!(client.counters().snapshot().retried_total, 3);
}

#[tokio::test]
async fn a_retry_that_would_land_past_the_expiration_is_not_taken() {
    // `apns-expiration` is what the push itself says it is worth: retrying past it hands Apple a
    // message it is contractually required to drop.
    let server = MockServer::start().await;
    mount(&server, 500, json!({ "reason": "InternalServerError" })).await;

    let expired = Push {
        expiration: common::epoch_secs(),
        ..alert_push()
    };
    let client = laddered(&server);
    let outcome = client
        .send(&expired, TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("send");
    assert!(matches!(
        outcome,
        Outcome::GaveUp {
            status: Some(500),
            ..
        }
    ));
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "the deadline had already passed, so no retry was worth taking"
    );
    assert_eq!(client.counters().snapshot().retried_total, 0);
}

#[tokio::test]
async fn a_transport_failure_on_a_priority_ten_push_also_walks_the_long_ladder() {
    // The VPS case: the gateway is not answering at all because the VPN tunnel is down.
    let clock: Arc<dyn Clock> = clock();
    let client = ApnsClient::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, TEST_TEAM_ID, clock)
        .expect("client")
        .with_base_url("http://127.0.0.1:1")
        .expect("base url")
        .with_backoff_ladders(
            vec![Duration::from_millis(1)],
            vec![Duration::from_millis(1); 4],
        );

    let outcome = client
        .send(&alert_push(), TOKEN, ApnsEnvironment::Sandbox)
        .await
        .expect("a transport failure is an Outcome, not an Err");
    assert!(matches!(outcome, Outcome::GaveUp { status: None, .. }));
    assert_eq!(client.counters().snapshot().retried_total, 4);
}
