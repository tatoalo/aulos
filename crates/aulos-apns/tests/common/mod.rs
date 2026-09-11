//! The harness every `aulos-apns` integration test shares.
//!
//! Its point is structural, exactly like `aulos-hooks`': there is **no SQLite and no engine**
//! anywhere in this directory. [`FakeDeviceStore`] is a `HashMap` behind the
//! `aulos_core::ports::DeviceStore` port, which is the proof that `aulos-apns` needs neither an
//! `aulos-store` nor an `aulos-queue` dependency (DESIGN §3, §25.1).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)] // each test binary uses a different slice of the harness

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aulos_core::clock::FakeClock;
use aulos_core::id::{ItemId, UnixMs};
use aulos_core::item::{Item, ItemView, Kind, ViewExtras};
use aulos_core::ports::{
    ApnsEnvironment, DeviceRecord, DeviceStore, LiveActivityRecord, PortError,
};
use aulos_core::progress::ProgressCell;
use aulos_core::request::DownloadRequest;
use aulos_core::selection::{Codec, DownloadType, FormatId, ProviderId, QualityId, Selection};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::status::Status;
use url::Url;

// ---------------------------------------------------------------------------
// The signing key
// ---------------------------------------------------------------------------

/// A throwaway P-256 key pair, generated once with
/// `openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt`.
///
/// It is committed rather than generated at test time so the suite needs no `openssl` binary on
/// the machine running it. It signs nothing but test fixtures and is not an APNs key: Apple has
/// never seen it, and the `kid`/`iss` the tests pair it with are made up.
pub const TEST_KEY_P8: &str = include_str!("../fixtures/apns_test_key.p8");

/// The matching public key, so a test can *verify* a token this crate produced rather than
/// re-implement the encoding and compare strings.
pub const TEST_KEY_PUB: &str = include_str!("../fixtures/apns_test_key.pub.pem");

/// The `kid` the tests use.
pub const TEST_KEY_ID: &str = "ABCD1234EF";

/// The `iss` the tests use.
pub const TEST_TEAM_ID: &str = "TEAM123456";

/// The bundle id the tests use, matching `APNS_TOPIC`'s default.
pub const TEST_BUNDLE_ID: &str = "com.tatoalo.aulos";

/// A clock parked at [`aulos_core::clock::DEFAULT_FAKE_EPOCH_MS`].
#[must_use]
pub fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock::default())
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

/// Builds the one wire shape the notifier works from.
#[derive(Clone)]
pub struct ItemBuilder {
    item: Item,
    cell: ProgressCell,
    extras: ViewExtras,
}

