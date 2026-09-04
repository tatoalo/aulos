//! The Rust side of the shim: spawn, fd-3 transport, the frame consumer, cancellation and the
//! two timers (DESIGN §9.1, §9.3, §9.7).
//!
//! # The transport, and why it is shaped like this
//!
//! - The protocol arrives on **fd 3**, a pipe this module creates and hands to the child with
//!   `command-fds`. The child's stdout goes to `/dev/null` on both sides of the boundary. That
//!   is the §9.1 override of BRIEF §9, and it exists because the BgUtils POT plugin,
//!   `yt-dlp-ejs` and its `deno` grandchildren print to stdout: a yt-dlp `logger` silences
//!   yt-dlp but not a plugin and not a grandchild, so a protocol on stdout would corrupt
//!   silently and intermittently.
//! - Lines are read by a **detached task** that forwards them over an `mpsc`, not by a
//!   `select!` arm calling [`Lines::next_line`] directly. `next_line` is not cancel-safe — it
//!   buffers across `fill_buf` calls — so a `select!` losing that race would silently truncate a
//!   line. `mpsc::Receiver::recv` is cancel-safe, and the bounded channel is honest
//!   backpressure onto the child.
//! - **stderr is drained by [`aulos_provider::proc::Child`] itself**, into a bounded ring. The
//!   drain is mandatory there and cannot be switched off, which is the point: a full 64 KiB
//!   stderr pipe blocks the child's next `write(2)` forever, and a blocked child neither
//!   downloads nor exits nor answers a signal.
//! - Cancellation, the hard timer and every contract violation take the same exit: `killpg`
//!   `SIGTERM` → grace → `SIGKILL`, then partial-file cleanup. A `Drop` guard on `Child` repeats
//!   it, so even a panic cannot leak a downloader.
//!
//! # `--replay`
//!
//! [`RunnerHandle::replay`] drives the *same* consumer from a recorded `.jsonl` transcript with
//! no Python, no network and no process. Every frame type, every ordering violation and every
//! error class is unit-tested through it, which is the cheapest available insurance against
//! nightly yt-dlp churn. (`python/ytdlp_runner.py --replay` is the mirror image: it re-emits a
//! transcript through the real pipe, so the transport itself is testable offline too.)

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aulos_core::config::Config;
use aulos_core::error::{ErrorCode, WireError};
use aulos_core::id::UnixMs;
use aulos_core::item::{FileRef, FileSlot};
use aulos_core::paths::RelPath;
use aulos_provider::entry::{EntryHints, EntryKind, LiveStatus, MediaEntry};
use aulos_provider::outcome::Outcome;
use aulos_provider::proc::{Child, EnvPolicy, Lines, ProcError, SpawnSpec};
use aulos_provider::provider::ProviderError;
use aulos_provider::sink::{ProgressSink, Stage};
use command_fds::{CommandFdExt as _, FdMapping};
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::errmap;
use crate::frames::{
    Body, ByeFrame, EntryFrame, ErrorFrame, Frame, Hello, MAX_LINE_BYTES, PROTOCOL, PpFrame,
    ResultFrame, Root,
};
use crate::job::{Job, Mode};
use crate::progress::{FrameStatus, ProgressState};

/// The interpreter the shim runs under, unless the caller names another.
pub const DEFAULT_PYTHON: &str = "python3";

/// Where the image puts the shim (DESIGN §18.1).
pub const DEFAULT_RUNNER_PATH: &str = "/app/python/ytdlp_runner.py";

/// The `tool` label a missing interpreter reports as [`ProviderError::ToolMissing`].
pub const TOOL: &str = "python3";

/// How much of the child's stderr is kept for the error tail (DESIGN §9.1).
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// The stderr ring's line budget for this provider. Larger than the `aulos-provider` default so
/// that [`STDERR_TAIL_BYTES`] is always available even when yt-dlp is verbose.
const STDERR_RING_LINES: usize = 512;
/// The stderr ring's byte budget: four tails' worth.
const STDERR_RING_BYTES: usize = 4 * STDERR_TAIL_BYTES;

/// The fd-3 line channel depth. Bounded, so a slow consumer back-pressures the child.
const LINE_CHANNEL: usize = 128;

/// How long to wait for the child to be reaped after fd 3 reaches EOF.
const REAP_TIMEOUT: Duration = Duration::from_secs(10);

/// The `error` message legacy produced for a resolution that yielded nothing usable.
///
/// Byte-identical to legacy's `__add_entry` string, because the v1 shim reports it verbatim as
/// `{"status":"error","msg":…}` (DESIGN §8.4, §11.7).
pub const EMPTY_DATA: &str = "Invalid/empty data was given.";

/// What the shim told us about itself in its `hello` frame.
///
/// Recorded on the [`RunnerHandle`] after every run, because `GET <p>version` and
/// `healthz.components.ytdlp_runner` report it (DESIGN §16.3).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ShimIdentity {
    /// The installed yt-dlp version.
    pub yt_dlp: Option<String>,
    /// The interpreter version.
    pub python: Option<String>,
    /// The loaded yt-dlp plugin packages.
    pub plugins: Vec<String>,
    /// Whether a POT provider plugin is loaded.
    pub pot_available: bool,
    /// The POT endpoint that plugin will use.
    pub pot_url: Option<String>,
}

