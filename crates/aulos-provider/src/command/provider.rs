//! The `command` [`Provider`] implementation: discovery, spawning, isolation and the three
//! success criteria (DESIGN §6.5, §6.5.3).
//!
//! # The spawn policy, in one place
//!
//! Both `resolve` and `download` run under the same policy, and none of it is optional
//! (DESIGN §6.5.3):
//!
//! - **cleared environment** plus `env.pass` names and `env.set` literals — a plugin never sees
//!   the server's tokens by accident;
//! - **own process group**, so a cancel is one `killpg` and no grandchild survives it;
//! - **`nice(5)`**, so a download never starves the server's own tasks;
//! - **`RLIMIT_AS` / `RLIMIT_CPU` / `RLIMIT_NOFILE` / `RLIMIT_FSIZE`** from `[limits]`;
//! - **no inherited descriptors** beyond stdio, and `out_dir` / `tmp_dir` as the only paths handed
//!   over;
//! - **no shell** — argv is templated element by element and handed to `execvp` verbatim;
//! - **a cumulative output budget**: over `limits.max_output_bytes` the group is killed and the
//!   item fails `contract`;
//! - **a stall watchdog**: `limits.download_stall_secs` with no progress kills the group.
//!
//! **Plugins are not a security boundary.** A plugin runs as the server user and can do anything
//! that user can; the list above bounds accidents, not malice. What makes this safe to ship is
//! that the plugin directory is operator-controlled and that
//! [`CommandProvider::audit`] exposes every argv so an operator can see what is installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use aulos_core::catalog::FormatCatalog;
use aulos_core::error::ErrorCode;
use aulos_core::paths::{RelPath, sanitize_path_component};
use aulos_core::reload::{ReloadFailure, ReloadReport};
use aulos_core::selection::{DownloadType, ProviderId};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::time::Instant;
use url::Url;

use super::hookspec::HookSpec;
use super::manifest::{
    CommandSpec, ExpectOutput, MatchSpec, PluginManifest, ResolveFormat, STDERR_TAIL_BYTES,
    StdinMode, load_manifest, partial_match,
};
use super::progress::{ProgressParser, ProgressUpdate, StatusTarget};
use super::sha256::sha256_hex_prefix;
use super::template::{TemplateCtx, render_argv};
use crate::entry::{EntryHints, EntryKind, MediaEntry};
use crate::outcome::Outcome;
use crate::proc::{Child, EnvPolicy, Rlimits, SpawnSpec, StderrRing, strip_ansi};
use crate::provider::{
    DegradedProvider, DownloadCtx, Match, Provider, ProviderError, ProviderHealth, ResolveCtx,
    SCORE_HOST_REGEX, SCORE_HOST_SUFFIX, SCORE_PATH_REGEX,
};
use crate::registry::{CommandLoadResult, CommandLoader, LoadedPlugin};
use crate::sink::{ProgressSink, Stage};

/// The `&'static str` tool label every plugin child is spawned with.
///
/// `argv[0]` is validated at manifest load time instead of being reported as
/// [`ProviderError::ToolMissing`], because a plugin's program name is not static and
/// `ToolMissing` names a binary the *image* is supposed to ship (DESIGN §6.1).
pub const PLUGIN_TOOL: &str = "plugin";

/// How many stderr chunks may queue between the drain and the progress parser before the oldest
/// are dropped. Progress is lossy by design; the budget accounting is approximate above this.
const STDERR_TAP_CAPACITY: usize = 1024;

/// The cap on a rendered `{out_name}`, so a 4000-character title cannot produce an `ENAMETOOLONG`.
const OUT_NAME_MAX: usize = 150;

/// Filesystem context a plugin needs that [`DownloadCtx`] does not carry.
///
/// `DownloadCtx` (DESIGN §6.1) has no `Paths`, so `{cookies_file}` — `STATE_DIR/cookies.txt` if
/// present, else `""` — cannot be derived at download time. It is supplied once, at discovery
/// time, instead. See `docs/INTEGRATION-NOTES.md`, WP-10.
#[derive(Clone, Debug, Default)]
pub struct PluginEnv {
    /// `STATE_DIR`. `{cookies_file}` resolves to `<state_dir>/cookies.txt` when that file exists.
    pub state_dir: PathBuf,
}

impl PluginEnv {
    /// The cookie jar path, or `""` when there is none (DESIGN §6.5.1).
    #[must_use]
    pub fn cookies_file(&self) -> String {
        if self.state_dir.as_os_str().is_empty() {
            return String::new();
        }
        let p = self.state_dir.join("cookies.txt");
        if p.is_file() {
            p.to_string_lossy().into_owned()
        } else {
            String::new()
        }
    }
}

/// A community `command` plugin, as a [`Provider`] (DESIGN §6.5).
pub struct CommandProvider {
    id: ProviderId,
    manifest: Arc<PluginManifest>,
    env: PluginEnv,
    /// `limits.max_concurrent`. Held even when `uses_global_slot` is true, because the engine only
    /// learns about a per-provider cap through `own_slots()`, which is reserved for the
    /// *instead-of-the-global-slot* case (DESIGN §6.5.1, §8.7).
    downloads: Semaphore,
    resolves: Semaphore,
    /// The last spawn, for `limits.min_request_interval_ms`.
    last_spawn: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for CommandProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandProvider")
            .field("id", &self.id)
            .field("version", &self.manifest.version)
            .finish_non_exhaustive()
    }
}

impl CommandProvider {
    /// Wraps a validated manifest.
    #[must_use]
    pub fn new(manifest: Arc<PluginManifest>, env: PluginEnv) -> Self {
        let id = manifest.provider_id();
        let downloads = Semaphore::new(manifest.limits.max_concurrent as usize);
        let resolves = Semaphore::new(manifest.limits.max_concurrent_resolves as usize);
        Self {
            id,
            manifest,
            env,
            downloads,
            resolves,
            last_spawn: Mutex::new(None),
        }
    }

    /// The manifest this provider was built from.
    #[must_use]
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Every argv this plugin can execute, unrendered — the audit view `GET api/v2/providers`
    /// serves so an operator can see what is installed (DESIGN §6.5.3).
    #[must_use]
    pub fn audit(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        if let Some(r) = &self.manifest.resolve {
            out.push(
                r.command
                    .argv_source()
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
            );
        }
        if let Some(d) = &self.manifest.download {
            out.push(
                d.command
                    .argv_source()
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
            );
        }
        out
    }

    /// Waits out `limits.min_request_interval_ms` and records this spawn.
    async fn politeness_gate(&self) {
        let interval = Duration::from_millis(self.manifest.limits.min_request_interval_ms);
        if interval.is_zero() {
            return;
        }
        let mut guard = self.last_spawn.lock().await;
        if let Some(prev) = *guard {
            let elapsed = Instant::now().saturating_duration_since(prev);
            if elapsed < interval {
                tokio::time::sleep(interval - elapsed).await;
            }
        }
        *guard = Some(Instant::now());
    }

