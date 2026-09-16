//! The `bgutil-pot` sidecar supervisor (DESIGN §16.2, BRIEF §14).
//!
//! Legacy launched the POT provider as an unsupervised `&` child of the entrypoint. When it died —
//! or, worse, when it wedged — yt-dlp started failing YouTube bot checks and *nothing anywhere
//! said so*: the container stayed healthy, the downloads just stopped working. That is the failure
//! this module exists to make visible.
//!
//! What it does, in one place:
//!
//! | Behaviour | Why |
//! |---|---|
//! | own process group (`process_group(0)`) | a `killpg` for a cancelled download can never reach the sidecar (§2.1, risk R9) |
//! | stdout → INFO, stderr → WARN, target `bgutil_pot` | the sidecar's own diagnostics reach the same log stream as everything else |
//! | backoff `1s, 2s, 4s … 60s` ±20 %, reset after 60 s of healthy uptime | a crash loop must not spin, but a one-off crash must recover fast |
//! | probe on spawn, then every 15 s, `GET {url}/ping` with a TCP-connect fallback | the provider's route set changes across versions, so a 404 is not "down" |
//! | **three consecutive probe failures force a restart** | a wedged sidecar is worse than a dead one, because a dead one restarts |
//! | `failed` after `AULOS_POT_MAX_RESTARTS` in 10 minutes | stop hammering; keep serving; say so in `healthz` |
//! | `SIGTERM` the group, 5 s, `SIGKILL` | the DESIGN §16.4 step 9 shutdown |
//!
//! The supervisor never fails the process: a POT-less server still downloads everything that is
//! not gated behind a YouTube bot check, and `healthz.components.pot` is where an operator finds
//! out.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::clock::{Clock, SystemClock};
use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, EventSender, Level, notice_code};
use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_core::id::UnixMs;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The `healthz` component key (DESIGN §16.3).
pub const COMPONENT: &str = "pot";

/// The `tracing` target the child's own output is logged under.
pub const LOG_TARGET: &str = "bgutil_pot";

/// How many consecutive probe failures force a restart of a still-running sidecar (DESIGN §16.2).
pub const PROBE_FAILURES_BEFORE_RESTART: u32 = 3;

/// The restart-budget window: `AULOS_POT_MAX_RESTARTS` restarts inside this are `failed`.
pub const RESTART_WINDOW: Duration = Duration::from_secs(600);

/// How long the sidecar must stay up for the backoff ladder to reset.
pub const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

/// The backoff ceiling.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The first backoff step.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The probe period.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Allow the newly spawned process to bind its listener before reporting a failed probe.
pub const STARTUP_RETRY_WINDOW: Duration = Duration::from_secs(5);

/// A hung probe must not leave the component starting indefinitely.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

const STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// `SIGTERM` → `SIGKILL` grace on shutdown and on a forced restart.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// What the supervisor believes about the sidecar right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PotStatus {
    /// `AULOS_POT_ENABLED=false`, or no command to run.
    Disabled,
    /// Spawned, no successful probe yet.
    Starting,
    /// Running and answering its probe.
    Up,
    /// Running, but its probe is failing. The next failure may force a restart.
    Degraded,
    /// Not running; a restart is scheduled.
    Down,
    /// `AULOS_POT_MAX_RESTARTS` reached inside [`RESTART_WINDOW`]; no more restarts.
    Failed,
}

impl PotStatus {
    /// The `healthz` status this maps onto.
    ///
    /// An unprobed sidecar is explicitly starting, without degrading the service.
    #[must_use]
    pub const fn component_status(self) -> ComponentStatus {
        match self {
            Self::Disabled => ComponentStatus::Disabled,
            Self::Up => ComponentStatus::Ok,
            Self::Starting => ComponentStatus::Starting,
            Self::Degraded => ComponentStatus::Degraded,
            Self::Down | Self::Failed => ComponentStatus::Down,
        }
    }
}

/// One probe attempt's outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProbeResult {
    /// Whether the sidecar answered.
    pub ok: bool,
    /// When, unix ms.
    pub at: UnixMs,
    /// The failure text, or `None`.
    pub detail: Option<Box<str>>,
}

/// The published sidecar state (DESIGN §16.2).
#[derive(Clone, Debug)]
pub struct PotState {
    /// What the supervisor believes.
    pub status: PotStatus,
    /// The child's pid, which is also its process-group id.
    pub pid: Option<u32>,
    /// Monotonic across the life of the process.
    pub restarts: u32,
    /// How the sidecar last stopped.
    pub last_exit: Option<Box<str>>,
    /// The last probe.
    pub last_probe: Option<ProbeResult>,
    /// When `status` last changed, unix ms.
    pub since: UnixMs,
    /// Why the sidecar is in this state, when there is more to say.
    pub detail: Option<Box<str>>,
    /// `AULOS_POT_URL`, echoed so `healthz` can name the endpoint.
    pub endpoint: Box<str>,
}

impl PotState {
    /// The initial state for a configured-but-not-yet-spawned sidecar.
    #[must_use]
    pub fn starting(endpoint: &str, now: UnixMs) -> Self {
        Self {
            status: PotStatus::Starting,
            pid: None,
            restarts: 0,
            last_exit: None,
            last_probe: None,
            since: now,
            detail: None,
            endpoint: endpoint.into(),
        }
    }

