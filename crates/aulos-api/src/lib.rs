//! Aulos v2 HTTP and WebSocket API.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod cors;
pub mod error;
pub mod files;
pub mod health;
pub mod trace;
pub mod v2;
pub mod view;
pub mod web;
pub mod ws;

use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::{Arc, Mutex, RwLock};

use arc_swap::ArcSwap;
use aulos_core::{
    Clock, Config, DeviceStore, HealthRegistry, SubscriptionsHandle, SystemClock, UnixMs,
};
use aulos_provider::Registry;
use aulos_queue::{EngineHandle, EventHub, StateView};
use aulos_store::Store;
use axum::Router;
use axum::routing::get;

pub use error::{ApiError, Json};

/// The `Sec-WebSocket-Protocol` value every v2 client offers (PROTOCOL §5.1).
pub const WS_SUBPROTOCOL: &str = auth::SUBPROTOCOL;

/// Build and runtime identity, for `capabilities` and `healthz`.
///
/// `yt_dlp` is `None` until the binary fills it in: the version comes from the Python shim's
/// identity handshake (`aulos_provider_ytdlp::RunnerHandle::identity`), and `aulos-api` may not
/// depend on that crate (DESIGN §3). `null` on the wire is the honest answer for a build that has
/// not probed the shim yet.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    /// `METUBE_VERSION` / `AULOS_VERSION`.
    pub version: Box<str>,
    /// The yt-dlp version the shim reported, or `None`.
    pub yt_dlp: Option<Box<str>>,
    /// Process start, unix ms — the base for `healthz.uptime_s` and `snapshot.server.started_at`.
    pub started_at: UnixMs,
}

impl ServerInfo {
    /// The identity of a process starting now.
    #[must_use]
    pub fn new(cfg: &Config, clock: &dyn Clock) -> Self {
        Self {
            version: cfg.version.clone(),
            yt_dlp: None,
            started_at: clock.now_ms(),
        }
    }

    /// The same identity with the shim's yt-dlp version attached.
    #[must_use]
    pub fn with_yt_dlp(mut self, version: impl Into<Box<str>>) -> Self {
        self.yt_dlp = Some(version.into());
        self
    }
}

/// The runtime counters and caches the API itself owns.
///
/// These are the four numbers `healthz.ws` reports plus two small caches. They live here rather
/// than in the engine because nothing else in the process can observe them: a socket's lag, a
/// slow-consumer disconnect, the directory walk and the deep-probe rate limit are all facts about
/// the HTTP layer.
#[derive(Debug, Default)]
pub struct Live {
    /// Sockets currently connected.
    pub clients: AtomicU64,
    /// `broadcast::error::RecvError::Lagged` occurrences across all sockets.
    pub lagged: AtomicU64,
    /// Sockets closed with `1013` for being too slow, or for exceeding the client cap.
    pub slow_disconnects: AtomicU64,
    /// When `healthz?probe=deep` last actually re-probed, unix ms. `0` means never.
    pub deep_probe_at: AtomicI64,
    /// The `api/v2/custom-dirs` walk, cached (PLAN WP-14: bounded, off-loop, cached).
    pub dirs: Mutex<Option<DirsCache>>,
    /// The result of the last `POST api/v2/ytdl-options/reload` this process served, as
    /// `(ok, msg)`.
    ///
    /// The snapshot's `ytdl_options` block prefers the `ytdl_options` health component when the
    /// binary maintains one (DESIGN §16.3); this is what makes the block honest in a build where
    /// nothing else writes that component.
    pub options_status: Mutex<Option<(bool, Box<str>)>>,
}

/// One cached `custom-dirs` answer.
#[derive(Clone, Debug)]
pub struct DirsCache {
    /// When the walk ran, unix ms.
    pub at: UnixMs,
    /// The payload.
    pub value: Arc<serde_json::Value>,
}

