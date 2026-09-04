//! The HTTP surface: the v2 REST routes, the WebSocket at `<prefix>ws` with its snapshot/delta
//! protocol, the v1 compatibility shim translating the legacy request and response shapes over the
//! same v2 core, static serving of completed downloads, `healthz`/`livez`, CORS, request tracing
//! and auth.
//!
//! The API is a leaf: only `aulos-server` may depend on it, and it never sees SQL (DESIGN §3 rules
//! A3/A4, §11, §14, §16.3).
//!
//! # Shape
//!
//! ```text
//!                  ┌──────────── trace: X-Request-Id, X-Aulos-Seq, the access log
//!   request ──────►│
//!                  ├── open:  GET <p>, healthz, livez, robots.txt, socket.io/* (501)
//!                  └── auth ──┬── <p>api/v2/*      v2::router
//!                             ├── <p>ws            ws::router
//!                             ├── <p>download/*    files::router
//!                             └── <p>add, history, … v1::router (legacy CORS)
//! ```
//!
//! Every read of queue state goes through [`aulos_queue::StateView`] — one atomic load, no
//! database round trip (DESIGN §15.2) — and every mutation goes through
//! [`aulos_queue::EngineHandle`], so an HTTP handler owns no queue state and takes no lock.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the error envelope, the `Json` responder | [`error`] |
//! | request ids, the two headers, the access log | [`trace`] |
//! | cookie passthrough, the proxy header, the bearer token | [`auth`] |
//! | CORS for v1 and v2 | [`cors`] |
//! | `download_url` — the one derived wire field | [`view`] |
//! | `POST downloads`, actions, `state`, `items`, `capabilities`, `catalog`, … | [`v2`] |
//! | the WebSocket session | [`ws`] |
//! | `add`, `history`, `delete`, `start`, the legacy subscription and cookie routes | [`v1`] |
//! | `download/*`, `audio_download/*`, `Range`, the JSON listing | [`files`] |
//! | `healthz`, `livez` | [`health`] |
//!
//! # BRIEF scope trims applied here
//!
//! - `GET <p>metrics` is **CUT**: the route exists and answers `404 not_found` so an operator's
//!   scrape config fails visibly rather than hanging, and no `metrics` crate is linked.
//! - The client → server `hello` **topic narrowing**, `ack`, `watch` and `unwatch` frames are
//!   **CUT** along with `ConnId` and the engine watch registry. `hello`, `ack`, `watch` and
//!   `unwatch` are still *accepted* (and answered with nothing) so a client written from
//!   PROTOCOL §5.11 is never disconnected for sending one; `ping`/`pong`, the `Lagged` resync,
//!   the lag budget, the frame-size cap and the client cap are all implemented.
//! - Because the snapshot carries every non-terminal record, children included,
//!   `truncated.groups` is always `[]` and `children_inline` is always `true` on a group.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod cors;
pub mod error;
pub mod files;
pub mod health;
pub mod trace;
pub mod v1;
pub mod v2;
pub mod view;
pub mod ws;

use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::{Arc, Mutex, RwLock};

use arc_swap::ArcSwap;
use aulos_core::{Clock, Config, HealthRegistry, SubscriptionsHandle, SystemClock, UnixMs};
use aulos_provider::Registry;
use aulos_queue::{EngineHandle, EventHub, StateView};
use aulos_store::Store;
use axum::Router;
use axum::routing::get;

pub use error::{ApiError, Json};

/// The `Sec-WebSocket-Protocol` value every v2 client offers (PROTOCOL §5.1).
pub const WS_SUBPROTOCOL: &str = auth::SUBPROTOCOL;

/// Build and runtime identity, for `capabilities`, `version` and `healthz`.
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
        Self {
            engine,
            state,
            hub,
            store,
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

/// The whole HTTP surface.
///
/// # The v1 seam
///
/// The v1 compatibility shim is WP-15's package (`src/v1/`). It mounts here, in exactly one place:
///
/// ```text
/// if state.cfg.v1_enabled {
///     router = router.merge(v1::router(state.clone()));
/// }
/// ```
///
/// Its routes are all disjoint from the ones below — v1 owns `<p>add`, `<p>history`, `<p>delete`,
/// `<p>start`, `<p>presets`, `<p>cancel-add`, `<p>subscribe`, `<p>subscriptions*`, the three cookie
/// routes and the `GET /` redirect, while `<p>version`, `<p>robots.txt`, `<p>`, `<p>socket.io/*`
/// and the file routes are served here for **both** protocol versions (PROTOCOL §10.1 lists them
/// under v1 because a v1 client uses them, not because the shim re-implements them).
pub fn router(state: ApiState) -> Router {
    let p = state.cfg.url_prefix.clone();
    let open: Router = Router::new()
        .route(&p.route(""), get(v2::meta::identity))
        .route(&p.route("version"), get(v2::meta::version))
        .route(&p.route("robots.txt"), get(v2::meta::robots))
        .route(&p.route("healthz"), get(health::healthz))
        .route(&p.route("livez"), get(health::livez))
        // v1.0: not implemented, see BRIEF — the Prometheus endpoint is CUT. The route stays so a
        // scrape config gets an honest 404 with the error envelope.
        .route(&p.route("metrics"), get(v2::meta::metrics_cut))
        .route(&p.route("socket.io/"), socketio_any())
        .route(&p.route("socket.io/{*rest}"), socketio_any())
        .with_state(state.clone());

    let guarded: Router = v2_router(state.clone())
        .merge(ws_router(state.clone()))
        .merge(files::router(state.clone()))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require,
        ));

    let mut router = open.merge(guarded);
    if let Some(layer) = cors::v2(&state.cfg.cors_allowed_origins) {
        router = router.layer(layer);
    }
    // WP-15's seam. Merged **after** the v2 CORS layer on purpose: `Router::layer` wraps only the
    // routes registered so far, so the v1 shim keeps legacy's own two-header CORS
    // (DESIGN §11.6) instead of inheriting v2's method/`Vary`/`Max-Age` set.
    if state.cfg.v1_enabled {
        router = router.merge(v1::router(state.clone()));
    }
    router.layer(axum::middleware::from_fn_with_state(state, trace::headers))
}

/// Every method on `socket.io/*` answers the same 501 (DESIGN §11.1).
fn socketio_any() -> axum::routing::MethodRouter<ApiState> {
    axum::routing::any(v2::meta::socketio_removed)
}

/// The `<p>api/v2/*` routes (PROTOCOL §4).
pub fn v2_router(state: ApiState) -> Router {
    v2::router(state)
}

/// The `<p>ws` route (PROTOCOL §5).
pub fn ws_router(state: ApiState) -> Router {
    ws::router(state)
}