    /// The state for `AULOS_POT_ENABLED=false`.
    #[must_use]
    pub fn disabled(endpoint: &str, now: UnixMs) -> Self {
        Self {
            status: PotStatus::Disabled,
            pid: None,
            restarts: 0,
            last_exit: None,
            last_probe: None,
            since: now,
            detail: Some("AULOS_POT_ENABLED=false".into()),
            endpoint: endpoint.into(),
        }
    }

    /// `healthz.components.pot`, exactly the DESIGN §16.3 shape.
    #[must_use]
    pub fn component(&self) -> ComponentHealth {
        let mut c = ComponentHealth::new(self.status.component_status())
            .with("pid", self.pid)
            .with("restarts", self.restarts)
            .with("endpoint", self.endpoint.to_string());
        if let Some(exit) = &self.last_exit {
            c = c.with("last_exit", exit.to_string());
        }
        if let Some(detail) = &self.detail {
            c = c.with("detail", detail.to_string());
        }
        if let Some(probe) = &self.last_probe {
            c = c
                .with("last_probe_ok", probe.ok)
                .with("last_probe_at", probe.at);
        }
        c
    }
}

/// How the sidecar's liveness is checked.
///
/// A trait rather than a bare function so the acceptance tests can drive a wedged sidecar without
/// a network: "hangs while its probe fails three times" is the interesting case, and it cannot be
/// produced by a real `bgutil-pot`.
#[async_trait::async_trait]
pub trait PotProbe: Send + Sync + std::fmt::Debug {
    /// `Ok(())` when the sidecar answered; `Err(detail)` otherwise.
    async fn probe(&self, endpoint: &str) -> Result<(), String>;
}

/// The shipping probe: `GET {endpoint}/ping`, falling back to a plain TCP connect.
///
/// The fallback is not belt-and-braces: the provider has renamed and moved its status route
/// between releases, so a 404 means "this version does not have `/ping`", not "the sidecar is
/// down". A TCP connect to its host:port is the version-independent liveness signal, and treating
/// a 404 as fatal would restart a perfectly healthy sidecar every 45 seconds forever.
#[derive(Debug)]
pub struct HttpProbe {
    client: reqwest::Client,
}

impl HttpProbe {
    /// A probe with a 5 s timeout and no proxy (the sidecar is on the loopback).
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for HttpProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PotProbe for HttpProbe {
    async fn probe(&self, endpoint: &str) -> Result<(), String> {
        let url = format!("{}/ping", endpoint.trim_end_matches('/'));
        match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => {
                // Any answer at all proves the listener is alive; only a *transport* failure is a
                // failure. The status is kept in the detail so an operator can still see a 500.
                tcp_probe(endpoint)
                    .await
                    .map_err(|e| format!("GET /ping → {} and {e}", r.status()))
            }
            Err(e) => tcp_probe(endpoint)
                .await
                .map_err(|tcp| format!("GET /ping failed ({e}) and {tcp}")),
        }
    }
}

/// A plain TCP connect to the endpoint's host:port.
async fn tcp_probe(endpoint: &str) -> Result<(), String> {
    let parsed = url::Url::parse(endpoint).map_err(|e| format!("AULOS_POT_URL is invalid: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "AULOS_POT_URL has no host".to_owned())?;
    let port = parsed.port_or_known_default().unwrap_or(4416);
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("TCP connect to {addr} failed: {e}")),
        Err(_) => Err(format!("TCP connect to {addr} timed out")),
    }
}

/// The tunable half of the supervisor, so a test can run the whole ladder in milliseconds.
#[derive(Clone, Debug)]
pub struct PotSettings {
    /// `AULOS_POT_CMD`, as an argv.
    pub cmd: Vec<Box<str>>,
    /// `AULOS_POT_URL`.
    pub endpoint: Box<str>,
    /// `AULOS_POT_MAX_RESTARTS`.
    pub max_restarts: u32,
    /// How often to probe.
    pub probe_interval: Duration,
    /// Retry startup connection failures during this window before marking the child degraded.
    pub startup_retry_window: Duration,
    /// Upper bound on a probe, including startup retries.
    pub probe_timeout: Duration,
    /// The first backoff step.
    pub backoff_base: Duration,
    /// The backoff ceiling.
    pub backoff_max: Duration,
    /// How long "healthy" is, for the backoff reset.
    pub healthy_uptime: Duration,
    /// The restart-budget window.
    pub restart_window: Duration,
    /// `SIGTERM` → `SIGKILL` grace.
    pub kill_grace: Duration,
}

impl PotSettings {
    /// The production values.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            cmd: cfg.pot_cmd.clone(),
            endpoint: cfg.pot_url.clone(),
            max_restarts: cfg.pot_max_restarts,
            probe_interval: PROBE_INTERVAL,
            startup_retry_window: STARTUP_RETRY_WINDOW,
            probe_timeout: PROBE_TIMEOUT,
            backoff_base: BACKOFF_BASE,
            backoff_max: BACKOFF_MAX,
            healthy_uptime: HEALTHY_UPTIME,
            restart_window: RESTART_WINDOW,
            kill_grace: Duration::from_millis(cfg.kill_grace_ms.max(1)),
        }
    }
}

