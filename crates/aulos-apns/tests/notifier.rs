//! The event handling: who gets pushed, when, and what the store is told afterwards
//! (DESIGN §25.2).
//!
//! Everything runs against a `HashMap` behind the `DeviceStore` port and a `wiremock` gateway, so
//! this file is the demonstration that the notifier needs neither SQLite nor an engine.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_apns::{ApnsClient, ApnsEnvironment, ApnsNotifier, DeviceStore, Notifier};
use aulos_core::clock::Clock;
use aulos_core::config::{RawEnv, load};
use aulos_core::event::{DomainEvent, RemoveReason};
use aulos_core::health::ComponentStatus;
use aulos_core::id::ItemId;
use aulos_core::status::Status;
use common::{
    Call, FakeDeviceStore, ItemBuilder, TEST_BUNDLE_ID, TEST_KEY_ID, TEST_KEY_P8, TEST_TEAM_ID,
    activity, clock, device,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The throttle window the rig runs with. Short enough that a trailing-edge assertion costs
/// milliseconds, long enough that a burst of ten events lands inside one window on a loaded
/// machine.
const WINDOW: Duration = Duration::from_millis(300);

struct Rig {
    server: MockServer,
    store: Arc<FakeDeviceStore>,
    notifier: ApnsNotifier,
}

impl Rig {
    async fn new() -> Self {
        Self::with_status(200, json!({})).await
    }

    async fn with_status(status: u16, body: Value) -> Self {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let store = FakeDeviceStore::new();
        let clock_dyn: Arc<dyn Clock> = clock();
        let client = ApnsClient::new(
            TEST_KEY_P8.as_bytes(),
            TEST_KEY_ID,
            TEST_TEAM_ID,
            Arc::clone(&clock_dyn),
        )
        .expect("client")
        .with_base_url(&server.uri())
        .expect("base url")
        .with_backoff(vec![Duration::from_millis(1)]);
        let notifier = ApnsNotifier::with_client(
            client,
            TEST_BUNDLE_ID,
            Arc::clone(&store) as Arc<dyn DeviceStore>,
            clock_dyn,
        )
        .with_update_interval(WINDOW);
        Self {
            server,
            store,
            notifier,
        }
    }

    /// Every request the gateway saw, as `(path, apns-push-type, body)`.
    async fn requests(&self) -> Vec<(String, String, Value)> {
        self.server
            .received_requests()
            .await
            .expect("requests")
            .iter()
            .map(|r| {
                (
                    r.url.path().to_owned(),
                    r.headers
                        .get("apns-push-type")
                        .map(|v| v.to_str().unwrap_or_default().to_owned())
                        .unwrap_or_default(),
                    serde_json::from_slice(&r.body).unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    async fn bodies(&self) -> Vec<Value> {
        self.requests()
            .await
            .into_iter()
            .map(|(_, _, b)| b)
            .collect()
    }

    async fn tokens(&self) -> Vec<String> {
        self.requests()
            .await
            .into_iter()
            .map(|(p, _, _)| p.trim_start_matches("/3/device/").to_owned())
            .collect()
    }
}

fn changed(from: Status, to: Status, item: &ItemBuilder) -> DomainEvent {
    DomainEvent::StatusChanged {
        id: item.item_id(),
        from,
        to,
        view: item.view(),
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn a_disabled_notifier_is_none_and_never_touches_the_key_file() {
    let cfg = load(&RawEnv::from_pairs([
        ("STATE_DIR", "/tmp"),
        ("DOWNLOAD_DIR", "/tmp"),
        // Deliberately nonsense: with APNS_ENABLED=false nothing may look at it.
        ("APNS_KEY_FILE", "/nope/missing.p8"),
    ]))
    .expect("config");
    assert!(!cfg.apns_enabled);
    let store = FakeDeviceStore::new() as Arc<dyn DeviceStore>;
    let clock: Arc<dyn Clock> = clock();
    assert!(
        ApnsNotifier::new(&cfg, store, clock)
            .expect("no error")
            .is_none()
    );
    assert_eq!(
        aulos_apns::ApnsHealth::disabled().status,
        ComponentStatus::Disabled
    );
}

#[test]
fn an_unreadable_key_file_is_an_error_the_binary_can_degrade_on() {
    let cfg = load(&RawEnv::from_pairs([
        ("STATE_DIR", "/tmp"),
        ("DOWNLOAD_DIR", "/tmp"),
        ("APNS_ENABLED", "true"),
        ("APNS_KEY_FILE", "/nope/missing.p8"),
        ("APNS_KEY_ID", TEST_KEY_ID),
        ("APNS_TEAM_ID", TEST_TEAM_ID),
    ]))
    .expect("config");
    let store = FakeDeviceStore::new() as Arc<dyn DeviceStore>;
    let clock: Arc<dyn Clock> = clock();
    let err =
        ApnsNotifier::new(&cfg, store, clock).expect_err("a missing key file must be an error");
    let health = aulos_apns::ApnsHealth::misconfigured(&err.to_string());
    assert_eq!(health.status, ComponentStatus::Degraded);
    assert!(health.last_error.unwrap().contains("missing.p8"));
}

#[tokio::test]
async fn the_config_path_builds_a_working_notifier_and_honours_the_base_url_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let key = dir.path().join("AuthKey.p8");
    std::fs::write(&key, TEST_KEY_P8).expect("write key");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let cfg = load(&RawEnv::from_pairs([
        ("STATE_DIR".to_owned(), "/tmp".to_owned()),
        ("DOWNLOAD_DIR".to_owned(), "/tmp".to_owned()),
        ("APNS_ENABLED".to_owned(), "true".to_owned()),
        (
            "APNS_KEY_FILE".to_owned(),
            key.to_string_lossy().into_owned(),
        ),
        ("APNS_KEY_ID".to_owned(), TEST_KEY_ID.to_owned()),
        ("APNS_TEAM_ID".to_owned(), TEST_TEAM_ID.to_owned()),
        ("APNS_BASE_URL_OVERRIDE".to_owned(), server.uri()),
    ]))
    .expect("config");
    assert_eq!(&*cfg.apns_topic, TEST_BUNDLE_ID, "the documented default");

    let store = FakeDeviceStore::new();
    store.add_device(device("aa11", ApnsEnvironment::Sandbox));
    let clock: Arc<dyn Clock> = clock();
    let notifier = ApnsNotifier::new(&cfg, Arc::clone(&store) as Arc<dyn DeviceStore>, clock)
        .expect("no error")
        .expect("enabled");

    let item = ItemBuilder::new("x").status(Status::Finished);
    notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    notifier.quiesce().await;

    assert_eq!(server.received_requests().await.expect("r").len(), 1);
    assert_eq!(notifier.health_handle().health().await.devices, 1);
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_finished_top_level_item_alerts_every_device_that_wants_alerts() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    rig.store
        .add_device(device("bb22", ApnsEnvironment::Production));
    let mut quiet = device("cc33", ApnsEnvironment::Sandbox);
    quiet.alerts = false;
    rig.store.add_device(quiet);

    let item = ItemBuilder::new("Big Buck Bunny").status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    let mut tokens = rig.tokens().await;
    tokens.sort();
    assert_eq!(tokens, ["aa11", "bb22"], "the opted-out device is skipped");
    for body in rig.bodies().await {
        assert_eq!(body["aps"]["alert"]["title"], json!("Download finished"));
        assert_eq!(body["item_id"], json!(item.item_id().to_string()));
    }
    assert_eq!(rig.notifier.health_handle().counters().sent_total, 2);
}

#[tokio::test]
async fn a_child_of_a_group_is_silent_and_the_group_carries_the_count() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let group_id = ItemId::new();

    let child = ItemBuilder::new("Episode 1")
        .status(Status::Finished)
        .child_of(group_id);
    rig.notifier
        .on_event(&DomainEvent::Completed(child.view()))
        .await;
    rig.notifier.quiesce().await;
    assert!(
        rig.bodies().await.is_empty(),
        "a 40-episode season must not ring the phone 41 times"
    );

    let group = ItemBuilder::new("Season 1")
        .id(group_id)
        .status(Status::Finished)
        .group(9, 12);
    rig.notifier
        .on_event(&DomainEvent::Completed(group.view()))
        .await;
    rig.notifier.quiesce().await;

    let bodies = rig.bodies().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0]["aps"]["alert"]["body"],
        json!("Season 1 — 9 of 12 done")
    );
}

#[tokio::test]
async fn a_cancelled_item_produces_no_alert() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Canceled);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;
    assert!(rig.bodies().await.is_empty());
}

// ---------------------------------------------------------------------------
// Live Activities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn leaving_the_queue_starts_one_activity_per_start_token_and_only_once() {
    let rig = Rig::new().await;
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.live_activity_start_token = Some("start-aa11".into());
    rig.store.add_device(d);
    // A device without a start token gets nothing.
    rig.store
        .add_device(device("bb22", ApnsEnvironment::Sandbox));

    let item = ItemBuilder::new("Big Buck Bunny").status(Status::Downloading);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "/3/device/start-aa11");
    assert_eq!(reqs[0].1, "liveactivity");
    assert_eq!(reqs[0].2["aps"]["event"], json!("start"));
    assert_eq!(
        reqs[0].2["aps"]["attributes-type"],
        json!("AulosDownloadAttributes")
    );

    // Pause and resume: still one start.
    let paused = item.clone().status(Status::Queued);
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Queued, &paused))
        .await;
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    assert_eq!(
        rig.requests().await.len(),
        1,
        "a resume is not a second start"
    );
}