impl ShimIdentity {
    fn from_hello(hello: &Hello) -> Self {
        let pot = hello.pot.clone().unwrap_or_default();
        Self {
            yt_dlp: hello.yt_dlp.clone(),
            python: hello.python.clone(),
            plugins: hello.plugins.clone(),
            pot_available: pot.available,
            pot_url: pot.url,
        }
    }
}

/// What one shim run produced.
///
/// `Selftest` is additive over the DESIGN §9.7 signature list: `probe` needs the `hello`
/// identity, and returning it through the existing enum keeps `run_job`'s signature exactly as
/// the design writes it.
#[derive(Clone, PartialEq, Debug)]
pub enum RunnerOutcome {
    /// `mode = extract`: one entry per video, or a single container entry with children.
    Extracted {
        /// The resolved entries.
        entries: Vec<MediaEntry>,
        /// Whether `extract.max_entries` cut the child list short.
        truncated: bool,
    },
    /// `mode = download`: the produced file and its artifacts.
    Downloaded(Outcome),
    /// `mode = outtmpl`: the evaluated templates, in request order.
    OutTmpl(Vec<String>),
    /// `mode = selftest`: the shim's own report.
    Selftest(ShimIdentity),
}

/// A configured way to run the shim. Cheap to clone; one per provider instance.
#[derive(Clone, Debug)]
pub struct RunnerHandle {
    python: PathBuf,
    runner: PathBuf,
    kill_grace: Duration,
    stall: Option<Duration>,
    hard: Option<Duration>,
    max_line_bytes: usize,
    env: Vec<(OsString, OsString)>,
    identity: Arc<Mutex<Option<ShimIdentity>>>,
}

impl Default for RunnerHandle {
    fn default() -> Self {
        Self::new(DEFAULT_PYTHON, DEFAULT_RUNNER_PATH)
    }
}

impl RunnerHandle {
    /// A handle with the DESIGN §17.3 defaults for the timers.
    #[must_use]
    pub fn new(python: impl Into<PathBuf>, runner: impl Into<PathBuf>) -> Self {
        Self {
            python: python.into(),
            runner: runner.into(),
            kill_grace: Duration::from_millis(5000),
            stall: Some(Duration::from_secs(900)),
            hard: None,
            max_line_bytes: MAX_LINE_BYTES,
            env: Vec::new(),
            identity: Arc::new(Mutex::new(None)),
        }
    }

    /// A handle wired to `AULOS_KILL_GRACE_MS`, `AULOS_JOB_STALL_SECS` and
    /// `AULOS_JOB_TIMEOUT_SECS`. `0` disables either timer, as the env table says.
    #[must_use]
    pub fn from_config(
        cfg: &Config,
        python: impl Into<PathBuf>,
        runner: impl Into<PathBuf>,
    ) -> Self {
        let secs = |v: u64| (v > 0).then(|| Duration::from_secs(v));
        Self {
            kill_grace: Duration::from_millis(cfg.kill_grace_ms.max(1)),
            stall: secs(cfg.job_stall_secs),
            hard: secs(cfg.job_timeout_secs),
            ..Self::new(python, runner)
        }
    }

    /// Overrides the `SIGTERM` → `SIGKILL` grace.
    #[must_use]
    pub const fn with_kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Overrides the no-frame stall watchdog. `None` disables it. Warn-only, never fatal.
    #[must_use]
    pub const fn with_stall(mut self, stall: Option<Duration>) -> Self {
        self.stall = stall;
        self
    }

    /// Overrides the hard per-job deadline. `None` disables it. Fatal: it cancels the job.
    #[must_use]
    pub const fn with_timeout(mut self, hard: Option<Duration>) -> Self {
        self.hard = hard;
        self
    }

    /// Overrides the fd-3 line cap. Over the cap the child is killed and the job is a
    /// `contract` failure.
    #[must_use]
    pub const fn with_max_line_bytes(mut self, cap: usize) -> Self {
        self.max_line_bytes = cap;
        self
    }

