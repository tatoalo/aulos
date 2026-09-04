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
use aulos_core::health::{ComponentHealth, ComponentStatus};
use aulos_provider::sink::ProgressSinkFactory;
use aulos_provider_ytdlp::runner::{DEFAULT_PYTHON, DEFAULT_RUNNER_PATH};
use aulos_provider_ytdlp::{Job, RunnerHandle, RunnerOutcome, ShimIdentity};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// How long a `--version` probe is given before it is treated as missing.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// The tool is not on `PATH`, or did not answer.
    Missing(String),
}

impl Probe {
    /// Whether the tool answered.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// The version line, or the failure text.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Ok(v) | Self::Missing(v) => v,
        }
    }

    /// The `healthz` component for this probe: `ok` with a `version`, or `down` with a `detail`.
    #[must_use]
    pub fn component(&self) -> ComponentHealth {
        match self {
            Self::Ok(v) => ComponentHealth::new(ComponentStatus::Ok).with("version", v.clone()),
            Self::Missing(why) => {
                ComponentHealth::new(ComponentStatus::Down).with("detail", why.clone())
            }
        }
    }
}

/// Runs `<program> <arg>` and keeps the first non-empty line of its output.
///
/// stdout and stderr are both considered, because `ffmpeg -version` prints to stdout while some
/// builds of `N_m3u8DL-RE` print to stderr, and "which stream did it use" is not information worth
/// encoding per tool.
pub async fn probe_tool(spec: &ToolSpec) -> Probe {
    let mut cmd = Command::new(spec.program);
    cmd.arg(spec.arg)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    let output = match tokio::time::timeout(VERSION_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Probe::Missing(format!("{}: {e}", spec.program)),
        Err(_) => {
            return Probe::Missing(format!(
                "{} did not answer {} within {}s",
                spec.program,
                spec.arg,
                VERSION_TIMEOUT.as_secs()
            ));
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    match first_line(&text).or_else(|| first_line(&err)) {
        Some(line) => Probe::Ok(line),
        None if output.status.success() => Probe::Ok(String::new()),
        None => Probe::Missing(format!("{} exited with {}", spec.program, output.status)),
    }
}

/// Probes every tool in `specs`, concurrently.
pub async fn probe_all(specs: &[ToolSpec]) -> Vec<(&'static str, Probe)> {
    let mut out = Vec::with_capacity(specs.len());
    let mut futures = Vec::with_capacity(specs.len());
    for spec in specs {
        futures.push(async move { (spec.component, probe_tool(spec).await) });
    }
    for f in futures {
        out.push(f.await);
    }
    out
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

    /// The optional tools that are missing, by component name.
    #[must_use]
    pub fn missing_optional(&self) -> Vec<&'static str> {
        self.tools
            .iter()
            .filter(|(_, p)| !p.is_ok())
            .map(|(name, _)| *name)
            .collect()
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
        health.set(name, probe.component());
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
    async fn a_missing_tool_is_down_with_a_detail_and_never_a_version() {
        let spec = ToolSpec {
            component: "nope",
            program: "aulos-no-such-tool-cf4a1",
            arg: "--version",
        };
        let probe = probe_tool(&spec).await;
        assert!(!probe.is_ok(), "{probe:?}");
        let component = probe.component();
        assert_eq!(component.status, ComponentStatus::Down);
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
            ],
        };
        let table = report.render();
        assert!(table.contains("python3 + yt-dlp MISSING"), "{table}");
        assert!(table.contains("ffmpeg version 6.1.1"), "{table}");
        assert!(table.contains("deno"), "{table}");
        assert!(!report.required_ok());
        assert_eq!(report.missing_optional(), ["deno"]);
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
        // A `down` optional tool degrades the roll-up but the store is what makes it fatal.
        assert!(!view.is_fatal());
    }
}
