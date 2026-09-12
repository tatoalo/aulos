//! The rig every integration test in this crate runs on: the real engine, the real aggregator,
//! the real hub and the real router, bound to a loopback port.
//!
//! Everything is local and nothing waits on wall-clock time longer than a WebSocket batch window:
//! a temporary SQLite file, the scripted `fake` provider (BRIEF §17), a [`FakeClock`] so the
//! snapshot's timestamps are deterministic, and an in-memory stand-in for the subscription manager
//! so the `api/v2/subscriptions*` routes can be driven without `aulos-subscriptions` (which
//! `aulos-api` must not depend on — DESIGN §3, §14.1).
//!
//! # The prefix rule
//!
//! [`Rig::start`] takes the `URL_PREFIX`, and every test runs itself under **both** `/` and
//! `/metube/` through [`for_each_prefix`]. That is the PLAN WP-14 requirement "the entire suite
//! runs twice", expressed so that a route built with a raw string instead of the `Prefix` newtype
//! fails in the same test that covers its behaviour.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_api::{ApiState, ServerInfo};
use aulos_core::config::{RawEnv, load};
use aulos_core::{
    BootId, CheckJob, Clock, Codec, Config, DownloadType, DownloadTypeSpec, EventRouter, FakeClock,
    FormatCatalog, FormatFlags, FormatId, FormatSpec, HealthRegistry, NamingPolicy, OptionKind,
    OptionSpec, ProviderId, QualityId, QualitySpec, Selection, SubCmd, SubError, SubId,
    SubscriberSpec, SubscriptionView, SubscriptionsHandle, YtdlOptions, ytdlp_catalog,
};
use aulos_provider::fake::FakeProvider;
use aulos_provider::{
    DownloadCtx, Match, Outcome, ProgressMsg, ProgressSink, Provider, ProviderError,
    ProviderHealth, Registry, ResolveCtx, SCORE_FALLBACK, SCORE_SC,
};
use aulos_queue::{Aggregator, Engine, EventHub};
use aulos_store::{Store, StoreOptions};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The wall-clock instant every rig starts at, so a snapshot's `server_time` is a constant.
pub const T0: i64 = 1_757_000_000_000;

/// The two prefixes the whole suite runs under (PLAN WP-14).
pub const PREFIXES: [&str; 2] = ["/", "/metube/"];

/// Runs `body` once per [`PREFIXES`] entry.
pub async fn for_each_prefix<F, Fut>(body: F)
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = ()>,
{
    for prefix in PREFIXES {
        body(prefix).await;
    }
}

/// A running server plus everything a test needs to drive and observe it.
pub struct Rig {
    pub dir: TempDir,
    pub cfg: Arc<Config>,
    pub state: ApiState,
    pub store: Store,
    pub hub: EventHub,
    pub clock: Arc<FakeClock>,
    pub subs: Arc<SubsFake>,
    pub addr: SocketAddr,
    pub http: reqwest::Client,
    pub prefix: String,
    _tasks: Vec<JoinHandle<()>>,
}

/// How a rig is put together.
pub struct RigBuilder {
    prefix: &'static str,
    env: Vec<(String, String)>,
    providers: Vec<Arc<dyn Provider>>,
    with_defaults: bool,
    /// `(directory name, plugin.toml body)`, written into `AULOS_PLUGINS_DIR` before the scan.
    plugins: Vec<(String, String)>,
}

impl RigBuilder {
    /// Overrides or adds an environment variable.
    ///
    /// The token `{dir}` in `value` is replaced with the rig's temporary directory, which is how a
    /// test reproduces a layout the shipped image has and the rig's defaults do not — most of all
    /// `STATE_DIR` *inside* `DOWNLOAD_DIR`, which is exactly what `docker/Dockerfile` sets.
    #[must_use]
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Drops a `plugin.toml` into `AULOS_PLUGINS_DIR` and installs the real command loader, so
    /// the rig exercises manifest discovery rather than a stub.
    #[must_use]
    pub fn plugin(mut self, name: &str, manifest: &str) -> Self {
        self.plugins.push((name.to_owned(), manifest.to_owned()));
        self
    }