    /// The `[headers]` table, rendered. Done first, because `{headers_curl}` and
    /// `{headers_crlf}` substitute the result.
    fn headers(&self, ctx: &TemplateCtx) -> Result<Vec<(String, String)>, ProviderError> {
        self.manifest
            .headers
            .iter()
            .map(|(name, t)| {
                t.render(ctx)
                    .map(|v| (name.to_string(), v))
                    .map_err(|e| ProviderError::Contract(format!("headers.{name}: {e}")))
            })
            .collect()
    }

    /// The environment policy of DESIGN §6.5.3: cleared, plus `env.pass` and `env.set`, plus one
    /// `AULOS_HEADER_<NAME>` per declared header.
    fn env_policy(
        &self,
        ctx: &TemplateCtx,
        headers: &[(String, String)],
    ) -> Result<EnvPolicy, ProviderError> {
        let mut set = Vec::with_capacity(self.manifest.env.set.len() + headers.len());
        for (k, t) in &self.manifest.env.set {
            let v = t
                .render(ctx)
                .map_err(|e| ProviderError::Contract(format!("env.set.{k}: {e}")))?;
            set.push((k.to_string().into(), v.into()));
        }
        for (name, value) in headers {
            let upper: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            set.push((format!("AULOS_HEADER_{upper}").into(), value.clone().into()));
        }
        Ok(EnvPolicy {
            clear: true,
            pass: self
                .manifest
                .env
                .pass
                .iter()
                .map(|p| p.to_string().into())
                .collect(),
            set,
        })
    }

    fn rlimits(&self) -> Rlimits {
        let l = &self.manifest.limits;
        Rlimits {
            address_space: (l.memory_bytes > 0).then_some(l.memory_bytes),
            file_size: (l.file_size_bytes > 0).then_some(l.file_size_bytes),
            cpu_secs: (l.cpu_secs > 0).then_some(l.cpu_secs),
            nofile: (l.nofile > 0).then_some(l.nofile),
        }
    }

    /// Builds the [`SpawnSpec`] for one command.
    fn spawn_spec(
        &self,
        command: &CommandSpec,
        argv: &[String],
        env: EnvPolicy,
        tap: Option<mpsc::Sender<Vec<u8>>>,
    ) -> SpawnSpec {
        let mut spec = SpawnSpec::new(PLUGIN_TOOL, &command.program)
            .args(argv.iter().skip(1).cloned())
            .cwd(&command.cwd)
            .env(env)
            .limits(self.rlimits())
            .stdin_piped(command.stdin == StdinMode::Json)
            .stdout_piped(true);
        if let Some(tx) = tap {
            spec = spec.stderr_tap(tx);
        }
        spec
    }
}

/// Scores `url` against a `[match]` section (DESIGN §6.3's score table).
///
/// A free function so the same rules serve a healthy [`CommandProvider`] and the
/// [`DegradedProvider`] stand-in built from a manifest that failed validation.
#[must_use]
pub fn match_url(spec: &MatchSpec, url: &Url) -> Match {
    if !spec.schemes.iter().any(|s| **s == *url.scheme()) {
        return Match::No;
    }
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return Match::No;
    }
    let path = url.path();
    if let Some(veto) = &spec.exclude_path_regex
        && veto.is_match(path)
    {
        return Match::No;
    }

    let suffix_hit = spec
        .hosts
        .iter()
        .any(|h| host == &**h || host.ends_with(&format!(".{h}")));
    let regex_hit = spec.host_regex.as_ref().is_some_and(|r| r.is_match(host));
    if !suffix_hit && !regex_hit {
        return Match::No;
    }
    let path_hit = spec.path_regex.as_ref().is_some_and(|r| r.is_match(path));

    let derived = if path_hit {
        SCORE_PATH_REGEX
    } else if regex_hit {
        SCORE_HOST_REGEX
    } else {
        SCORE_HOST_SUFFIX
    };
    Match::Strong(spec.priority.unwrap_or(derived))
}

#[async_trait]
impl Provider for CommandProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn matches(&self, url: &Url) -> Match {
        self.manifest
            .match_spec
            .as_ref()
            .map_or(Match::No, |spec| match_url(spec, url))
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.manifest.catalog)
    }

    async fn resolve(
        &self,
        url: &Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        let Some(resolve) = &self.manifest.resolve else {
            // `capabilities.resolve = false` ⇒ one synthetic Video entry from the URL
            // (DESIGN §6.5.1).
            return Ok(vec![synthetic_entry(url)]);
        };
        let _permit = self
            .resolves
            .acquire()
            .await
            .map_err(|_| ProviderError::Canceled)?;
        self.politeness_gate().await;

        let dt = ctx.request.selection.download_type;
        let tctx = TemplateCtx {
            url: Some(url.clone()),
            media_id: default_media_id(url),
            title: title_from(url),
            out_dir: ctx.paths.root_for(dt).to_path_buf(),
            tmp_dir: ctx.paths.temp.clone(),
            out_name: out_name(&ctx.request.custom_name_prefix, &title_from(url)),
            output_ext: self
                .manifest
                .download
                .as_ref()
                .map(|d| d.output_ext.to_string())
                .unwrap_or_default(),
            cookies_file: self.env.cookies_file(),
            plugin_dir: self.manifest.dir.clone(),
            ..selection_ctx(ctx.request)
        };
        let headers = self.headers(&tctx)?;
        let tctx = TemplateCtx { headers, ..tctx };
        let argv = render_argv(&resolve.command.argv, &tctx)
            .map_err(|e| ProviderError::Contract(format!("resolve.command: {e}")))?;
        let env = self.env_policy(&tctx, &tctx.headers)?;
        let spec = self.spawn_spec(&resolve.command, &argv, env, None);

        let deadline = Duration::from_secs(self.manifest.limits.resolve_timeout_secs);
        let run = self.run_resolve(&spec, resolve.format, &tctx, url, &ctx);
        match tokio::time::timeout(deadline, run).await {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Timeout(format!(
                "the plugin's resolve command did not finish within {}s",
                self.manifest.limits.resolve_timeout_secs
            ))),
        }
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        let download = self.manifest.download.as_ref().ok_or_else(|| {
            ProviderError::Unsupported("this plugin declares no download command".to_owned())
        })?;
        let _permit = self
            .downloads
            .acquire()
            .await
            .map_err(|_| ProviderError::Canceled)?;
        self.politeness_gate().await;
        sink.stage(Stage::Preparing, None).await;

        let tctx = self.download_ctx(&ctx, &download.output_ext)?;
        let argv = render_argv(&download.command.argv, &tctx)
            .map_err(|e| ProviderError::Contract(format!("download.command: {e}")))?;
        let env = self.env_policy(&tctx, &tctx.headers)?;

        let (tap_tx, tap_rx) = mpsc::channel(STDERR_TAP_CAPACITY);
        let spec = self.spawn_spec(&download.command, &argv, env, Some(tap_tx));
        tracing::debug!(provider = %self.id, argv = ?argv, "spawning plugin download");

        let started = SystemTime::now();
        let run = Run {
            provider: self,
            expect: download.expect_output,
            out_path: PathBuf::from(tctx.out_path()),
            out_dir: ctx.out_dir.clone(),
            started,
            tctx,
        };
        run.execute(spec, tap_rx, &ctx, &sink).await
    }

    fn own_slots(&self) -> Option<usize> {
        let l = &self.manifest.limits;
        (!l.uses_global_slot).then_some(l.max_concurrent as usize)
    }

    async fn probe(&self) -> ProviderHealth {
        for command in [
            self.manifest.resolve.as_ref().map(|r| &r.command),
            self.manifest.download.as_ref().map(|d| &d.command),
        ]
        .into_iter()
        .flatten()
        {
            if !command.program.is_file() {
                return ProviderHealth::Down(
                    format!("{} no longer exists", command.program.display()).into(),
                );
            }
        }
        ProviderHealth::Ok
    }
}