/// Everything a handler needs, cloned per request (PLAN WP-14).
///
/// Deviations from the PLAN's interface block, both forced by the BRIEF scope trims and by what a
/// handler can actually reach:
///
/// - no `conn_ids`: minting a `ConnId` is only useful for `watch`/`unwatch`, which are CUT;
/// - `clock`, `info` and `live` are additive — `server_time`, `uptime_s` and the `ws` block of
///   `healthz` have no other source, and injecting the clock is what makes the snapshot's
///   timestamps deterministic in a test.
#[derive(Clone)]
pub struct ApiState {
    /// Every mutation.
    pub engine: EngineHandle,
    /// Every read of queue state: one atomic load (DESIGN §15.2).
    pub state: StateView,
    /// `seq`, the replay ring, and the frame bus.
    pub hub: EventHub,
    /// Typed reads only — paged history, subscriptions, the import report.
    pub store: Store,
    /// APNs device and Live Activity registrations (PROTOCOL §4.8, DESIGN §25).
    ///
    /// A port rather than the [`Store`] it is defaulted to, because the four `devices` routes are
    /// the *only* thing in this crate that touches those tables and a test wants to drive them
    /// without a database. [`ApiState::new`] fills it in from `store`, so no caller has to know
    /// the two are the same object today.
    pub devices: Arc<dyn DeviceStore>,
    /// Provider selection, catalogs and the plugin reload.
    pub registry: Arc<RwLock<Registry>>,
    /// The effective configuration.
    pub cfg: Arc<Config>,
    /// The live `YTDL_OPTIONS` snapshot.
    pub ytdl: Arc<ArcSwap<aulos_core::YtdlOptions>>,
    /// The `healthz` component map.
    pub health: Arc<HealthRegistry>,
    /// An mpsc sender with no logic (DESIGN §14.1), so this crate never sees
    /// `aulos-subscriptions`.
    pub subs: SubscriptionsHandle,
    /// Build and runtime identity.
    pub info: Arc<ServerInfo>,
    /// The API's own counters and caches.
    pub live: Arc<Live>,
    /// Wall-clock and monotonic time.
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiState")
            .field("prefix", &self.cfg.url_prefix)
            .field("boot_id", &self.hub.boot_id())
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl ApiState {
    /// The state a process assembles at boot, with a [`SystemClock`] and no yt-dlp version yet.
    ///
    /// `aulos-server` builds it this way and then replaces `info` once the shim has answered.
    #[allow(clippy::too_many_arguments)] // one argument per collaborator; a builder would hide it
    #[must_use]
    pub fn new(
        engine: EngineHandle,
        state: StateView,
        hub: EventHub,
        store: Store,
        registry: Arc<RwLock<Registry>>,
        cfg: Arc<Config>,
        ytdl: Arc<ArcSwap<aulos_core::YtdlOptions>>,
        health: Arc<HealthRegistry>,
        subs: SubscriptionsHandle,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let info = Arc::new(ServerInfo::new(&cfg, clock.as_ref()));
        let devices: Arc<dyn DeviceStore> = Arc::new(store.clone());
        Self {
            engine,
            state,
            hub,
            store,
            devices,
            registry,
            cfg,
            ytdl,
            health,
            subs,
            info,
            live: Arc::new(Live::default()),
            clock,
        }
    }

    /// Replaces the [`DeviceStore`] the `devices` routes write through.
    ///
    /// The default is the SQLite store; this exists for a test that wants an in-memory stand-in,
    /// and for a build that persists registrations somewhere else.
    #[must_use]
    pub fn with_devices(mut self, devices: Arc<dyn DeviceStore>) -> Self {
        self.devices = devices;
        self
    }

    /// Replaces the clock. Used by this crate's tests and by `check-config`.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.info = Arc::new(ServerInfo {
            started_at: clock.now_ms(),
            ..(*self.info).clone()
        });
        self.clock = clock;
        self
    }

    /// Replaces the identity block.
    #[must_use]
    pub fn with_info(mut self, info: ServerInfo) -> Self {
        self.info = Arc::new(info);
        self
    }

    /// Unix milliseconds now.
    #[must_use]
    pub fn now_ms(&self) -> UnixMs {
        self.clock.now_ms()
    }

    /// The `X-Aulos-Seq` / body `seq` value: the frame cursor as of this moment (PROTOCOL §6.4).
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.hub.head().0
    }

    /// The `protocol` block PROTOCOL §5.3 and §4.3 both carry.
    #[must_use]
    pub fn protocol_block(&self) -> serde_json::Value {
        aulos_queue::protocol_block(&self.cfg)
    }
}

