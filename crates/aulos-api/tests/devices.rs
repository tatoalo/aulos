//! The four push-registration routes (PROTOCOL §4.8, DESIGN §25), under both prefixes.
//!
//! Everything here goes through the real router, the real auth layer and the real SQLite store the
//! rig opens, so a route registered under the wrong prefix or outside the auth layer fails in the
//! same test that covers its behaviour.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use aulos_core::{DeviceStore, ItemId};
use serde_json::{Value, json};
use support::{Rig, for_each_prefix, status_and_body};

/// A 64-character lowercase-hex token.
fn token(seed: &str) -> String {
    seed.repeat(64 / seed.len())
}

/// The body PROTOCOL §4.8 documents, verbatim.
fn registration(start_token: Option<&str>) -> Value {
    json!({
        "platform": "ios",
        "bundle_id": "com.tatoalo.aulos",
        "environment": "sandbox",
        "alerts": true,
        "live_activity_start_token": start_token,
        "app_version": "1.0.0 (3)",
    })
}

/// `PUT` with a JSON body. The rig has `post`/`patch`/`delete` but no `put`.
async fn put(rig: &Rig, suffix: &str, body: &Value) -> (u16, Value) {
    let response = rig
        .http
        .put(rig.url(suffix))
        .json(body)
        .send()
        .await
        .unwrap();
    status_and_body(response).await
}

#[tokio::test]
async fn a_device_registers_idempotently_and_deregisters_idempotently() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let device = token("ab");
        let path = format!("api/v2/devices/{device}");

        let (status, body) = put(&rig, &path, &registration(Some(&token("cd")))).await;
        assert_eq!(status, 204, "{body}");
        assert_eq!(body, Value::Null, "204 carries no body");

        let stored = DeviceStore::devices(&rig.store).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(&*stored[0].token, device);
        assert_eq!(&*stored[0].bundle_id, "com.tatoalo.aulos");
        assert_eq!(stored[0].environment.as_str(), "sandbox");
        assert!(stored[0].alerts);
        assert_eq!(
            stored[0].live_activity_start_token.as_deref(),
            Some(&*token("cd"))
        );
        assert_eq!(stored[0].app_version.as_deref(), Some("1.0.0 (3)"));

        // A repeat PUT is an upsert, not a second row, and it carries the new values.
        let mut again = registration(None);
        again["alerts"] = json!(false);
        again["environment"] = json!("production");
        let (status, body) = put(&rig, &path, &again).await;
        assert_eq!(status, 204, "{body}");
        let stored = DeviceStore::devices(&rig.store).await.unwrap();
        assert_eq!(stored.len(), 1, "keyed on the token: {stored:?}");
        assert!(!stored[0].alerts);
        assert_eq!(stored[0].environment.as_str(), "production");
        assert_eq!(stored[0].live_activity_start_token, None);

        for _ in 0..2 {
            let (status, body) = rig.delete(&path).await;
            assert_eq!(status, 204, "an idempotent delete: {body}");
        }
        assert!(DeviceStore::devices(&rig.store).await.unwrap().is_empty());

        // Deleting a token that was never registered is a 204 too.
        let (status, _) = rig.delete(&format!("api/v2/devices/{}", token("ef"))).await;
        assert_eq!(status, 204);
    })
    .await;
}

#[tokio::test]
async fn a_bundle_id_outside_apns_topic_is_refused() {
    // `bundle_id` becomes the `apns-topic` of every push to this device, so a free-form value
    // would let any holder of the API token choose which app the operator's ES256 provider key
    // signs for. Only `APNS_TOPIC` (default `com.tatoalo.aulos`) and its extensions are accepted.
    let rig = Rig::start("").await;
    let path = format!("api/v2/devices/{}", token("ab"));

    let mut foreign = registration(None);
    foreign["bundle_id"] = json!("com.someone.else");
    let (status, body) = put(&rig, &path, &foreign).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "validation_failed");
    assert_eq!(body["error"]["field"], "bundle_id");
    assert!(DeviceStore::devices(&rig.store).await.unwrap().is_empty());

    // A near-miss that merely shares a prefix is not an extension either.
    let mut lookalike = registration(None);
    lookalike["bundle_id"] = json!("com.tatoalo.aulos2");
    let (status, body) = put(&rig, &path, &lookalike).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["field"], "bundle_id");

    // An extension of the topic — a widget or an App Clip — is accepted and kept verbatim.
    let mut widget = registration(None);
    widget["bundle_id"] = json!("com.tatoalo.aulos.clip");
    let (status, body) = put(&rig, &path, &widget).await;
    assert_eq!(status, 204, "{body}");
    let stored = DeviceStore::devices(&rig.store).await.unwrap();
    assert_eq!(&*stored[0].bundle_id, "com.tatoalo.aulos.clip");
}