#[tokio::test]
async fn a_child_of_a_group_never_starts_an_activity() {
    let rig = Rig::new().await;
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.live_activity_start_token = Some("start-aa11".into());
    rig.store.add_device(d);

    let child = ItemBuilder::new("Episode 1")
        .status(Status::Downloading)
        .child_of(ItemId::new());
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &child))
        .await;
    rig.notifier.quiesce().await;
    assert!(rig.requests().await.is_empty());
}

#[tokio::test]
async fn an_update_goes_to_every_registered_activity_with_the_live_activity_topic() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Downloading).progress(
        42.5,
        Some(2_100_000.0),
        Some(68),
        Some(123),
        Some(456),
    );
    rig.store.add_activity(activity(
        "aa11",
        item.item_id(),
        "act-1",
        ApnsEnvironment::Sandbox,
    ));

    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "/3/device/act-1");
    assert_eq!(reqs[0].2["aps"]["event"], json!("update"));
    assert_eq!(reqs[0].2["aps"]["content-state"]["percent"], json!(42.5));

    let topic = rig.server.received_requests().await.expect("r")[0]
        .headers
        .get("apns-topic")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(topic, "com.tatoalo.aulos.push-type.liveactivity");
    assert_eq!(rig.notifier.health_handle().counters().live_activities, 1);
}

