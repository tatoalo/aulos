//! The event handling: who gets pushed, when, and what the store is told afterwards
//! (DESIGN §25.2).
//!
//! Everything runs against a `HashMap` behind the `DeviceStore` port and a `wiremock` gateway, so
//! this file is the demonstration that the notifier needs neither SQLite nor an engine.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_apns::{
    ApnsClient, ApnsEnvironment, ApnsNotifier, DeviceStore, Notifier, ProgressReader,
};
use aulos_core::clock::Clock;
use aulos_core::config::{RawEnv, load};
use aulos_core::event::{DomainEvent, RemoveReason};
use aulos_core::health::ComponentStatus;
use aulos_core::id::ItemId;
use aulos_core::source::SourceKind;
use aulos_core::status::Status;
use common::{
    Call, FakeDeviceStore, FakeProgress, ItemBuilder, TEST_BUNDLE_ID, TEST_KEY_ID, TEST_KEY_P8,
    TEST_TEAM_ID, activity, clock, device, device_of_install,
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

/// The progress cadence the rig runs with — the same ratio to [`WINDOW`] that the shipped
/// `PROGRESS_INTERVAL` has to `UPDATE_INTERVAL`, so a cadence assertion is not also a throttle
/// assertion.
const CADENCE: Duration = Duration::from_millis(750);

struct Rig {
    server: MockServer,
    store: Arc<FakeDeviceStore>,
    /// The published snapshot the notifier pulls live numbers from. Only wired into the notifier
    /// by [`Rig::pulling_progress`]; the other rigs leave the notifier on the event's own view.
    progress: Arc<FakeProgress>,
    notifier: ApnsNotifier,
}

impl Rig {
    async fn new() -> Self {
        Self::with_status(200, json!({})).await
    }

    async fn with_status(status: u16, body: Value) -> Self {
        Self::build(status, body, false).await
    }

    /// A rig with `APNS_PUSH_ALL=true`: every item pushes, whoever added it.
    async fn pushing_everything() -> Self {
        Self::build(200, json!({}), true).await
    }

    /// A rig whose notifier reads live progress through the port (DESIGN §15.1, §25.4).
    async fn pulling_progress() -> Self {
        Self::assemble(200, json!({}), false, true).await
    }

    async fn build(status: u16, body: Value, push_all: bool) -> Self {
        Self::assemble(status, body, push_all, false).await
    }

    async fn assemble(status: u16, body: Value, push_all: bool, pull: bool) -> Self {
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
        let progress = FakeProgress::new();
        let mut notifier = ApnsNotifier::with_client(
            client,
            TEST_BUNDLE_ID,
            Arc::clone(&store) as Arc<dyn DeviceStore>,
            clock_dyn,
            push_all,
        )
        .with_update_interval(WINDOW)
        .with_progress_interval(CADENCE);
        if pull {
            notifier = notifier.with_progress(Arc::clone(&progress) as Arc<dyn ProgressReader>);
        }
        Self {
            server,
            store,
            progress,
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
        "retried_total",
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
async fn a_second_activity_registered_mid_download_does_not_push_for_ever() {
    // A household with a phone and an iPad, or one device after iOS rotated its update token.
    // The two registrations' throttle windows are then offset by less than one interval, and a
    // trailing edge that decides "up to date" from a timestamp alone can never converge: each
    // pass re-stamps whichever record it just sent, pushing it back inside the other's window, so
    // the timer re-delivers the *same* content-state at N pushes per interval, for ever.
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    rig.store
        .add_device(device("bb22", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    let frame = |percent: f64| base.clone().progress(percent, None, None, None, None);

    // t = 0: the phone alone, and the leading frame goes straight out.
    rig.notifier
        .on_event(&changed(
            Status::Downloading,
            Status::Downloading,
            &frame(10.0),
        ))
        .await;
    rig.notifier.quiesce().await;

    // Half a window in: throttled, so the trailing-edge timer takes it at t = WINDOW. That is what
    // leaves "act-1" stamped mid-window rather than at the registration-cache boundary.
    tokio::time::sleep(WINDOW / 2).await;
    rig.notifier
        .on_event(&changed(
            Status::Downloading,
            Status::Downloading,
            &frame(20.0),
        ))
        .await;
    rig.notifier.quiesce().await;

    // The iPad starts its own activity. The notifier sees it on the next cache miss — half a
    // window after the phone was last stamped, which is the offset that used to be fatal.
    rig.store
        .add_activity(activity("bb22", id, "act-2", ApnsEnvironment::Sandbox));
    tokio::time::sleep(WINDOW / 2).await;
    rig.notifier
        .on_event(&changed(
            Status::Downloading,
            Status::Downloading,
            &frame(30.0),
        ))
        .await;

    // Let every trailing edge play out — deliberately without `quiesce`, which would simply hang
    // for ever on the bug this pins.
    tokio::time::sleep(WINDOW * 4).await;
    let settled = rig.requests().await.len();
    assert_eq!(
        rig.notifier.in_flight(),
        0,
        "the trailing-edge timer must finish, not spin"
    );

    tokio::time::sleep(WINDOW * 4).await;
    assert_eq!(
        rig.requests().await.len(),
        settled,
        "there is nothing new to deliver, so nothing more may be sent"
    );

    let to_ipad = rig
        .requests()
        .await
        .into_iter()
        .filter(|(p, _, _)| p == "/3/device/act-2")
        .count();
    assert_eq!(
        to_ipad, 1,
        "the iPad gets the state once, not once per window"
    );
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn a_start_lost_to_an_unreadable_device_table_is_retried() {
    // The latch used to be taken in `on_event`, before it was known that a start would be
    // attempted at all — so a store hiccup, or a full task set, burned the item's one chance and
    // the Live Activity simply never appeared for that download.
    let rig = Rig::new().await;
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.live_activity_start_token = Some("start-aa11".into());
    rig.store.add_device(d);
    rig.store.fail_devices(true);

    let item = ItemBuilder::new("x").status(Status::Downloading);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    assert!(
        rig.requests().await.is_empty(),
        "nothing could be read, so nothing was sent"
    );

    rig.store.fail_devices(false);
    let paused = item.clone().status(Status::Queued);
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Queued, &paused))
        .await;
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1, "the next start edge retries");
    assert_eq!(reqs[0].0, "/3/device/start-aa11");
    assert_eq!(reqs[0].2["aps"]["event"], json!("start"));
}

#[tokio::test]
async fn a_start_token_registered_late_still_gets_an_activity() {
    // Nobody could receive a start, so the item must not be latched as started.
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let item = ItemBuilder::new("x").status(Status::Downloading);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    assert!(rig.requests().await.is_empty());

    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.live_activity_start_token = Some("start-aa11".into());
    rig.store.add_device(d);

    let paused = item.clone().status(Status::Queued);
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Queued, &paused))
        .await;
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "/3/device/start-aa11");
}

#[tokio::test]
async fn a_store_hiccup_at_completion_still_ends_the_activity() {
    // DESIGN §25.2 makes the end unconditional: a cancelled or finished download must never leave
    // a progress ring spinning on the lock screen. A failed read used to be indistinguishable from
    // "this item had no registrations", and nothing retries a `Completed`.
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let item = ItemBuilder::new("x").status(Status::Downloading);
    let id = item.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    // One update, so the notifier has the registration cached.
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    let before = rig.requests().await.len();
    assert_eq!(before, 1);

    rig.store.fail_activities(true);
    let done = item.clone().status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(done.view()))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    let end = reqs[before..]
        .iter()
        .find(|(p, _, _)| p == "/3/device/act-1")
        .expect("an end push, sent from the cached registrations");
    assert_eq!(end.2["aps"]["event"], json!("end"));
    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivitiesFor(id)),
        "and the rows are still swept"
    );
    assert!(rig.store.activity_tokens().is_empty());
}

