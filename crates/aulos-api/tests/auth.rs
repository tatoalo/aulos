//! Auth: cookie passthrough, the bearer token, the trusted proxy header — and never a redirect
//! (PROTOCOL §1.4, DESIGN §16.6), under both prefixes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use support::{Rig, for_each_prefix};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test]
async fn with_nothing_configured_the_api_is_open() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, _) = rig.get("api/v2/state").await;
        assert_eq!(status, 200, "cookies flow through and the proxy decides");
    })
    .await;
}

#[tokio::test]
async fn a_configured_token_is_required_and_compared_exactly() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;

        let (status, body) = rig.get("api/v2/state").await;
        assert_eq!(status, 401, "{body}");
        assert_eq!(body["error"]["code"], "unauthorized");
        assert_eq!(body["error"]["message"], "authentication required");
        assert!(body["error"]["request_id"].as_str().is_some());

        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .bearer_auth("s3crets")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            401,
            "a near-miss is still a miss"
        );

        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .header("authorization", "Bearer s3cret ")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "surrounding whitespace in the header value is trimmed, per RFC 6750"
        );

        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);
    })
    .await;
}

#[tokio::test]
async fn a_401_never_redirects() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_TRUSTED_PROXY_AUTH_HEADER", "Remote-User")
            .start()
            .await;
        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let response = no_redirect
            .get(rig.url("api/v2/state"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);
        assert!(
            !response.headers().contains_key("location"),
            "never a 303 to a login page (PROTOCOL §1.4)"
        );
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json; charset=utf-8",
            "never a 200 with an HTML body either"
        );
        let (_, body) = support::status_and_body(response).await;
        assert_eq!(body["error"]["code"], "unauthorized");

        // The header, present and non-empty, is enough.
        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .header("Remote-User", "alice")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);

        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .header("Remote-User", "  ")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401, "blank is absent");
    })
    .await;
}

#[tokio::test]
async fn the_two_mechanisms_compose() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_TRUSTED_PROXY_AUTH_HEADER", "Remote-User")
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;
        for request in [
            rig.http.get(rig.url("api/v2/state")).bearer_auth("s3cret"),
            rig.http
                .get(rig.url("api/v2/state"))
                .header("Remote-User", "alice"),
        ] {
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status().as_u16(),
                200,
                "either mechanism satisfies"
            );
        }
        let (status, _) = rig.get("api/v2/state").await;
        assert_eq!(status, 401, "neither does not");
    })
    .await;
}

#[tokio::test]
async fn the_file_routes_are_behind_the_same_auth() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;
        rig.write_download("clip.mp4", b"bytes");
        let (status, body) = rig.get("download/clip.mp4").await;
        assert_eq!(status, 401, "{body}");
        let response = rig
            .http
            .get(rig.url("download/clip.mp4"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
    })
    .await;
}

#[tokio::test]
async fn the_websocket_accepts_the_token_three_ways() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;

        // No token: the upgrade is refused with a 401, not with a socket that closes later.
        let plain = tokio_tungstenite::connect_async(rig.ws_url("ws")).await;
        assert!(plain.is_err(), "an unauthenticated upgrade must fail");

        // 1. `?token=`.
        let (mut socket, _) = tokio_tungstenite::connect_async(rig.ws_url("ws?token=s3cret"))
            .await
            .expect("?token= is accepted");
        assert!(support::next_frame(&mut socket).await["t"] == "snapshot");
        socket.close(None).await.unwrap();

        // 2. The `bearer.<token>` subprotocol.
        let mut request = rig.ws_url("ws").into_client_request().unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            "aulos.v2, bearer.s3cret".parse().unwrap(),
        );
        let (mut socket, response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the subprotocol list is accepted");
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            "aulos.v2",
            "the server selects the protocol version, not the token"
        );
        assert!(support::next_frame(&mut socket).await["t"] == "snapshot");
        socket.close(None).await.unwrap();

        // 3. An `Authorization` header, for a client that can set one on the upgrade.
        let mut request = rig.ws_url("ws").into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer s3cret".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("a bearer header is accepted");
        let snapshot = support::next_frame(&mut socket).await;
        assert_eq!(snapshot["t"], "snapshot");
        assert_eq!(snapshot["server"]["url_prefix"], json!(prefix));
    })
    .await;
}