#[tokio::test]
async fn updates_are_throttled_and_the_last_state_still_arrives() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    // Ten progress frames inside one throttle window.
    for i in 1..=10 {
        let item = base
            .clone()
            .progress(f64::from(i) * 5.0, None, None, None, None);
        rig.notifier
            .on_event(&changed(Status::Downloading, Status::Downloading, &item))
            .await;
    }
    rig.notifier.quiesce().await;

    let bodies = rig.bodies().await;
    assert_eq!(bodies.len(), 2, "the leading frame plus the trailing edge");
    assert_eq!(bodies[0]["aps"]["content-state"]["percent"], json!(5.0));
    assert_eq!(
        bodies[1]["aps"]["content-state"]["percent"],
        json!(50.0),
        "the LAST state is always delivered"
    );
    assert_eq!(
        aulos_apns::UPDATE_INTERVAL,
        Duration::from_secs(2),
        "the contract's floor; the rig shortens it only for the test"
    );
}

#[tokio::test]
async fn a_status_change_defeats_the_throttle() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    rig.notifier
        .on_event(&changed(Status::Preparing, Status::Downloading, &base))
        .await;
    let pp = base.clone().status(Status::Postprocessing);
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Postprocessing, &pp))
        .await;
    rig.notifier.quiesce().await;

    let bodies = rig.bodies().await;
    assert_eq!(
        bodies.len(),
        2,
        "no waiting when the status word itself moved"
    );
    assert_eq!(
        bodies[1]["aps"]["content-state"]["status"],
        json!("postprocessing")
    );
}

#[tokio::test]
async fn completing_an_item_ends_its_activities_and_clears_the_registrations() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Finished);
    let id = item.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    let end = reqs
        .iter()
        .find(|(p, _, _)| p == "/3/device/act-1")
        .expect("an end push");
    assert_eq!(end.2["aps"]["event"], json!("end"));
    assert_eq!(
        end.2["aps"]["dismissal-date"],
        json!(end.2["aps"]["timestamp"].as_i64().unwrap() + 900)
    );
    assert!(
        reqs.iter().any(|(p, _, _)| p == "/3/device/aa11"),
        "and the alert"
    );

    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivitiesFor(id)),
        "the registrations are cleared after the end push"
    );
    assert!(rig.store.activity_tokens().is_empty());
}