/// The `n`-th backoff delay, before jitter: `base * 2^n`, capped.
#[must_use]
pub fn backoff_delay(consecutive: u32, base: Duration, max: Duration) -> Duration {
    let shift = consecutive.min(16);
    let scaled = base.saturating_mul(1_u32 << shift);
    scaled.min(max)
}

/// ±20 % jitter around `d`, derived from the process's own clock so no RNG dependency is needed.
///
/// The exact distribution does not matter — the point is that two containers restarted together do
/// not retry in lockstep — and `rand` is a workspace dependency this crate has no other use for.
#[must_use]
pub fn jitter(d: Duration, seed: u64) -> Duration {
    if d.is_zero() {
        return d;
    }
    let millis = d.as_millis().min(u128::from(u64::MAX)) as u64;
    // seed → [0, 400] permille, i.e. ±20 %.
    let permille = 800 + (seed % 401);
    Duration::from_millis(millis.saturating_mul(permille) / 1000)
}

/// The supervisor handle (DESIGN §16.2).
///
/// Cheap to clone; the state is one `ArcSwap` load, so `healthz` never blocks on it.
#[derive(Clone, Debug)]
pub struct PotSupervisor {
    state: Arc<ArcSwap<PotState>>,
}

impl PotSupervisor {
    /// Spawns the supervisor with the production settings, the HTTP probe and the system clock.
    ///
    /// `AULOS_POT_ENABLED=false` (or an empty `AULOS_POT_CMD`) returns a handle whose state is
    /// [`PotStatus::Disabled`] and a task that returns immediately — so the caller has one code
    /// path and `healthz` still names the component.
    #[must_use]
    pub fn spawn(cfg: &Arc<Config>, health: Arc<HealthRegistry>) -> (Self, JoinHandle<()>) {
        Self::builder(PotSettings::from_config(cfg))
            .with_enabled(cfg.pot_enabled)
            .spawn_with(health)
    }

    /// A builder over explicit settings, for the wiring (which owns the shutdown token and the
    /// event sender) and for the tests.
    #[must_use]
    pub fn builder(settings: PotSettings) -> PotBuilder {
        PotBuilder {
            settings,
            enabled: true,
            probe: Arc::new(HttpProbe::new()),
            clock: Arc::new(SystemClock),
            shutdown: CancellationToken::new(),
            events: None,
        }
    }

    /// The current state. One atomic load.
    #[must_use]
    pub fn state(&self) -> Arc<PotState> {
        self.state.load_full()
    }

    /// The sidecar's pid, when it is running. The pgid-isolation regression test reads this.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.state().pid
    }
}

/// Builds a [`PotSupervisor`] with injected collaborators.
pub struct PotBuilder {
    settings: PotSettings,
    enabled: bool,
    probe: Arc<dyn PotProbe>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    events: Option<EventSender>,
}

impl std::fmt::Debug for PotBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PotBuilder")
            .field("settings", &self.settings)
            .field("enabled", &self.enabled)
            .field("probe", &self.probe)
            .finish_non_exhaustive()
    }
}

impl PotBuilder {
    /// `false` produces a disabled supervisor.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Replaces the liveness probe.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn PotProbe>) -> Self {
        self.probe = probe;
        self
    }

    /// Replaces the clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The token the DESIGN §16.4 shutdown cancels.
    #[must_use]
    pub fn with_shutdown(mut self, token: CancellationToken) -> Self {
        self.shutdown = token;
        self
    }

    /// Publishes a `pot_down` notice when the sidecar enters [`PotStatus::Failed`].
    #[must_use]
    pub fn with_events(mut self, events: EventSender) -> Self {
        self.events = Some(events);
        self
    }

    /// Spawns the loop and publishes into `health`.
    #[must_use]
    pub fn spawn_with(self, health: Arc<HealthRegistry>) -> (PotSupervisor, JoinHandle<()>) {
        let now = self.clock.now_ms();
        let disabled = !self.enabled || self.settings.cmd.is_empty();
        let initial = if disabled {
            PotState::disabled(&self.settings.endpoint, now)
        } else {
            PotState::starting(&self.settings.endpoint, now)
        };
        let state = Arc::new(ArcSwap::from_pointee(initial.clone()));
        health.set(COMPONENT, initial.component());

        let supervisor = PotSupervisor {
            state: Arc::clone(&state),
        };
        if disabled {
            tracing::info!("the bgutil-pot sidecar is disabled");
            return (supervisor, tokio::spawn(std::future::ready(())));
        }
        let task = Task {
            settings: self.settings,
            probe: self.probe,
            clock: self.clock,
            shutdown: self.shutdown,
            events: self.events,
            health,
            state,
        };
        (supervisor, tokio::spawn(task.run()))
    }
}

/// Why the inner supervision loop stopped watching one child.
#[derive(Debug)]
enum Stop {
    /// The child exited on its own.
    Exited(Box<str>),
    /// Three consecutive probe failures with the child still alive (DESIGN §16.2).
    Wedged(Box<str>),
    /// The process is shutting down.
    Shutdown,
}

/// The supervisor's own state, owned by its task.
struct Task {
    settings: PotSettings,
    probe: Arc<dyn PotProbe>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    events: Option<EventSender>,
    health: Arc<HealthRegistry>,
    state: Arc<ArcSwap<PotState>>,
}