    /// Adds one environment variable for the child, applied after the inherited environment.
    ///
    /// The shim's environment is part of its contract (`YTDL_*`, the proxy variables, the POT
    /// plugin's own settings), so nothing is cleared; this is for the few values the *server*
    /// decides, and for pointing the test suite at a stubbed `yt_dlp` on `PYTHONPATH`.
    #[must_use]
    pub fn with_env_var(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The path to the shim this handle runs.
    #[must_use]
    pub fn runner_path(&self) -> &Path {
        &self.runner
    }

    /// The last `hello` this handle saw, for `healthz` and `GET <p>version`.
    #[must_use]
    pub fn identity(&self) -> Option<ShimIdentity> {
        self.identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn remember(&self, identity: &ShimIdentity) {
        *self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity.clone());
    }

    fn spec(&self) -> SpawnSpec {
        SpawnSpec::new(TOOL, self.python.clone())
            .arg(self.runner.clone())
            .stdin_piped(true)
            .stdout_piped(false)
            .stderr_ring(STDERR_RING_LINES, STDERR_RING_BYTES)
            .max_line_bytes(self.max_line_bytes)
            .kill_grace(self.kill_grace)
            // DESIGN §9.1: the child's stderr reaches `tracing` **as it arrives**, classified by
            // yt-dlp's own prefixes. The bounded ring still keeps the tail for `error.message`.
            .stderr_line_hook(log_child_line)
            .env(EnvPolicy {
                // The shim's environment is part of its contract: `YTDL_*`, the proxy variables,
                // the locale and the POT plugin's own settings all come from the process env.
                clear: false,
                pass: Vec::new(),
                set: [
                    (OsString::from("PYTHONUNBUFFERED"), OsString::from("1")),
                    (
                        OsString::from("PYTHONDONTWRITEBYTECODE"),
                        OsString::from("1"),
                    ),
                ]
                .into_iter()
                // Caller-supplied variables last, so `with_env_var` can override either default.
                .chain(self.env.iter().cloned())
                .collect(),
            })
    }

    /// Runs one job to completion.
    ///
    /// # Errors
    /// Whatever the shim reported, mapped through [`crate::errmap`]; [`ProviderError::Contract`]
    /// for a protocol violation (a sequence gap, a missing terminator, an over-long line);
    /// [`ProviderError::Canceled`] when `cancel` fires; [`ProviderError::Timeout`] when the hard
    /// deadline expires; [`ProviderError::ToolMissing`] when the interpreter is absent.
    pub async fn run(
        &self,
        job: &Job,
        sink: &ProgressSink,
        cancel: &CancellationToken,
    ) -> Result<RunnerOutcome, ProviderError> {
        let spec = self.spec();
        let (reader_fd, writer_fd) =
            std::io::pipe().map_err(|source| ProcError::Spawn { tool: TOOL, source })?;

        let mut cmd = spec.to_command();
        cmd.fd_mappings(vec![FdMapping {
            parent_fd: OwnedFd::from(writer_fd),
            child_fd: 3,
        }])
        .map_err(|e| ProviderError::Other(format!("fd 3 mapping rejected: {e}")))?;
        // `Child::spawn_command` consumes the command, which closes the parent's copy of the
        // write end — without that, fd 3 would never reach EOF.
        let mut child = Child::spawn_command(&spec, cmd)?;
        let pid = child.pid();
        let stderr = child.stderr();

        let receiver = tokio::net::unix::pipe::Receiver::from_owned_fd(OwnedFd::from(reader_fd))
            .map_err(|source| ProcError::Io { tool: TOOL, source })?;

        if let Some(mut stdin) = child.take_stdin() {
            let line = job.to_line();
            if let Err(source) = stdin.write_all(line.as_bytes()).await {
                return Err(ProcError::Io { tool: TOOL, source }.into());
            }
            let _ = stdin.shutdown().await;
            drop(stdin); // EOF on the child's stdin: it never reads again.
        }

        let (tx, mut rx) = mpsc::channel::<Result<String, ProcError>>(LINE_CHANNEL);
        let cap = self.max_line_bytes;
        let pump = tokio::spawn(async move {
            let mut lines = Lines::new(receiver, TOOL, cap);
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if tx.send(Ok(line)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        tracing::debug!(target: "ytdlp.child", pid, mode = %job.mode, job = %job.job_id, "shim started");
        let mut consumer = Consumer::new(job);
        let outcome = self.pump(&mut consumer, &mut rx, sink, cancel, job).await;
        pump.abort();

        // Whatever happened, the child must not outlive this call.
        let status = match &outcome {
            Ok(()) => tokio::time::timeout(REAP_TIMEOUT, child.wait())
                .await
                .ok()
                .and_then(Result::ok),
            Err(_) => {
                let s = child.kill_group().await;
                cleanup_partials(job, consumer.progress.partials());
                s
            }
        };

        if let Some(identity) = &consumer.identity {
            self.remember(identity);
        }
        // Every line has already reached `tracing` through `log_child_line`; the ring is only the
        // tail quoted back to the user. `wait`/`kill_group` do not join the drain, so settle it
        // first or the tail is a race (see the WP-11 entry in `docs/INTEGRATION-NOTES.md`).
        child.drained().await;
        let tail = stderr.tail(STDERR_TAIL_BYTES);

        match outcome {
            Err(e) => Err(e),
            Ok(()) => consumer.finish(status.and_then(|s| s.code()), &tail),
        }
    }

    /// The `select!` loop: fd-3 lines, cancellation, the stall timer and the hard timer.
    async fn pump(
        &self,
        consumer: &mut Consumer<'_>,
        rx: &mut mpsc::Receiver<Result<String, ProcError>>,
        sink: &ProgressSink,
        cancel: &CancellationToken,
        job: &Job,
    ) -> Result<(), ProviderError> {
        // A disabled timer is modelled as a far-future deadline rather than as a `None` branch,
        // so the loop body has no conditional-arm bookkeeping.
        let far = Duration::from_secs(86_400 * 365);
        let stall = self.stall.unwrap_or(far);
        let hard_at = tokio::time::Instant::now() + self.hard.unwrap_or(far);
        let hard = tokio::time::sleep_until(hard_at);
        let stall_timer = tokio::time::sleep(stall);
        tokio::pin!(hard, stall_timer);

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    tracing::info!(job = %job.job_id, "cancelling the yt-dlp shim");
                    return Err(ProviderError::Canceled);
                }
                () = &mut hard => {
                    return Err(ProviderError::Timeout(format!(
                        "the job exceeded its {} s deadline",
                        self.hard.unwrap_or(far).as_secs()
                    )));
                }
                () = &mut stall_timer => {
                    // Warn-only, per the `AULOS_JOB_STALL_SECS` row of DESIGN §17.3.
                    tracing::warn!(
                        job = %job.job_id,
                        stall_s = stall.as_secs(),
                        "no frame from the yt-dlp shim; still waiting"
                    );
                    stall_timer.as_mut().reset(tokio::time::Instant::now() + stall);
                }
                line = rx.recv() => match line {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(line)) => {
                        stall_timer.as_mut().reset(tokio::time::Instant::now() + stall);
                        consumer.accept(&line, sink).await?;
                    }
                },
            }
        }
    }