impl CommandProvider {
    /// The template context for a download.
    fn download_ctx(
        &self,
        ctx: &DownloadCtx<'_>,
        output_ext: &str,
    ) -> Result<TemplateCtx, ProviderError> {
        let entry = ctx.entry;
        let base = TemplateCtx {
            url: Some(entry.url.clone()),
            media_id: entry.media_id.to_string(),
            title: entry.title.to_string(),
            out_dir: ctx.out_dir.clone(),
            tmp_dir: ctx.tmp_dir.clone(),
            out_name: out_name(&ctx.request.custom_name_prefix, &entry.title),
            output_ext: output_ext.to_owned(),
            state: entry.state.clone(),
            playlist_index: entry.hints.playlist_index,
            playlist_count: entry.hints.playlist_count,
            playlist_title: entry
                .hints
                .playlist_title
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            cookies_file: self.env.cookies_file(),
            plugin_dir: self.manifest.dir.clone(),
            ..selection_ctx(ctx.request)
        };
        let headers = self.headers(&base)?;
        Ok(TemplateCtx { headers, ..base })
    }

    /// Spawns the resolve command and parses its stdout (DESIGN §6.5.3).
    async fn run_resolve(
        &self,
        spec: &SpawnSpec,
        format: ResolveFormat,
        tctx: &TemplateCtx,
        url: &Url,
        ctx: &ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        let mut child = Child::spawn(spec)?;
        let stderr = child.stderr();
        if let Some(mut stdin) = child.take_stdin() {
            write_stdin_json(&mut stdin, tctx).await;
        }
        let mut frames = FrameReader::new(url.clone());
        let mut budget = self.manifest.limits.max_output_bytes;
        let mut whole = String::new();

        if let Some(stdout) = child.stdout_lines() {
            loop {
                tokio::select! {
                    biased;
                    () = ctx.cancel.cancelled() => {
                        child.kill_group().await;
                        return Err(ProviderError::Canceled);
                    }
                    line = stdout.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                budget = match budget.checked_sub(line.len() as u64 + 1) {
                                    Some(left) => left,
                                    None => {
                                        child.kill_group().await;
                                        return Err(over_budget(self.manifest.limits.max_output_bytes));
                                    }
                                };
                                match format {
                                    ResolveFormat::JsonLines => frames.push_line(&line),
                                    ResolveFormat::Json => {
                                        whole.push_str(&line);
                                        whole.push('\n');
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                child.kill_group().await;
                                return Err(e.into());
                            }
                        }
                    }
                }
            }
        }

        let status = child.wait().await?;
        if format == ResolveFormat::Json {
            frames.push_document(&whole)?;
        }
        if let Some(err) = frames.error.take() {
            return Err(err);
        }
        if !status.success() {
            return Err(exit_failure(&status, &stderr));
        }
        let entries = frames.finish();
        if entries.is_empty() {
            return Err(ProviderError::Unsupported(
                "the plugin's resolve command produced no entries".to_owned(),
            ));
        }
        Ok(entries)
    }
}

/// One download run. A struct only so the values the loop needs are named once.
struct Run<'a> {
    provider: &'a CommandProvider,
    expect: ExpectOutput,
    out_path: PathBuf,
    out_dir: PathBuf,
    started: SystemTime,
    /// The very context `argv` was rendered from, so a `stdin = "json"` payload describes the same
    /// `{out_path}` / `{output_ext}` the command line was given (DESIGN §6.5.1).
    tctx: TemplateCtx,
}

