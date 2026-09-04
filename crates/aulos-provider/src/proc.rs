//! The child-process helper all three providers share (DESIGN §6.5.3, §9.7, §2.3).
//!
//! Every process this server spawns goes through here, because three of the four things that go
//! wrong with child processes are invisible until production:
//!
//! 1. **A full stderr pipe deadlocks the child forever.** A 64 KiB pipe with nobody reading it
//!    blocks the next `write(2)`, and a blocked child neither downloads nor exits nor answers a
//!    signal. Legacy could not have this bug because it never piped stderr; we do, so the drain is
//!    **mandatory** — [`Child::spawn`] starts it before returning and there is no way to opt out.
//! 2. **Killing the child does not kill its children.** Legacy's `proc.kill()` orphaned the
//!    `ffmpeg` that yt-dlp had spawned, which kept writing to a cancelled item's `.part` file.
//!    Every child here gets its own process group ([`process_group(0)`]) and every kill is a
//!    `killpg`.
//! 3. **A panic leaks the process.** [`Child`]'s `Drop` sends the same `SIGTERM` → grace →
//!    `SIGKILL` sequence, so an unwind on the Rust side cannot leave a downloader running.
//!
//! The fourth is an unbounded line: a child that prints a 4 GiB JSON line would take the server
//! with it, so the line reader has a hard cap and reports [`ProcError::LineTooLong`], which is a
//! `contract` failure.
//!
//! [`process_group(0)`]: std::os::unix::process::CommandExt::process_group

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aulos_core::error::ErrorCode;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child as TokioChild, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::provider::ProviderError;

/// The stderr ring's line budget (DESIGN §2.3).
pub const STDERR_RING_LINES: usize = 64;
/// The stderr ring's byte budget (DESIGN §2.3).
pub const STDERR_RING_BYTES: usize = 32 * 1024;
/// The default line cap for a protocol stream. `aulos-provider-ytdlp` raises it to 8 MiB for the
/// fd-3 transcript, which can carry a whole `info` dict (DESIGN §9.7).
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;
/// The default grace between `SIGTERM` and `SIGKILL`, matching `AULOS_KILL_GRACE_MS`.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(5);
/// The niceness every spawned child gets, so a download never starves the server's own tasks
/// (DESIGN §6.5.3).
pub const DEFAULT_NICE: i32 = 5;

/// Everything that can go wrong around a child process.
#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    /// `argv[0]` does not exist or is not executable.
    #[error("required tool {tool} not found")]
    NotFound {
        /// The static label of the tool that is missing.
        tool: &'static str,
        /// The underlying failure.
        source: io::Error,
    },
    /// The spawn failed for a reason other than a missing binary.
    #[error("failed to spawn {tool}: {source}")]
    Spawn {
        /// The static label of the tool.
        tool: &'static str,
        /// The underlying failure.
        source: io::Error,
    },
    /// An I/O failure talking to a running child.
    #[error("io error on {tool}: {source}")]
    Io {
        /// The static label of the tool.
        tool: &'static str,
        /// The underlying failure.
        source: io::Error,
    },
    /// The child printed a line longer than the configured cap. The child is killed and this is a
    /// protocol violation, not a transport hiccup.
    #[error("{tool} wrote a line longer than {cap} bytes")]
    LineTooLong {
        /// The static label of the tool.
        tool: &'static str,
        /// The cap that was exceeded.
        cap: usize,
    },
}

impl ProcError {
    /// The wire code (DESIGN §5).
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::NotFound { .. } => ErrorCode::ToolMissing,
            Self::LineTooLong { .. } => ErrorCode::Contract,
            Self::Spawn { .. } | Self::Io { .. } => ErrorCode::Internal,
        }
    }

    /// Never retryable: a missing binary and a protocol violation are both permanent.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        false
    }

    /// The static tool label this failure is about.
    #[must_use]
    pub const fn tool(&self) -> &'static str {
        match *self {
            Self::NotFound { tool, .. }
            | Self::Spawn { tool, .. }
            | Self::Io { tool, .. }
            | Self::LineTooLong { tool, .. } => tool,
        }
    }
}

impl From<ProcError> for ProviderError {
    /// Maps onto the provider taxonomy, preserving the two codes that carry information: a missing
    /// binary becomes [`ProviderError::ToolMissing`] (which is why [`SpawnSpec`] carries a
    /// `&'static str` tool label at all) and an over-long line becomes
    /// [`ProviderError::Contract`]. `ENOSPC` anywhere becomes [`ProviderError::Disk`].
    fn from(e: ProcError) -> Self {
        let is_enospc = |src: &io::Error| src.raw_os_error() == Some(nix::libc::ENOSPC);
        match &e {
            ProcError::NotFound { tool, .. } => Self::ToolMissing(tool),
            ProcError::LineTooLong { .. } => Self::Contract(e.to_string()),
            ProcError::Spawn { source, .. } | ProcError::Io { source, .. } if is_enospc(source) => {
                Self::Disk(e.to_string())
            }
            ProcError::Spawn { .. } | ProcError::Io { .. } => Self::Other(e.to_string()),
        }
    }
}