    /// Drives the consumer from a recorded transcript: no process, no Python, no network.
    ///
    /// # Errors
    /// As [`RunnerHandle::run`], plus [`ProviderError::Other`] when the file cannot be read.
    pub async fn replay(
        &self,
        transcript: impl AsRef<Path>,
        job: &Job,
        sink: &ProgressSink,
        cancel: &CancellationToken,
    ) -> Result<RunnerOutcome, ProviderError> {
        let path = transcript.as_ref();
        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ProviderError::Other(format!("cannot replay {}: {e}", path.display())))?;
        let mut consumer = Consumer::new(job);
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if cancel.is_cancelled() {
                return Err(ProviderError::Canceled);
            }
            if line.len() > self.max_line_bytes {
                return Err(ProcError::LineTooLong {
                    tool: TOOL,
                    cap: self.max_line_bytes,
                }
                .into());
            }
            consumer.accept(line, sink).await?;
        }
        if let Some(identity) = &consumer.identity {
            self.remember(identity);
        }
        consumer.finish(Some(0), "")
    }
}

/// Runs one job with the default handle (`python3 /app/python/ytdlp_runner.py`).
///
/// The DESIGN §9.7 free function. A caller that needs a configured interpreter, the config
/// timers or the `hello` identity uses [`RunnerHandle`] directly.
///
/// # Errors
/// As [`RunnerHandle::run`].
pub async fn run_job(
    job: Job,
    sink: &ProgressSink,
    cancel: &CancellationToken,
) -> Result<RunnerOutcome, ProviderError> {
    RunnerHandle::default().run(&job, sink, cancel).await
}

/// Logs one line of the child's stderr, classifying yt-dlp's own `ERROR:` / `WARNING:` prefixes
/// (DESIGN §9.1).
///
/// Installed as the [`SpawnSpec::stderr_line_hook`], so it runs on the mandatory drain the moment
/// a line is complete — a 40-minute download's warnings are visible while it is still running,
/// not only in the post-mortem. It must stay cheap: it is on the drain task, and a stalled drain
/// is a deadlocked child.
///
/// [`SpawnSpec::stderr_line_hook`]: aulos_provider::proc::SpawnSpec::stderr_line_hook
fn log_child_line(pid: u32, line: &str) {
    if line.starts_with("ERROR") || line.starts_with("WARNING") {
        tracing::warn!(target: "ytdlp.child", pid, "{line}");
    } else {
        tracing::debug!(target: "ytdlp.child", pid, "{line}");
    }
}

/// Removes the partial files a killed job left behind (Δ C18).
///
/// Only paths inside the job's own output or scratch directory are touched, and only the
/// `.part` / `.ytdl` pair yt-dlp actually writes. Legacy deleted at most one `.part` — and
/// usually none, because its `tmpfilename` had been cleared by the next frame.
fn cleanup_partials(job: &Job, partials: &[Box<str>]) {
    let roots: Vec<&Path> = [
        Some(job.policy.download_dir.as_path()),
        Some(job.policy.temp_dir.as_path()),
        job.download_root.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|p| !p.as_os_str().is_empty())
    .collect();

    for partial in partials {
        let path = Path::new(&**partial);
        if !roots.is_empty() && !roots.iter().any(|r| path.starts_with(r)) {
            tracing::debug!(path = %path.display(), "partial is outside every known root; left alone");
            continue;
        }
        for candidate in [path.to_path_buf(), path.with_extension("ytdl")] {
            match std::fs::remove_file(&candidate) {
                Ok(()) => tracing::debug!(path = %candidate.display(), "removed a partial file"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::debug!(path = %candidate.display(), error = %e, "could not remove a partial");
                }
            }
        }
    }
}