// ---------------------------------------------------------------------------
// Who gets pushed at all (DESIGN §12.6, §25.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn it_is_interested_only_in_what_the_phone_added() {
    let rig = Rig::new().await;
    assert_eq!(rig.notifier.id(), "apns");
    assert!(
        rig.notifier
            .interested(&ItemBuilder::new("x").source(SourceKind::Ios).view())
    );
    for kind in [
        SourceKind::ApiV2,
        SourceKind::ApiV1,
        SourceKind::Telegram,
        SourceKind::Subscription,
        SourceKind::Restart,
        SourceKind::Retry,
    ] {
        assert!(
            !rig.notifier
                .interested(&ItemBuilder::new("x").source(kind).view()),
            "{kind} is somebody else's to report"
        );
    }

    // The escape hatch restores the old "every item" behaviour.
    let all = Rig::pushing_everything().await;
    for kind in SourceKind::ALL {
        assert!(
            all.notifier
                .interested(&ItemBuilder::new("x").source(kind).view()),
            "APNS_PUSH_ALL=true: {kind}"
        );
    }
}

/// The operator's rule, at the only place it is observable: a web add rings nobody's phone.
#[tokio::test]
async fn a_finished_api_item_sends_no_alert_while_an_ios_one_does() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let web = ItemBuilder::new("From the web")
        .source(SourceKind::ApiV2)
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(web.view()))
        .await;
    rig.notifier.quiesce().await;
    assert!(
        rig.requests().await.is_empty(),
        "Telegram or the web UI reported this one"
    );

    let phone = ItemBuilder::new("From the phone")
        .source(SourceKind::Ios)
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(phone.view()))
        .await;
    rig.notifier.quiesce().await;
    assert_eq!(rig.tokens().await, ["aa11"]);
}

