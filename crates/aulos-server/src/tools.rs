//! External-tool probes (DESIGN §16.1 step 9, §16.3, §3.1 `doctor`).
//!
//! Two consumers share this module, which is why it is not inside [`crate::doctor`]:
//!
//! - the boot sequence, where `python3` + `yt-dlp` are **fatal** (the `ytdlp` provider is the
//!   fallback for every URL, so without them the server can download nothing at all) and
//!   `ffmpeg`, `ffprobe`, `N_m3u8DL-RE` and `deno` are warnings that mark their `healthz`
//!   component degraded;
//! - `aulos-server doctor`, which prints the same table and exits non-zero on a missing
//!   *required* tool.
//!
//! The yt-dlp probe deliberately goes through the Python shim's own `mode = selftest` rather than
//! `python3 -c "import yt_dlp"`: the shim is what production runs, its handshake reports the
//! loaded plugin list (which is how "is the POT plugin actually installed?" becomes observable),
//! and a shim that cannot start is exactly as fatal as a missing interpreter.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, EventSender};
use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_provider::sink::ProgressSinkFactory;
use aulos_provider_ytdlp::runner::{DEFAULT_PYTHON, DEFAULT_RUNNER_PATH};
use aulos_provider_ytdlp::{Job, RunnerHandle, RunnerOutcome, ShimIdentity};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// How long a `--version` probe is given before it is treated as slow.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the shim handshake is given.
pub const SHIM_TIMEOUT: Duration = Duration::from_secs(60);

/// One optional external tool: the `healthz` component name and how to ask for its version.
#[derive(Clone, Copy, Debug)]
pub struct ToolSpec {
    /// The `healthz` component key (DESIGN §16.3), e.g. `nm3u8dl`.
    pub component: &'static str,
    /// The program name, looked up on `PATH`.
    pub program: &'static str,
    /// The argument that makes it print a version.
    pub arg: &'static str,
}

/// The optional tools of DESIGN §16.1 step 9, in `healthz` order.
///
/// `bgutil-pot` is **not** here: its health is the supervisor's `pot` component (DESIGN §16.2),
/// which reports a live pid and a live probe rather than a version string. [`doctor_tools`] adds
/// it, because `doctor` runs with no supervisor.
pub const OPTIONAL_TOOLS: &[ToolSpec] = &[
    ToolSpec {
        component: "ffmpeg",
        program: "ffmpeg",
        arg: "-version",
    },
    ToolSpec {
        component: "ffprobe",
        program: "ffprobe",
        arg: "-version",
    },
    ToolSpec {
        component: "nm3u8dl",
        program: "N_m3u8DL-RE",
        arg: "--version",
    },
    ToolSpec {
        component: "deno",
        program: "deno",
        arg: "--version",
    },
];

/// What `doctor` probes: [`OPTIONAL_TOOLS`] plus the POT sidecar binary.
#[must_use]
pub fn doctor_tools() -> Vec<ToolSpec> {
    let mut v = OPTIONAL_TOOLS.to_vec();
    v.push(ToolSpec {
        component: "bgutil_pot",
        program: "bgutil-pot",
        arg: "--version",
    });
    v
}

/// The result of one probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The tool ran and reported this version line.
    Ok(String),
    /// The tool could not be spawned because it is absent or not executable.
    Missing(String),
    /// The tool did not answer before the deadline.
    Slow(String),
    /// The tool failed to run or exited unsuccessfully.
    Failed(String),
}

impl Probe {
    /// Whether the tool answered.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Whether a background probe may recover this failure.
    #[must_use]
    pub const fn should_retry(&self) -> bool {
        matches!(self, Self::Slow(_) | Self::Failed(_))
    }