impl Run<'_> {
    async fn execute(
        &self,
        spec: SpawnSpec,
        mut tap: mpsc::Receiver<Vec<u8>>,
        ctx: &DownloadCtx<'_>,
        sink: &ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        let limits = &self.provider.manifest.limits;
        let progress_spec = &self.provider.manifest.progress;
        let argv = spec.argv();
        let mut child = Child::spawn(&spec)?;
        let stderr = child.stderr();
        // Releasing our clone of the stderr tap's sender leaves the drain task holding the only
        // one, which is what makes `tap.recv()` below end at end-of-stream instead of blocking
        // forever — and therefore what makes the stderr tail deterministic rather than a race
        // against a task that may not have been polled yet.
        drop(spec);
        if let Some(mut stdin) = child.take_stdin() {
            write_stdin_json(&mut stdin, &self.tctx).await;
        }
        let mut stdout = child.take_stdout();

        // A fresh parser per run, so the throttle and the carry buffer start clean.
        let mut parser = ProgressParser::new(clone_progress_spec(progress_spec));
        let mut result_frame: Option<ResultFrame> = None;
        let mut lines = LineAccumulator::default();
        let mut budget = limits.max_output_bytes;
        let mut last_activity = Instant::now();
        let stall = Duration::from_secs(limits.download_stall_secs.max(1));
        let hard_deadline = (limits.download_hard_timeout_secs > 0)
            .then(|| Instant::now() + Duration::from_secs(limits.download_hard_timeout_secs));
        let mut announced_downloading = false;
        let mut exit = None;

        while exit.is_none() {
            let stall_at = last_activity + stall;
            // The handlers only *name* what happened; acting on it happens after this statement,
            // where the branch futures — one of which holds `&mut child` — have been dropped.
            let step = tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => Step::Cancelled,
                chunk = read_chunk(&mut stdout) => match chunk {
                    Ok(bytes) => Step::Stdout(bytes),
                    Err(e) => Step::Failed(e),
                },
                Some(bytes) = tap.recv() => Step::Stderr(bytes),
                status = child.wait() => match status {
                    Ok(s) => Step::Exited(s),
                    Err(e) => Step::Failed(e),
                },
                () = sleep_until_opt(hard_deadline) => Step::HardTimeout,
                () = tokio::time::sleep_until(stall_at) => Step::Stalled,
            };

            match step {
                Step::Cancelled => {
                    self.cancel(&mut child).await;
                    return Err(ProviderError::Canceled);
                }
                Step::Failed(e) => {
                    child.kill_group().await;
                    return Err(e.into());
                }
                Step::HardTimeout => {
                    child.kill_group().await;
                    return Err(ProviderError::Timeout(format!(
                        "the plugin exceeded its {}s hard timeout",
                        limits.download_hard_timeout_secs
                    )));
                }
                Step::Stalled => {
                    child.kill_group().await;
                    return Err(ProviderError::Timeout(format!(
                        "the plugin produced no output for {}s",
                        limits.download_stall_secs
                    )));
                }
                Step::Exited(status) => exit = Some(status),
                // stdout reached end of stream: stop polling it and wait for the exit.
                Step::Stdout(None) => stdout = None,
                Step::Stdout(Some(bytes)) => {
                    last_activity = Instant::now();
                    if !spend(&mut budget, bytes.len()) {
                        child.kill_group().await;
                        return Err(over_budget(limits.max_output_bytes));
                    }
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    for line in lines.push(&text) {
                        if let Some(frame) = ResultFrame::parse(&line) {
                            result_frame = Some(frame);
                        }
                    }
                    if progress_spec.source.reads_stdout()
                        && let Some(update) = parser.feed(&text, Instant::now())
                    {
                        emit(sink, &update, &mut announced_downloading).await;
                    }
                }
                Step::Stderr(bytes) => {
                    last_activity = Instant::now();
                    if !spend(&mut budget, bytes.len()) {
                        child.kill_group().await;
                        return Err(over_budget(limits.max_output_bytes));
                    }
                    if progress_spec.source.reads_stderr() {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        if let Some(update) = parser.feed(&text, Instant::now()) {
                            emit(sink, &update, &mut announced_downloading).await;
                        }
                    }
                }
            }
        }

        // Whatever the child wrote just before exiting. `stdout` is `None` once it reached end of
        // stream, and `read_chunk` is deliberately *pending* in that case — it exists to be one
        // arm of the `select!` above — so this loop reads the reader directly.
        while let Some(reader) = stdout.as_mut() {
            match reader.next_chunk().await {
                Ok(Some(bytes)) => {
                    if !spend(&mut budget, bytes.len()) {
                        break;
                    }
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    for line in lines.push(&text) {
                        if let Some(frame) = ResultFrame::parse(&line) {
                            result_frame = Some(frame);
                        }
                    }
                    if progress_spec.source.reads_stdout()
                        && let Some(update) = parser.feed(&text, Instant::now())
                    {
                        emit(sink, &update, &mut announced_downloading).await;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        // The child is gone, so its stderr is at end of stream and the drain task is finishing.
        // `recv` therefore terminates, and when it does the ring holds everything the child ever
        // wrote — which is what `exit_failure` reports to the user.
        while let Some(bytes) = tap.recv().await {
            if progress_spec.source.reads_stderr() {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(update) = parser.feed(&text, Instant::now()) {
                    emit(sink, &update, &mut announced_downloading).await;
                }
            }
        }
        if let Some(line) = lines.finish()
            && let Some(frame) = ResultFrame::parse(&line)
        {
            result_frame = Some(frame);
        }
        if let Some(update) = parser.flush() {
            emit(sink, &update, &mut announced_downloading).await;
        }

        let Some(status) = exit else {
            return Err(ProviderError::Other(
                "the plugin process vanished without an exit status".to_owned(),
            ));
        };
        if !status.success() {
            tracing::warn!(
                provider = %self.provider.id,
                code = status.code().unwrap_or(-1),
                argv = ?argv,
                "plugin download failed"
            );
            return Err(exit_failure(&status, &stderr));
        }
        self.outcome(result_frame, ctx)
    }

    /// Cancels per `capabilities.cancel` (DESIGN §6.5.1).
    ///
    /// `process_group` and `cooperative` both go through `killpg`; `none` skips the `SIGTERM` and
    /// lets the drop guard's `SIGKILL` do the work, because nothing here can promise a child
    /// unlimited time.
    async fn cancel(&self, child: &mut Child) {
        use super::manifest::CancelPolicy;
        match self.provider.manifest.capabilities.cancel {
            CancelPolicy::ProcessGroup | CancelPolicy::Cooperative => {
                child.kill_group().await;
            }
            CancelPolicy::None => {
                tracing::debug!(
                    provider = %self.provider.id,
                    "capabilities.cancel = \"none\": leaving the group to the drop guard"
                );
            }
        }
    }

    /// Judges success and builds the [`Outcome`] (DESIGN §6.5.3's table).
    fn outcome(
        &self,
        frame: Option<ResultFrame>,
        ctx: &DownloadCtx<'_>,
    ) -> Result<Outcome, ProviderError> {
        let produced: PathBuf = match self.expect {
            ExpectOutput::PathTemplate => {
                let size = non_empty_file(&self.out_path).ok_or_else(|| {
                    ProviderError::Contract(format!(
                        "expect_output = \"path_template\" but {} does not exist or is empty",
                        self.out_path.display()
                    ))
                })?;
                return self.build(&self.out_path, size, ctx);
            }
            ExpectOutput::ResultFrame => {
                let frame = frame.ok_or_else(|| {
                    ProviderError::Contract(
                        "expect_output = \"result_frame\" but the plugin printed no {\"t\":\"result\"} line"
                            .to_owned(),
                    )
                })?;
                let path = if Path::new(&frame.path).is_absolute() {
                    PathBuf::from(&frame.path)
                } else {
                    self.out_dir.join(&frame.path)
                };
                let size = non_empty_file(&path).ok_or_else(|| {
                    ProviderError::Contract(format!(
                        "the plugin's result frame names {}, which does not exist or is empty",
                        path.display()
                    ))
                })?;
                return self.build(&path, frame.size.unwrap_or(size), ctx);
            }
            ExpectOutput::NewestInDir => {
                newest_since(&self.out_dir, self.started).ok_or_else(|| {
                    ProviderError::Contract(format!(
                        "expect_output = \"newest_in_dir\" but {} gained no file",
                        self.out_dir.display()
                    ))
                })?
            }
        };
        let size = non_empty_file(&produced).unwrap_or(0);
        self.build(&produced, size, ctx)
    }

    fn build(
        &self,
        produced: &Path,
        size: u64,
        ctx: &DownloadCtx<'_>,
    ) -> Result<Outcome, ProviderError> {
        let base = produced
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| {
                ProviderError::Contract(format!("{} has no file name", produced.display()))
            })?;
        let relative = match &ctx.request.folder {
            Some(folder) => format!("{}/{base}", folder.as_str()),
            None => base,
        };
        let filename = RelPath::parse(&relative).map_err(|e| {
            ProviderError::Contract(format!("the produced file name is not usable: {e}"))
        })?;
        Ok(Outcome::file(filename, size))
    }
}

/// What one iteration of the download loop observed.
///
/// The `select!` handlers produce one of these and nothing else, so that acting on it — which
/// usually means `child.kill_group()` — happens after the branch futures have been dropped.
/// Calling `&mut child` inside a handler while another branch's future still borrows it does not
/// compile, and working around it with a second `select!` would lose the stall deadline.
enum Step {
    /// A stdout chunk, or `None` at end of stream.
    Stdout(Option<Vec<u8>>),
    /// A stderr chunk, from the drain's tap.
    Stderr(Vec<u8>),
    /// The child exited.
    Exited(std::process::ExitStatus),
    /// `ctx.cancel` fired.
    Cancelled,
    /// `limits.download_stall_secs` elapsed with no output.
    Stalled,
    /// `limits.download_hard_timeout_secs` elapsed.
    HardTimeout,
    /// An I/O failure on the child.
    Failed(crate::proc::ProcError),
}

/// Copies a validated [`super::progress::ProgressSpec`] for a per-run parser.
///
/// `ProgressSpec` holds compiled `Regex`es and is deliberately not `Clone`, because a manifest
/// owns exactly one and cloning it by accident would recompile nothing but waste memory. A run
/// genuinely needs its own, so the copy is explicit and named.
fn clone_progress_spec(spec: &super::progress::ProgressSpec) -> super::progress::ProgressSpec {
    super::progress::ProgressSpec {
        kind: spec.kind,
        source: spec.source,
        strip_ansi: spec.strip_ansi,
        cr_as_newline: spec.cr_as_newline,
        last_match_wins: spec.last_match_wins,
        min_interval_ms: spec.min_interval_ms,
        patterns: spec.patterns.clone(),
        units: spec.units.clone(),
        status_map: spec.status_map.clone(),
    }
}

/// Forwards one parsed update to the sink, announcing `downloading` the first time real progress
/// arrives so a plugin that never sets a status still leaves `preparing`.
async fn emit(sink: &ProgressSink, update: &ProgressUpdate, announced: &mut bool) {
    if let Some(target) = update.status {
        match target {
            StatusTarget::Stage(stage) => {
                sink.stage(stage, update.msg.as_deref().map(Into::into))
                    .await;
                *announced = true;
            }
            // A plugin announcing a terminal status is advisory only: the engine writes the
            // terminal status from this call's return value (DESIGN §6.2).
            StatusTarget::Terminal(_) => {}
        }
    } else if !*announced {
        sink.stage(Stage::Downloading, update.msg.as_deref().map(Into::into))
            .await;
        *announced = true;
    }
    if update.raw != aulos_core::progress::RawProgress::default() {
        sink.progress(update.raw);
    }
}

async fn read_chunk(
    stdout: &mut Option<crate::proc::Lines<tokio::process::ChildStdout>>,
) -> Result<Option<Vec<u8>>, crate::proc::ProcError> {
    match stdout {
        Some(reader) => reader.next_chunk().await,
        // No stdout left: never ready, so the `select!` waits on the other branches.
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn spend(budget: &mut u64, n: usize) -> bool {
    match budget.checked_sub(n as u64) {
        Some(left) => {
            *budget = left;
            true
        }
        None => false,
    }
}

fn over_budget(limit: u64) -> ProviderError {
    ProviderError::Contract(format!(
        "the plugin wrote more than its {limit}-byte output budget"
    ))
}

fn exit_failure(status: &std::process::ExitStatus, stderr: &StderrRing) -> ProviderError {
    let tail = strip_ansi(&stderr.tail(STDERR_TAIL_BYTES));
    let code = status
        .code()
        .map_or_else(|| "a signal".to_owned(), |c| format!("exit code {c}"));
    if tail.trim().is_empty() {
        ProviderError::Other(format!("the plugin failed with {code} and printed nothing"))
    } else {
        ProviderError::Other(format!("the plugin failed with {code}: {tail}"))
    }
}

fn non_empty_file(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    (meta.is_file() && meta.len() > 0).then_some(meta.len())
}

/// The newest regular file in `dir` modified at or after `since`.
fn newest_since(dir: &Path, since: SystemTime) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        // A one-second slack: many filesystems store whole-second mtimes, so a file written in the
        // same second the job started would otherwise look older than the job.
        if modified + Duration::from_secs(1) < since {
            continue;
        }
        if best.as_ref().is_none_or(|(t, _)| modified >= *t) {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

async fn write_stdin_json(stdin: &mut tokio::process::ChildStdin, ctx: &TemplateCtx) {
    use tokio::io::AsyncWriteExt as _;
    let payload = serde_json::json!({
        "url": ctx.url.as_ref().map(Url::to_string),
        "media_id": ctx.media_id,
        "title": ctx.title,
        "out_dir": ctx.out_dir,
        "tmp_dir": ctx.tmp_dir,
        "out_name": ctx.out_name,
        "out_path": ctx.out_path(),
        "output_ext": ctx.output_ext,
        "download_type": ctx.download_type.map(DownloadType::as_str),
        "format": ctx.format,
        "quality": ctx.quality,
        "codec": ctx.codec,
        "subtitle_language": ctx.subtitle_language,
        "subtitle_mode": ctx.subtitle_mode,
        "state": ctx.state,
        "playlist_index": ctx.playlist_index,
        "playlist_count": ctx.playlist_count,
        "playlist_title": ctx.playlist_title,
        "cookies_file": ctx.cookies_file,
        "headers": ctx.headers.iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect::<serde_json::Map<String, serde_json::Value>>(),
        "plugin_dir": ctx.plugin_dir,
    });
    let mut line = payload.to_string();
    line.push('\n');
    if let Err(e) = stdin.write_all(line.as_bytes()).await {
        tracing::debug!(error = %e, "could not write the plugin's stdin payload");
    }
    // Dropping stdin closes it, which is how the plugin knows the payload is complete.
}

/// The `{"t":"result", …}` line of DESIGN §6.5.3.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ResultFrame {
    path: String,
    size: Option<u64>,
}

impl ResultFrame {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if !line.starts_with('{') {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value.get("t").and_then(serde_json::Value::as_str) != Some("result") {
            return None;
        }
        let path = value.get("path")?.as_str()?.to_owned();
        Some(Self {
            path,
            size: value.get("size").and_then(serde_json::Value::as_u64),
        })
    }
}

/// Splits arbitrary chunks into `\n`-terminated lines, for the `result_frame` scan.
#[derive(Debug, Default)]
struct LineAccumulator {
    carry: String,
}

impl LineAccumulator {
    fn push(&mut self, chunk: &str) -> Vec<String> {
        self.carry.push_str(chunk);
        let mut out = Vec::new();
        while let Some(at) = self.carry.find('\n') {
            out.push(self.carry[..at].trim_end_matches('\r').to_owned());
            self.carry = self.carry[at + 1..].to_owned();
        }
        if self.carry.len() > 1024 * 1024 {
            self.carry.clear();
        }
        out
    }

    fn finish(&mut self) -> Option<String> {
        (!self.carry.trim().is_empty()).then(|| std::mem::take(&mut self.carry))
    }
}

/// Accumulates `resolve` stdout frames into [`MediaEntry`]s (DESIGN §6.5.3).
struct FrameReader {
    url: Url,
    group: Option<(String, String, Option<u32>)>,
    entries: Vec<MediaEntry>,
    error: Option<ProviderError>,
}

impl FrameReader {
    fn new(url: Url) -> Self {
        Self {
            url,
            group: None,
            entries: Vec::new(),
            error: None,
        }
    }

    fn push_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if !line.starts_with('{') {
            tracing::debug!(line, "ignoring non-JSON output from a plugin's resolve");
            return;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => self.push_value(&value),
            Err(e) => tracing::warn!(error = %e, "a plugin's resolve printed an unparseable line"),
        }
    }

    fn push_document(&mut self, text: &str) -> Result<(), ProviderError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
            ProviderError::Contract(format!(
                "resolve.format = \"json\" but stdout is not one JSON document: {e}"
            ))
        })?;
        match value {
            serde_json::Value::Array(items) => {
                for item in &items {
                    self.push_value(item);
                }
            }
            other => self.push_value(&other),
        }
        Ok(())
    }

    fn push_value(&mut self, value: &serde_json::Value) {
        let t = value
            .get("t")
            .and_then(serde_json::Value::as_str)
            // A bare object without `t` is an entry (DESIGN §6.5.3, author ergonomics).
            .unwrap_or("entry");
        match t {
            "entry" => {
                if let Some(entry) = self.entry_from(value) {
                    self.entries.push(entry);
                }
            }
            "group" => {
                let media_id = value
                    .get("media_id")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(|| default_media_id(&self.url), ToOwned::to_owned);
                let title = value
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(|| title_from(&self.url), ToOwned::to_owned);
                let expected = value
                    .get("expected")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok());
                self.group = Some((media_id, title, expected));
            }
            "note" => {
                if let Some(msg) = value.get("message").and_then(serde_json::Value::as_str) {
                    tracing::info!(note = msg, "plugin note");
                }
            }
            "error" => {
                if self.error.is_none() {
                    let code = value
                        .get("code")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|c| ErrorCode::ALL.into_iter().find(|k| k.as_str() == c))
                        .unwrap_or(ErrorCode::Internal);
                    let message = value
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the plugin reported an error without a message");
                    self.error = Some(ProviderError::from_code(code, message));
                }
            }
            other => tracing::warn!(t = other, "ignoring an unknown frame from a plugin"),
        }
    }

    fn entry_from(&self, value: &serde_json::Value) -> Option<MediaEntry> {
        let url = match value.get("url").and_then(serde_json::Value::as_str) {
            Some(raw) => match Url::parse(raw) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(url = raw, error = %e, "a plugin entry carried an unparseable url");
                    return None;
                }
            },
            None => self.url.clone(),
        };
        let media_id = value
            .get("media_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| default_media_id(&url), ToOwned::to_owned);
        let title = value
            .get("title")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .map_or_else(|| title_from(&url), ToOwned::to_owned);
        let mut entry = MediaEntry::video(media_id, title, url);
        entry.state = value
            .get("state")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        entry.hints = EntryHints {
            duration: value.get("duration").and_then(serde_json::Value::as_f64),
            ext: value
                .get("ext")
                .and_then(serde_json::Value::as_str)
                .map(Into::into),
            thumbnail: value
                .get("thumbnail")
                .and_then(serde_json::Value::as_str)
                .map(Into::into),
            uploader: value
                .get("uploader")
                .and_then(serde_json::Value::as_str)
                .map(Into::into),
            ..EntryHints::default()
        };
        Some(entry)
    }

    /// The entries, wrapped in a playlist when a `group` frame declared one.
    fn finish(mut self) -> Vec<MediaEntry> {
        let Some((media_id, title, expected)) = self.group.take() else {
            return self.entries;
        };
        let count = expected.or_else(|| u32::try_from(self.entries.len()).ok());
        for (i, child) in self.entries.iter_mut().enumerate() {
            child.hints.playlist_index = u32::try_from(i + 1).ok();
            child.hints.playlist_count = count;
            child.hints.playlist_title = Some(title.as_str().into());
        }
        let mut parent = MediaEntry::video(media_id, title.clone(), self.url.clone());
        parent.kind = EntryKind::Playlist {
            title: title.into(),
            entries: std::mem::take(&mut self.entries),
        };
        parent.hints.playlist_count = count;
        vec![parent]
    }
}