/// Which environment a child sees (DESIGN §6.5.3).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct EnvPolicy {
    /// Start from an empty environment. A `command` plugin always does; the yt-dlp shim does not,
    /// because `YTDL_*`, proxy and locale variables are part of its contract.
    pub clear: bool,
    /// Names inherited from the server process, applied after the clear.
    pub pass: Vec<OsString>,
    /// Literal key/value pairs, applied last so they win.
    pub set: Vec<(OsString, OsString)>,
}

/// Resource limits applied in the child between `fork` and `exec` (DESIGN §6.5.3).
///
/// `None` means "leave the inherited limit alone". `address_space` is `RLIMIT_AS`, which exists
/// only on Linux — the deployment target — and is silently skipped elsewhere so the test suite
/// still runs on a developer's macOS machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rlimits {
    /// `RLIMIT_AS`, bytes. Linux only.
    pub address_space: Option<u64>,
    /// `RLIMIT_FSIZE`, bytes — the cap on any single file the child writes.
    pub file_size: Option<u64>,
    /// `RLIMIT_CPU`, seconds of CPU time.
    pub cpu_secs: Option<u64>,
    /// `RLIMIT_NOFILE`, open descriptors.
    pub nofile: Option<u64>,
}

impl Rlimits {
    /// Whether anything at all would be applied.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.address_space.is_none()
            && self.file_size.is_none()
            && self.cpu_secs.is_none()
            && self.nofile.is_none()
    }
}

/// A real-time observer for the child's stderr lines (DESIGN §9.1).
///
/// The mandatory drain is the only reader of the stderr pipe, so it is the only place a line can
/// be seen *as it arrives*; a consumer that reads [`StderrRing`] after the job instead gets the
/// same information at the wrong time (and only the retained tail of it). Installed with
/// [`SpawnSpec::stderr_line_hook`], it is called once per line with the child's pid, on the drain
/// task, before the line reaches the ring.
///
/// It must not block: a slow observer slows the drain, and a *stalled* drain is the deadlock this
/// module exists to prevent. Log from it; do not do IO in it.
#[derive(Clone)]
pub struct StderrLineHook(StderrLineObserver);

/// The boxed observer behind [`StderrLineHook`]: `(pid, line)`.
type StderrLineObserver = Arc<dyn Fn(u32, &str) + Send + Sync>;

impl StderrLineHook {
    /// Wraps an observer.
    #[must_use]
    pub fn new(f: impl Fn(u32, &str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Calls the observer.
    pub fn call(&self, pid: u32, line: &str) {
        (self.0)(pid, line);
    }
}

impl std::fmt::Debug for StderrLineHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StderrLineHook(..)")
    }
}

/// How a child process is to be spawned.
///
/// Built with the chained setters; every default is the safe one (own process group, `nice(5)`,
/// stderr piped into a bounded ring, stdout and stdin closed).
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    tool: &'static str,
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: EnvPolicy,
    limits: Rlimits,
    nice: Option<i32>,
    stdin_piped: bool,
    stdout_piped: bool,
    ring_lines: usize,
    ring_bytes: usize,
    max_line_bytes: usize,
    kill_grace: Duration,
    stderr_tap: Option<mpsc::Sender<Vec<u8>>>,
    stderr_line_hook: Option<StderrLineHook>,
}