/// Which terminator the transcript carried.
#[derive(Clone, Debug)]
enum Terminal {
    Result(Box<ResultFrame>),
    Error(ErrorFrame),
}

/// The frame consumer: ordering checks plus the per-mode accumulation.
///
/// Shared verbatim between [`RunnerHandle::run`] and [`RunnerHandle::replay`], which is what
/// makes the replay suite meaningful — it exercises the production state machine, not a stub.
struct Consumer<'a> {
    job: &'a Job,
    expect_n: u64,
    saw_hello: bool,
    saw_bye: bool,
    terminal: Option<Terminal>,
    identity: Option<ShimIdentity>,
    // extract
    root: Option<Root>,
    entries: Vec<MediaEntry>,
    info: Option<Value>,
    // download
    progress: ProgressState,
    started_downloading: bool,
    stage: Stage,
    primary: Option<(String, Option<u64>)>,
    chapters: Vec<FileRef>,
    subtitles: Vec<FileRef>,
    seen_artifacts: BTreeSet<(String, String)>,
}

impl<'a> Consumer<'a> {
    fn new(job: &'a Job) -> Self {
        Self {
            job,
            expect_n: 1,
            saw_hello: false,
            saw_bye: false,
            terminal: None,
            identity: None,
            root: None,
            entries: Vec::new(),
            info: None,
            progress: ProgressState::new(),
            started_downloading: false,
            // The engine has already written `preparing` before spawning, so a `phase` frame that
            // arrives before the first `progress` must not regress the status.
            stage: Stage::Preparing,
            primary: None,
            chapters: Vec::new(),
            subtitles: Vec::new(),
            seen_artifacts: BTreeSet::new(),
        }
    }

    fn contract(msg: impl Into<String>) -> ProviderError {
        ProviderError::Contract(msg.into())
    }

    /// Parses and applies one transcript line.
    async fn accept(&mut self, line: &str, sink: &ProgressSink) -> Result<(), ProviderError> {
        let frame: Frame = serde_json::from_str(line).map_err(|e| {
            Self::contract(format!(
                "the shim wrote a line that is not a frame at n={}: {e}",
                self.expect_n
            ))
        })?;

        if frame.v != PROTOCOL {
            return Err(Self::contract(format!(
                "frame envelope version {} is not supported (expected {PROTOCOL})",
                frame.v
            )));
        }
        if frame.n != self.expect_n {
            return Err(Self::contract(format!(
                "frame sequence gap: expected n={}, got n={} ({})",
                self.expect_n,
                frame.n,
                frame.body.kind()
            )));
        }
        self.expect_n += 1;

        if self.saw_bye {
            return Err(Self::contract(format!(
                "the shim wrote a {} frame after bye",
                frame.body.kind()
            )));
        }
        if !self.saw_hello && !matches!(frame.body, Body::Hello(_)) {
            return Err(Self::contract(format!(
                "the first frame was {}, not hello",
                frame.body.kind()
            )));
        }
        if self.terminal.is_some() && !matches!(frame.body, Body::Bye(_)) {
            return Err(Self::contract(format!(
                "the shim wrote a {} frame after its terminator",
                frame.body.kind()
            )));
        }

        match frame.body {
            Body::Hello(hello) => self.on_hello(&hello)?,
            Body::Resolved(r) => self.root = Some(r.root),
            Body::Entry(e) => self.on_entry(&e),
            Body::Progress(p) => {
                if !self.started_downloading {
                    self.started_downloading = true;
                    self.stage = Stage::Downloading;
                    sink.stage(Stage::Downloading, None).await;
                }
                let status = FrameStatus::parse(p.status.as_deref());
                let raw = self.progress.apply(&p);
                sink.progress(raw);
                if status == FrameStatus::Error {
                    // A per-stream problem is a notice, not a terminator (DESIGN §9.4).
                    let msg = p.msg.clone().unwrap_or_else(|| "stream error".to_owned());
                    sink.log(aulos_core::event::Level::Warn, &msg);
                }
            }
            Body::Pp(pp) => self.on_pp(pp, sink).await,
            Body::Artifact(a) => {
                self.record(&a.role, &a.path, a.size, a.language.as_deref(), sink)
                    .await;
            }
            Body::Phase(ph) => {
                sink.stage(self.stage, Some(ph.msg.into_boxed_str())).await;
            }
            Body::Info(i) => self.info = Some(i.entry),
            Body::Log(l) => {
                match l.level.as_str() {
                    "warning" => sink.log(aulos_core::event::Level::Warn, &l.message),
                    "error" => sink.log(aulos_core::event::Level::Error, &l.message),
                    // `debug` and `info` are child chatter: they belong in the log, not in the
                    // item's event stream.
                    _ => tracing::debug!(target: "ytdlp.child", "{}", l.message),
                }
            }
            Body::Result(r) => self.terminal = Some(Terminal::Result(r)),
            Body::Error(e) => self.terminal = Some(Terminal::Error(e)),
            Body::Bye(b) => self.on_bye(&b),
            Body::Unknown => {
                tracing::debug!(
                    n = frame.n,
                    "skipping a frame type this build does not know"
                );
            }
        }
        Ok(())
    }