/// The synthetic entry a `capabilities.resolve = false` plugin gets (DESIGN §6.5.1).
fn synthetic_entry(url: &Url) -> MediaEntry {
    MediaEntry::video(default_media_id(url), title_from(url), url.clone())
}

/// `sha256(url)[..16]` (DESIGN §6.5.3).
fn default_media_id(url: &Url) -> String {
    sha256_hex_prefix(url.as_str(), 16)
}

/// A display title from a URL: the last non-empty path segment, else the host, else the URL.
fn title_from(url: &Url) -> String {
    url.path_segments()
        .and_then(|s| s.rev().find(|seg| !seg.is_empty()))
        .map(ToOwned::to_owned)
        .or_else(|| url.host_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| url.to_string())
}

/// `{out_name}`: the prefix plus the title, sanitised, without an extension (DESIGN §6.5.1).
fn out_name(prefix: &str, title: &str) -> String {
    let joined = format!("{prefix}{title}");
    // `sanitize_path_component` deliberately leaves `/` alone, because a yt-dlp output *template*
    // may legitimately produce a nested path (DESIGN §9.8). `{out_name}` is a **basename**, so a
    // separator here would silently move the file — hence the extra pass.
    let mut name: String = sanitize_path_component(&joined)
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    if name.chars().count() > OUT_NAME_MAX {
        name = name.chars().take(OUT_NAME_MAX).collect();
    }
    let trimmed = name
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .trim()
        .to_owned();
    if trimmed.is_empty() {
        "download".to_owned()
    } else {
        trimmed
    }
}

