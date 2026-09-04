//! The `plain` (`reqwest`) client against a real loopback HTTP server.
//!
//! The unit tests drive the scrape pipeline through an in-process `ScHttp` mock, which proves the
//! *logic*. This file proves the other half: that [`PlainClient`] itself sends what the pipeline
//! asks it to, follows a redirect, records `Set-Cookie` into the jar the download engines read,
//! and that the whole S1→S4 chain works end to end over a socket with no network access.
//!
//! `wiremock` binds `127.0.0.1`, so there is still no network at test time.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "a panic is the reporting mechanism in a test"
)]

use std::sync::Arc;

use aulos_provider_sc::http::{PlainClient, ScHttp, ScReq};
use aulos_provider_sc::{SiteVersions, fresh_stream, inertia};
use url::Url;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const IT_PAGE: &str = include_str!("fixtures/sc/it_page.html");
const VIXCLOUD: &str = include_str!("fixtures/sc/vixcloud_streams_active.html");
const VERSION: &str = "d41d8cd98f00b204e9800998ecf8427e";

fn client() -> Arc<dyn ScHttp> {
    Arc::new(PlainClient::new().expect("the plain client must build"))
}

async fn mount_version(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/it"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(IT_PAGE)
                .insert_header("set-cookie", "sid=abc123; Path=/"),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_plain_client_reads_the_site_version_over_http() {
    let server = MockServer::start().await;
    mount_version(&server).await;
    let base = Url::parse(&server.uri()).expect("uri");
    let v = inertia::fetch_version(client().as_ref(), &base)
        .await
        .expect("a version");
    assert_eq!(&*v, VERSION);
}

#[tokio::test]
async fn the_plain_client_sends_the_inertia_headers_the_site_demands() {
    let server = MockServer::start().await;
    mount_version(&server).await;
    // The mock only answers when all three headers are present, so a missing one is a 404 and the
    // assertion below fails with the status rather than passing silently.
    Mock::given(method("GET"))
        .and(path("/it/watch/123"))
        .and(header("x-inertia", "true"))
        .and(header("x-inertia-version", VERSION))
        .and(header("accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"props":{"ok":true}}"#))
        .mount(&server)
        .await;

    let base = Url::parse(&server.uri()).expect("uri");
    let page = inertia::inertia_get(
        client().as_ref(),
        &SiteVersions::new(),
        &base,
        "/it/watch/123",
    )
    .await
    .expect("the inertia call");
    assert_eq!(inertia::props(&page)["ok"], true);
}

#[tokio::test]
async fn a_real_403_drives_the_one_shot_version_refresh() {
    let server = MockServer::start().await;
    mount_version(&server).await;
    // First call: stale version rejected. Second call: the refreshed version is accepted.
    Mock::given(method("GET"))
        .and(path("/it/watch/123"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/it/watch/123"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"props":{"retried":true}}"#))
        .mount(&server)
        .await;

    let base = Url::parse(&server.uri()).expect("uri");
    let page = inertia::inertia_get(
        client().as_ref(),
        &SiteVersions::new(),
        &base,
        "/it/watch/123",
    )
    .await
    .expect("the retry must succeed");
    assert_eq!(inertia::props(&page)["retried"], true);
    // `/it` twice: the initial fetch and the forced refresh.
    let requests = server
        .received_requests()
        .await
        .expect("the mock records requests");
    let it_calls = requests.iter().filter(|r| r.url.path() == "/it").count();
    assert_eq!(it_calls, 2);
}

#[tokio::test]
async fn a_redirect_is_followed_and_the_final_url_is_reported() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/go"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/landed", server.uri()).as_str()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/landed"))
        .respond_with(ResponseTemplate::new(200).set_body_string("here"))
        .mount(&server)
        .await;

    let res = client()
        .get(ScReq::get(
            Url::parse(&format!("{}/go", server.uri())).expect("uri"),
        ))
        .await
        .expect("a response");
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "here");
    assert_eq!(res.url.path(), "/landed");
}

#[tokio::test]
async fn the_whole_pipeline_runs_over_a_socket_and_hands_the_engines_the_cookie_jar() {
    let server = MockServer::start().await;
    let uri = server.uri();
    mount_version(&server).await;

    // The watch page's `embedUrl` and the embed page's iframe both point back at the mock, so the
    // full S1→S4 chain stays on loopback.
    let watch_body = serde_json::json!({
        "component": "Titles/Watch",
        "props": {
            "title": { "id": 9, "name": "Una Serie Qualunque", "type": "tv" },
            "episode": { "id": 456, "number": 3, "name": "Il Segreto",
                          "season": { "id": 402, "number": 2 } },
            "embedUrl": format!("{uri}/embed/456"),
        },
        "version": VERSION,
    })
    .to_string();
    Mock::given(method("GET"))
        .and(path("/it/watch/9"))
        .and(query_param("e", "456"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(watch_body)
                .insert_header("set-cookie", "cf_clearance=zzz; Path=/"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/embed/456"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "<html><body><iframe src=\"{uri}/vix/98765?token=abc\"></iframe></body></html>"
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/vix/98765"))
        .respond_with(ResponseTemplate::new(200).set_body_string(VIXCLOUD))
        .mount(&server)
        .await;

    let base = Url::parse(&uri).expect("uri");
    let watch = Url::parse(&format!("{uri}/it/watch/9?e=456")).expect("uri");
    let http = client();
    let target = fresh_stream(http.as_ref(), &base, &watch)
        .await
        .expect("a stream target");

    assert_eq!(
        target.m3u8.as_str(),
        "https://vixcloud.co/playlist/98765?b=1&token=tok3n-value&expires=1759000000"
    );
    assert_eq!(
        target.header("Referer"),
        Some(format!("{uri}/vix/98765?token=abc").as_str())
    );
    assert_eq!(
        target.header("Origin").map(str::to_owned),
        Some(uri.clone())
    );
    // Both `Set-Cookie`s the site sent are in the header the download engines pass on.
    assert!(target.cookies.contains("sid=abc123"), "{}", target.cookies);
    assert!(
        target.cookies.contains("cf_clearance=zzz"),
        "{}",
        target.cookies
    );

    // And the m3u8 itself was never fetched (DESIGN §10.5 removes the legacy debug GET).
    let requests = server
        .received_requests()
        .await
        .expect("the mock records requests");
    assert!(
        !requests.iter().any(|r| r.url.path().contains("/playlist/")),
        "{:?}",
        requests.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}