    /// The version line, or the failure text.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Ok(v) | Self::Missing(v) | Self::Slow(v) | Self::Failed(v) => v,
        }
    }

    /// The `healthz` component for this probe: `ok` with a `version`, or **`degraded`** with a
    /// `detail`.
    ///
    /// `degraded`, not `down`, is DESIGN §16.1 step 9's own word for these four: "ffmpeg, ffprobe,
    /// N_m3u8DL-RE, deno are WARN and **mark the component degraded**". It is also the only answer
    /// that keeps the container alive: [`aulos_core::HealthRegistry`] rolls the whole view up to
    /// the *worst* component, so one `down` optional tool would make `healthz` report
    /// `"status":"down"` — and a missing `deno` must not get a server that is downloading fine
    /// restarted every two minutes.
    #[must_use]
    pub fn component(&self) -> ComponentHealth {
        match self {
            Self::Ok(v) => ComponentHealth::new(ComponentStatus::Ok).with("version", v.clone()),
            Self::Missing(why) | Self::Slow(why) | Self::Failed(why) => {
                ComponentHealth::new(ComponentStatus::Degraded).with("detail", why.clone())
            }
        }
    }

    fn retry_component(&self) -> ComponentHealth {
        let component = self.component();
        if self.should_retry() {
            component.with(
                "detail",
                format!("{}; retrying in the background", self.detail()),
            )
        } else {
            component
        }
    }
}

/// Runs `<program> <arg>` and keeps the first non-empty line of its output.
///
/// stdout and stderr are both considered, because `ffmpeg -version` prints to stdout while some
/// builds of `N_m3u8DL-RE` print to stderr, and "which stream did it use" is not information worth
/// encoding per tool.
pub async fn probe_tool(spec: &ToolSpec) -> Probe {
    probe_tool_with_timeout(spec, VERSION_TIMEOUT).await
}

async fn probe_tool_with_timeout(spec: &ToolSpec, timeout: Duration) -> Probe {
    let mut cmd = Command::new(spec.program);
    cmd.arg(spec.arg)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let detail = format!("{}: {e}", spec.program);
            return match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    Probe::Missing(detail)
                }
                _ => Probe::Failed(detail),
            };
        }
    };
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Probe::Failed(format!("{}: {e}", spec.program)),
        Err(_) => {
            return Probe::Slow(format!(
                "{} {} timed out after {timeout:?}",
                spec.program, spec.arg,
            ));
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    let line = first_line(&text)
        .or_else(|| first_line(&err))
        .unwrap_or_default();
    if output.status.success() {
        Probe::Ok(line)
    } else {
        Probe::Failed(format!(
            "{} exited with {}: {line}",
            spec.program, output.status
        ))
    }
}

/// Probes every tool in `specs`, concurrently.
pub async fn probe_all(specs: &[ToolSpec]) -> Vec<(&'static str, Probe)> {
    futures_util::future::join_all(
        specs
            .iter()
            .map(|spec| async move { (spec.component, probe_tool(spec).await) }),
    )
    .await
}

/// Re-probes a transient boot failure until it recovers, becomes missing, or shuts down.
pub async fn retry_tool(
    spec: ToolSpec,
    health: Arc<HealthRegistry>,
    events: EventSender,
    shutdown: CancellationToken,
) {
    retry_probe(spec.component, health, events, shutdown, || {
        probe_tool(&spec)
    })
    .await;
}