/// `APNS_PUSH_ALL=true` is the escape hatch for an operator who drives the server from `curl` and
/// still wants the phone to ring.
#[tokio::test]
async fn push_all_restores_the_alert_for_a_non_ios_item() {
    let rig = Rig::pushing_everything().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let tg = ItemBuilder::new("From the bot")
        .source(SourceKind::Telegram)
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(tg.view()))
        .await;
    rig.notifier.quiesce().await;

    assert_eq!(rig.tokens().await, ["aa11"]);
}

/// A non-iOS item starts no Live Activity — the push-to-start is a fan-out to *devices*, and that
/// is what the source gate is for.
#[tokio::test]
async fn a_non_ios_item_starts_no_live_activity() {
    let rig = Rig::new().await;
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.live_activity_start_token = Some("start-1".into());
    rig.store.add_device(d);

    let web = ItemBuilder::new("From the web").source(SourceKind::ApiV2);

    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &web))
        .await;
    rig.notifier.quiesce().await;

    assert!(rig.requests().await.is_empty());
}

/// ...but an activity the app *did* register is updated whatever the item's origin: the
/// registration is the permission, and refusing to push it is a ring frozen at its first frame
/// (DESIGN §25.2). The app adopts a running activity for an item it did not add on launch
/// reconciliation, and the knob can be flipped while one is live.
#[tokio::test]
async fn a_registered_activity_is_updated_even_for_an_item_the_gate_would_silence() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let web = ItemBuilder::new("From the web").source(SourceKind::ApiV2);
    rig.store.add_activity(activity(
        "aa11",
        web.item_id(),
        "act-1",
        ApnsEnvironment::Sandbox,
    ));

    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &web))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1, "the update, and no start: {reqs:?}");
    assert_eq!(reqs[0].0, "/3/device/act-1");
    assert_eq!(reqs[0].1, "liveactivity");
    assert_eq!(reqs[0].2["aps"]["event"], json!("update"));
}

/// A Live Activity is ended even for an item the gate would otherwise silence: the knob can be
/// flipped while one is live, and nothing else ever closes a progress ring (DESIGN §25.2).
#[tokio::test]
async fn a_live_activity_is_still_ended_for_an_item_that_no_longer_pushes() {
    let rig = Rig::new().await;
    // Alerts off, so this test is about the end alone.
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.alerts = false;
    rig.store.add_device(d);
    let web = ItemBuilder::new("From the web").source(SourceKind::ApiV2);
    let id = web.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    let done = web.clone().status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(done.view()))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1, "the end, and only the end: {reqs:?}");
    assert_eq!(reqs[0].0, "/3/device/act-1");
    assert_eq!(reqs[0].2["aps"]["event"], json!("end"));
    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivitiesFor(id)),
        "and the rows are swept"
    );
}