#[tokio::test]
async fn a_registration_rejects_a_bad_token_platform_or_environment() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let device = token("ab");
        let path = format!("api/v2/devices/{device}");

        // The token is in the path, so a bad one is caught before the body is even read.
        for bad in ["short", &"z".repeat(64), &"a".repeat(201)] {
            let (status, body) = put(
                &rig,
                &format!("api/v2/devices/{bad}"),
                &registration(None),
            )
            .await;
            assert_eq!(status, 400, "{bad}: {body}");
            assert_eq!(body["error"]["code"], "validation_failed", "{bad}");
            assert_eq!(body["error"]["field"], "token", "{bad}");
        }

        let cases: [(&str, Value); 7] = [
            ("platform", json!({ "bundle_id": "com.tatoalo.aulos", "environment": "sandbox" })),
            (
                "platform",
                json!({ "platform": "android", "bundle_id": "com.tatoalo.aulos", "environment": "sandbox" }),
            ),
            ("bundle_id", json!({ "platform": "ios", "environment": "sandbox" })),
            (
                "bundle_id",
                json!({ "platform": "ios", "bundle_id": "not a bundle id", "environment": "sandbox" }),
            ),
            ("environment", json!({ "platform": "ios", "bundle_id": "com.tatoalo.aulos" })),
            (
                "environment",
                json!({ "platform": "ios", "bundle_id": "com.tatoalo.aulos", "environment": "staging" }),
            ),
            (
                "live_activity_start_token",
                json!({
                    "platform": "ios",
                    "bundle_id": "com.tatoalo.aulos",
                    "environment": "sandbox",
                    "live_activity_start_token": "nope",
                }),
            ),
        ];
        for (field, body) in cases {
            let (status, answer) = put(&rig, &path, &body).await;
            assert_eq!(status, 400, "{field}: {answer}");
            assert_eq!(answer["error"]["code"], "validation_failed", "{field}");
            assert_eq!(answer["error"]["field"], field, "{answer}");
            assert!(answer["error"]["request_id"].as_str().is_some(), "{answer}");
        }

        assert!(
            DeviceStore::devices(&rig.store).await.unwrap().is_empty(),
            "a rejected registration writes nothing"
        );
    })
    .await;
}

/// PROTOCOL §4.1's forward-compatibility rule: a client that sends a field this build does not
/// know is never rejected for it.
#[tokio::test]
async fn an_unknown_field_is_ignored_rather_than_rejected() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut body = registration(None);
        body["a_field_from_a_later_client"] = json!({ "nested": true });
        let (status, answer) = put(&rig, &format!("api/v2/devices/{}", token("ab")), &body).await;
        assert_eq!(status, 204, "{answer}");
        assert_eq!(DeviceStore::devices(&rig.store).await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn a_live_activity_registers_against_a_known_device_and_any_ulid() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let device = token("ab");
        let (status, _) = put(
            &rig,
            &format!("api/v2/devices/{device}"),
            &registration(None),
        )
        .await;
        assert_eq!(status, 204);

        // The item need not exist: the app starts the activity before the server has the row.
        let ghost = ItemId::new();
        let path = format!("api/v2/devices/{device}/live-activities/{ghost}");
        let (status, body) = put(&rig, &path, &json!({ "update_token": token("cd") })).await;
        assert_eq!(status, 204, "{body}");

        let stored = rig.store.live_activities_for(ghost).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(&*stored[0].device_token, device);
        assert_eq!(&*stored[0].update_token, token("cd"));
        assert_eq!(
            stored[0].environment.as_str(),
            "sandbox",
            "the environment is copied from the device"
        );

        // A rotated update token replaces the row rather than adding one.
        let (status, _) = put(&rig, &path, &json!({ "update_token": token("ef") })).await;
        assert_eq!(status, 204);
        let stored = rig.store.live_activities_for(ghost).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(&*stored[0].update_token, token("ef"));

        for _ in 0..2 {
            let (status, body) = rig.delete(&path).await;
            assert_eq!(status, 204, "an idempotent delete: {body}");
        }
        assert!(
            rig.store
                .live_activities_for(ghost)
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

#[tokio::test]
async fn a_live_activity_for_an_unknown_device_is_a_404_envelope() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let item = ItemId::new();
        let path = format!("api/v2/devices/{}/live-activities/{item}", token("ab"));
        let (status, body) = put(&rig, &path, &json!({ "update_token": token("cd") })).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
        assert!(body["error"]["request_id"].as_str().is_some(), "{body}");

        // The delete is idempotent even so: the caller wants the row gone and it is gone.
        let (status, body) = rig.delete(&path).await;
        assert_eq!(status, 204, "{body}");
    })
    .await;
}