    /// Registers an extra provider, after the three defaults.
    #[must_use]
    pub fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.providers.push(provider);
        self
    }

    /// Registers **only** the providers given, with no defaults.
    #[must_use]
    pub fn without_default_providers(mut self) -> Self {
        self.with_defaults = false;
        self
    }

    /// Builds the store, the engine, the aggregator, the hub, the router and the listener.
    pub async fn start(self) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["downloads", "audio", "temp", "state", "plugins"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let path = |sub: &str| dir.path().join(sub).to_string_lossy().into_owned();
        let mut env: Vec<(String, String)> = vec![
            ("STATE_DIR".into(), path("state")),
            ("DOWNLOAD_DIR".into(), path("downloads")),
            ("AUDIO_DOWNLOAD_DIR".into(), path("audio")),
            ("TEMP_DIR".into(), path("temp")),
            ("AULOS_PLUGINS_DIR".into(), path("plugins")),
            ("AULOS_DB_PATH".into(), path("state/aulos.db")),
            ("AULOS_DB_FLUSH_MS".into(), "5".into()),
            ("AULOS_WS_BATCH_MS".into(), "50".into()),
            ("AULOS_WS_URGENT_MS".into(), "5".into()),
            ("CUSTOM_DIRS".into(), "true".into()),
            ("CREATE_CUSTOM_DIRS".into(), "true".into()),
            ("METUBE_VERSION".into(), "2026.09.04".into()),
            ("URL_PREFIX".into(), self.prefix.to_owned()),
        ];
        let root = dir.path().to_string_lossy().into_owned();
        env.extend(
            self.env
                .into_iter()
                .map(|(k, v)| (k, v.replace("{dir}", &root))),
        );
        // A `{dir}`-built path may not exist yet (`STATE_DIR` moved under the download root, say).
        for (key, value) in &env {
            if key.ends_with("_DIR") && value.starts_with(&root) {
                std::fs::create_dir_all(value).unwrap();
            }
        }
        let cfg = Arc::new(load(&RawEnv::from_pairs(env)).expect("the rig config must load"));

        let store = Store::open(
            StoreOptions::from_config(&cfg)
                .with_flush_ms(5)
                .with_readers(2)
                .with_busy_timeout_ms(500),
        )
        .unwrap();

        let plugins_dir = dir.path().join("plugins");
        for (name, manifest) in &self.plugins {
            let plugin_dir = plugins_dir.join(name);
            std::fs::create_dir_all(&plugin_dir).unwrap();
            std::fs::write(plugin_dir.join("plugin.toml"), manifest).unwrap();
        }

        let mut registry = Registry::new();
        if self.with_defaults {
            registry.register(Arc::new(ytdlp_like()));
            registry.register(Arc::new(sc_like()));
            registry.register(Arc::new(fake_downloader()));
        }
        for provider in self.providers {
            registry.register(provider);
        }
        if !self.plugins.is_empty() {
            registry.set_command_loader(Arc::new(
                aulos_provider::command::CommandPluginLoader::default(),
            ));
            registry.reload_commands(&plugins_dir);
        }
        let registry = Arc::new(RwLock::new(registry));

        let clock = Arc::new(FakeClock::new(T0));
        let ytdl = Arc::new(ArcSwap::from_pointee(
            YtdlOptions::load(
                &cfg.ytdl_options,
                cfg.ytdl_options_file.as_deref(),
                &cfg.ytdl_options_presets,
                cfg.ytdl_options_presets_file.as_deref(),
            )
            .unwrap_or_default(),
        ));

        let (mut router_actor, events) = EventRouter::new(4_096);
        let inbox = router_actor.subscribe(SubscriberSpec::aggregator());
        let router_task = router_actor.spawn();

        let (progress_tx, progress_rx) = mpsc::channel::<ProgressMsg>(1_024);
        let (engine, handle) = Engine::new(
            store.clone(),
            Arc::clone(&registry),
            Arc::clone(&cfg),
            Arc::clone(&ytdl),
            Arc::clone(&clock) as Arc<dyn Clock>,
            events,
            progress_tx,
        );
        let engine_task = engine.spawn();

        let hub = EventHub::new(store.seq_allocator(), BootId::new(), &cfg);
        let (aggregator, state_view) = Aggregator::new(
            hub.clone(),
            Arc::clone(&cfg),
            Arc::clone(&clock) as Arc<dyn Clock>,
        );
        let agg_task = aggregator.spawn(progress_rx, inbox, handle.clone());

        let health = Arc::new(HealthRegistry::new());
        health.set_identity(hub.boot_id());
        let (subs_handle, subs_rx) = SubscriptionsHandle::channel(32);
        let subs = Arc::new(SubsFake::default());
        let subs_task = subs.clone().spawn(subs_rx);

        let state = ApiState::new(
            handle,
            state_view,
            hub.clone(),
            store.clone(),
            Arc::clone(&registry),
            Arc::clone(&cfg),
            ytdl,
            Arc::clone(&health),
            subs_handle,
        )
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
        .with_info(ServerInfo {
            version: cfg.version.clone(),
            yt_dlp: Some("2026.8.30.232658.dev0".into()),
            started_at: T0 - 43_201_000,
        });

        let app = aulos_api::router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Let the aggregator initialise its tick phase before the first read.
        tokio::task::yield_now().await;

        Rig {
            dir,
            cfg: Arc::clone(&cfg),
            state,
            store,
            hub,
            clock,
            subs,
            addr,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            prefix: cfg.url_prefix.as_str().to_owned(),
            _tasks: vec![router_task, engine_task, agg_task, subs_task, serve_task],
        }
    }
}

