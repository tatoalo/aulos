//! Retired routes reject requests without changing v2 state.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use reqwest::Method;
use support::{Rig, for_each_prefix};

#[tokio::test]
async fn retired_routes_cannot_mutate_state_under_either_prefix() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;
        for (method, route) in [
            (Method::POST, "add"),
            (Method::GET, "history"),
            (Method::POST, "delete"),
            (Method::POST, "start"),
            (Method::POST, "cancel-add"),
            (Method::GET, "presets"),
            (Method::GET, "cookie-status"),
            (Method::POST, "upload-cookies"),
            (Method::POST, "delete-cookies"),
            (Method::POST, "subscribe"),
            (Method::GET, "subscriptions"),
            (Method::POST, "subscriptions/update"),
            (Method::POST, "subscriptions/check"),
            (Method::POST, "subscriptions/delete"),
            (Method::GET, "version"),
            (Method::GET, "socket.io/"),
            (Method::POST, "socket.io/"),
        ] {
            let response = rig
                .http
                .request(method, rig.url(route))
                .bearer_auth("s3cret")
                .json(&serde_json::json!({"url":"https://fake.test/retired","quality":"best"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 404, "{prefix}{route}");
        }
        let state: serde_json::Value = rig
            .http
            .get(rig.url("api/v2/state"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["items"], serde_json::json!([]));
        assert_eq!(rig.get_raw("robots.txt").await.status().as_u16(), 200);
        if prefix != "/" {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            for path in ["/", prefix.trim_end_matches('/')] {
                let response = client
                    .get(format!("http://{}{path}", rig.addr))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 302);
                assert_eq!(response.headers()["location"], prefix);
            }
        }
    })
    .await;
}