#[tokio::test]
async fn a_live_activity_rejects_a_bad_item_id_or_update_token() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let device = token("ab");
        put(
            &rig,
            &format!("api/v2/devices/{device}"),
            &registration(None),
        )
        .await;

        let (status, body) = put(
            &rig,
            &format!("api/v2/devices/{device}/live-activities/not-a-ulid"),
            &json!({ "update_token": token("cd") }),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "item_id");

        let item = ItemId::new();
        let path = format!("api/v2/devices/{device}/live-activities/{item}");
        for bad in [json!({}), json!({ "update_token": "nope" })] {
            let (status, body) = put(&rig, &path, &bad).await;
            assert_eq!(status, 400, "{bad}: {body}");
            assert_eq!(body["error"]["field"], "update_token", "{body}");
        }
    })
    .await;
}

/// Every mutating v2 route has to say `Content-Type: application/json` (PROTOCOL §1.2, the CSRF
/// gate of DESIGN §16.6). The two `PUT`s are no exception.
#[tokio::test]
async fn a_put_without_the_json_content_type_is_a_400() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let response = rig
            .http
            .put(rig.url(&format!("api/v2/devices/{}", token("ab"))))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(registration(None).to_string())
            .send()
            .await
            .unwrap();
        let (status, body) = status_and_body(response).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "Content-Type");
    })
    .await;
}

/// The routes are inside the auth layer, like every other `api/v2/*` route.
#[tokio::test]
async fn the_routes_are_behind_auth() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;
        let device = token("ab");
        let item = ItemId::new();
        let paths = [
            format!("api/v2/devices/{device}"),
            format!("api/v2/devices/{device}/live-activities/{item}"),
        ];

        for path in &paths {
            let (status, body) = put(&rig, path, &registration(None)).await;
            assert_eq!(status, 401, "{path}: {body}");
            assert_eq!(body["error"]["code"], "unauthorized");
            let (status, body) = rig.delete(path).await;
            assert_eq!(status, 401, "{path}: {body}");
            assert_eq!(body["error"]["code"], "unauthorized");
        }

        // With the token, the same calls work.
        let response = rig
            .http
            .put(rig.url(&paths[0]))
            .bearer_auth("s3cret")
            .json(&registration(None))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 204);
        assert_eq!(DeviceStore::devices(&rig.store).await.unwrap().len(), 1);
    })
    .await;
}

/// Deleting the item takes its Live Activity registration with it, so the notifier stops pushing
/// to an activity whose download no longer exists (DESIGN §25).
#[tokio::test]
async fn deleting_an_item_forgets_its_live_activity() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let device = token("ab");
        put(
            &rig,
            &format!("api/v2/devices/{device}"),
            &registration(None),
        )
        .await;

        let id = rig.add("https://fake.test/one").await;
        let item: ItemId = id.parse().unwrap();
        let (status, _) = put(
            &rig,
            &format!("api/v2/devices/{device}/live-activities/{id}"),
            &json!({ "update_token": token("cd") }),
        )
        .await;
        assert_eq!(status, 204);
        assert_eq!(rig.store.live_activities_for(item).await.unwrap().len(), 1);

        let (status, body) = rig.delete(&format!("api/v2/items/{id}")).await;
        assert_eq!(status, 204, "{body}");
        rig.settle().await;
        assert!(
            rig.store
                .live_activities_for(item)
                .await
                .unwrap()
                .is_empty(),
            "the registration goes with the item"
        );
        assert_eq!(
            DeviceStore::devices(&rig.store).await.unwrap().len(),
            1,
            "the device itself stays registered"
        );
    })
    .await;
}