    fn on_hello(&mut self, hello: &Hello) -> Result<(), ProviderError> {
        if hello.protocol != PROTOCOL {
            return Err(Self::contract(format!(
                "the shim speaks protocol {} but this build speaks {PROTOCOL}",
                hello.protocol
            )));
        }
        self.saw_hello = true;
        self.identity = Some(ShimIdentity::from_hello(hello));
        Ok(())
    }

    fn on_bye(&mut self, bye: &ByeFrame) {
        self.saw_bye = true;
        tracing::debug!(
            target: "ytdlp.child",
            elapsed_ms = bye.elapsed_ms,
            frames = bye.frames,
            peak_rss_kb = bye.peak_rss_kb,
            "shim finished"
        );
    }

    fn on_entry(&mut self, frame: &EntryFrame) {
        match media_entry(frame, self.root.as_ref()) {
            Some(entry) => self.entries.push(entry),
            None => tracing::warn!(
                index = frame.index,
                "skipping a resolved entry with no usable url"
            ),
        }
    }

    async fn on_pp(&mut self, pp: PpFrame, sink: &ProgressSink) {
        match pp.status.as_str() {
            "started" => {
                self.stage = Stage::Postprocessing;
                let msg = format!("{}…", pp.postprocessor).into_boxed_str();
                sink.stage(Stage::Postprocessing, Some(msg)).await;
            }
            "processing" if self.stage != Stage::Postprocessing => {
                self.stage = Stage::Postprocessing;
                sink.stage(Stage::Postprocessing, None).await;
            }
            _ => {}
        }
        for file in &pp.subtitles {
            self.record(
                "subtitle",
                &file.path,
                file.size,
                file.language.as_deref(),
                sink,
            )
            .await;
        }
        for file in &pp.chapters {
            self.record("chapter", &file.path, file.size, None, sink)
                .await;
        }
    }

    async fn record(
        &mut self,
        role: &str,
        path: &str,
        size: Option<u64>,
        language: Option<&str>,
        sink: &ProgressSink,
    ) {
        if let Some((slot, file)) = self.remember_artifact(role, path, size, language) {
            sink.file(slot, file).await;
        }
    }

    /// De-duplicates by `(role, path)` — the shim reports a chapter both as an `artifact` frame
    /// and inside the `pp` frame's list, and `SplitChapters` fires more than once.
    fn remember_artifact(
        &mut self,
        role: &str,
        path: &str,
        size: Option<u64>,
        language: Option<&str>,
    ) -> Option<(FileSlot, FileRef)> {
        if path.is_empty() {
            return None;
        }
        if role == "media" {
            // Last one wins: `Merger` then `FFmpegVideoConvertor` then `MoveFiles` all nominate.
            self.primary = Some((path.to_owned(), size));
            return None;
        }
        if !self
            .seen_artifacts
            .insert((role.to_owned(), path.to_owned()))
        {
            return None;
        }
        let file = FileRef {
            filename: self.job.rebase(path).into(),
            size,
            download_url: None,
            lang: language.map(Into::into),
        };
        let slot = match role {
            "chapter" => FileSlot::Chapter,
            "subtitle" => FileSlot::Subtitle,
            other => {
                tracing::debug!(
                    role = other,
                    path,
                    "ignoring an artifact role this build has no slot for"
                );
                return None;
            }
        };
        match slot {
            FileSlot::Chapter => self.chapters.push(file.clone()),
            FileSlot::Subtitle => self.subtitles.push(file.clone()),
        }
        Some((slot, file))
    }

    /// Validates the ordering guarantees and builds the outcome.
    fn finish(
        mut self,
        exit_code: Option<i32>,
        stderr_tail: &str,
    ) -> Result<RunnerOutcome, ProviderError> {
        if !self.saw_hello {
            return Err(Self::contract(exit_note(
                "the shim wrote no hello frame",
                exit_code,
                stderr_tail,
            )));
        }
        let Some(terminal) = self.terminal.take() else {
            return Err(Self::contract(exit_note(
                "the shim wrote neither a result nor an error frame",
                exit_code,
                stderr_tail,
            )));
        };
        if !self.saw_bye {
            return Err(Self::contract(exit_note(
                "the shim wrote no bye frame",
                exit_code,
                stderr_tail,
            )));
        }

        let result = match terminal {
            Terminal::Error(e) => return Err(errmap::from_frame(&e)),
            Terminal::Result(r) => r,
        };

        match self.job.mode {
            Mode::Selftest => {
                if !result.ok {
                    return Err(ProviderError::Other("the shim self-test failed".to_owned()));
                }
                let mut identity = self.identity.unwrap_or_default();
                if identity.yt_dlp.is_none() {
                    identity.yt_dlp = result.yt_dlp.clone();
                }
                Ok(RunnerOutcome::Selftest(identity))
            }
            Mode::OutTmpl => match result.templates {
                Some(templates) => Ok(RunnerOutcome::OutTmpl(templates)),
                None => Err(Self::contract(
                    "an outtmpl result carried no templates array",
                )),
            },
            Mode::Extract => self.extracted(&result),
            Mode::Download => {
                if !result.ok {
                    let code = result.retcode.unwrap_or(-1);
                    let mut msg = format!("yt-dlp exited with code {code}");
                    if !stderr_tail.is_empty() {
                        msg.push_str(": ");
                        msg.push_str(&tail_snippet(stderr_tail));
                    }
                    return Err(ProviderError::Other(msg));
                }
                Ok(RunnerOutcome::Downloaded(self.outcome(&result)))
            }
        }
    }