impl Rig {
    /// A rig under `prefix`, with the three default providers.
    pub async fn start(prefix: &'static str) -> Self {
        Self::builder(prefix).start().await
    }

    /// A configurable rig.
    #[must_use]
    pub fn builder(prefix: &'static str) -> RigBuilder {
        RigBuilder {
            prefix,
            env: Vec::new(),
            providers: Vec::new(),
            with_defaults: true,
            plugins: Vec::new(),
        }
    }

    /// An absolute URL for a path under the prefix.
    #[must_use]
    pub fn url(&self, suffix: &str) -> String {
        format!("http://{}{}", self.addr, self.cfg.url_prefix.route(suffix))
    }

    /// A `ws://` URL for a path under the prefix.
    #[must_use]
    pub fn ws_url(&self, suffix: &str) -> String {
        format!("ws://{}{}", self.addr, self.cfg.url_prefix.route(suffix))
    }

    /// `GET`, as a `(status, body)` pair.
    pub async fn get(&self, suffix: &str) -> (u16, Value) {
        let response = self.http.get(self.url(suffix)).send().await.unwrap();
        status_and_body(response).await
    }

    /// `GET`, keeping the whole response so a test can read headers.
    pub async fn get_raw(&self, suffix: &str) -> reqwest::Response {
        self.http.get(self.url(suffix)).send().await.unwrap()
    }

    /// `POST` with a JSON body.
    pub async fn post(&self, suffix: &str, body: &Value) -> (u16, Value) {
        let response = self
            .http
            .post(self.url(suffix))
            .json(body)
            .send()
            .await
            .unwrap();
        status_and_body(response).await
    }

    /// `PATCH` with a JSON body.
    pub async fn patch(&self, suffix: &str, body: &Value) -> (u16, Value) {
        let response = self
            .http
            .patch(self.url(suffix))
            .json(body)
            .send()
            .await
            .unwrap();
        status_and_body(response).await
    }

    /// `DELETE`.
    pub async fn delete(&self, suffix: &str) -> (u16, Value) {
        let response = self.http.delete(self.url(suffix)).send().await.unwrap();
        status_and_body(response).await
    }

    /// Adds one URL and returns its id, waiting only for the `202`.
    pub async fn add(&self, url: &str) -> String {
        let (status, body) = self.post("api/v2/downloads", &json!({ "url": url })).await;
        assert_eq!(status, 202, "add rejected: {body}");
        body["ids"][0].as_str().unwrap().to_owned()
    }

    /// Waits until `GET api/v2/items/{id}` reports `status`, or panics.
    pub async fn until_status(&self, id: &str, status: &str) -> Value {
        self.until(
            &format!("item {id} in {status}"),
            |item| item["status"] == status,
            id,
        )
        .await
    }

    /// The item's current `status`, without waiting.
    pub async fn status_of(&self, id: &str) -> String {
        let (code, body) = self.get(&format!("api/v2/items/{id}")).await;
        assert_eq!(code, 200, "{body}");
        body["status"].as_str().unwrap().to_owned()
    }