impl Task {
    async fn run(self) {
        let mut restarts_in_window: Vec<std::time::Instant> = Vec::new();
        let mut consecutive_backoff: u32 = 0;
        let mut total_restarts: u32 = 0;

        loop {
            if self.shutdown.is_cancelled() {
                self.publish(PotStatus::Down, None, total_restarts, Some("shutting down"));
                return;
            }

            let mut child = match self.spawn_child() {
                Ok(child) => child,
                Err(e) => {
                    tracing::error!(error = %e, "could not spawn the bgutil-pot sidecar");
                    self.publish(
                        PotStatus::Down,
                        None,
                        total_restarts,
                        Some(&format!("spawn failed: {e}")),
                    );
                    if !self
                        .sleep_backoff(consecutive_backoff, total_restarts)
                        .await
                    {
                        return;
                    }
                    consecutive_backoff = consecutive_backoff.saturating_add(1);
                    total_restarts = total_restarts.saturating_add(1);
                    restarts_in_window.push(std::time::Instant::now());
                    if self.budget_exhausted(&mut restarts_in_window) {
                        self.enter_failed(total_restarts).await;
                        return;
                    }
                    continue;
                }
            };
            let pid = child.id();
            tracing::info!(pid, endpoint = %self.settings.endpoint, "bgutil-pot started");
            self.publish(PotStatus::Starting, pid, total_restarts, None);
            let started = std::time::Instant::now();

            let stop = self.watch(&mut child, pid, total_restarts).await;

            let uptime = started.elapsed();
            match &stop {
                Stop::Shutdown => {
                    self.terminate(&mut child, pid).await;
                    tracing::info!("bgutil-pot stopped for shutdown");
                    self.publish(
                        PotStatus::Down,
                        None,
                        total_restarts,
                        Some("stopped for shutdown"),
                    );
                    return;
                }
                Stop::Wedged(detail) => {
                    tracing::warn!(pid, %detail, "force-restarting a wedged bgutil-pot");
                    self.terminate(&mut child, pid).await;
                    self.record_exit(detail.clone(), total_restarts);
                }
                Stop::Exited(detail) => {
                    tracing::warn!(pid, %detail, "bgutil-pot exited");
                    self.record_exit(detail.clone(), total_restarts);
                }
            }

            // A sidecar that stayed up long enough is treated as a one-off crash, not a loop.
            if uptime >= self.settings.healthy_uptime {
                consecutive_backoff = 0;
            }

            total_restarts = total_restarts.saturating_add(1);
            restarts_in_window.push(std::time::Instant::now());
            if self.budget_exhausted(&mut restarts_in_window) {
                self.enter_failed(total_restarts).await;
                return;
            }
            if !self
                .sleep_backoff(consecutive_backoff, total_restarts)
                .await
            {
                return;
            }
            consecutive_backoff = consecutive_backoff.saturating_add(1);
        }
    }