    fn extracted(self, result: &ResultFrame) -> Result<RunnerOutcome, ProviderError> {
        let root = self.root;
        let mut entries = self.entries;
        if entries.is_empty() {
            // Legacy's verbatim string; the v1 shim reports it as-is (DESIGN §8.4, §11.7).
            return Err(ProviderError::Unsupported(EMPTY_DATA.to_owned()));
        }

        if let Some(root) = root.as_ref().filter(|r| r.is_redirect()) {
            let target = entries
                .first()
                .map(|e| e.url.clone())
                .or_else(|| root.webpage_url.as_deref().and_then(|u| Url::parse(u).ok()));
            if let Some(url) = target {
                let mut entry = entries.remove(0);
                entry.kind = EntryKind::Redirect { url };
                return Ok(RunnerOutcome::Extracted {
                    entries: vec![entry],
                    truncated: result.truncated,
                });
            }
        }

        if let Some(root) = root.as_ref().filter(|r| r.is_container()) {
            let title: Box<str> = root
                .title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or("Playlist")
                .into();
            let url = root
                .webpage_url
                .as_deref()
                .and_then(|u| Url::parse(u).ok())
                .unwrap_or_else(|| entries[0].url.clone());
            let media_id: Box<str> = root
                .id
                .as_deref()
                .filter(|i| !i.is_empty())
                .map_or_else(|| url.as_str().into(), Into::into);
            let count = root
                .playlist_count
                .or_else(|| u32::try_from(entries.len()).ok());
            let mut parent = MediaEntry::video(media_id, title.clone(), url);
            parent.hints = EntryHints {
                playlist_count: count,
                playlist_title: Some(title.clone()),
                uploader: root.uploader.as_deref().map(Into::into),
                ..EntryHints::default()
            };
            for (index, child) in entries.iter_mut().enumerate() {
                let position = u32::try_from(index + 1).ok();
                child.hints.playlist_index = child.hints.playlist_index.or(position);
                child.hints.playlist_count = child.hints.playlist_count.or(count);
                child.hints.playlist_title = child
                    .hints
                    .playlist_title
                    .clone()
                    .or_else(|| Some(title.clone()));
            }
            parent.kind = EntryKind::Playlist { title, entries };
            return Ok(RunnerOutcome::Extracted {
                entries: vec![parent],
                truncated: result.truncated,
            });
        }

        // A single video: the `info` frame is the richer blob, so prefer it as the entry state.
        if let Some(info) = self.info
            && let Some(first) = entries.first_mut()
        {
            first.state = info;
        }
        Ok(RunnerOutcome::Extracted {
            entries,
            truncated: result.truncated,
        })
    }

    fn outcome(&self, result: &ResultFrame) -> Outcome {
        // The `result` frame's own filename wins; the last `media` artifact is the fallback for a
        // shim that only reported the artifact.
        let chosen = result
            .filename
            .as_deref()
            .map(|p| (p.to_owned(), result.size))
            .or_else(|| self.primary.clone())
            .or_else(|| self.progress.filename().map(|f| (f.to_owned(), None)));

        let (filename, size) = match chosen {
            Some((path, size)) => (RelPath::parse(self.job.rebase(&path)).ok(), size),
            None => (None, None),
        };

        Outcome {
            filename,
            size: size.or(result.size),
            chapter_files: self.chapters.clone(),
            subtitle_files: self.subtitles.clone(),
            entry_final: self.info.clone(),
        }
    }
}

/// How much of the stderr tail is quoted inside an error *message*.
///
/// [`ProviderError::message`] caps at 512 characters, and it truncates from the **front** — so
/// quoting the whole 8 KiB tail would fill the message with the oldest, least useful noise and
/// throw away the actual error. The last few hundred characters are where the diagnosis is; the
/// full tail is still in the logs, via [`log_child_line`].
const TAIL_IN_MESSAGE: usize = 360;

/// The end of `tail`, at most [`TAIL_IN_MESSAGE`] characters, on a character boundary.
fn tail_snippet(tail: &str) -> String {
    let mut chars: Vec<char> = tail.chars().collect();
    if chars.len() <= TAIL_IN_MESSAGE {
        return tail.to_owned();
    }
    chars.drain(..chars.len() - TAIL_IN_MESSAGE);
    let mut out = String::from("…");
    out.extend(chars);
    out
}