/// An item the phone was holding an activity for is the phone's to announce, whatever `source`
/// says: `source` is written once at the add and never rewritten (DESIGN §4.4), so the
/// registration is the only thing that knows the phone is watching this download.
#[tokio::test]
async fn a_tracked_item_alerts_even_when_its_source_is_not_ios() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let tg = ItemBuilder::new("From the bot").source(SourceKind::Telegram);
    rig.store.add_activity(activity(
        "aa11",
        tg.item_id(),
        "act-1",
        ApnsEnvironment::Sandbox,
    ));

    let done = tg.clone().status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(done.view()))
        .await;
    rig.notifier.quiesce().await;

    let mut tokens = rig.tokens().await;
    tokens.sort();
    assert_eq!(tokens, ["aa11", "act-1"], "the end *and* the alert");
}

/// The other half of the same rule: an untracked non-iOS item still rings nobody.
#[tokio::test]
async fn an_untracked_non_ios_item_still_alerts_nobody() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let tg = ItemBuilder::new("From the bot")
        .source(SourceKind::Telegram)
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(tg.view()))
        .await;
    rig.notifier.quiesce().await;

    assert!(
        rig.requests().await.is_empty(),
        "Telegram reported this one"
    );
}

// ---------------------------------------------------------------------------
// Which device, not just whether (DESIGN §25.2, decision 42)
// ---------------------------------------------------------------------------

/// The operator's rule one turn finer: a download started on the phone alerts the phone, and the
/// iPad in the next room stays quiet.
///
/// `X-Aulos-Install` puts the adding install in `source.ref` (PROTOCOL §1.3) and the same value is
/// registered as the device's `install_id` (§4.8); the alert goes where the two agree.
#[tokio::test]
async fn an_alert_goes_only_to_the_install_that_added_the_item() {
    let rig = Rig::new().await;
    rig.store.add_device(device_of_install(
        "aa11",
        "install-phone",
        ApnsEnvironment::Sandbox,
    ));
    rig.store.add_device(device_of_install(
        "bb22",
        "install-ipad",
        ApnsEnvironment::Sandbox,
    ));

    let item = ItemBuilder::new("Big Buck Bunny")
        .added_by_install("install-phone")
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    assert_eq!(rig.tokens().await, ["aa11"], "the iPad added nothing");
}

/// An iOS item with no install — an app build that predates `X-Aulos-Install` — is every alerting
/// device, which is exactly the fan-out that shipped before the key existed.
#[tokio::test]
async fn an_ios_item_without_an_install_alerts_every_device() {
    let rig = Rig::new().await;
    rig.store.add_device(device_of_install(
        "aa11",
        "install-phone",
        ApnsEnvironment::Sandbox,
    ));
    rig.store.add_device(device_of_install(
        "bb22",
        "install-ipad",
        ApnsEnvironment::Sandbox,
    ));
    // And a registration from a build that reports no install at all.
    rig.store
        .add_device(device("cc33", ApnsEnvironment::Sandbox));

    let item = ItemBuilder::new("Big Buck Bunny")
        .source(SourceKind::Ios)
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    let mut tokens = rig.tokens().await;
    tokens.sort();
    assert_eq!(tokens, ["aa11", "bb22", "cc33"]);
}

/// The mirror: once an item names its install, a registration that names *none* is some other
/// install and hears nothing. Sending the header without the field is the one way to go silent.
#[tokio::test]
async fn a_device_with_no_install_is_not_the_install_that_added_the_item() {
    let rig = Rig::new().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));

    let item = ItemBuilder::new("Big Buck Bunny")
        .added_by_install("install-phone")
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    assert!(rig.requests().await.is_empty(), "a different install");
}

/// `APNS_PUSH_ALL=true` is "every device", and that has to outrank the install key too — it is the
/// operator's escape hatch, not a second filter.
#[tokio::test]
async fn push_all_ignores_the_install_key() {
    let rig = Rig::pushing_everything().await;
    rig.store.add_device(device_of_install(
        "aa11",
        "install-phone",
        ApnsEnvironment::Sandbox,
    ));
    rig.store.add_device(device_of_install(
        "bb22",
        "install-ipad",
        ApnsEnvironment::Sandbox,
    ));

    let item = ItemBuilder::new("Big Buck Bunny")
        .added_by_install("install-phone")
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(item.view()))
        .await;
    rig.notifier.quiesce().await;

    let mut tokens = rig.tokens().await;
    tokens.sort();
    assert_eq!(tokens, ["aa11", "bb22"]);
}