impl ItemBuilder {
    /// A `downloading` top-level video item with no progress reported yet.
    #[must_use]
    pub fn new(title: &str) -> Self {
        let url = Url::parse("https://videos.test/watch/9").expect("url");
        let selection = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").expect("format"),
            QualityId::parse("best").expect("quality"),
        );
        let request = DownloadRequest::new(url.clone(), selection);
        Self {
            item: Item {
                id: ItemId::new(),
                kind: Kind::Item,
                group_id: None,
                group_index: None,
                ord: 1,
                url,
                canonical_key: "test".into(),
                provider: Some(ProviderId::parse("ytdlp").expect("provider")),
                media_id: Some("abc".into()),
                title: title.into(),
                status: Status::Downloading,
                auto_start: true,
                msg: None,
                error: None,
                request,
                entry: None,
                filename: None,
                size: None,
                chapter_files: Vec::new(),
                subtitle_files: Vec::new(),
                created_at: 0,
                started_at: None,
                finished_at: None,
                attempt: 0,
                // The default is `ios` because that is the only kind this notifier pushes for
                // (DESIGN §25.2): a rig item is standing in for something the app added. Use
                // [`Self::source`] for the tests that are about the gate itself.
                source: SourceRef::bare(SourceKind::Ios),
                children_total: None,
                clear_after: None,
            },
            cell: ProgressCell::new(tokio::time::Instant::now()),
            extras: ViewExtras::default(),
        }
    }

    /// Sets who added the item — the routing key `APNS_PUSH_ALL=false` gates on (DESIGN §25.2).
    #[must_use]
    pub fn source(mut self, kind: SourceKind) -> Self {
        self.item.source = SourceRef::bare(kind);
        self
    }

    /// Sets the origin to the iOS app **and** names the install it came from: what
    /// `X-Aulos-Install` puts in `source.ref` (PROTOCOL §1.3).
    #[must_use]
    pub fn added_by_install(mut self, install: &str) -> Self {
        self.item.source = SourceRef::with_ref(SourceKind::Ios, install);
        self
    }

    /// Fixes the id, so a test can talk about the same item twice.
    #[must_use]
    pub fn id(mut self, id: ItemId) -> Self {
        self.item.id = id;
        self
    }

    /// The item's id.
    #[must_use]
    pub fn item_id(&self) -> ItemId {
        self.item.id
    }

    /// Sets the status.
    #[must_use]
    pub fn status(mut self, status: Status) -> Self {
        self.item.status = status;
        self
    }

    /// Makes this a child of `group`.
    #[must_use]
    pub fn child_of(mut self, group: ItemId) -> Self {
        self.item.group_id = Some(group);
        self.item.group_index = Some(1);
        self
    }

    /// Makes this a group row with the given roll-up counters.
    #[must_use]
    pub fn group(mut self, done: u32, total: u32) -> Self {
        self.item.kind = Kind::Group;
        self.item.children_total = Some(total);
        self.extras.children_done = Some(done);
        self
    }

    /// Sets the whole progress cell.
    #[must_use]
    pub fn progress(
        mut self,
        percent: f64,
        speed: Option<f64>,
        eta: Option<i64>,
        downloaded: Option<u64>,
        total: Option<u64>,
    ) -> Self {
        self.cell.percent = percent;
        self.cell.speed = speed;
        self.cell.eta = eta;
        self.cell.downloaded_bytes = downloaded;
        self.cell.total_bytes = total;
        self
    }

    /// Sets only the estimate, leaving `total_bytes` null.
    #[must_use]
    pub fn total_estimate(mut self, estimate: u64) -> Self {
        self.cell.total_bytes_estimate = Some(estimate);
        self
    }

    /// Sets the human status line.
    #[must_use]
    pub fn msg(mut self, msg: &str) -> Self {
        self.item.msg = Some(msg.into());
        self
    }

    /// Sets the ready-to-open URL.
    #[must_use]
    pub fn download_url(mut self, url: &str) -> Self {
        self.extras.download_url = Some(Arc::from(url));
        self
    }

    /// The wire view.
    #[must_use]
    pub fn view(&self) -> Arc<ItemView> {
        Arc::new(ItemView::from_item(
            &self.item,
            Some(&self.cell),
            &self.extras,
        ))
    }
}

// ---------------------------------------------------------------------------
// The store port
// ---------------------------------------------------------------------------

/// One call through the [`DeviceStore`] port.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Call {
    /// `upsert_device`.
    UpsertDevice(Box<str>),
    /// `remove_device`.
    RemoveDevice(Box<str>),
    /// `devices`.
    Devices,
    /// `upsert_live_activity`.
    UpsertLiveActivity(Box<str>, ItemId),
    /// `remove_live_activity`.
    RemoveLiveActivity(Box<str>, ItemId),
    /// `live_activities_for`.
    LiveActivitiesFor(ItemId),
    /// `remove_live_activities_for`.
    RemoveLiveActivitiesFor(ItemId),
}

/// A `HashMap`-backed [`DeviceStore`] that records every call.
#[derive(Debug, Default)]
pub struct FakeDeviceStore {
    devices: Mutex<Vec<DeviceRecord>>,
    activities: Mutex<Vec<LiveActivityRecord>>,
    calls: Mutex<Vec<Call>>,
    /// When set, `devices()` answers a [`PortError`] instead of the table.
    fail_devices: AtomicBool,
    /// When set, `live_activities_for()` answers a [`PortError`].
    fail_activities: AtomicBool,
}

impl FakeDeviceStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers a device.
    pub fn add_device(&self, device: DeviceRecord) {
        let mut d = self.devices.lock().unwrap();
        d.retain(|x| x.token != device.token);
        d.push(device);
    }

    /// Registers a Live Activity.
    pub fn add_activity(&self, activity: LiveActivityRecord) {
        let mut a = self.activities.lock().unwrap();
        a.retain(|x| !(x.device_token == activity.device_token && x.item_id == activity.item_id));
        a.push(activity);
    }

    /// Makes `devices()` fail, the way a busy or locked store does.
    pub fn fail_devices(&self, on: bool) {
        self.fail_devices.store(on, Ordering::SeqCst);
    }

    /// Makes `live_activities_for()` fail.
    pub fn fail_activities(&self, on: bool) {
        self.fail_activities.store(on, Ordering::SeqCst);
    }

    /// Every call, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The device tokens still registered.
    #[must_use]
    pub fn device_tokens(&self) -> Vec<Box<str>> {
        self.devices
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.token.clone())
            .collect()
    }

    /// The Live Activity update tokens still registered.
    #[must_use]
    pub fn activity_tokens(&self) -> Vec<Box<str>> {
        self.activities
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.update_token.clone())
            .collect()
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
}