/// The five selection fields, shared by the resolve and download contexts.
fn selection_ctx(request: &aulos_core::request::DownloadRequest) -> TemplateCtx {
    TemplateCtx {
        download_type: Some(request.selection.download_type),
        format: request.selection.format.as_str().to_owned(),
        quality: request.selection.quality.as_str().to_owned(),
        codec: request.selection.codec.as_str().to_owned(),
        subtitle_language: request.subtitle_language.as_str().to_owned(),
        subtitle_mode: request.subtitle_mode.as_str().to_owned(),
        count: 1,
        ..TemplateCtx::default()
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// What one plugin-directory scan found.
#[derive(Default)]
pub struct Scan {
    /// The plugins that produced a provider, degraded or not.
    pub plugins: Vec<LoadedPlugin>,
    /// Every `[[hook]]` table across every manifest, in directory order.
    pub hooks: Vec<HookSpec>,
    /// The directories that produced nothing at all — not even a matcher.
    pub failed: Vec<ReloadFailure>,
    /// Non-fatal load problems, as `<dir>: <key>: <message>`. Surfaced in `healthz`.
    pub warnings: Vec<Box<str>>,
}

impl std::fmt::Debug for Scan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan")
            .field("plugins", &self.plugins.len())
            .field("hooks", &self.hooks.len())
            .field("failed", &self.failed)
            .field("warnings", &self.warnings)
            .finish()
    }
}