    /// Watches one child until it exits, wedges, or shutdown is requested.
    async fn watch(&self, child: &mut Child, pid: Option<u32>, restarts: u32) -> Stop {
        let mut ticker = tokio::time::interval(self.settings.probe_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failures: u32 = 0;
        let mut starting = true;

        loop {
            let result = tokio::select! {
                status = child.wait() => {
                    let detail: Box<str> = match status {
                        Ok(s) => describe_exit(&s).into(),
                        Err(e) => format!("wait failed: {e}").into_boxed_str(),
                    };
                    return Stop::Exited(detail);
                }
                () = self.shutdown.cancelled() => return Stop::Shutdown,
                result = async {
                    ticker.tick().await;
                    self.probe_with_startup_retry(starting).await
                } => result,
            };
            if starting {
                starting = false;
                ticker.reset();
            }
            let at = self.clock.now_ms();
            match result {
                Ok(()) => {
                    failures = 0;
                    self.publish_probe(
                        PotStatus::Up,
                        pid,
                        restarts,
                        ProbeResult {
                            ok: true,
                            at,
                            detail: None,
                        },
                    );
                }
                Err(detail) => {
                    failures += 1;
                    tracing::warn!(pid, failures, %detail, "the bgutil-pot health probe failed");
                    let probe = ProbeResult {
                        ok: false,
                        at,
                        detail: Some(detail.into_boxed_str()),
                    };
                    self.publish_probe(PotStatus::Degraded, pid, restarts, probe);
                    if failures >= PROBE_FAILURES_BEFORE_RESTART {
                        return Stop::Wedged(
                            format!("{failures} consecutive probe failures").into_boxed_str(),
                        );
                    }
                }
            }
        }
    }

    async fn probe_with_startup_retry(&self, starting: bool) -> Result<(), String> {
        let probe = async {
            let started = tokio::time::Instant::now();
            loop {
                let result = self.probe.probe(&self.settings.endpoint).await;
                if result.is_ok()
                    || !starting
                    || started.elapsed() >= self.settings.startup_retry_window
                {
                    return result;
                }
                tokio::time::sleep(STARTUP_RETRY_INTERVAL).await;
            }
        };
        tokio::time::timeout(self.settings.probe_timeout, probe)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "POT probe timed out after {:?}",
                    self.settings.probe_timeout
                ))
            })
    }

    /// Spawns the sidecar in its own process group, with both output streams piped into `tracing`.
    fn spawn_child(&self) -> std::io::Result<Child> {
        let (program, args) = self
            .settings
            .cmd
            .split_first()
            .ok_or_else(|| std::io::Error::other("AULOS_POT_CMD is empty"))?;
        let mut cmd = Command::new(&**program);
        cmd.args(args.iter().map(|a| &**a))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Its own group leader, so a `killpg` for a download can never reach it (risk R9).
            .process_group(0);
        let mut child = cmd.spawn()?;
        if let Some(out) = child.stdout.take() {
            tokio::spawn(drain(out, tracing::Level::INFO));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(drain(err, tracing::Level::WARN));
        }
        Ok(child)
    }

    /// `SIGTERM` the group, wait the grace, then `SIGKILL` (DESIGN §16.2, §16.4 step 9).
    async fn terminate(&self, child: &mut Child, pid: Option<u32>) {
        if let Some(pid) = pid {
            signal_group(pid, nix::sys::signal::Signal::SIGTERM);
            if tokio::time::timeout(self.settings.kill_grace, child.wait())
                .await
                .is_err()
            {
                tracing::warn!(
                    pid,
                    "bgutil-pot ignored SIGTERM; sending SIGKILL to its group"
                );
                signal_group(pid, nix::sys::signal::Signal::SIGKILL);
                let _ = child.wait().await;
            }
        } else {
            let _ = child.kill().await;
        }
    }

    /// Records the exit reason without changing the restart counter.
    fn record_exit(&self, detail: Box<str>, restarts: u32) {
        let mut next = (*self.state.load_full()).clone();
        next.status = PotStatus::Down;
        next.pid = None;
        next.restarts = restarts;
        next.last_exit = Some(detail.clone());
        next.detail = Some(detail);
        next.since = self.clock.now_ms();
        self.store(next);
    }

    /// Whether the restart budget for [`PotSettings::restart_window`] is used up.
    fn budget_exhausted(&self, window: &mut Vec<std::time::Instant>) -> bool {
        let cutoff = self.settings.restart_window;
        window.retain(|at| at.elapsed() <= cutoff);
        u32::try_from(window.len()).unwrap_or(u32::MAX) >= self.settings.max_restarts.max(1)
    }

    /// Enters the terminal `failed` state: log, publish, notice, stop restarting.
    async fn enter_failed(&self, restarts: u32) {
        let detail = format!(
            "{restarts} restarts inside {}s; not restarting again. \
             YouTube downloads may fail bot checks. Check the sidecar with \
             `docker exec <container> bgutil-pot server` and see healthz.components.pot.",
            self.settings.restart_window.as_secs()
        );
        tracing::error!(restarts, "{detail}");
        self.publish(PotStatus::Failed, None, restarts, Some(&detail));
        if let Some(events) = &self.events {
            events
                .publish(DomainEvent::Notice {
                    level: Level::Error,
                    code: notice_code::POT_DOWN,
                    id: None,
                    message: detail.into_boxed_str(),
                })
                .await;
        }
    }

    /// Sleeps the backoff. Returns `false` when shutdown interrupted it.
    async fn sleep_backoff(&self, consecutive: u32, restarts: u32) -> bool {
        let base = backoff_delay(
            consecutive,
            self.settings.backoff_base,
            self.settings.backoff_max,
        );
        let delay = jitter(base, self.clock.now_ms().unsigned_abs());
        tracing::info!(
            delay_ms = delay.as_millis() as u64,
            restarts,
            "restarting bgutil-pot after backoff"
        );
        tokio::select! {
            () = self.shutdown.cancelled() => false,
            () = tokio::time::sleep(delay) => true,
        }
    }

    fn publish(&self, status: PotStatus, pid: Option<u32>, restarts: u32, detail: Option<&str>) {
        let mut next = (*self.state.load_full()).clone();
        if next.status != status {
            next.since = self.clock.now_ms();
        }
        next.status = status;
        next.pid = pid;
        next.restarts = restarts;
        next.detail = detail.map(Into::into);
        if status == PotStatus::Starting {
            next.last_probe = None;
        }
        self.store(next);
    }

    fn publish_probe(
        &self,
        status: PotStatus,
        pid: Option<u32>,
        restarts: u32,
        probe: ProbeResult,
    ) {
        let mut next = (*self.state.load_full()).clone();
        if next.status != status {
            next.since = self.clock.now_ms();
        }
        next.detail = probe.detail.clone();
        next.status = status;
        next.pid = pid;
        next.restarts = restarts;
        next.last_probe = Some(probe);
        self.store(next);
    }

    fn store(&self, next: PotState) {
        let component = next.component();
        self.state.store(Arc::new(next));
        self.health.set(COMPONENT, component);
    }
}

/// Forwards one of the child's output streams into `tracing`.
async fn drain<R: tokio::io::AsyncRead + Unpin>(reader: R, level: tracing::Level) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if level == tracing::Level::INFO {
            tracing::info!(target: LOG_TARGET, "{line}");
        } else {
            tracing::warn!(target: LOG_TARGET, "{line}");
        }
    }
}

/// `SIGTERM`/`SIGKILL` to a whole process group, ignoring "already gone".
fn signal_group(pid: u32, signal: nix::sys::signal::Signal) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    if let Err(e) = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), signal)
        && e != nix::errno::Errno::ESRCH
    {
        tracing::debug!(pid, ?signal, "killpg failed: {e}");
    }
}