/// A Telegram item alerts nobody whatever any install says — and when the phone *was* tracking it,
/// the alert-if-tracked rule still fires for every alerting device, because `ref` is then a chat
/// id no registration can match. That rule is unchanged by the install key.
#[tokio::test]
async fn a_telegram_item_is_unaffected_by_the_install_key() {
    let rig = Rig::new().await;
    rig.store.add_device(device_of_install(
        "aa11",
        "install-phone",
        ApnsEnvironment::Sandbox,
    ));
    rig.store.add_device(device_of_install(
        "bb22",
        "install-ipad",
        ApnsEnvironment::Sandbox,
    ));

    // Untracked: nobody.
    let untracked = ItemBuilder::new("From the bot")
        .source(SourceKind::Telegram)
        .status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(untracked.view()))
        .await;
    rig.notifier.quiesce().await;
    assert!(rig.requests().await.is_empty(), "Telegram reported it");

    // Tracked by the phone: the end push, plus the alert to every alerting device.
    let tracked = ItemBuilder::new("From the bot").source(SourceKind::Telegram);
    rig.store.add_activity(activity(
        "aa11",
        tracked.item_id(),
        "act-1",
        ApnsEnvironment::Sandbox,
    ));
    rig.notifier
        .on_event(&DomainEvent::Completed(
            tracked.clone().status(Status::Finished).view(),
        ))
        .await;
    rig.notifier.quiesce().await;

    let mut tokens = rig.tokens().await;
    tokens.sort();
    assert_eq!(tokens, ["aa11", "act-1", "bb22"]);
}

/// The push-to-start follows the same key: only the install that added the download gets a Live
/// Activity.
#[tokio::test]
async fn a_live_activity_starts_only_on_the_install_that_added_the_item() {
    let rig = Rig::new().await;
    let mut phone = device_of_install("aa11", "install-phone", ApnsEnvironment::Sandbox);
    phone.live_activity_start_token = Some("start-phone".into());
    rig.store.add_device(phone);
    let mut ipad = device_of_install("bb22", "install-ipad", ApnsEnvironment::Sandbox);
    ipad.live_activity_start_token = Some("start-ipad".into());
    rig.store.add_device(ipad);

    let item = ItemBuilder::new("Big Buck Bunny")
        .added_by_install("install-phone")
        .status(Status::Downloading);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1, "one start, on one install: {reqs:?}");
    assert_eq!(reqs[0].0, "/3/device/start-phone");
    assert_eq!(reqs[0].2["aps"]["event"], json!("start"));
}

/// And when the *only* device holding a start token is another install, nothing is sent **and the
/// latch stays open** — the item's one start must not be spent on nobody, so the phone registering
/// its push-to-start token thirty seconds into the download still gets an activity.
#[tokio::test]
async fn a_start_for_another_install_does_not_burn_the_items_one_start() {
    let rig = Rig::new().await;
    let mut ipad = device_of_install("bb22", "install-ipad", ApnsEnvironment::Sandbox);
    ipad.live_activity_start_token = Some("start-ipad".into());
    rig.store.add_device(ipad);

    let item = ItemBuilder::new("Big Buck Bunny")
        .added_by_install("install-phone")
        .status(Status::Downloading);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    assert!(rig.requests().await.is_empty(), "the iPad added nothing");

    // The phone registers late, and the next start edge finds it.
    let mut phone = device_of_install("aa11", "install-phone", ApnsEnvironment::Sandbox);
    phone.live_activity_start_token = Some("start-phone".into());
    rig.store.add_device(phone);
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].0, "/3/device/start-phone");
}