/// Scans `dir` for plugin directories (DESIGN §6.5).
///
/// Never fails as a whole: an unreadable directory is an empty scan, a manifest that parses far
/// enough to claim URLs is a degraded plugin, and anything worse is a [`ReloadFailure`].
#[must_use]
pub fn scan(dir: &Path) -> Scan {
    scan_with(dir, &PluginEnv::default())
}

/// [`scan`], with the filesystem context `{cookies_file}` needs.
#[must_use]
pub fn scan_with(dir: &Path, env: &PluginEnv) -> Scan {
    let mut out = Scan::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(dir = %dir.display(), error = %e, "no plugin directory to scan");
            return out;
        }
    };
    // Sorted, so registration order — the documented tie-break — does not depend on the order the
    // filesystem happened to return directories in.
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .filter(|p| p.join(super::manifest::MANIFEST_FILE).is_file())
        .collect();
    dirs.sort();

    for plugin_dir in dirs {
        let name: Box<str> = plugin_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        match load_manifest(&plugin_dir) {
            Ok(mut manifest) => {
                for w in &manifest.warnings {
                    out.warnings
                        .push(format!("{name}: {}: {}", w.key, w.message).into());
                }
                // Hooks are handed to `aulos-hooks`, which executes them; the provider does not
                // need them, so they are moved out rather than shared.
                out.hooks.extend(std::mem::take(&mut manifest.hooks));
                let fingerprint = manifest.fingerprint;
                let declares_provider = manifest.declares_provider();
                if declares_provider {
                    let provider = CommandProvider::new(Arc::new(manifest), env.clone());
                    out.plugins.push(LoadedPlugin {
                        provider: Arc::new(provider),
                        degraded: None,
                        fingerprint: Some(fingerprint),
                    });
                }
            }
            Err(e) => {
                let reason = e.reason();
                tracing::warn!(plugin = %name, reason = %reason, "plugin manifest rejected");
                if !e.has_partial_match() || !super::manifest::is_plugin_name(&name) {
                    out.failed.push(ReloadFailure { name, reason });
                    continue;
                }
                let Ok(id) = ProviderId::parse(&format!("command:{name}")) else {
                    out.failed.push(ReloadFailure { name, reason });
                    continue;
                };
                let catalog = Arc::new(empty_catalog(&id));
                let provider: Arc<dyn Provider> = match partial_match(&plugin_dir) {
                    Some(spec) => {
                        let spec = Arc::new(spec);
                        Arc::new(DegradedProvider::new(
                            id,
                            reason.clone(),
                            catalog,
                            Box::new(move |url| match_url(&spec, url)),
                        ))
                    }
                    None => Arc::new(DegradedProvider::unmatched(id, reason.clone(), catalog)),
                };
                out.plugins.push(LoadedPlugin {
                    provider,
                    degraded: Some(reason.clone()),
                    fingerprint: None,
                });
                out.failed.push(ReloadFailure { name, reason });
            }
        }
    }
    out
}

fn empty_catalog(id: &ProviderId) -> FormatCatalog {
    FormatCatalog {
        provider: id.clone(),
        version: 1,
        naming: aulos_core::catalog::NamingPolicy::Template,
        download_types: Vec::new(),
    }
}

/// Discovers every `command` provider and community hook in `dir` (PLAN WP-10).
///
/// The returned providers include the `Degraded` stand-ins, and every degraded or unloadable
/// directory also appears in `ReloadReport.failed` so `healthz` can show it. The
/// [`Registry`](crate::registry::Registry) path — [`CommandPluginLoader`] installed with
/// `Registry::set_command_loader` — is what carries the `Degraded` *state*; this function is the
/// convenience form for a caller that wants the hooks as well.
#[must_use]
pub fn discover(dir: &Path) -> (Vec<Arc<dyn Provider>>, Vec<HookSpec>, ReloadReport) {
    let scan = scan_with(dir, &PluginEnv::default());
    let providers: Vec<Arc<dyn Provider>> = scan
        .plugins
        .iter()
        .map(|p| Arc::clone(&p.provider))
        .collect();
    let report = ReloadReport {
        added: scan.plugins.iter().map(|p| p.provider.id()).collect(),
        updated: Vec::new(),
        removed: Vec::new(),
        failed: scan.failed,
        warnings: scan.warnings,
    };
    (providers, scan.hooks, report)
}

/// The [`CommandLoader`] to install with `Registry::set_command_loader` (DESIGN §6.5).
///
/// It also remembers the `[[hook]]` tables of the most recent scan, so a `SIGHUP` or a
/// `POST api/v2/plugins/reload` gives `aulos-hooks` the new community hooks without a second walk
/// of the directory.
#[derive(Debug, Default)]
pub struct CommandPluginLoader {
    env: PluginEnv,
    hooks: std::sync::Mutex<Vec<Arc<HookSpec>>>,
    warnings: std::sync::Mutex<Vec<Box<str>>>,
}