impl SpawnSpec {
    /// A spec for `program`, labelled `tool`.
    ///
    /// `tool` is a `&'static str` because it is what a missing binary reports as
    /// [`ProviderError::ToolMissing`], and that variant is `&'static str` by design (DESIGN §6.1).
    /// Use the tool's canonical name — `"ffmpeg"`, `"python3"`, `"N_m3u8DL-RE"` — or `"plugin"`
    /// for a `command` plugin, whose `argv[0]` is validated at manifest load time instead.
    #[must_use]
    pub fn new(tool: &'static str, program: impl Into<OsString>) -> Self {
        Self {
            tool,
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: EnvPolicy::default(),
            limits: Rlimits::default(),
            nice: Some(DEFAULT_NICE),
            stdin_piped: false,
            stdout_piped: false,
            ring_lines: STDERR_RING_LINES,
            ring_bytes: STDERR_RING_BYTES,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            kill_grace: DEFAULT_KILL_GRACE,
            stderr_tap: None,
            stderr_line_hook: None,
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Replaces the environment policy.
    #[must_use]
    pub fn env(mut self, policy: EnvPolicy) -> Self {
        self.env = policy;
        self
    }

    /// Applies resource limits.
    #[must_use]
    pub const fn limits(mut self, limits: Rlimits) -> Self {
        self.limits = limits;
        self
    }

    /// Overrides the niceness. `None` leaves the inherited priority alone.
    #[must_use]
    pub const fn nice(mut self, nice: Option<i32>) -> Self {
        self.nice = nice;
        self
    }

    /// Gives the child a writable stdin.
    #[must_use]
    pub const fn stdin_piped(mut self, yes: bool) -> Self {
        self.stdin_piped = yes;
        self
    }

    /// Gives the child a readable stdout, available as [`Child::stdout_lines`].
    #[must_use]
    pub const fn stdout_piped(mut self, yes: bool) -> Self {
        self.stdout_piped = yes;
        self
    }

    /// Resizes the stderr ring.
    #[must_use]
    pub const fn stderr_ring(mut self, lines: usize, bytes: usize) -> Self {
        self.ring_lines = lines;
        self.ring_bytes = bytes;
        self
    }

    /// Sets the hard cap on one line of stdout.
    #[must_use]
    pub const fn max_line_bytes(mut self, cap: usize) -> Self {
        self.max_line_bytes = cap;
        self
    }

    /// Sets the `SIGTERM` → `SIGKILL` grace.
    #[must_use]
    pub const fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Duplicates every chunk the child writes to stderr into `tx`, **in addition** to the
    /// bounded ring.
    ///
    /// Added for the `command` plugin, whose `progress.source = "stderr" | "both"` has to parse
    /// stderr rather than merely keep its tail (DESIGN §6.5.1), and whose `max_output_bytes`
    /// budget counts stdout **and** stderr (DESIGN §6.5.1). The ring is unaffected, so the
    /// mandatory drain and the error tail keep working exactly as before.
    ///
    /// Delivery is `try_send`: a full channel drops the chunk rather than blocking the drain,
    /// because a blocked drain is the deadlock this module exists to prevent. Progress is lossy
    /// by design; size the channel so the budget stays approximately honest.
    #[must_use]
    pub fn stderr_tap(mut self, tx: mpsc::Sender<Vec<u8>>) -> Self {
        self.stderr_tap = Some(tx);
        self
    }

    /// Observes every stderr **line** as the drain reads it (DESIGN §9.1).
    ///
    /// [`Self::stderr_tap`]'s sibling for consumers that want lines rather than raw chunks and
    /// want them *now* rather than at the end of the job: `f` is called with the child's pid and
    /// one complete line, on the drain task, before the line reaches the ring. The ring and the
    /// tap are unaffected.
    ///
    /// This is how the yt-dlp shim's stderr reaches `tracing` at DEBUG (WARN for `ERROR:` /
    /// `WARNING:`) while the job is still running, which is what DESIGN §9.1 asks for and what
    /// reading [`StderrRing`] after the fact cannot do. `f` must not block — see
    /// [`StderrLineHook`].
    #[must_use]
    pub fn stderr_line_hook(mut self, f: impl Fn(u32, &str) + Send + Sync + 'static) -> Self {
        self.stderr_line_hook = Some(StderrLineHook::new(f));
        self
    }

    /// The tool label.
    #[must_use]
    pub const fn tool_name(&self) -> &'static str {
        self.tool
    }

    /// The argv this spec will exec, for `GET api/v2/providers`' audit view (DESIGN §6.5.3).
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    /// Builds the [`Command`], applying every part of the policy **except** the spawn itself.
    ///
    /// Exposed so a provider that needs one more thing — `aulos-provider-ytdlp` passes the shim a
    /// third file descriptor with `command-fds` — can add it and then hand the command back to
    /// [`Child::spawn_command`]. Nothing else may bypass [`Child`]: the stderr drain is what makes
    /// this helper worth having.
    #[must_use]
    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        if self.env.clear {
            cmd.env_clear();
            for name in &self.env.pass {
                if let Some(v) = std::env::var_os(name) {
                    cmd.env(name, v);
                }
            }
        }
        for (k, v) in &self.env.set {
            cmd.env(k, v);
        }
        cmd.stdin(if self.stdin_piped {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(if self.stdout_piped {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        // Never `Stdio::null()`: the ring is the error tail every provider reports, and an
        // undrained pipe is the deadlock this module exists to prevent.
        cmd.stderr(Stdio::piped());
        // Its own process group, so a cancel is one `killpg` and a grandchild cannot survive it.
        cmd.process_group(0);
        // tokio's own safety net for the *direct* child if we are dropped without a runtime.
        cmd.kill_on_drop(true);

        let nice = self.nice;
        let limits = self.limits;
        if nice.is_some() || !limits.is_empty() {
            // SAFETY: the closure runs between `fork` and `exec` in the child. `setrlimit` and
            // `nice` are async-signal-safe syscall wrappers; nothing here allocates, locks or
            // touches the Rust runtime.
            unsafe {
                cmd.as_std_mut().pre_exec(move || {
                    if let Some(n) = nice {
                        // Best effort: lowering priority is never worth failing a download over.
                        let _ = nix::libc::nice(n);
                    }
                    apply_rlimits(&limits);
                    Ok(())
                });
            }
        }
        cmd
    }
}

/// Applies [`Rlimits`] in the freshly forked child. Failures are ignored: a limit we cannot set is
/// not a reason to fail the job, and the operator's own `ulimit` still applies.
fn apply_rlimits(limits: &Rlimits) {
    use nix::sys::resource::{Resource, setrlimit};

    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(v) = limits.address_space {
        let _ = setrlimit(Resource::RLIMIT_AS, v, v);
    }
    if let Some(v) = limits.file_size {
        let _ = setrlimit(Resource::RLIMIT_FSIZE, v, v);
    }
    if let Some(v) = limits.cpu_secs {
        let _ = setrlimit(Resource::RLIMIT_CPU, v, v);
    }
    if let Some(v) = limits.nofile {
        let _ = setrlimit(Resource::RLIMIT_NOFILE, v, v);
    }
}

/// A bounded, cheap-to-clone view of a child's most recent stderr (DESIGN §2.3).
///
/// Oldest lines are evicted, so the ring holds the **end** of the output — which is where the
/// error message is. `dropped()` counts what fell out, so a report can say "…and 4102 earlier
/// lines" instead of pretending the tail is the whole story.
#[derive(Clone, Debug)]
pub struct StderrRing {
    inner: Arc<Mutex<Ring>>,
}

#[derive(Debug)]
struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    dropped: u64,
}

impl StderrRing {
    /// An empty ring with the given budgets.
    #[must_use]
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Ring {
                lines: VecDeque::new(),
                bytes: 0,
                max_lines: max_lines.max(1),
                max_bytes: max_bytes.max(1),
                dropped: 0,
            })),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut Ring) -> T) -> T {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// Appends one line, evicting from the front until both budgets hold.
    pub fn push(&self, line: &str) {
        let line = strip_ansi(line);
        let line = line.trim_end().to_owned();
        self.with(|r| {
            r.bytes += line.len() + 1;
            r.lines.push_back(line);
            while r.lines.len() > r.max_lines || (r.bytes > r.max_bytes && r.lines.len() > 1) {
                if let Some(old) = r.lines.pop_front() {
                    r.bytes = r.bytes.saturating_sub(old.len() + 1);
                    r.dropped += 1;
                }
            }
        });
    }

    /// The retained lines, oldest first.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.with(|r| r.lines.iter().cloned().collect())
    }

    /// The last `max_bytes` bytes of retained output, newline-joined and ANSI-free.
    ///
    /// This is what a provider puts in `error.message`: 2 KiB for a `command` plugin
    /// (DESIGN §6.5.3), 8 KiB for the yt-dlp shim (DESIGN §9.7).
    #[must_use]
    pub fn tail(&self, max_bytes: usize) -> String {
        self.with(|r| {
            let mut out: VecDeque<&str> = VecDeque::new();
            let mut total = 0usize;
            for line in r.lines.iter().rev() {
                if total + line.len() + 1 > max_bytes && !out.is_empty() {
                    break;
                }
                total += line.len() + 1;
                out.push_front(line.as_str());
            }
            let joined = out.into_iter().collect::<Vec<_>>().join("\n");
            if joined.len() > max_bytes {
                // One single line longer than the whole budget: keep its end.
                let start = joined.len() - max_bytes;
                let start = (start..joined.len())
                    .find(|i| joined.is_char_boundary(*i))
                    .unwrap_or(joined.len());
                joined[start..].to_owned()
            } else {
                joined
            }
        })
    }

    /// Whether anything was captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.with(|r| r.lines.is_empty())
    }

    /// How many lines were evicted to stay inside the budgets.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.with(|r| r.dropped)
    }
}