/// The HTTP surface.
pub fn router(state: ApiState) -> Router {
    let p = state.cfg.url_prefix.clone();
    // `GET <p>` is content-negotiated: an `Accept` list containing `text/html` gets the embedded
    // page, and everything else — `application/json`, the bare `*/*` that `curl` sends, no
    // `Accept` at all — gets the identity document byte for byte, which is what the iOS app and
    // every existing script depend on. With `AULOS_WEB_UI=false` it is the identity document for
    // every `Accept`.
    let mut open: Router = Router::new()
        .route(&p.route(""), get(web::root))
        .route(&p.route("robots.txt"), get(v2::meta::robots))
        .route(&p.route("healthz"), get(health::healthz))
        .route(&p.route("livez"), get(health::livez))
        // v1.0: not implemented, see BRIEF — the Prometheus endpoint is CUT. The route stays so a
        // scrape config gets an honest 404 with the error envelope.
        .route(&p.route("metrics"), get(v2::meta::metrics_cut))
        .with_state(state.clone());

    if !p.is_root() {
        let to = p.as_str().to_owned();
        let trimmed = to.trim_end_matches('/').to_owned();
        let root_target = to.clone();
        open = open
            .route(
                "/",
                get(move || async move {
                    (
                        axum::http::StatusCode::FOUND,
                        [(axum::http::header::LOCATION, root_target)],
                    )
                }),
            )
            .route(
                &trimmed,
                get(move || async move {
                    (
                        axum::http::StatusCode::FOUND,
                        [(axum::http::header::LOCATION, to)],
                    )
                }),
            );
    }

    let guarded: Router = v2_router(state.clone())
        .merge(ws_router(state.clone()))
        .merge(files::router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require,
        ));

    // The page, its two assets, its two icons and its manifest are served **without** auth even
    // when `AULOS_API_TOKEN` or the trusted-proxy header is configured: they hold nothing secret,
    // a browser cannot put a bearer token on a document navigation, and the page's own job is to
    // turn a 401 from the API into a token prompt. Every API route below keeps its auth unchanged.
    // With `AULOS_WEB_UI=false` nothing is registered, so the routes fall through to
    // `no_such_route` and answer the standard `404 not_found` envelope.
    let open = if state.cfg.web_ui {
        open.merge(web::router(state.clone()))
    } else {
        open
    };

    let mut router = open.merge(guarded);
    if let Some(layer) = cors::v2(&state.cfg.cors_allowed_origins) {
        router = router.layer(layer);
    }
    // PROTOCOL §1.5: "every non-2xx response, without exception" is the error envelope, and §1.2
    // types every body as JSON. Without these two, axum answers a typo'd path and a wrong method
    // with a zero-length body and no `Content-Type`, and §1.4 then tells the client to blame its
    // reverse proxy for what is a plain routing miss. Registered after every merge, because
    // `method_not_allowed_fallback` attaches to the method routers registered so far.
    router = router
        .fallback(no_such_route)
        .method_not_allowed_fallback(wrong_method);
    router.layer(axum::middleware::from_fn_with_state(state, trace::headers))
}

/// The catch-all `404`, in the §1.5 envelope. `trace::headers` fills in its `request_id`.
async fn no_such_route(method: axum::http::Method, uri: axum::http::Uri) -> error::ApiError {
    error::ApiError::not_found(format!("no route for {method} {}", uri.path()))
}

/// The catch-all `405`, in the §1.5 envelope.
///
/// axum's own answer carries `Allow` and an empty body; this keeps the header (it is added by the
/// method router around us) and gives the body a shape a client can decode.
async fn wrong_method(method: axum::http::Method, uri: axum::http::Uri) -> error::ApiError {
    error::ApiError::of(
        aulos_core::ErrorCode::MethodNotAllowed,
        format!("{method} is not allowed on {}", uri.path()),
    )
}

/// The `<p>api/v2/*` routes (PROTOCOL §4).
pub fn v2_router(state: ApiState) -> Router {
    v2::router(state)
}

/// The `<p>ws` route (PROTOCOL §5).
pub fn ws_router(state: ApiState) -> Router {
    ws::router(state)
}