#[async_trait::async_trait]
impl DeviceStore for FakeDeviceStore {
    async fn upsert_device(&self, device: DeviceRecord) -> Result<(), PortError> {
        self.record(Call::UpsertDevice(device.token.clone()));
        self.add_device(device);
        Ok(())
    }

    async fn remove_device(&self, token: &str) -> Result<(), PortError> {
        self.record(Call::RemoveDevice(token.into()));
        self.devices.lock().unwrap().retain(|d| &*d.token != token);
        self.activities
            .lock()
            .unwrap()
            .retain(|a| &*a.device_token != token);
        Ok(())
    }

    async fn devices(&self) -> Result<Vec<DeviceRecord>, PortError> {
        self.record(Call::Devices);
        if self.fail_devices.load(Ordering::SeqCst) {
            return Err(PortError::Store(
                "injected: the device table is unreadable".into(),
            ));
        }
        Ok(self.devices.lock().unwrap().clone())
    }

    async fn upsert_live_activity(&self, activity: LiveActivityRecord) -> Result<(), PortError> {
        self.record(Call::UpsertLiveActivity(
            activity.device_token.clone(),
            activity.item_id,
        ));
        self.add_activity(activity);
        Ok(())
    }

    async fn remove_live_activity(
        &self,
        device_token: &str,
        item: ItemId,
    ) -> Result<(), PortError> {
        self.record(Call::RemoveLiveActivity(device_token.into(), item));
        self.activities
            .lock()
            .unwrap()
            .retain(|a| !(&*a.device_token == device_token && a.item_id == item));
        Ok(())
    }

    async fn live_activities_for(
        &self,
        item: ItemId,
    ) -> Result<Vec<LiveActivityRecord>, PortError> {
        self.record(Call::LiveActivitiesFor(item));
        if self.fail_activities.load(Ordering::SeqCst) {
            return Err(PortError::Store(
                "injected: the live_activities table is unreadable".into(),
            ));
        }
        Ok(self
            .activities
            .lock()
            .unwrap()
            .iter()
            .filter(|a| a.item_id == item)
            .cloned()
            .collect())
    }

    async fn remove_live_activities_for(&self, item: ItemId) -> Result<(), PortError> {
        self.record(Call::RemoveLiveActivitiesFor(item));
        self.activities
            .lock()
            .unwrap()
            .retain(|a| a.item_id != item);
        Ok(())
    }
}

/// A device with alerts on, no Live Activity start token and no install id — the legacy app build
/// every pre-`X-Aulos-Install` test in this directory is standing in for.
#[must_use]
pub fn device(token: &str, env: ApnsEnvironment) -> DeviceRecord {
    DeviceRecord {
        token: token.into(),
        platform: "ios".into(),
        bundle_id: TEST_BUNDLE_ID.into(),
        environment: env,
        alerts: true,
        live_activity_start_token: None,
        install_id: None,
        app_version: Some("1.0.0 (3)".into()),
        registered_at: 0,
        last_seen_at: 0,
    }
}

/// The same device, registered under one install (PROTOCOL §4.8's `install_id`).
#[must_use]
pub fn device_of_install(token: &str, install: &str, env: ApnsEnvironment) -> DeviceRecord {
    DeviceRecord {
        install_id: Some(install.into()),
        ..device(token, env)
    }
}

/// One Live Activity registration.
#[must_use]
pub fn activity(
    device_token: &str,
    item: ItemId,
    update_token: &str,
    env: ApnsEnvironment,
) -> LiveActivityRecord {
    LiveActivityRecord {
        device_token: device_token.into(),
        item_id: item,
        update_token: update_token.into(),
        environment: env,
        registered_at: 0,
    }
}

/// The default fake epoch, in whole seconds — what the payload builders stamp.
#[must_use]
pub fn epoch_secs() -> UnixMs {
    aulos_core::clock::DEFAULT_FAKE_EPOCH_MS / 1_000
}