/// Removes ANSI escape sequences (CSI and OSC) and carriage returns from `s`.
///
/// Written here rather than pulled from a crate because `aulos-provider` may not take a dependency
/// the DESIGN §3 table does not list, and because every consumer wants exactly this: the stderr
/// ring, the plugin progress parser (DESIGN §6.5.1 `strip_ansi`) and
/// [`crate::provider::ProviderError::message`].
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte in 0x40..=0x7e.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs until BEL or ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-byte escape: drop both.
            Some(_) | None => {}
        }
    }
    out
}

/// A reader that yields whole lines and refuses to grow without a bound.
///
/// `\n` and `\r\n` both terminate a line; a `\r` on its own does **not**, because a progress
/// repaint frame is not a line and the plugin grammar has its own `cr_as_newline` option for that
/// (DESIGN §6.5.1).
#[derive(Debug)]
pub struct Lines<R> {
    reader: BufReader<R>,
    tool: &'static str,
    cap: usize,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    /// Wraps `inner`, failing any line longer than `cap` bytes.
    #[must_use]
    pub fn new(inner: R, tool: &'static str, cap: usize) -> Self {
        Self {
            reader: BufReader::new(inner),
            tool,
            cap: cap.max(1),
            buf: Vec::new(),
        }
    }

    /// The next chunk of bytes, exactly as the pipe delivered it, or `None` at end of stream.
    ///
    /// Added for the `command` plugin progress grammar (DESIGN §6.5.1), which needs to see a bare
    /// `\r` repaint frame: [`Self::next_line`] deliberately does **not** treat a lone `\r` as a
    /// terminator, so a tool that repaints for a minute without printing a newline would deliver
    /// nothing at all through it. A chunk reader has no such blind spot, and
    /// [`crate::command::ProgressParser`] does its own framing.
    ///
    /// Mixing this with [`Self::next_line`] on the same stream is legal but pointless — one call
    /// consumes what the other would have framed.
    ///
    /// # Errors
    /// [`ProcError::Io`] on a read failure.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ProcError> {
        let available = self
            .reader
            .fill_buf()
            .await
            .map_err(|source| ProcError::Io {
                tool: self.tool,
                source,
            })?;
        if available.is_empty() {
            return Ok(None);
        }
        let n = available.len();
        let owned = available.to_vec();
        self.reader.consume(n);
        Ok(Some(owned))
    }

    /// The next line, or `None` at end of stream.
    ///
    /// Invalid UTF-8 is replaced rather than rejected: a truncated multi-byte character in a
    /// progress repaint must not kill a download.
    ///
    /// # Errors
    /// [`ProcError::LineTooLong`] when a line exceeds the cap — the caller must kill the child.
    /// [`ProcError::Io`] on a read failure.
    pub async fn next_line(&mut self) -> Result<Option<String>, ProcError> {
        self.buf.clear();
        loop {
            // `fill_buf`/`consume` rather than `read_until`, so a child that never emits a
            // newline cannot make us buffer its whole output before the cap is noticed: peak
            // memory here is the cap plus one `BufReader` block, not the line's real length.
            let available = self
                .reader
                .fill_buf()
                .await
                .map_err(|source| ProcError::Io {
                    tool: self.tool,
                    source,
                })?;
            if available.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    self.buf.extend_from_slice(&available[..pos]);
                    self.reader.consume(pos + 1);
                    if self.buf.last() == Some(&b'\r') {
                        self.buf.pop();
                    }
                    break;
                }
                None => {
                    let n = available.len();
                    self.buf.extend_from_slice(available);
                    self.reader.consume(n);
                }
            }
            if self.buf.len() > self.cap {
                return Err(ProcError::LineTooLong {
                    tool: self.tool,
                    cap: self.cap,
                });
            }
        }
        if self.buf.len() > self.cap {
            return Err(ProcError::LineTooLong {
                tool: self.tool,
                cap: self.cap,
            });
        }
        Ok(Some(String::from_utf8_lossy(&self.buf).into_owned()))
    }
}