/// The update and the end never look at the install: they address a registration the app made for
/// that one item, and a ring the server refuses to close is a ring nothing else will (DESIGN
/// §25.2). This is the case the install key could most easily have broken — another install
/// adopting an activity for a download this phone started.
#[tokio::test]
async fn updates_and_ends_ignore_the_install_key() {
    let rig = Rig::new().await;
    let mut ipad = device_of_install("bb22", "install-ipad", ApnsEnvironment::Sandbox);
    ipad.alerts = false; // so this test is about the activity alone
    rig.store.add_device(ipad);

    let item = ItemBuilder::new("Big Buck Bunny").added_by_install("install-phone");
    let id = item.item_id();
    rig.store
        .add_activity(activity("bb22", id, "act-ipad", ApnsEnvironment::Sandbox));

    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    let reqs = rig.requests().await;
    assert_eq!(
        reqs.len(),
        1,
        "the update reached the other install: {reqs:?}"
    );
    assert_eq!(reqs[0].0, "/3/device/act-ipad");
    assert_eq!(reqs[0].2["aps"]["event"], json!("update"));

    rig.notifier
        .on_event(&DomainEvent::Completed(
            item.clone().status(Status::Finished).view(),
        ))
        .await;
    rig.notifier.quiesce().await;
    let reqs = rig.requests().await;
    assert_eq!(reqs.len(), 2, "and so did the end: {reqs:?}");
    assert_eq!(reqs[1].0, "/3/device/act-ipad");
    assert_eq!(reqs[1].2["aps"]["event"], json!("end"));
    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivitiesFor(id))
    );
}

/// The regression the ungated bookkeeping exists for: the terminal fallback is only useful if the
/// record cache was filled, and the cache is filled by the *update* path. Gating that path on the
/// source left the fallback permanently empty for exactly the non-iOS items the ungated end
/// protects — the mirror of `a_store_hiccup_at_completion_still_ends_the_activity`.
#[tokio::test]
async fn a_store_hiccup_at_completion_still_ends_a_non_ios_activity() {
    let rig = Rig::new().await;
    let mut d = device("aa11", ApnsEnvironment::Sandbox);
    d.alerts = false;
    rig.store.add_device(d);
    let item = ItemBuilder::new("From the web")
        .source(SourceKind::ApiV2)
        .status(Status::Downloading);
    let id = item.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    // One update, so the notifier has the registration cached.
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Downloading, &item))
        .await;
    rig.notifier.quiesce().await;
    let before = rig.requests().await.len();
    assert_eq!(before, 1);

    rig.store.fail_activities(true);
    let done = item.clone().status(Status::Finished);
    rig.notifier
        .on_event(&DomainEvent::Completed(done.view()))
        .await;
    rig.notifier.quiesce().await;

    let reqs = rig.requests().await;
    let end = reqs[before..]
        .iter()
        .find(|(p, _, _)| p == "/3/device/act-1")
        .expect("an end push, sent from the cached registrations");
    assert_eq!(end.2["aps"]["event"], json!("end"));
    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivitiesFor(id)),
        "and the rows are still swept"
    );
}

// ---------------------------------------------------------------------------
// Live progress, pulled from the published snapshot (DESIGN §15.1, §25.4)
// ---------------------------------------------------------------------------

/// The `content-state` of the update the gateway saw last.
fn last_state(bodies: &[Value]) -> &Value {
    &bodies.last().expect("at least one update")["aps"]["content-state"]
}

/// Long enough for one spawned push to reach the local gateway.
///
/// `quiesce` is not available to these tests: a progressing item with a live activity on it keeps
/// a trailing-edge timer armed on purpose, so the task set is never empty and waiting for it to be
/// would hang for ever — which is exactly the property the cadence tests assert.
async fn settle() {
    tokio::time::sleep(WINDOW / 2).await;
}