async fn retry_probe<F, Fut>(
    component: &str,
    health: Arc<HealthRegistry>,
    events: EventSender,
    shutdown: CancellationToken,
    mut probe: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Probe>,
{
    let retries = async {
        for seconds in [5, 15, 30, 60].into_iter().chain(std::iter::repeat(300)) {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            let result = probe().await;
            if health.set(component, result.retry_component()) {
                events
                    .publish(DomainEvent::HealthChanged(health.snapshot()))
                    .await;
            }
            if !result.should_retry() {
                if result.is_ok() {
                    tracing::info!(
                        component,
                        version = result.detail(),
                        "external tool probe recovered"
                    );
                } else {
                    tracing::warn!(
                        component,
                        detail = result.detail(),
                        "external tool is missing; stopping retries"
                    );
                }
                return;
            }
        }
    };
    tokio::select! {
        biased;
        () = shutdown.cancelled() => {},
        () = retries => {},
    }
}

/// The first non-empty, trimmed line of `text`.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

/// The interpreter and shim paths a probe should use.
///
/// The image ships `python3 /app/python/ytdlp_runner.py`; a developer running the binary from a
/// checkout has neither, so the shim next to this source tree is tried as well. Nothing in
/// production depends on the second candidate — it exists so `doctor` is useful outside a
/// container.
#[must_use]
pub fn shim_paths() -> (PathBuf, PathBuf) {
    let image = PathBuf::from(DEFAULT_RUNNER_PATH);
    if image.is_file() {
        return (PathBuf::from(DEFAULT_PYTHON), image);
    }
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../aulos-provider-ytdlp/python/ytdlp_runner.py");
    if local.is_file() {
        return (PathBuf::from(DEFAULT_PYTHON), local);
    }
    (PathBuf::from(DEFAULT_PYTHON), image)
}

/// What the shim handshake reported, or why it could not be run.
#[derive(Clone, Debug)]
pub enum ShimProbe {
    /// The shim started and answered a `selftest`.
    Ok(Box<ShimIdentity>),
    /// The shim could not be run, or reported no yt-dlp.
    Failed(String),
}

impl ShimProbe {
    /// The yt-dlp version, when there is one.
    #[must_use]
    pub fn yt_dlp(&self) -> Option<&str> {
        match self {
            Self::Ok(id) => id.yt_dlp.as_deref(),
            Self::Failed(_) => None,
        }
    }

    /// `healthz.components.ytdlp_runner` (DESIGN §16.3).
    #[must_use]
    pub fn component(&self) -> ComponentHealth {
        match self {
            Self::Ok(id) => ComponentHealth::new(ComponentStatus::Ok)
                .with("python", id.python.clone())
                .with("yt_dlp", id.yt_dlp.clone())
                .with("plugins", id.plugins.clone())
                .with("pot_available", id.pot_available),
            Self::Failed(why) => {
                ComponentHealth::new(ComponentStatus::Down).with("detail", why.clone())
            }
        }
    }
}

/// Runs the Python shim's `mode = selftest` once.
///
/// This is the DESIGN §16.1 step 9 fatal probe *and* the source of `healthz.yt_dlp`,
/// `capabilities.yt_dlp` and `GET <p>version`'s `yt-dlp` (the WP-14 request in
/// `docs/INTEGRATION-NOTES.md`).
pub async fn probe_shim(cfg: &Config, python: PathBuf, runner: PathBuf) -> ShimProbe {
    // A detached sink: a selftest reports no progress, and there is no item to report it for.
    let (factory, rx) = ProgressSinkFactory::channel();
    let sink = factory.for_item(aulos_core::ItemId::new());
    drop(rx);

    let handle = RunnerHandle::from_config(cfg, python, runner).with_timeout(Some(SHIM_TIMEOUT));
    let job = Job::selftest("boot-probe");
    match handle.run(&job, &sink, &CancellationToken::new()).await {
        Ok(RunnerOutcome::Selftest(identity)) => {
            if identity.yt_dlp.is_some() {
                ShimProbe::Ok(Box::new(identity))
            } else {
                ShimProbe::Failed(
                    "the yt-dlp shim started but reported no yt-dlp version".to_owned(),
                )
            }
        }
        Ok(other) => ShimProbe::Failed(format!("the shim answered a selftest with {other:?}")),
        Err(e) => ShimProbe::Failed(e.message().clone()),
    }
}

/// The whole probe set, for the boot sequence and for `doctor`.
#[derive(Debug)]
pub struct Report {
    /// The Python shim handshake — required.
    pub shim: ShimProbe,
    /// One entry per optional tool, in [`OPTIONAL_TOOLS`] order.
    pub tools: Vec<(&'static str, Probe)>,
}

impl Report {
    /// Whether every **required** probe succeeded.
    #[must_use]
    pub fn required_ok(&self) -> bool {
        matches!(self.shim, ShimProbe::Ok(_))
    }

    /// The optional tools that did not answer successfully, by component name.
    #[must_use]
    pub fn degraded_optional(&self) -> Vec<&'static str> {
        self.tools
            .iter()
            .filter(|(_, p)| !p.is_ok())
            .map(|(name, _)| *name)
            .collect()
    }

    /// Optional boot probes that should be retried after the server starts.
    pub fn retryable_tools(&self) -> impl Iterator<Item = &'static ToolSpec> + '_ {
        OPTIONAL_TOOLS.iter().filter(|spec| {
            self.tools
                .iter()
                .any(|(name, probe)| *name == spec.component && probe.should_retry())
        })
    }

    /// The printable table `doctor` and the boot log both use.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(512);
        let _ = writeln!(out, "required:");
        match &self.shim {
            ShimProbe::Ok(id) => {
                let _ = writeln!(
                    out,
                    "  python3          ok    {}",
                    id.python.as_deref().unwrap_or("?")
                );
                let _ = writeln!(
                    out,
                    "  yt-dlp           ok    {}",
                    id.yt_dlp.as_deref().unwrap_or("?")
                );
                let _ = writeln!(
                    out,
                    "  yt-dlp plugins   ok    {}",
                    if id.plugins.is_empty() {
                        "(none)".to_owned()
                    } else {
                        id.plugins.join(", ")
                    }
                );
            }
            ShimProbe::Failed(why) => {
                let _ = writeln!(out, "  python3 + yt-dlp MISSING  {why}");
            }
        }
        let _ = writeln!(out, "optional:");
        for (name, probe) in &self.tools {
            let (mark, detail) = match probe {
                Probe::Ok(v) => ("ok   ", v.as_str()),
                Probe::Missing(why) => ("MISSING", why.as_str()),
                Probe::Slow(why) => ("SLOW", why.as_str()),
                Probe::Failed(why) => ("FAILED", why.as_str()),
            };
            let _ = writeln!(out, "  {name:<16} {mark} {detail}");
        }
        out
    }
}