#[tokio::test]
async fn a_cancelled_item_still_ends_its_activity() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Canceled);
    rig.store.add_activity(activity(
        "aa11",
        item.item_id(),
        "act-1",
        ApnsEnvironment::Sandbox,
    ));

    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1, "the end push, and no alert");
    assert_eq!(reqs[0].0, "/3/device/act-1");
    assert_eq!(reqs[0].2["aps"]["event"], json!("end"));
}

// ---------------------------------------------------------------------------
// Pruning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_410_on_a_device_token_removes_the_device() {
    let rig = Rig::with_status(410, json!({ "reason": "Unregistered" })).await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let item = ItemBuilder::new("x").status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveDevice("aa11".into()))
    );
    assert!(rig.store.device_tokens().is_empty());
    let health = rig.notifier.health_handle().counters();
    assert_eq!(health.pruned_tokens_total, 1);
    assert_eq!(health.failed_total, 1);
    assert_eq!(health.last_error.as_deref(), Some("410 Unregistered"));
}

#[tokio::test]
async fn a_400_bad_device_token_on_an_update_removes_only_that_activity() {
    let rig = Rig::with_status(400, json!({ "reason": "BadDeviceToken" })).await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Downloading);
    let id = item.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivity("aa11".into(), id))
    );
    assert_eq!(
        rig.store.device_tokens(),
        vec![Box::<str>::from("aa11")],
        "the device itself survives a dead ACTIVITY token"
    );
}

#[tokio::test]
async fn a_403_that_survives_the_remint_degrades_healthz() {
    let rig = Rig::with_status(403, json!({ "reason": "InvalidProviderToken" })).await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let item = ItemBuilder::new("x").status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    let health = rig.notifier.health_handle().health().await;
    assert_eq!(health.status, ComponentStatus::Degraded);
    assert_eq!(health.devices, 1);
    assert_eq!(health.pruned_tokens_total, 0);
    assert!(
        health
            .last_error
            .as_deref()
            .unwrap()
            .contains("InvalidProviderToken")
    );
    // And it is the shape `healthz` publishes.
    let detail = health.component();
    assert_eq!(detail.detail["devices"], json!(1));
    for key in [
        "devices",
        "live_activities",
        "sent_total",
        "failed_total",
        "pruned_tokens_total",
        "last_error",
        "last_sent_at",
    ] {
        assert!(detail.detail.contains_key(key), "{key} must be present");
    }
}

// ---------------------------------------------------------------------------
// Housekeeping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn removing_an_item_forgets_that_it_was_started() {
    let rig = Rig::new().await;
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.live_activity_start_token = Some("start-aa11".into());
    rig.store.add_device(d);

    let item = ItemBuilder::new("x").status(Status::Downloading);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    assert_eq!(rig.requests().await.len(), 1);

    rig.notifier
        .on_event(&DomainEvent::Removed {
            ids: vec![item.item_id()],
            reason: RemoveReason::Deleted,
        })
        .await;
    // A retry of the same id may start a fresh activity.
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    assert_eq!(rig.requests().await.len(), 2);
}

#[tokio::test]
async fn an_item_nobody_registered_costs_one_store_read_per_throttle_window() {
    let rig = Rig::new().await;
    let item = ItemBuilder::new("x").status(Status::Downloading);
    for _ in 0..5 {
        rig.notifier
            .on_event(&changed(Status::Downloading, Status::Downloading, &item))
            .await;
    }
    rig.notifier.quiesce().await;

    let reads = rig
        .store
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::LiveActivitiesFor(_)))
        .count();
    assert_eq!(reads, 1, "the cache absorbs the progress storm");
    assert!(rig.requests().await.is_empty());

    // ...and it does expire.
    tokio::time::sleep(WINDOW + Duration::from_millis(20)).await;
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    let reads = rig
        .store
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::LiveActivitiesFor(_)))
        .count();
    assert_eq!(reads, 2);
}

#[tokio::test]
async fn shutdown_drains_the_task_set() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.shutdown().await;
    assert_eq!(rig.notifier.in_flight(), 0);
}

#[tokio::test]
async fn it_is_interested_in_every_item() {
    let rig = Rig::new().await;
    let item = ItemBuilder::new("x");
    assert!(rig.notifier.interested(&item.view()));
    assert_eq!(rig.notifier.id(), "apns");
}