    /// Waits until the item satisfies `pred`.
    pub async fn until(&self, what: &str, pred: impl Fn(&Value) -> bool, id: &str) -> Value {
        for _ in 0..600 {
            let (code, body) = self.get(&format!("api/v2/items/{id}")).await;
            if code == 200 && pred(&body) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (_, body) = self.get(&format!("api/v2/items/{id}")).await;
        panic!("timed out waiting for {what}; the item is {body}");
    }

    /// Waits until the published snapshot satisfies `pred`.
    pub async fn until_state(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..600 {
            let (_, body) = self.get("api/v2/state").await;
            if pred(&body) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (_, body) = self.get("api/v2/state").await;
        panic!("timed out waiting for {what}; the state is {body}");
    }

    /// Lets the engine and the aggregator drain everything queued so far.
    pub async fn settle(&self) {
        for _ in 0..6 {
            self.state.engine.tick().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The absolute download root.
    #[must_use]
    pub fn download_dir(&self) -> PathBuf {
        self.cfg.paths.download.clone()
    }

    /// Writes a file into the download root and returns its relative path.
    pub fn write_download(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.download_dir().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

/// Splits a response into its status and its JSON body (`null` when it has none).
pub async fn status_and_body(response: reqwest::Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

// ---------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------

/// A provider that answers a chosen [`Match`] and a chosen catalog, delegating the work.
pub struct Wrapped {
    inner: FakeProvider,
    id: ProviderId,
    answer: Match,
    hosts: Vec<String>,
    catalog: Arc<FormatCatalog>,
}

#[async_trait::async_trait]
impl Provider for Wrapped {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn matches(&self, url: &url::Url) -> Match {
        // A catch-all still declines a scheme it could never fetch, exactly as the real `ytdlp`
        // provider does — that is what makes `unsupported_url` reachable.
        if !matches!(url.scheme(), "http" | "https") {
            return Match::No;
        }
        if self.hosts.is_empty() {
            return self.answer;
        }
        let host = url.host_str().unwrap_or_default().to_lowercase();
        if self.hosts.iter().any(|h| host.contains(h.as_str())) {
            self.answer
        } else {
            Match::No
        }
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        url: &url::Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<aulos_provider::MediaEntry>, ProviderError> {
        self.inner.resolve(url, ctx).await
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        self.inner.download(ctx, sink).await
    }

    async fn probe(&self) -> ProviderHealth {
        self.inner.probe().await
    }
}

/// The catch-all: `Weak(1)` on every host, carrying the **real** `ytdlp` catalog.
///
/// It stands in for `aulos-provider-ytdlp`, which `aulos-api` may not depend on, and it is what
/// makes the `capabilities.formats` assertion meaningful: the sixteen legacy entries of
/// PROTOCOL §4.5 come from `aulos_core::ytdlp_catalog`, the same function the real provider
/// returns.
#[must_use]
pub fn ytdlp_like() -> Wrapped {
    Wrapped {
        inner: FakeProvider::from_toml("id = \"ytdlp\"\n").unwrap(),
        id: ProviderId::parse("ytdlp").unwrap(),
        answer: Match::Weak(SCORE_FALLBACK),
        hosts: Vec::new(),
        catalog: Arc::new(ytdlp_catalog()),
    }
}

/// A `streamingcommunity`-shaped provider: `Strong(200)` on its host, one advisory rendition, and
/// `naming: "provider"` (PROTOCOL §4.6's worked example).
#[must_use]
pub fn sc_like() -> Wrapped {
    Wrapped {
        inner: FakeProvider::from_toml(
            "id = \"streamingcommunity\"\nhosts = [\"streamingcommunity.test\"]\n",
        )
        .unwrap(),
        id: ProviderId::parse("streamingcommunity").unwrap(),
        answer: Match::Strong(SCORE_SC),
        hosts: vec!["streamingcommunity.test".to_owned()],
        catalog: Arc::new(sc_catalog()),
    }
}

/// The advisory single-quality catalog.
#[must_use]
pub fn sc_catalog() -> FormatCatalog {
    FormatCatalog {
        provider: ProviderId::parse("streamingcommunity").unwrap(),
        version: 1,
        naming: NamingPolicy::Provider,
        download_types: vec![DownloadTypeSpec {
            id: "video".into(),
            label: "Video".into(),
            default_format: "mp4".into(),
            options: vec![OptionSpec {
                id: "auto_start".into(),
                label: "Start immediately".into(),
                kind: OptionKind::Bool,
                default: json!(true),
                choices: Vec::new(),
                help: None,
            }],
            formats: vec![FormatSpec {
                id: "mp4".into(),
                label: "MP4".into(),
                default_quality: "best".into(),
                qualities: vec![QualitySpec {
                    id: "best".into(),
                    label: "Source".into(),
                    notice: None,
                }],
                codecs: Vec::new(),
                notice: Some(
                    "StreamingCommunity serves one source rendition; quality is ignored.".into(),
                ),
                flags: FormatFlags {
                    advisory: true,
                    requires_ffmpeg: true,
                    lossy_remux: false,
                    slow: false,
                },
            }],
        }],
    }
}

/// The provider that actually downloads, on `fake.test`.
#[must_use]
pub fn fake_downloader() -> FakeProvider {
    FakeProvider::from_toml("id = \"fake\"\nscore = 200\nstrong = true\nhosts = [\"fake.test\"]\n")
        .unwrap()
}

/// A provider whose download hangs until it is cancelled.
#[must_use]
pub fn hanging() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        strong = true
        hosts = ["fake.test"]

        [[timeline]]
        download = [
            { kind = "stage", stage = "preparing" },
            { kind = "stage", stage = "downloading" },
            { kind = "hang" },
        ]
    "#,
    )
    .unwrap()
}

/// A provider that expands a URL containing `playlist` into `count` children.
#[must_use]
pub fn expanding(count: usize) -> FakeProvider {
    FakeProvider::from_toml(&format!(
        r#"
        id = "fake"
        score = 200
        strong = true
        hosts = ["fake.test"]

        [[timeline]]
        url_regex = "playlist"
        resolve = [{{ kind = "expand_playlist", count = {count} }}]

        [[timeline]]
        resolve = []
    "#
    ))
    .unwrap()
}

/// A provider whose resolution never finishes, so an item parks in `resolving`.
#[must_use]
pub fn slow_resolve() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        strong = true
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "wait", ms = 600000 }]
    "#,
    )
    .unwrap()
}

/// The default selection: `video / auto / mp4 / best`.
#[must_use]
pub fn selection() -> Selection {
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").unwrap(),
        QualityId::parse("best").unwrap(),
    )
}

// ---------------------------------------------------------------------------
// the subscription manager stand-in
// ---------------------------------------------------------------------------

/// An in-memory subscription manager, good enough for the API's contract.
///
/// It reproduces the three legacy rejections the routes have to surface — a single-video URL, a
/// duplicate, and an unknown id — and nothing else. `aulos-subscriptions` is the real thing and
/// `aulos-api` must not depend on it (DESIGN §3), which is exactly why the handlers are one
/// message each.
#[derive(Default)]
pub struct SubsFake {
    rows: Mutex<Vec<SubscriptionView>>,
    pub checks: Mutex<Vec<Vec<SubId>>>,
}

impl SubsFake {
    /// Spawns the receiver loop.
    pub fn spawn(self: Arc<Self>, mut rx: mpsc::Receiver<SubCmd>) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                self.handle(cmd);
            }
        })
    }

    /// Every subscription it holds.
    #[must_use]
    pub fn all(&self) -> Vec<SubscriptionView> {
        self.rows.lock().unwrap().clone()
    }

    fn handle(&self, cmd: SubCmd) {
        match cmd {
            SubCmd::Add {
                request,
                check_interval_minutes,
                name,
                ack,
            } => {
                let url: Box<str> = Box::from(request.url.as_str());
                let selection = &request.selection;
                let folder = request.folder.as_ref();
                let mut rows = self.rows.lock().unwrap();
                let answer = if url.contains("watch?v=") {
                    Err(SubError::VideoOnly)
                } else if rows.iter().any(|r| *r.url == *url) {
                    Err(SubError::AlreadySubscribed)
                } else {
                    let view = SubscriptionView {
                        id: SubId::new(),
                        // The manager's rule: a non-blank name wins, a blank one is named after
                        // the feed — which here is always `Veritasium`.
                        name: name
                            .as_deref()
                            .map(str::trim)
                            .filter(|n| !n.is_empty())
                            .map_or_else(|| Arc::from("Veritasium"), Arc::from),
                        url: Arc::from(&*url),
                        enabled: true,
                        check_interval_minutes: check_interval_minutes.unwrap_or(60).max(1),
                        download_type: selection.download_type,
                        codec: selection.codec,
                        format: selection.format.as_arc(),
                        quality: selection.quality.as_arc(),
                        folder: folder.map_or_else(|| Arc::from(""), |f| Arc::from(f.as_str())),
                        last_checked: None,
                        seen_count: 0,
                        error: None,
                        next_due: Some(T0 + 3_600_000),
                        consecutive_failures: 0,
                        checking: false,
                    };
                    rows.push(view.clone());
                    Ok(Box::new(view))
                };
                let _ = ack.send(answer);
            }
            SubCmd::Update { id, changes, ack } => {
                let mut rows = self.rows.lock().unwrap();
                let answer = match rows.iter_mut().find(|r| r.id == id) {
                    None => Err(SubError::NotFound(id)),
                    Some(row) => {
                        if let Some(enabled) = changes.enabled {
                            row.enabled = enabled;
                        }
                        if let Some(minutes) = changes.check_interval_minutes {
                            row.check_interval_minutes = minutes.max(1);
                        }
                        if let Some(name) = &changes.name {
                            row.name = Arc::from(&**name);
                        }
                        Ok(Box::new(row.clone()))
                    }
                };
                let _ = ack.send(answer);
            }
            SubCmd::Delete { ids, ack } => {
                let mut rows = self.rows.lock().unwrap();
                let mut removed = Vec::new();
                for id in ids {
                    if let Some(at) = rows.iter().position(|r| r.id == id) {
                        rows.remove(at);
                        removed.push(id);
                    }
                }
                let _ = ack.send(Ok(removed));
            }
            SubCmd::Check { ids, ack } => {
                let rows = self.rows.lock().unwrap();
                let subscriptions = if ids.is_empty() {
                    rows.iter().map(|r| r.id.clone()).collect()
                } else {
                    ids.clone()
                };
                self.checks.lock().unwrap().push(subscriptions.clone());
                let _ = ack.send(Ok(CheckJob {
                    job_id: "01JC00000000000000000000JOB".into(),
                    subscriptions,
                }));
            }
            SubCmd::List { ack } => {
                let _ = ack.send(Ok(self.rows.lock().unwrap().clone()));
            }
            SubCmd::Health { ack } => {
                let _ = ack.send(aulos_core::SubsHealth::default());
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// snapshot helpers
// ---------------------------------------------------------------------------

/// The insta redactions every snapshot needs: the ids, the boot id and the frame cursor are all
/// fresh per run, and none of them is what a snapshot is asserting.
#[must_use]
pub fn redactions() -> HashMap<String, String> {
    HashMap::new()
}

/// Replaces every ULID-shaped string and every `seq`-ish number with a placeholder, recursively.
///
/// Used instead of insta's selector-based redactions because the payloads are `serde_json::Value`s
/// whose shapes differ per endpoint, and one walker covers all of them.
pub fn normalise(value: &mut Value) {
    match value {
        Value::String(text) => {
            if is_ulid(text) {
                *text = "[id]".to_owned();
            } else if text.contains("01J") && text.len() > 26 {
                // an ETag or a message with an id inside it
                *text = ulid_free(text);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                normalise(item);
            }
        }
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let Some(entry) = map.get_mut(&key) else {
                    continue;
                };
                match key.as_str() {
                    "seq" | "from" | "to" | "done_total" | "total" | "generation"
                    | "frames_total" | "update_time" | "uptime_s" | "bytes" | "updated_at"
                    | "commits_total" | "db_bytes" | "wal_bytes" => {
                        if entry.is_number() {
                            *entry = json!("[n]");
                        } else if entry.is_null() {
                            // keep an explicit null: it is a documented value
                        } else {
                            normalise(entry);
                        }
                    }
                    "etag" => *entry = json!("[etag]"),
                    _ => normalise(entry),
                }
            }
        }
        _ => {}
    }
}

/// Whether a string looks like a ULID.
fn is_ulid(text: &str) -> bool {
    text.len() == 26
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

/// Replaces every 26-character ULID-shaped run inside a longer string.
fn ulid_free(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 26 <= chars.len() {
            let candidate: String = chars[i..i + 26].iter().collect();
            if is_ulid(&candidate) {
                out.push_str("[id]");
                i += 26;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// WebSocket helpers
// ---------------------------------------------------------------------------

/// The client socket type `tokio_tungstenite::connect_async` hands back.
pub type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connects to `<p>ws` with the given query suffix (`""` for none).
pub async fn connect(rig: &Rig, suffix: &str) -> Socket {
    let (socket, _) = tokio_tungstenite::connect_async(rig.ws_url(suffix))
        .await
        .expect("the ws upgrade must succeed");
    socket
}

/// The next **text** frame, parsed. Transport pings and pongs are skipped.
///
/// # Panics
/// After five seconds, or if the socket closes first — both are test failures.
pub async fn next_frame(socket: &mut Socket) -> Value {
    match try_next_frame(socket, Duration::from_secs(5)).await {
        Some(frame) => frame,
        None => panic!("timed out waiting for a ws frame"),
    }
}

/// The next text frame, or `None` if none arrives inside `within`.
pub async fn try_next_frame(socket: &mut Socket, within: Duration) -> Option<Value> {
    use futures_util::StreamExt;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        // A timeout, a closed stream and a read error are all "no more frames" to a test.
        let Ok(Some(Ok(message))) = tokio::time::timeout(left, socket.next()).await else {
            return None;
        };
        match message {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                return serde_json::from_str(&text).ok();
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => return None,
            _ => {}
        }
    }
}

/// Reads frames until one of type `kind` arrives, returning it plus everything skipped.
pub async fn next_frame_of(socket: &mut Socket, kind: &str) -> Value {
    for _ in 0..200 {
        let frame = next_frame(socket).await;
        if frame["t"] == kind {
            return frame;
        }
    }
    panic!("no {kind} frame arrived");
}

/// Sends one client frame.
pub async fn send_frame(socket: &mut Socket, body: &Value) {
    try_send_frame(socket, body)
        .await
        .expect("the client frame must be sent");
}

/// Sends one client frame, tolerating a socket the server has already closed — which is the
/// correct outcome for a frame over the size cap.
// The `Err` variant is tungstenite's own error enum; boxing it here would only make every call
// site unwrap one more layer to see what the socket actually said.
#[allow(clippy::result_large_err)]
pub async fn try_send_frame(
    socket: &mut Socket,
    body: &Value,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    use futures_util::SinkExt;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            body.to_string().into(),
        ))
        .await
}

/// Waits for the socket to close, returning the close code when there was one.
pub async fn close_code(socket: &mut Socket) -> Option<u16> {
    use futures_util::StreamExt;
    for _ in 0..200 {
        // Anything but a close frame — a timeout, a reset, an error — means the code could not be
        // observed, which the caller treats as "closed without one".
        let Ok(Some(Ok(message))) =
            tokio::time::timeout(Duration::from_secs(5), socket.next()).await
        else {
            return None;
        };
        if let tokio_tungstenite::tungstenite::Message::Close(frame) = message {
            return frame.map(|f| u16::from(f.code));
        }
    }
    None
}

/// The PROTOCOL §7 apply step, verbatim, so a test can reconstruct client state from frames.
///
/// It is the algorithm a client author is told to implement, which is the point: if this function
/// and the server disagree, one of them is wrong and the test says so.
pub fn apply(state: &mut HashMap<String, Value>, frame: &Value) {
    match frame["t"].as_str().unwrap_or_default() {
        "snapshot" => {
            state.clear();
            for list in ["items", "done"] {
                for item in frame[list].as_array().into_iter().flatten() {
                    if let Some(id) = item["id"].as_str() {
                        state.insert(id.to_owned(), item.clone());
                    }
                }
            }
        }
        // `added` and `completed` are upserts keyed on id.
        "added" | "completed" => {
            for item in frame["items"].as_array().into_iter().flatten() {
                if let Some(id) = item["id"].as_str() {
                    state.insert(id.to_owned(), item.clone());
                }
            }
        }
        "removed" => {
            for id in frame["ids"].as_array().into_iter().flatten() {
                if let Some(id) = id.as_str() {
                    state.remove(id);
                }
            }
        }
        // A `delta` never creates a record: an absent key means unchanged, an explicit null means
        // changed to null, and a patch for an unknown id is skipped (PROTOCOL §5.4).
        "delta" => {
            for patch in frame["items"].as_array().into_iter().flatten() {
                let Some(id) = patch["id"].as_str() else {
                    continue;
                };
                let Some(row) = state.get_mut(id) else {
                    continue;
                };
                let (Some(row), Some(fields)) = (row.as_object_mut(), patch.as_object()) else {
                    continue;
                };
                for (key, value) in fields {
                    if key != "id" {
                        row.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        _ => {}
    }
}