/// The DESIGN §16.2 exit description: `exited with code 1` / `killed by signal 9`.
#[must_use]
pub fn describe_exit(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    if let Some(code) = status.code() {
        format!("exited with code {code}")
    } else if let Some(sig) = status.signal() {
        format!("killed by signal {sig}")
    } else {
        format!("exited: {status}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use aulos_core::config::RawEnv;

    use super::*;

    fn settings(cmd: &[&str]) -> PotSettings {
        PotSettings {
            cmd: cmd.iter().map(|s| Box::from(*s)).collect(),
            endpoint: "http://127.0.0.1:4416".into(),
            max_restarts: 10,
            probe_interval: Duration::from_millis(20),
            startup_retry_window: Duration::ZERO,
            probe_timeout: Duration::from_secs(30),
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(4),
            healthy_uptime: Duration::from_secs(3600),
            restart_window: Duration::from_secs(600),
            kill_grace: Duration::from_millis(200),
        }
    }

    /// A probe that answers from a script, then repeats its last answer.
    #[derive(Debug)]
    struct ScriptedProbe {
        answers: Vec<bool>,
        calls: AtomicU32,
    }

    impl ScriptedProbe {
        fn new(answers: Vec<bool>) -> Arc<Self> {
            Arc::new(Self {
                answers,
                calls: AtomicU32::new(0),
            })
        }
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl PotProbe for ScriptedProbe {
        async fn probe(&self, _endpoint: &str) -> Result<(), String> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed) as usize;
            let ok = self
                .answers
                .get(n)
                .copied()
                .or_else(|| self.answers.last().copied())
                .unwrap_or(true);
            if ok {
                Ok(())
            } else {
                Err("scripted failure".to_owned())
            }
        }
    }

    #[test]
    fn the_backoff_ladder_is_one_two_four_capped_at_sixty() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(60);
        let ladder: Vec<u64> = (0..9)
            .map(|n| backoff_delay(n, base, max).as_secs())
            .collect();
        assert_eq!(ladder, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        // The saturating shift must not panic or wrap for an absurd counter.
        assert_eq!(backoff_delay(u32::MAX, base, max), max);
    }

    #[test]
    fn the_jitter_stays_inside_plus_or_minus_twenty_percent() {
        let d = Duration::from_secs(10);
        for seed in [0_u64, 1, 7, 199, 400, 401, 999, u64::MAX] {
            let j = jitter(d, seed);
            assert!(
                j >= Duration::from_millis(8_000) && j <= Duration::from_millis(12_000),
                "seed {seed} produced {j:?}"
            );
        }
        assert_eq!(jitter(Duration::ZERO, 5), Duration::ZERO);
    }

    #[test]
    fn a_status_maps_onto_the_documented_healthz_status() {
        assert_eq!(
            PotStatus::Up.component_status(),
            ComponentStatus::Ok,
            "an answering sidecar is ok"
        );
        assert_eq!(
            PotStatus::Starting.component_status(),
            ComponentStatus::Starting
        );
        assert_eq!(
            PotStatus::Degraded.component_status(),
            ComponentStatus::Degraded,
            "a sidecar whose probe is failing is not yet down"
        );
        assert_eq!(PotStatus::Down.component_status(), ComponentStatus::Down);
        assert_eq!(PotStatus::Failed.component_status(), ComponentStatus::Down);
        assert_eq!(
            PotStatus::Disabled.component_status(),
            ComponentStatus::Disabled
        );
    }

    #[test]
    fn the_component_is_the_design_16_3_shape() {
        let state = PotState {
            status: PotStatus::Down,
            pid: None,
            restarts: 3,
            last_exit: Some("exited with code 1".into()),
            last_probe: None,
            since: 17,
            detail: Some("3 consecutive probe failures".into()),
            endpoint: "http://127.0.0.1:4416".into(),
        };
        let c = state.component();
        assert_eq!(c.status, ComponentStatus::Down);
        assert_eq!(c.detail["pid"], serde_json::Value::Null);
        assert_eq!(c.detail["restarts"], 3);
        assert_eq!(c.detail["endpoint"], "http://127.0.0.1:4416");
        assert_eq!(c.detail["last_exit"], "exited with code 1");
        assert_eq!(c.detail["detail"], "3 consecutive probe failures");
    }

    #[tokio::test]
    async fn a_disabled_supervisor_publishes_disabled_and_spawns_nothing() {
        let cfg = Arc::new(
            aulos_core::config::load(&RawEnv::from_pairs([("AULOS_POT_ENABLED", "false")]))
                .unwrap(),
        );
        let health = Arc::new(HealthRegistry::new());
        let (sup, task) = PotSupervisor::spawn(&cfg, Arc::clone(&health));
        task.await.unwrap();
        assert_eq!(sup.state().status, PotStatus::Disabled);
        assert_eq!(sup.pid(), None);
        assert_eq!(
            health.snapshot().components[COMPONENT].status,
            ComponentStatus::Disabled
        );
        // Disabled is not a failure, so the roll-up stays `ok`.
        assert_eq!(health.snapshot().status, ComponentStatus::Ok);
    }

    #[tokio::test]
    async fn a_sidecar_that_exits_immediately_is_restarted_until_the_budget_runs_out() {
        let health = Arc::new(HealthRegistry::new());
        let mut s = settings(&["/bin/sh", "-c", "exit 1"]);
        s.max_restarts = 3;
        let (sup, task) = PotSupervisor::builder(s)
            .with_probe(ScriptedProbe::new(vec![true]))
            .spawn_with(Arc::clone(&health));

        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("the supervisor must give up rather than loop forever")
            .unwrap();

        let state = sup.state();
        assert_eq!(state.status, PotStatus::Failed, "{state:?}");
        assert_eq!(state.restarts, 3);
        assert_eq!(
            state.last_exit.as_deref(),
            Some("exited with code 1"),
            "the exit reason must be reported verbatim"
        );
        let component = &health.snapshot().components[COMPONENT];
        assert_eq!(component.status, ComponentStatus::Down);
        assert!(
            component.detail["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("not restarting again"),
            "{component:?}"
        );
    }

    #[tokio::test]
    async fn a_hanging_sidecar_whose_probe_fails_three_times_is_force_restarted() {
        let health = Arc::new(HealthRegistry::new());
        let mut s = settings(&["/bin/sh", "-c", "sleep 30"]);
        s.max_restarts = 2; // one force-restart, then the budget stops the loop
        let probe = ScriptedProbe::new(vec![false]);
        let (sup, task) = PotSupervisor::builder(s)
            .with_probe(Arc::clone(&probe) as Arc<dyn PotProbe>)
            .spawn_with(Arc::clone(&health));

        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("a wedged sidecar must be force-restarted, not waited on")
            .unwrap();

        let state = sup.state();
        assert_eq!(state.status, PotStatus::Failed, "{state:?}");
        assert_eq!(
            state.last_exit.as_deref(),
            Some("3 consecutive probe failures"),
            "the force-restart reason must say so"
        );
        assert!(
            probe.calls() >= PROBE_FAILURES_BEFORE_RESTART,
            "the probe ran {} times",
            probe.calls()
        );
    }

    #[tokio::test]
    async fn a_healthy_sidecar_stays_up_and_reports_ok() {
        let health = Arc::new(HealthRegistry::new());
        let shutdown = CancellationToken::new();
        let (sup, task) = PotSupervisor::builder(settings(&["/bin/sh", "-c", "sleep 30"]))
            .with_probe(ScriptedProbe::new(vec![true]))
            .with_shutdown(shutdown.clone())
            .spawn_with(Arc::clone(&health));

        // Wait for the first successful probe.
        let up = wait_for(&sup, PotStatus::Up).await;
        assert!(up, "the sidecar never reported up: {:?}", sup.state());
        let pid = sup.pid().expect("a running sidecar has a pid");
        assert_eq!(
            health.snapshot().components[COMPONENT].status,
            ComponentStatus::Ok
        );

        // The pgid-isolation regression (risk R9): the sidecar is its own group leader, so a
        // `killpg` aimed at a download's group cannot reach it.
        let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(
            i32::try_from(pid).unwrap(),
        )))
        .expect("the child must have a process group");
        assert_eq!(
            pgid.as_raw(),
            i32::try_from(pid).unwrap(),
            "the sidecar must be its own process-group leader"
        );

        let mut victim = tokio::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let victim_pid = victim.id().unwrap();
        signal_group(victim_pid, nix::sys::signal::Signal::SIGKILL);
        let _ = victim.wait().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            sup.pid(),
            Some(pid),
            "killing another process group must not touch the sidecar"
        );

        // Shutdown terminates it.
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("shutdown must stop the supervisor")
            .unwrap();
        assert_eq!(sup.state().status, PotStatus::Down);
        assert_eq!(sup.pid(), None);
        // The process really is gone: `kill -0` on the group fails with ESRCH.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let alive = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
            None,
        );
        assert!(alive.is_err(), "the sidecar's group must be gone");
    }

    #[tokio::test]
    async fn startup_probes_immediately_and_brief_connection_failures_stay_neutral() {
        for answers in [vec![true], vec![false, false, true]] {
            let health = Arc::new(HealthRegistry::new());
            let shutdown = CancellationToken::new();
            let mut s = settings(&["/bin/sh", "-c", "sleep 30"]);
            s.probe_interval = PROBE_INTERVAL;
            s.startup_retry_window = STARTUP_RETRY_WINDOW;
            let probe = ScriptedProbe::new(answers);
            let (sup, task) = PotSupervisor::builder(s)
                .with_probe(Arc::clone(&probe) as Arc<dyn PotProbe>)
                .with_shutdown(shutdown.clone())
                .spawn_with(Arc::clone(&health));

            assert_eq!(sup.state().status, PotStatus::Starting);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let snapshot = health.snapshot();
                    assert_eq!(snapshot.status, ComponentStatus::Ok);
                    assert!(matches!(
                        snapshot.components[COMPONENT].status,
                        ComponentStatus::Starting | ComponentStatus::Ok
                    ));
                    if sup.state().status == PotStatus::Up {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("startup must not wait for the 15-second probe interval");
            let calls = probe.calls();
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(
                probe.calls(),
                calls,
                "resume the normal cadence after startup"
            );
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    #[derive(Debug)]
    struct PendingProbe;

    #[async_trait::async_trait]
    impl PotProbe for PendingProbe {
        async fn probe(&self, _endpoint: &str) -> Result<(), String> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn an_unanswered_startup_probe_expires_and_does_not_block_shutdown() {
        for timeout in [Duration::from_millis(50), PROBE_TIMEOUT] {
            let health = Arc::new(HealthRegistry::new());
            let shutdown = CancellationToken::new();
            let mut s = settings(&["/bin/sh", "-c", "sleep 30"]);
            s.probe_interval = PROBE_INTERVAL;
            s.probe_timeout = timeout;
            let (sup, task) = PotSupervisor::builder(s)
                .with_probe(Arc::new(PendingProbe))
                .with_shutdown(shutdown.clone())
                .spawn_with(Arc::clone(&health));
            if timeout < PROBE_TIMEOUT {
                assert!(wait_for(&sup, PotStatus::Degraded).await);
                assert_eq!(health.snapshot().status, ComponentStatus::Degraded);
                let state = sup.state();
                let probe = state.last_probe.as_ref().unwrap();
                assert!(!probe.ok);
                assert!(probe.detail.as_deref().unwrap().contains("timed out"));
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(sup.state().status, PotStatus::Starting);
            }
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("shutdown must interrupt a pending probe")
                .unwrap();
        }
    }

    #[derive(Debug, Default)]
    struct RestartProbe {
        calls: AtomicU32,
        ready: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl PotProbe for RestartProbe {
        async fn probe(&self, _endpoint: &str) -> Result<(), String> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err("first child failed".into());
            }
            self.ready.notified().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_restarted_child_starts_neutral_without_the_previous_probe_result() {
        let health = Arc::new(HealthRegistry::new());
        let shutdown = CancellationToken::new();
        let mut s = settings(&["/bin/sh", "-c", "sleep 30"]);
        s.probe_interval = PROBE_INTERVAL;
        let probe = Arc::new(RestartProbe::default());
        let (sup, task) = PotSupervisor::builder(s)
            .with_probe(Arc::clone(&probe) as Arc<dyn PotProbe>)
            .with_shutdown(shutdown.clone())
            .spawn_with(Arc::clone(&health));
        assert!(wait_for(&sup, PotStatus::Degraded).await);
        signal_group(sup.pid().unwrap(), nix::sys::signal::Signal::SIGKILL);
        assert!(wait_for(&sup, PotStatus::Starting).await);
        assert_eq!(sup.state().restarts, 1);
        assert!(sup.state().last_probe.is_none());
        assert!(sup.state().detail.is_none());
        let snapshot = health.snapshot();
        assert_eq!(snapshot.status, ComponentStatus::Ok);
        assert_eq!(
            snapshot.components[COMPONENT].status,
            ComponentStatus::Starting
        );
        assert!(
            !snapshot.components[COMPONENT]
                .detail
                .contains_key("last_probe_ok")
        );
        probe.ready.notify_one();
        assert!(wait_for(&sup, PotStatus::Up).await);
        shutdown.cancel();
        task.await.unwrap();
    }

    async fn wait_for(sup: &PotSupervisor, want: PotStatus) -> bool {
        for _ in 0..200 {
            if sup.state().status == want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[test]
    fn the_settings_come_from_the_env_table() {
        let cfg = aulos_core::config::load(&RawEnv::from_pairs([
            ("AULOS_POT_CMD", "bgutil-pot server --port 4416"),
            ("AULOS_POT_URL", "http://127.0.0.1:9999"),
            ("AULOS_POT_MAX_RESTARTS", "4"),
        ]))
        .unwrap();
        let s = PotSettings::from_config(&cfg);
        assert_eq!(
            s.cmd.iter().map(|c| &**c).collect::<Vec<_>>(),
            ["bgutil-pot", "server", "--port", "4416"]
        );
        assert_eq!(&*s.endpoint, "http://127.0.0.1:9999");
        assert_eq!(s.max_restarts, 4);
        assert_eq!(s.probe_interval, PROBE_INTERVAL);
        assert_eq!(s.startup_retry_window, STARTUP_RETRY_WINDOW);
        assert_eq!(s.probe_timeout, PROBE_TIMEOUT);
    }

    #[tokio::test]
    async fn the_tcp_fallback_answers_for_a_listener_with_no_ping_route() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and immediately drop: a connect is all the fallback needs.
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        let endpoint = format!("http://{addr}");
        assert!(
            tcp_probe(&endpoint).await.is_ok(),
            "a bound port must satisfy the fallback"
        );
        assert!(
            HttpProbe::new().probe(&endpoint).await.is_ok(),
            "no /ping route must still be reported up"
        );
    }

    #[tokio::test]
    async fn a_closed_port_fails_both_halves_of_the_probe() {
        // Port 1 rather than a bound-then-released ephemeral port: the OS hands ephemeral ports
        // back out immediately, so a concurrent test that binds one can make "almost certainly
        // free" false — which it did.
        let endpoint = "http://127.0.0.1:1";
        assert!(tcp_probe(endpoint).await.is_err());
        assert!(HttpProbe::new().probe(endpoint).await.is_err());
    }

    #[test]
    fn an_exit_is_described_the_way_design_16_2_writes_it() {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            describe_exit(&std::process::ExitStatus::from_raw(1 << 8)),
            "exited with code 1"
        );
        assert_eq!(
            describe_exit(&std::process::ExitStatus::from_raw(9)),
            "killed by signal 9"
        );
    }
}