impl CommandPluginLoader {
    /// A loader with no filesystem context, so `{cookies_file}` renders empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A loader that can resolve `{cookies_file}`.
    #[must_use]
    pub fn with_env(env: PluginEnv) -> Self {
        Self {
            env,
            hooks: std::sync::Mutex::new(Vec::new()),
            warnings: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The community hooks the most recent [`CommandLoader::load`] found.
    #[must_use]
    pub fn hooks(&self) -> Vec<Arc<HookSpec>> {
        self.hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The clamps and `${VAR}` substitutions the most recent scan warned about.
    #[must_use]
    pub fn warnings(&self) -> Vec<Box<str>> {
        self.warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl CommandLoader for CommandPluginLoader {
    fn load(&self, dir: &Path) -> CommandLoadResult {
        let scan = scan_with(dir, &self.env);
        *self
            .hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            scan.hooks.into_iter().map(Arc::new).collect();
        *self
            .warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = scan.warnings.clone();
        CommandLoadResult {
            plugins: scan.plugins,
            failed: scan.failed,
            warnings: scan.warnings,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn out_names_are_sanitised_and_bounded() {
        assert_eq!(out_name("", "Album — Deluxe"), "Album — Deluxe");
        assert_eq!(out_name("01 - ", "Track"), "01 - Track");
        // A hostile title cannot escape the output directory: `{out_name}` is always exactly one
        // path component, and never a traversal one.
        let hostile = out_name("", "../../etc/passwd");
        assert!(!hostile.contains('/'), "{hostile}");
        assert!(!hostile.starts_with('.'), "{hostile}");
        assert_eq!(out_name("", "a/b"), "a_b");
        assert_eq!(out_name("", "a\\b"), "a_b");
        // A 4000-character title is truncated rather than handed to `open(2)`.
        assert!(out_name("", &"x".repeat(4000)).chars().count() <= OUT_NAME_MAX);
        // An entirely unusable title still produces a name.
        assert_eq!(out_name("", "..."), "download");
        assert_eq!(out_name("", ""), "download");
    }

    #[test]
    fn a_title_comes_from_the_last_path_segment() {
        let t = |s: &str| title_from(&Url::parse(s).unwrap());
        assert_eq!(t("https://bandcamp.com/album/914"), "914");
        assert_eq!(t("https://bandcamp.com/album/"), "album");
        assert_eq!(t("https://bandcamp.com/a/b/c"), "c");
        assert_eq!(t("https://bandcamp.com/"), "bandcamp.com");
    }

    #[test]
    fn a_result_frame_is_recognised_and_nothing_else_is() {
        let f = ResultFrame::parse(r#"{"t":"result","path":"a.flac","size":4096}"#).unwrap();
        assert_eq!(f.path, "a.flac");
        assert_eq!(f.size, Some(4096));
        assert!(
            ResultFrame::parse(r#"{"t":"result","path":"a.flac"}"#)
                .unwrap()
                .size
                .is_none()
        );
        assert!(ResultFrame::parse(r#"{"t":"entry","path":"a"}"#).is_none());
        assert!(ResultFrame::parse(r#"{"t":"result"}"#).is_none(), "no path");
        assert!(ResultFrame::parse("not json").is_none());
        assert!(ResultFrame::parse("").is_none());
    }

    #[test]
    fn the_line_accumulator_carries_partial_lines() {
        let mut acc = LineAccumulator::default();
        assert!(acc.push("{\"t\":\"res").is_empty());
        assert_eq!(acc.push("ult\"}\nnext\r\n"), ["{\"t\":\"result\"}", "next"]);
        assert!(acc.finish().is_none());
        assert!(acc.push("trailing").is_empty());
        assert_eq!(acc.finish().as_deref(), Some("trailing"));
    }

    #[test]
    fn frames_become_entries_and_a_group() {
        let url = Url::parse("https://bandcamp.com/album/914").unwrap();
        let mut r = FrameReader::new(url.clone());
        r.push_line(r#"{"t":"group","media_id":"bc:album:914","title":"Album — Deluxe","kind":"playlist","expected":3}"#);
        r.push_line(r#"{"t":"entry","media_id":"bc:track:1","url":"https://bandcamp.com/track/1","title":"One","duration":183.0,"state":{"stream_id":"a91f"}}"#);
        // A bare object with no `t` is an entry (author ergonomics).
        r.push_line(r#"{"url":"https://bandcamp.com/track/2","title":"Two"}"#);
        r.push_line(r#"{"t":"note","message":"1 track is region-locked and was skipped"}"#);
        // An unknown `t` is one WARN and a skip, so the format can grow.
        r.push_line(r#"{"t":"from_the_future","x":1}"#);
        r.push_line("not json at all");
        assert!(r.error.is_none());

        let entries = r.finish();
        assert_eq!(entries.len(), 1, "a group frame wraps the children");
        let parent = &entries[0];
        assert_eq!(&*parent.media_id, "bc:album:914");
        assert_eq!(parent.hints.playlist_count, Some(3));
        let children = parent.children();
        assert_eq!(children.len(), 2);
        assert_eq!(&*children[0].title, "One");
        assert_eq!(children[0].hints.playlist_index, Some(1));
        assert_eq!(children[0].hints.playlist_count, Some(3));
        assert_eq!(children[0].hints.duration, Some(183.0));
        assert_eq!(children[0].state["stream_id"], "a91f");
        assert_eq!(children[1].hints.playlist_index, Some(2));
        assert_eq!(
            children[1].hints.playlist_title.as_deref(),
            Some("Album — Deluxe")
        );
        // A `media_id`-less entry defaults to sha256(url)[..16].
        assert_eq!(
            &*children[1].media_id,
            &sha256_hex_prefix("https://bandcamp.com/track/2", 16)
        );
    }

    #[test]
    fn an_error_frame_becomes_a_typed_error() {
        let url = Url::parse("https://bandcamp.com/album/914").unwrap();
        let mut r = FrameReader::new(url);
        r.push_line(r#"{"t":"error","code":"unavailable","message":"album 914 not found","retryable":false}"#);
        let e = r.error.take().unwrap();
        assert_eq!(e.code(), ErrorCode::Unavailable);
        assert_eq!(e.message(), "album 914 not found");
        assert!(!e.retryable());
        // An unknown code is `internal` rather than a parse failure.
        let mut r = FrameReader::new(Url::parse("https://h/x").unwrap());
        r.push_line(r#"{"t":"error","code":"who_knows","message":"m"}"#);
        assert_eq!(r.error.take().unwrap().code(), ErrorCode::Internal);
    }

    #[test]
    fn json_documents_are_accepted_as_a_whole_or_as_an_array() {
        let url = Url::parse("https://h/x").unwrap();
        let mut r = FrameReader::new(url.clone());
        r.push_document(r#"[{"title":"a","url":"https://h/a"},{"title":"b","url":"https://h/b"}]"#)
            .unwrap();
        assert_eq!(r.finish().len(), 2);
        let mut r = FrameReader::new(url.clone());
        r.push_document(r#"{"title":"only","url":"https://h/only"}"#)
            .unwrap();
        assert_eq!(r.finish().len(), 1);
        let mut r = FrameReader::new(url);
        let e = r.push_document("{ not json").unwrap_err();
        assert_eq!(e.code(), ErrorCode::Contract);
    }

    #[test]
    fn the_synthetic_entry_needs_no_resolve_command() {
        let url = Url::parse("https://h/watch/42").unwrap();
        let e = synthetic_entry(&url);
        assert_eq!(&*e.title, "42");
        assert_eq!(&*e.media_id, &sha256_hex_prefix(url.as_str(), 16));
        assert_eq!(e.url, url);
        assert!(!e.is_playlist());
    }

    #[test]
    fn the_output_budget_is_spent_and_then_refused() {
        let mut budget = 10u64;
        assert!(spend(&mut budget, 4));
        assert_eq!(budget, 6);
        assert!(spend(&mut budget, 6));
        assert_eq!(budget, 0);
        assert!(!spend(&mut budget, 1));
        assert_eq!(over_budget(64).code(), ErrorCode::Contract);
    }

    #[test]
    fn a_cookie_jar_is_reported_only_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let env = PluginEnv {
            state_dir: dir.path().to_path_buf(),
        };
        assert_eq!(env.cookies_file(), "");
        std::fs::write(dir.path().join("cookies.txt"), "# Netscape\n").unwrap();
        assert_eq!(
            env.cookies_file(),
            dir.path().join("cookies.txt").to_string_lossy()
        );
        assert_eq!(PluginEnv::default().cookies_file(), "");
    }
}