/// A spawned child process with its own process group, a drained stderr ring and a bounded stdout
/// line reader.
///
/// Dropping this kills the whole group — see the module docs.
#[derive(Debug)]
pub struct Child {
    child: TokioChild,
    pgid: Pid,
    tool: &'static str,
    stdin: Option<ChildStdin>,
    stdout: Option<Lines<ChildStdout>>,
    stderr: StderrRing,
    /// The mandatory stderr drain, kept so [`Child::wait_drained`] can join it.
    drain: Option<tokio::task::JoinHandle<()>>,
    kill_grace: Duration,
    reaped: bool,
}

impl Child {
    /// Spawns `spec`.
    ///
    /// # Errors
    /// [`ProcError::NotFound`] when `argv[0]` does not exist, [`ProcError::Spawn`] otherwise.
    pub fn spawn(spec: &SpawnSpec) -> Result<Self, ProcError> {
        Self::spawn_command(spec, spec.to_command())
    }

    /// Spawns a [`Command`] that was built by [`SpawnSpec::to_command`] and then customised.
    ///
    /// The `spec` is still needed for the tool label and the ring and grace settings; passing a
    /// command built from a *different* spec would attach the wrong stderr budget, so don't.
    ///
    /// # Errors
    /// As [`Self::spawn`].
    pub fn spawn_command(spec: &SpawnSpec, mut cmd: Command) -> Result<Self, ProcError> {
        let mut child = cmd.spawn().map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                ProcError::NotFound {
                    tool: spec.tool,
                    source,
                }
            } else {
                ProcError::Spawn {
                    tool: spec.tool,
                    source,
                }
            }
        })?;

        // `process_group(0)` makes the child a group leader, so pgid == pid. `id()` is `Some`
        // until the child is reaped, and we have not awaited it yet.
        let pid = child.id().unwrap_or(0);
        let pgid = Pid::from_raw(i32::try_from(pid).unwrap_or(0));

        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .map(|o| Lines::new(o, spec.tool, spec.max_line_bytes));

        let stderr = StderrRing::new(spec.ring_lines, spec.ring_bytes);
        let mut drain = None;
        if let Some(pipe) = child.stderr.take() {
            let ring = stderr.clone();
            let tool = spec.tool;
            let tap = spec.stderr_tap.clone();
            let line_hook = spec.stderr_line_hook.clone();
            // The mandatory drain (DESIGN §2.3). It ends when the pipe closes, i.e. when the child
            // exits, so it cannot outlive the job.
            let cap = spec.ring_bytes.max(4096);
            drain = Some(tokio::spawn(async move {
                let mut reader = BufReader::new(pipe);
                let mut line: Vec<u8> = Vec::new();
                loop {
                    // Chunked rather than `read_line`, because a child that writes a megabyte
                    // with no newline in it must not be able to make the drain allocate a
                    // megabyte: this is a **bounded** drain, and it is the only reader of the
                    // pipe, so bounding it cannot deadlock the child.
                    let chunk = match reader.fill_buf().await {
                        Ok([]) => break,
                        Ok(available) => {
                            let n = available.len();
                            let owned = available.to_vec();
                            reader.consume(n);
                            owned
                        }
                        Err(e) => {
                            tracing::debug!(tool, error = %e, "stderr drain ended");
                            break;
                        }
                    };
                    if let Some(tap) = &tap {
                        let _ = tap.try_send(chunk.clone());
                    }
                    for b in chunk {
                        if b == b'\n' || line.len() >= cap {
                            let text = String::from_utf8_lossy(&line);
                            if let Some(hook) = &line_hook {
                                hook.call(pid, &text);
                            }
                            ring.push(&text);
                            line.clear();
                        }
                        if b != b'\n' {
                            line.push(b);
                        }
                    }
                }
                if !line.is_empty() {
                    let text = String::from_utf8_lossy(&line);
                    if let Some(hook) = &line_hook {
                        hook.call(pid, &text);
                    }
                    ring.push(&text);
                }
            }));
        }

        tracing::debug!(tool = spec.tool, pid, argv = ?spec.argv(), "spawned");
        Ok(Self {
            child,
            pgid,
            tool: spec.tool,
            stdin,
            stdout,
            stderr,
            drain,
            kill_grace: spec.kill_grace,
            reaped: false,
        })
    }

    /// The child's pid, which is also its process group id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        u32::try_from(self.pgid.as_raw()).unwrap_or(0)
    }

    /// The tool label this child was spawned with.
    #[must_use]
    pub const fn tool(&self) -> &'static str {
        self.tool
    }

    /// The child's stdin, if it was piped. Takes it, so it can be closed by dropping — which is
    /// how the yt-dlp shim is told the job is fully written.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// The child's stdout line reader, if stdout was piped.
    pub fn stdout_lines(&mut self) -> Option<&mut Lines<ChildStdout>> {
        self.stdout.as_mut()
    }

    /// Takes the stdout reader out, so it can be polled in the same `select!` as [`Self::wait`].
    ///
    /// [`Self::stdout_lines`] borrows the whole [`Child`], which makes "read stdout **or** notice
    /// the child exited, whichever happens first" unwritable. The `command` provider needs exactly
    /// that: a plugin's stall watchdog, its cancellation token, its output budget and its exit all
    /// race in one loop (DESIGN §6.5.3). Taking the reader out is the only borrow-safe shape.
    pub fn take_stdout(&mut self) -> Option<Lines<ChildStdout>> {
        self.stdout.take()
    }

    /// A handle on the captured stderr. Cheap to clone and safe to read while the child runs.
    #[must_use]
    pub fn stderr(&self) -> StderrRing {
        self.stderr.clone()
    }

    /// Waits for the child to exit.
    ///
    /// # Errors
    /// [`ProcError::Io`] if the wait itself fails.
    pub async fn wait(&mut self) -> Result<ExitStatus, ProcError> {
        let status = self.child.wait().await.map_err(|source| ProcError::Io {
            tool: self.tool,
            source,
        })?;
        self.reaped = true;
        Ok(status)
    }

    /// The bound [`Self::wait_drained`] puts on joining the stderr drain.
    pub const DRAIN_JOIN_GRACE: Duration = Duration::from_millis(250);

    /// Waits for the child to exit **and** for the mandatory stderr drain to reach end of stream.
    ///
    /// [`Self::wait`] reaps the child without joining the drain task, so reading
    /// [`Self::stderr`]'s tail the instant it returns is a race — the last chunk can still be in
    /// the pipe, and the tail comes back empty about one run in twenty. Anything that puts the
    /// stderr tail in a user-visible error message (DESIGN §6.5.3, §9.6) should wait here.
    ///
    /// The join is bounded by [`Self::DRAIN_JOIN_GRACE`], because a pipe closes only when *every*
    /// writer closes it: a grandchild that inherited stderr and outlived its parent must not be
    /// able to wedge the caller. A timeout is logged at debug and leaves the tail as it stands.
    ///
    /// # Errors
    /// As [`Self::wait`]. A drain that panicked or timed out is logged, not returned — the exit
    /// status is the caller's answer either way.
    pub async fn wait_drained(&mut self) -> Result<ExitStatus, ProcError> {
        let status = self.wait().await?;
        self.drained().await;
        Ok(status)
    }

    /// Waits for the mandatory stderr drain to reach end of stream, bounded by
    /// [`Self::DRAIN_JOIN_GRACE`].
    ///
    /// [`Self::wait_drained`] is the one-call form. This is the same guarantee for a caller that
    /// already has the exit status — the `select!` loops that race stdout, cancellation and the
    /// exit (DESIGN §6.5.3, §10.5) get their status from the `select!` and only then want a
    /// settled tail. Idempotent: a second call is a no-op.
    pub async fn drained(&mut self) {
        let Some(handle) = self.drain.take() else {
            return;
        };
        match tokio::time::timeout(Self::DRAIN_JOIN_GRACE, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(tool = self.tool, error = %e, "stderr drain task ended abnormally");
            }
            Err(_) => tracing::debug!(
                tool = self.tool,
                "stderr drain did not settle; a grandchild may still hold the pipe"
            ),
        }
    }

    /// Kills the whole process group: `SIGTERM`, then `SIGKILL` after the grace period.
    ///
    /// This is the cancel path (DESIGN §6.5.3, §9.7) and the fix for legacy's orphaned `ffmpeg`
    /// grandchildren. Returns the child's exit status, which is `None` only if the wait failed.
    pub async fn kill_group(&mut self) -> Option<ExitStatus> {
        if self.reaped {
            return None;
        }
        self.signal_group(Signal::SIGTERM);
        match tokio::time::timeout(self.kill_grace, self.child.wait()).await {
            Ok(Ok(status)) => {
                self.reaped = true;
                Some(status)
            }
            Ok(Err(e)) => {
                tracing::debug!(tool = self.tool, error = %e, "wait failed after SIGTERM");
                None
            }
            Err(_) => {
                tracing::warn!(
                    tool = self.tool,
                    pid = self.pid(),
                    grace_ms = self.kill_grace.as_millis(),
                    "process group ignored SIGTERM, sending SIGKILL"
                );
                self.signal_group(Signal::SIGKILL);
                match self.child.wait().await {
                    Ok(status) => {
                        self.reaped = true;
                        Some(status)
                    }
                    Err(e) => {
                        tracing::debug!(tool = self.tool, error = %e, "wait failed after SIGKILL");
                        None
                    }
                }
            }
        }
    }

    fn signal_group(&self, sig: Signal) {
        if self.pgid.as_raw() <= 1 {
            return;
        }
        if let Err(e) = killpg(self.pgid, sig) {
            // ESRCH just means the group is already gone, which is the happy path.
            if e != nix::errno::Errno::ESRCH {
                tracing::debug!(tool = self.tool, pgid = self.pgid.as_raw(), ?sig, error = %e, "killpg failed");
            }
        }
    }
}