/// Appends the child's exit code and the end of its stderr to a contract message.
fn exit_note(what: &str, exit_code: Option<i32>, stderr_tail: &str) -> String {
    let mut msg = what.to_owned();
    match exit_code {
        Some(code) => msg.push_str(&format!(" (exit code {code})")),
        None => msg.push_str(" (exit status unknown)"),
    }
    if !stderr_tail.is_empty() {
        msg.push_str(": ");
        msg.push_str(&tail_snippet(stderr_tail));
    }
    msg
}

/// Builds a [`MediaEntry`] from one `entry` frame.
///
/// Returns `None` when the entry has no parseable URL at all, which is the "no usable id/url"
/// case of DESIGN §8.4: for a child it is skipped, and if that leaves nothing the whole
/// resolution reports [`EMPTY_DATA`].
fn media_entry(frame: &EntryFrame, root: Option<&Root>) -> Option<MediaEntry> {
    let entry = &frame.entry;
    let url = str_of(entry, "webpage_url")
        .or_else(|| str_of(entry, "url"))
        .or_else(|| str_of(entry, "original_url"))
        .and_then(|u| Url::parse(u).ok())?;

    let media_id: Box<str> = str_of(entry, "id")
        .filter(|i| !i.is_empty())
        // A provider with no natural id falls back to the URL, which is a stable canonical key
        // for dedupe (DESIGN §8.5) and satisfies `MediaEntry::media_id`'s "never empty".
        .map_or_else(|| url.as_str().into(), Into::into);
    let title: Box<str> = str_of(entry, "title")
        .filter(|t| !t.is_empty())
        .map_or_else(|| url.as_str().into(), Into::into);

    let live = live_status(entry);
    let note = frame
        .note
        .as_deref()
        .or_else(|| str_of(entry, "msg"))
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let pre_error = note.map(|n| {
        // §8.4: `not_yet_live` for an upcoming stream, otherwise the entry-level `msg` is an
        // `unsupported_url`. The text is preserved verbatim either way.
        let code = if live.is_upcoming() {
            ErrorCode::NotYetLive
        } else {
            ErrorCode::UnsupportedUrl
        };
        WireError::new(code, n)
    });

    Some(MediaEntry {
        media_id,
        title,
        url,
        kind: EntryKind::Video,
        pre_error,
        live,
        state: Value::Object(entry.clone()),
        hints: hints_of(entry, root, frame.index),
    })
}

fn hints_of(entry: &Map<String, Value>, root: Option<&Root>, index: u32) -> EntryHints {
    let playlist_index = num_of(entry, "playlist_index")
        .or_else(|| (root.is_some_and(Root::is_container) && index > 0).then_some(index));
    EntryHints {
        playlist_index,
        playlist_count: num_of(entry, "playlist_count")
            .or_else(|| num_of(entry, "n_entries"))
            .or_else(|| root.and_then(|r| r.playlist_count)),
        playlist_title: str_of(entry, "playlist_title")
            .or_else(|| str_of(entry, "playlist"))
            .map(Into::into),
        channel_index: num_of(entry, "channel_index"),
        channel_count: num_of(entry, "channel_count"),
        channel_title: str_of(entry, "channel").map(Into::into),
        ext: str_of(entry, "ext").map(Into::into),
        duration: entry.get("duration").and_then(aulos_core::progress::number),
        filesize_approx: entry
            .get("filesize_approx")
            .and_then(aulos_core::progress::number)
            .and_then(|v| (v >= 0.0).then_some(v))
            .map(|v| v as u64),
        thumbnail: str_of(entry, "thumbnail").map(Into::into),
        uploader: str_of(entry, "uploader")
            .or_else(|| str_of(entry, "channel"))
            .map(Into::into),
    }
}

fn live_status(entry: &Map<String, Value>) -> LiveStatus {
    let at: Option<UnixMs> = entry
        .get("release_timestamp")
        .and_then(aulos_core::progress::number)
        .map(|secs| (secs * 1000.0) as UnixMs);
    match str_of(entry, "live_status") {
        Some("is_upcoming") => LiveStatus::IsUpcoming { at },
        Some("is_live") => LiveStatus::IsLive,
        Some("was_live" | "post_live") => LiveStatus::WasLive,
        _ => {
            if entry.get("is_live").and_then(Value::as_bool) == Some(true) {
                LiveStatus::IsLive
            } else if entry.get("was_live").and_then(Value::as_bool) == Some(true) {
                LiveStatus::WasLive
            } else {
                LiveStatus::NotLive
            }
        }
    }
}

fn str_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

fn num_of(map: &Map<String, Value>, key: &str) -> Option<u32> {
    map.get(key)
        .and_then(aulos_core::progress::number)
        .and_then(|v| (v >= 0.0).then_some(v))
        .map(|v| v as u32)
}