/// Runs every probe.
pub async fn probe_everything(cfg: &Config, specs: &[ToolSpec]) -> Report {
    let (python, runner) = shim_paths();
    let (shim, tools) = tokio::join!(probe_shim(cfg, python, runner), probe_all(specs));
    Report { shim, tools }
}

/// Publishes the probe results into the health registry (DESIGN §16.1 step 9, §16.3).
pub fn publish(report: &Report, health: &aulos_core::HealthRegistry) {
    health.set("ytdlp_runner", report.shim.component());
    for (name, probe) in &report.tools {
        health.set(name, probe.retry_component());
    }
}

/// The `Arc` form the wiring passes around.
pub type SharedReport = Arc<Report>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_optional_set_is_exactly_the_design_16_1_list() {
        let names: Vec<&str> = OPTIONAL_TOOLS.iter().map(|t| t.component).collect();
        assert_eq!(names, ["ffmpeg", "ffprobe", "nm3u8dl", "deno"]);
        // `doctor` adds the sidecar binary, which has no `healthz` component of its own.
        assert!(
            doctor_tools()
                .iter()
                .any(|t| t.program == "bgutil-pot" && t.arg == "--version")
        );
    }

    #[tokio::test]
    async fn a_missing_tool_is_degraded_with_a_detail_and_never_retried() {
        let spec = ToolSpec {
            component: "nope",
            program: "aulos-no-such-tool-cf4a1",
            arg: "--version",
        };
        let probe = probe_tool(&spec).await;
        assert!(matches!(probe, Probe::Missing(_)), "{probe:?}");
        assert!(!probe.should_retry());
        let component = probe.component();
        assert_eq!(
            component.status,
            ComponentStatus::Degraded,
            "DESIGN §16.1 step 9: an optional tool marks its component degraded, not down -- \
             `HealthRegistry` rolls up to the worst component, so `down` here would make the \
             container unhealthy over a missing deno"
        );
        assert!(component.detail.contains_key("detail"), "{component:?}");
        assert!(!component.detail.contains_key("version"), "{component:?}");
    }

    #[tokio::test]
    async fn a_present_tool_reports_its_first_output_line() {
        // `/bin/echo` exists on every platform this ships to and answers instantly.
        let spec = ToolSpec {
            component: "echo",
            program: "/bin/echo",
            arg: "version 1.2.3",
        };
        let probe = probe_tool(&spec).await;
        assert_eq!(probe, Probe::Ok("version 1.2.3".to_owned()));
        assert_eq!(probe.component().status, ComponentStatus::Ok);
    }

    #[tokio::test]
    async fn a_timeout_is_slow_and_retryable() {
        let spec = ToolSpec {
            component: "sleep",
            program: "/bin/sleep",
            arg: "30",
        };
        let probe = probe_tool_with_timeout(&spec, Duration::from_millis(50)).await;
        assert!(matches!(probe, Probe::Slow(_)), "{probe:?}");
        assert!(probe.should_retry());
        assert_eq!(probe.component().status, ComponentStatus::Degraded);
        assert!(probe.detail().contains("timed out after 50ms"));
        assert!(!probe.component().detail.contains_key("version"));
    }

    #[tokio::test]
    async fn permission_denied_is_missing_and_not_retried() {
        let spec = ToolSpec {
            component: "directory",
            program: "/",
            arg: "--version",
        };
        let probe = probe_tool(&spec).await;
        assert!(matches!(probe, Probe::Missing(_)), "{probe:?}");
        assert!(!probe.should_retry());
    }

    #[tokio::test]
    async fn an_unsuccessful_exit_is_failed_even_when_it_prints_output() {
        let spec = ToolSpec {
            component: "ls",
            program: "/bin/ls",
            arg: "/aulos-no-such-file-cf4a1",
        };
        let probe = probe_tool(&spec).await;
        assert!(matches!(probe, Probe::Failed(_)), "{probe:?}");
        assert!(probe.should_retry());
        assert!(probe.detail().contains("exited with"));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_back_off_until_recovery_and_publish_only_changed_health() {
        use aulos_core::event::{EventRouter, SubscriberSpec};

        let health = Arc::new(HealthRegistry::new());
        let slow = Probe::Slow("ffmpeg -version timed out after 30s".into());
        health.set("ffmpeg", slow.retry_component());
        let (mut router, events) = EventRouter::new(16);
        let mut inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();
        let start = tokio::time::Instant::now();
        let mut attempts = Vec::new();
        retry_probe(
            "ffmpeg",
            Arc::clone(&health),
            events,
            CancellationToken::new(),
            || {
                attempts.push(start.elapsed().as_secs());
                std::future::ready(if attempts.len() < 7 {
                    slow.clone()
                } else {
                    Probe::Ok("ffmpeg version 7.1.5".into())
                })
            },
        )
        .await;
        assert_eq!(attempts, [5, 20, 50, 110, 410, 710, 1010]);
        router_task.await.unwrap();
        let event = inbox.recv().await.unwrap();
        let DomainEvent::HealthChanged(view) = event.as_ref() else {
            panic!("{event:?}")
        };
        assert_eq!(view.components["ffmpeg"].status, ComponentStatus::Ok);
        assert_eq!(
            view.components["ffmpeg"].detail["version"],
            "ffmpeg version 7.1.5"
        );
        assert!(!view.components["ffmpeg"].detail.contains_key("detail"));
        assert_eq!(view.components, health.snapshot().components);
        assert!(
            inbox.recv().await.is_none(),
            "unchanged failures should not publish health events"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_stops_if_the_tool_becomes_missing() {
        let health = Arc::new(HealthRegistry::new());
        let (router, events) = aulos_core::event::EventRouter::new(16);
        let router_task = router.spawn();
        let mut attempts = 0;
        retry_probe(
            "ffprobe",
            Arc::clone(&health),
            events,
            CancellationToken::new(),
            || {
                attempts += 1;
                assert_eq!(attempts, 1);
                std::future::ready(Probe::Missing("ffprobe: not found".into()))
            },
        )
        .await;
        router_task.await.unwrap();
        let component = health.snapshot().components["ffprobe"].clone();
        assert_eq!(component.status, ComponentStatus::Degraded);
        assert_eq!(component.detail["detail"], "ffprobe: not found");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_backoff_and_in_flight_probes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        for stop_after in [2, 6] {
            let health = Arc::new(HealthRegistry::new());
            let (router, events) = aulos_core::event::EventRouter::new(16);
            let router_task = router.spawn();
            let shutdown = CancellationToken::new();
            let token = shutdown.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(stop_after)).await;
                token.cancel();
            });
            let started = Arc::new(AtomicBool::new(false));
            let dropped = Arc::new(AtomicBool::new(false));
            retry_probe("ffmpeg", health, events, shutdown, || {
                let started = Arc::clone(&started);
                let guard = Dropped(Arc::clone(&dropped));
                async move {
                    let _guard = guard;
                    started.store(true, Ordering::SeqCst);
                    std::future::pending::<Probe>().await
                }
            })
            .await;
            router_task.await.unwrap();
            assert_eq!(started.load(Ordering::SeqCst), stop_after > 5);
            assert_eq!(dropped.load(Ordering::SeqCst), stop_after > 5);
        }
    }

    #[test]
    fn the_first_line_skips_leading_blanks_and_trims() {
        assert_eq!(
            first_line("\n\n  hello  \nworld\n").as_deref(),
            Some("hello")
        );
        assert_eq!(first_line("   \n \n"), None);
    }

    #[test]
    fn the_rendered_table_names_every_probe_and_marks_the_failures() {
        let report = Report {
            shim: ShimProbe::Failed("python3 is not installed".to_owned()),
            tools: vec![
                ("ffmpeg", Probe::Ok("ffmpeg version 6.1.1".to_owned())),
                ("deno", Probe::Missing("deno: not found".to_owned())),
                (
                    "ffprobe",
                    Probe::Slow("ffprobe -version timed out after 30s".into()),
                ),
                ("nm3u8dl", Probe::Failed("temporary spawn failure".into())),
            ],
        };
        let table = report.render();
        assert!(table.contains("python3 + yt-dlp MISSING"), "{table}");
        assert!(table.contains("ffmpeg version 6.1.1"), "{table}");
        assert!(table.contains("deno"), "{table}");
        assert!(table.contains("ffprobe          SLOW"), "{table}");
        assert!(table.contains("nm3u8dl          FAILED"), "{table}");
        assert!(!report.required_ok());
        assert_eq!(report.degraded_optional(), ["deno", "ffprobe", "nm3u8dl"]);
        assert_eq!(
            report
                .retryable_tools()
                .map(|spec| spec.component)
                .collect::<Vec<_>>(),
            ["ffprobe", "nm3u8dl"]
        );
    }

    #[test]
    fn publishing_writes_one_component_per_probe() {
        let registry = aulos_core::HealthRegistry::new();
        let report = Report {
            shim: ShimProbe::Failed("no python".to_owned()),
            tools: vec![("ffmpeg", Probe::Ok("6.1.1".to_owned()))],
        };
        publish(&report, &registry);
        let view = registry.snapshot();
        assert_eq!(
            view.components.get("ytdlp_runner").map(|c| c.status),
            Some(ComponentStatus::Down)
        );
        assert_eq!(
            view.components.get("ffmpeg").map(|c| c.status),
            Some(ComponentStatus::Ok)
        );
        // Only the store makes `healthz` answer 503.
        assert!(!view.is_fatal());
    }
}