#[tokio::test]
async fn an_update_carries_the_published_percent_not_the_events_zero() {
    // The bug this pins: `Engine::view` builds every `StatusChanged` view with a `None` progress
    // cell, so `ItemView::percent` is 0.0 for every non-group item until it finishes. The island
    // read that view and sat at 0 % for the whole download.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    rig.progress.publish(
        base.clone()
            .progress(42.5, Some(2_100_000.0), Some(68), Some(123), Some(456))
            .view(),
    );

    // The event carries the engine's view: percent 0, every byte counter null.
    assert_eq!(base.view().percent, 0.0, "the event view really is empty");
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;
    settle().await;

    let bodies = rig.bodies().await;
    let state = last_state(&bodies);
    assert_eq!(state["percent"], json!(42.5));
    assert_eq!(state["speed"], json!(2_100_000.0));
    assert_eq!(state["eta"], json!(68));
    assert_eq!(state["downloadedBytes"], json!(123));
    assert_eq!(state["totalBytes"], json!(456));
    assert_eq!(
        bodies.last().expect("a body")["aps"]["relevance-score"],
        json!(0.425),
        "the score follows the pulled percent, not the event's"
    );
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn the_status_word_is_the_events_even_when_the_snapshot_lags_behind() {
    // The aggregator applies the same event asynchronously, so the snapshot can still say
    // `downloading` when the item has just entered `postprocessing`. Taking the snapshot's status
    // wholesale would push the stale word; only the numbers are pulled.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    rig.progress
        .publish(base.clone().progress(90.0, None, None, None, None).view());

    let pp = base.clone().status(Status::Postprocessing);
    rig.notifier
        .on_event(&changed(Status::Downloading, Status::Postprocessing, &pp))
        .await;
    settle().await;

    let bodies = rig.bodies().await;
    let state = last_state(&bodies);
    assert_eq!(state["status"], json!("postprocessing"), "the event's word");
    assert_eq!(state["percent"], json!(90.0), "the snapshot's numbers");
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn a_downloading_item_keeps_moving_with_no_events_at_all() {
    // The whole point of the cadence: a backgrounded app receives nothing but pushes, and the
    // engine publishes no event per progress frame. Without the pull the island freezes at the
    // number the last status change carried.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    let frame = |percent: f64| {
        base.clone()
            .progress(percent, None, None, None, None)
            .view()
    };

    rig.progress.publish(frame(10.0));
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;
    settle().await;
    assert_eq!(last_state(&rig.bodies().await)["percent"], json!(10.0));

    // Not one further event — only the snapshot moves.
    for percent in [25.0, 50.0] {
        rig.progress.publish(frame(percent));
        tokio::time::sleep(CADENCE + CADENCE / 3).await;
        assert_eq!(
            last_state(&rig.bodies().await)["percent"],
            json!(percent),
            "the trailing-edge timer re-read the snapshot"
        );
    }

    for body in rig.bodies().await {
        assert_eq!(body["aps"]["event"], json!("update"));
        assert!(
            body["aps"]["stale-date"].is_i64(),
            "every update tells the widget when to stop trusting it"
        );
    }
    assert_eq!(
        aulos_apns::PROGRESS_INTERVAL,
        Duration::from_secs(5),
        "the shipped cadence; the rig shortens it only for the test"
    );
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn a_stalled_download_costs_no_push_while_the_timer_keeps_watching() {
    // A download whose numbers stop moving must not cost a push every cadence: the pulled view is
    // compared against what the item already has pending, and an identical one is not a frame.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    rig.progress
        .publish(base.clone().progress(30.0, None, None, None, None).view());

    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;
    settle().await;
    let after_first = rig.requests().await.len();
    assert_eq!(after_first, 1);

    tokio::time::sleep(CADENCE * 3).await;
    assert_eq!(
        rig.requests().await.len(),
        after_first,
        "nothing changed, so nothing was sent"
    );
    let reads = rig.progress.reads();
    assert!(reads >= 3, "but it kept looking: {reads} reads");
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn a_registration_that_arrives_after_the_download_started_gets_updates_on_the_cadence() {
    // The bug this pins: the phone registers its activity a few seconds *after* the add, so the
    // `downloading` edge found no registration, sent nothing and armed no timer — and nothing
    // re-read the registrations until the next status change. The island sat on its first frame
    // for the whole download.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    let frame = |percent: f64| {
        base.clone()
            .progress(percent, None, None, None, None)
            .view()
    };

    rig.progress.publish(frame(10.0));
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;
    settle().await;
    assert!(
        rig.requests().await.is_empty(),
        "nothing is registered yet, so nothing may be pushed"
    );

    // The app finishes starting its activity and registers the update token.
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    rig.progress.publish(frame(25.0));

    // No further `StatusChanged`: the cadence alone has to notice.
    tokio::time::sleep(CADENCE + CADENCE / 3).await;
    let bodies = rig.bodies().await;
    assert!(
        !bodies.is_empty(),
        "the late registration was never noticed"
    );
    assert_eq!(last_state(&bodies)["percent"], json!(25.0));
    assert_eq!(
        bodies.last().expect("a body")["aps"]["event"],
        json!("update")
    );

    rig.progress.publish(frame(60.0));
    tokio::time::sleep(CADENCE + CADENCE / 3).await;
    assert_eq!(
        last_state(&rig.bodies().await)["percent"],
        json!(60.0),
        "and then it keeps to the cadence"
    );
    assert_eq!(
        rig.tokens().await.iter().filter(|t| *t == "act-1").count(),
        rig.requests().await.len(),
        "every push went to the registration, and only to it"
    );
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn an_unregistered_download_costs_no_push_while_the_cadence_watches_for_one() {
    // The other half of the late-registration fix: the timer is armed for every progressing item,
    // so it must stay silent for the items — every Telegram or web add — nobody is watching.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let frame = |percent: f64| {
        base.clone()
            .progress(percent, None, None, None, None)
            .view()
    };

    rig.progress.publish(frame(10.0));
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;
    for percent in [30.0, 70.0] {
        rig.progress.publish(frame(percent));
        tokio::time::sleep(CADENCE + CADENCE / 3).await;
    }
    assert!(
        rig.requests().await.is_empty(),
        "no registration, no push - however often the numbers move"
    );
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn leaving_the_progressing_statuses_ends_the_cadence() {
    // The cadence has to terminate on its own, not only when `Completed` forgets the item: a
    // paused download that kept a timer alive for ever would be one leaked task per pause.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    rig.progress
        .publish(base.clone().progress(60.0, None, None, None, None).view());
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;
    tokio::time::sleep(CADENCE / 3).await;
    assert_eq!(rig.notifier.in_flight(), 1, "the timer owns the item");

    // The user paused it: the snapshot says `queued`, which is not a progressing status.
    rig.progress.publish(
        base.clone()
            .status(Status::Queued)
            .progress(60.0, None, None, None, None)
            .view(),
    );
    tokio::time::sleep(CADENCE * 3).await;
    assert_eq!(
        rig.notifier.in_flight(),
        0,
        "the trailing-edge timer finished rather than spinning"
    );
    let settled = rig.requests().await.len();
    tokio::time::sleep(CADENCE * 2).await;
    assert_eq!(rig.requests().await.len(), settled, "and it stays stopped");
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn completing_an_item_stops_its_cadence_and_still_ends_the_activity() {
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let base = ItemBuilder::new("x").status(Status::Downloading);
    let id = base.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));
    rig.progress
        .publish(base.clone().progress(80.0, None, None, None, None).view());
    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &base))
        .await;

    let done = base.clone().status(Status::Finished);
    rig.progress.forget(id);
    rig.notifier
        .on_event(&DomainEvent::Completed(done.view()))
        .await;
    tokio::time::sleep(CADENCE * 3).await;

    assert_eq!(rig.notifier.in_flight(), 0);
    let events: Vec<Value> = rig
        .bodies()
        .await
        .into_iter()
        .map(|b| b["aps"]["event"].clone())
        .collect();
    assert!(events.contains(&json!("end")), "{events:?}");
    assert!(
        rig.store
            .calls()
            .contains(&Call::RemoveLiveActivitiesFor(id))
    );
    rig.notifier.shutdown().await;
}

#[tokio::test]
async fn a_group_keeps_the_engines_roll_up_when_the_snapshot_has_nothing() {
    // Groups already carry real numbers on the event (`Engine::view` overwrites them from the
    // `GroupAcc`), and the pull must never replace them with zeroes.
    let rig = Rig::pulling_progress().await;
    rig.store
        .add_device(device("aa11", ApnsEnvironment::Sandbox));
    let group = ItemBuilder::new("Season 1")
        .status(Status::Downloading)
        .group(3, 12)
        .progress(25.0, Some(1_000.0), Some(300), Some(10), None);
    let id = group.item_id();
    rig.store
        .add_activity(activity("aa11", id, "act-1", ApnsEnvironment::Sandbox));

    rig.notifier
        .on_event(&changed(Status::Queued, Status::Downloading, &group))
        .await;
    settle().await;

    let bodies = rig.bodies().await;
    assert_eq!(last_state(&bodies)["percent"], json!(25.0));
    assert_eq!(last_state(&bodies)["speed"], json!(1_000.0));
    rig.notifier.shutdown().await;
}