impl Drop for Child {
    /// The panic-safety net: the same `SIGTERM` → grace → `SIGKILL` sequence, so an unwind cannot
    /// leave a downloader running.
    ///
    /// `Drop` cannot await, so the `SIGKILL` half is deferred to a detached task when a runtime is
    /// available (it always is in the server and in a `#[tokio::test]`); without one, the group
    /// gets its `SIGTERM` and tokio's `kill_on_drop` deals with the direct child.
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        self.signal_group(Signal::SIGTERM);
        let (pgid, grace, tool) = (self.pgid, self.kill_grace, self.tool);
        if pgid.as_raw() > 1
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                tokio::time::sleep(grace).await;
                if let Err(e) = killpg(pgid, Signal::SIGKILL) {
                    if e != nix::errno::Errno::ESRCH {
                        tracing::debug!(tool, pgid = pgid.as_raw(), error = %e, "deferred SIGKILL failed");
                    }
                } else {
                    tracing::warn!(tool, pgid = pgid.as_raw(), "process group killed by the drop guard");
                }
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_tail_and_counts_what_it_dropped() {
        let ring = StderrRing::new(3, 1024);
        for i in 0..10 {
            ring.push(&format!("line {i}"));
        }
        assert_eq!(ring.lines(), ["line 7", "line 8", "line 9"]);
        assert_eq!(ring.dropped(), 7);
        assert!(!ring.is_empty());
        assert_eq!(ring.tail(1024), "line 7\nline 8\nline 9");
        // A byte budget evicts too, but never below one line.
        let tiny = StderrRing::new(64, 8);
        tiny.push("aaaaaaaaaaaaaaaaaaaa");
        tiny.push("bb");
        assert_eq!(tiny.lines(), ["bb"]);
    }

    #[test]
    fn the_tail_is_byte_bounded_even_for_one_huge_line() {
        let ring = StderrRing::new(8, 1_000_000);
        ring.push(&"x".repeat(5000));
        let tail = ring.tail(100);
        assert_eq!(tail.len(), 100);
        assert!(tail.chars().all(|c| c == 'x'));
    }

    #[test]
    fn the_tail_never_splits_a_character() {
        let ring = StderrRing::new(8, 1_000_000);
        ring.push(&"é".repeat(50)); // 100 bytes
        let tail = ring.tail(11);
        assert!(tail.len() <= 11);
        assert!(tail.chars().all(|c| c == 'é'));
    }

    #[test]
    fn ansi_and_carriage_returns_are_stripped() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[0;31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("a\rb"), "ab");
        assert_eq!(strip_ansi("\u{1b}[2K\u{1b}[1G 42%"), " 42%");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}x"), "x");
        assert_eq!(strip_ansi("\u{1b}]0;t\u{1b}\\x"), "x");
        // Only CSI and OSC are understood; a two-byte escape drops ESC and its selector only.
        assert_eq!(strip_ansi("\u{1b}(Bx"), "Bx");
        assert_eq!(strip_ansi("\u{1b}"), "");
        // A real N_m3u8DL-RE repaint frame.
        assert_eq!(
            strip_ansi("\r\u{1b}[?25l  Vid 1080x1920 | 42.10% 100/238\u{1b}[?25h"),
            "  Vid 1080x1920 | 42.10% 100/238"
        );
    }

    #[test]
    fn the_ring_stores_ansi_free_lines() {
        let ring = StderrRing::new(4, 1024);
        ring.push("\u{1b}[31merror: boom\u{1b}[0m\n");
        assert_eq!(ring.lines(), ["error: boom"]);
    }

    #[test]
    fn proc_errors_map_onto_the_taxonomy() {
        let not_found = ProcError::NotFound {
            tool: "ffmpeg",
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert_eq!(not_found.code(), ErrorCode::ToolMissing);
        assert_eq!(not_found.tool(), "ffmpeg");
        assert!(!not_found.retryable());
        assert!(matches!(
            ProviderError::from(not_found),
            ProviderError::ToolMissing("ffmpeg")
        ));

        let long = ProcError::LineTooLong {
            tool: "plugin",
            cap: 10,
        };
        assert_eq!(long.code(), ErrorCode::Contract);
        assert!(matches!(
            ProviderError::from(long),
            ProviderError::Contract(_)
        ));

        let enospc = ProcError::Io {
            tool: "ffmpeg",
            source: io::Error::from_raw_os_error(nix::libc::ENOSPC),
        };
        assert!(matches!(
            ProviderError::from(enospc),
            ProviderError::Disk(_)
        ));

        let other = ProcError::Io {
            tool: "ffmpeg",
            source: io::Error::other("nope"),
        };
        assert_eq!(other.code(), ErrorCode::Internal);
        assert!(matches!(
            ProviderError::from(other),
            ProviderError::Other(_)
        ));
    }

    #[test]
    fn the_spec_reports_its_argv_and_defaults() {
        let spec = SpawnSpec::new("python3", "/usr/bin/python3")
            .arg("runner.py")
            .args(["--mode", "extract"]);
        assert_eq!(
            spec.argv(),
            ["/usr/bin/python3", "runner.py", "--mode", "extract"]
        );
        assert_eq!(spec.tool_name(), "python3");
        assert_eq!(spec.nice, Some(DEFAULT_NICE));
        assert_eq!(spec.kill_grace, DEFAULT_KILL_GRACE);
        assert_eq!(spec.ring_lines, STDERR_RING_LINES);
        assert!(spec.limits.is_empty());
    }

    #[tokio::test]
    async fn lines_splits_on_lf_and_crlf_but_not_on_a_bare_cr() {
        let data = b"a\nb\r\nc\rd\ne".to_vec();
        let mut lines = Lines::new(std::io::Cursor::new(data), "t", 1024);
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("a"));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("b"));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("c\rd"));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("e"));
        assert_eq!(lines.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn lines_reports_an_over_long_line_as_a_contract_failure() {
        let data = vec![b'x'; 4096];
        let mut lines = Lines::new(std::io::Cursor::new(data), "plugin", 64);
        let err = lines.next_line().await.expect_err("must reject");
        assert_eq!(err.code(), ErrorCode::Contract);
        assert!(matches!(err, ProcError::LineTooLong { cap: 64, .. }));
    }

    #[tokio::test]
    async fn lines_replaces_invalid_utf8() {
        let mut lines = Lines::new(std::io::Cursor::new(vec![b'a', 0xff, b'\n']), "t", 64);
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("a\u{fffd}")
        );
    }
}
