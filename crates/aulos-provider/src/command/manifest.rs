//! The complete `plugin.toml` schema of DESIGN §6.5.1, and the load-time validation of §6.5.2.
//!
//! Two layers, deliberately:
//!
//! - a private `raw` serde model of plain strings and integers, which is what `toml` deserialises
//!   into and which is where a *syntax* error gets its line and column; and
//! - [`PluginManifest`], the **validated** model, which holds compiled [`regex::Regex`]es, parsed
//!   [`Template`]s, clamped [`Limits`] and a ready [`aulos_core::catalog::FormatCatalog`].
//!
//! Nothing downstream of [`load_manifest`] can therefore hold an uncompilable regex or an unknown
//! template token: the type says it was checked. Every check in the §6.5.2 table maps onto a
//! [`ManifestError`] whose `Display` is exactly the string the provider is registered
//! `Degraded(reason)` with, or onto a clamp plus one WARN.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aulos_core::catalog::{
    Choice, CodecSpec, DownloadTypeSpec, FormatCatalog, FormatFlags, FormatSpec, NamingPolicy,
    OptionKind, OptionSpec, QualitySpec,
};
use aulos_core::selection::ProviderId;
use regex::Regex;
use serde::Deserialize;

use super::hookspec::{self, HookSpec};
use super::progress::ProgressSpec;
use super::template::{Template, TemplateError, Token, TokenScope};

/// The manifest file every plugin directory must contain.
pub const MANIFEST_FILE: &str = "plugin.toml";
/// The only supported `manifest_version` (DESIGN §6.5.1).
pub const MANIFEST_VERSION: u32 = 1;
/// The plugin directory-name shape (DESIGN §6.5).
pub const PLUGIN_NAME_PATTERN: &str = "^[a-z0-9][a-z0-9_-]{0,31}$";
/// The hard cap on `limits.max_concurrent` and `limits.max_concurrent_resolves` (DESIGN §6.5.2).
pub const MAX_CONCURRENT_CAP: u32 = 32;
/// The hard cap on every `limits.*_secs` timeout: 24 hours (DESIGN §6.5.2).
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
/// The default cumulative stdout+stderr budget (DESIGN §6.5.1).
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// The floor the output budget is clamped up to, so a typo cannot make every plugin unusable.
pub const MIN_MAX_OUTPUT_BYTES: u64 = 4096;
/// How much of a failing plugin's stderr reaches `error.message` (DESIGN §6.5.3).
pub const STDERR_TAIL_BYTES: usize = 2048;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a `plugin.toml` was rejected (DESIGN §6.5.2).
///
/// `Display` is the `Degraded(reason)` string, so an operator reading `healthz` sees the key and
/// the reason without a second lookup.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The directory has no `plugin.toml`, or it could not be read.
    #[error("{file}: {source}")]
    Read {
        /// The path that was tried.
        file: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or a value has the wrong type. `message` carries `toml`'s own
    /// line/column span.
    #[error("{file}: {message}")]
    Syntax {
        /// The manifest path.
        file: PathBuf,
        /// `toml`'s rendered error, spans included.
        message: String,
    },
    /// A semantic check failed. `key` is the dotted manifest key it is about.
    #[error("{key}: {message}")]
    Invalid {
        /// The dotted key, e.g. `download.command[0]`.
        key: String,
        /// What is wrong with it.
        message: String,
    },
}

impl ManifestError {
    /// A semantic rejection about `key`.
    #[must_use]
    pub fn invalid(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            key: key.into(),
            message: message.into(),
        }
    }

    /// The `Degraded(reason)` text: this error's `Display`, capped so a pathological `toml` span
    /// cannot dominate a `healthz` payload.
    #[must_use]
    pub fn reason(&self) -> Box<str> {
        let mut s = self.to_string();
        if s.chars().count() > 400 {
            s = s.chars().take(397).collect::<String>() + "...";
        }
        s.replace('\n', " ").into()
    }

    /// Whether the manifest parsed far enough that a matcher could still be built from it.
    ///
    /// A [`ManifestError::Read`] or [`ManifestError::Syntax`] answers `false` — there is nothing
    /// to build a matcher from, so the directory becomes a `ReloadFailure` rather than a
    /// `Degraded` provider.
    #[must_use]
    pub const fn has_partial_match(&self) -> bool {
        matches!(self, Self::Invalid { .. })
    }
}

impl From<TemplateError> for ManifestError {
    fn from(e: TemplateError) -> Self {
        Self::invalid("template", e.to_string())
    }
}

/// A non-fatal problem: the manifest loaded, but a value was clamped or auto-corrected
/// (DESIGN §6.5.2's "clamp + WARN" row).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Warning {
    /// The dotted key.
    pub key: Box<str>,
    /// What was done about it.
    pub message: Box<str>,
}

impl Warning {
    pub(super) fn new(key: impl Into<Box<str>>, message: impl Into<Box<str>>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// The raw serde model
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawManifest {
    manifest_version: Option<u32>,
    name: Option<String>,
    version: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(rename = "match")]
    match_: Option<RawMatch>,
    #[serde(default)]
    capabilities: RawCapabilities,
    #[serde(default)]
    limits: RawLimits,
    resolve: Option<RawCommand>,
    download: Option<RawDownload>,
    #[serde(default)]
    progress: RawProgress,
    #[serde(default)]
    env: RawEnv,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    catalog: Option<RawCatalog>,
    #[serde(default, rename = "hook")]
    hooks: Vec<hookspec::RawHook>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMatch {
    #[serde(default)]
    hosts: Vec<String>,
    host_regex: Option<String>,
    path_regex: Option<String>,
    exclude_path_regex: Option<String>,
    schemes: Option<Vec<String>>,
    priority: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCapabilities {
    #[serde(default)]
    resolve: bool,
    #[serde(default)]
    playlists: bool,
    streaming_resolve: Option<bool>,
    #[serde(default)]
    subtitles: bool,
    #[serde(default)]
    chapters: bool,
    #[serde(default)]
    thumbnails: bool,
    #[serde(default)]
    nfo_capable: bool,
    cancel: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawLimits {
    max_concurrent: Option<u32>,
    uses_global_slot: Option<bool>,
    max_concurrent_resolves: Option<u32>,
    min_request_interval_ms: Option<u64>,
    resolve_timeout_secs: Option<u64>,
    download_stall_secs: Option<u64>,
    download_hard_timeout_secs: Option<u64>,
    max_output_bytes: Option<u64>,
    memory_bytes: Option<u64>,
    cpu_secs: Option<u64>,
    nofile: Option<u64>,
    file_size_bytes: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCommand {
    #[serde(default)]
    command: Vec<String>,
    format: Option<String>,
    stdin: Option<String>,
    cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDownload {
    #[serde(default)]
    command: Vec<String>,
    cwd: Option<String>,
    stdin: Option<String>,
    expect_output: Option<String>,
    output_ext: Option<String>,
    overwrite: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProgress {
    kind: Option<String>,
    source: Option<String>,
    strip_ansi: Option<bool>,
    cr_as_newline: Option<bool>,
    last_match_wins: Option<bool>,
    min_interval_ms: Option<u64>,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    units: BTreeMap<String, String>,
    #[serde(default)]
    status_map: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawEnv {
    #[serde(default)]
    pass: Vec<String>,
    #[serde(default)]
    set: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    naming: Option<String>,
    #[serde(default)]
    download_types: Vec<RawDownloadType>,
}

#[derive(Debug, Deserialize)]
struct RawDownloadType {
    id: String,
    label: Option<String>,
    default_format: Option<String>,
    #[serde(default)]
    formats: Vec<RawFormat>,
    #[serde(default)]
    options: Vec<RawOption>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    id: String,
    label: Option<String>,
    default_quality: Option<String>,
    #[serde(default)]
    qualities: Vec<RawQuality>,
    #[serde(default)]
    codecs: Vec<RawQuality>,
    notice: Option<String>,
    #[serde(default)]
    advisory: bool,
    #[serde(default)]
    requires_ffmpeg: bool,
    #[serde(default)]
    lossy_remux: bool,
    #[serde(default)]
    slow: bool,
}

#[derive(Debug, Deserialize)]
struct RawQuality {
    id: String,
    label: Option<String>,
    notice: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawOption {
    id: String,
    label: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    default: serde_json::Value,
    #[serde(default)]
    choices: Vec<RawQuality>,
    help: Option<String>,
    min: Option<i64>,
    max: Option<i64>,
    pattern: Option<String>,
}

// ---------------------------------------------------------------------------
// The validated model
// ---------------------------------------------------------------------------

/// `[match]` — which URLs this plugin claims (DESIGN §6.5.1).
#[derive(Debug)]
pub struct MatchSpec {
    /// Host suffixes, lower-cased. Score [`crate::provider::SCORE_HOST_SUFFIX`].
    pub hosts: Vec<Box<str>>,
    /// An anchored regex over the normalised host. Score [`crate::provider::SCORE_HOST_REGEX`].
    pub host_regex: Option<Regex>,
    /// When set **and** matching, promotes the score to
    /// [`crate::provider::SCORE_PATH_REGEX`].
    pub path_regex: Option<Regex>,
    /// A veto: a match here is `Match::No`.
    pub exclude_path_regex: Option<Regex>,
    /// Accepted URL schemes, lower-cased.
    pub schemes: Vec<Box<str>>,
    /// Overrides the derived score.
    pub priority: Option<u8>,
}

/// `[capabilities]` (DESIGN §6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capabilities {
    /// `false` ⇒ a synthetic single `Video` entry from the URL.
    pub resolve: bool,
    /// The plugin can produce playlists.
    pub playlists: bool,
    /// Resolve stdout is read line-by-line and children stream out. Defaults to `resolve`.
    pub streaming_resolve: bool,
    /// Catalogue hint.
    pub subtitles: bool,
    /// Catalogue hint.
    pub chapters: bool,
    /// Catalogue hint.
    pub thumbnails: bool,
    /// Catalogue hint.
    pub nfo_capable: bool,
    /// How a cancel reaches the child.
    pub cancel: CancelPolicy,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            resolve: false,
            playlists: false,
            streaming_resolve: false,
            subtitles: false,
            chapters: false,
            thumbnails: false,
            nfo_capable: false,
            cancel: CancelPolicy::ProcessGroup,
        }
    }
}

/// `capabilities.cancel` (DESIGN §6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CancelPolicy {
    /// `SIGTERM` then `SIGKILL` to the whole process group. The default and the only one that
    /// cannot leak a grandchild.
    #[default]
    ProcessGroup,
    /// `SIGTERM` to the group and then wait for the child to exit on its own.
    Cooperative,
    /// The plugin claims it cannot be cancelled. The group is still killed at the grace deadline —
    /// nothing in this server can promise a child unlimited time — but no signal is sent first.
    None,
}

impl CancelPolicy {
    /// The manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessGroup => "process_group",
            Self::Cooperative => "cooperative",
            Self::None => "none",
        }
    }
}

/// `[limits]`, already clamped to the DESIGN §6.5.2 hard caps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Per-provider download semaphore, `1..=32`.
    pub max_concurrent: u32,
    /// `false` ⇒ `own_slots()` only, like StreamingCommunity.
    pub uses_global_slot: bool,
    /// Per-provider resolve semaphore, `1..=32`.
    pub max_concurrent_resolves: u32,
    /// Politeness gate between spawns.
    pub min_request_interval_ms: u64,
    /// Resolve deadline, seconds.
    pub resolve_timeout_secs: u64,
    /// No progress for this many seconds ⇒ kill.
    pub download_stall_secs: u64,
    /// Absolute download deadline, seconds. `0` = off.
    pub download_hard_timeout_secs: u64,
    /// Cumulative stdout+stderr budget. Over ⇒ kill + `contract`.
    pub max_output_bytes: u64,
    /// `RLIMIT_AS`, bytes. `0` = off.
    pub memory_bytes: u64,
    /// `RLIMIT_CPU`, seconds. `0` = off.
    pub cpu_secs: u64,
    /// `RLIMIT_NOFILE`.
    pub nofile: u64,
    /// `RLIMIT_FSIZE`, bytes. `0` = off.
    ///
    /// DESIGN §6.5.3 requires `RLIMIT_FSIZE` to be applied but §6.5.1's table has no key for it;
    /// `limits.file_size_bytes` is that key. See `docs/INTEGRATION-NOTES.md`, WP-10.
    pub file_size_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            uses_global_slot: true,
            max_concurrent_resolves: 1,
            min_request_interval_ms: 0,
            resolve_timeout_secs: 60,
            download_stall_secs: 600,
            download_hard_timeout_secs: 0,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            memory_bytes: 0,
            cpu_secs: 0,
            nofile: 1024,
            file_size_bytes: 0,
        }
    }
}

/// `resolve.format` (DESIGN §6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ResolveFormat {
    /// One JSON object per line, read as the child prints them.
    #[default]
    JsonLines,
    /// One JSON document — an object or an array of entries — on stdout.
    Json,
}

/// `*.stdin` (DESIGN §6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StdinMode {
    /// stdin is `/dev/null`.
    #[default]
    None,
    /// The whole request is written as one JSON line and stdin is then closed.
    Json,
}

/// `download.expect_output` — how success is judged (DESIGN §6.5.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExpectOutput {
    /// Exit 0 **and** `{out_path}` exists and is non-empty.
    #[default]
    PathTemplate,
    /// Exit 0 **and** a `{"t":"result","path":…}` line was printed.
    ResultFrame,
    /// Exit 0 **and** ≥ 1 file in `{out_dir}` newer than job start; newest wins.
    NewestInDir,
}

impl ExpectOutput {
    /// The manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PathTemplate => "path_template",
            Self::ResultFrame => "result_frame",
            Self::NewestInDir => "newest_in_dir",
        }
    }
}

/// A validated argv template plus its spawn context, shared by `[resolve]` and `[download]`.
#[derive(Debug)]
pub struct CommandSpec {
    /// The templated argv. Element 0 is already resolved to an absolute program path.
    pub argv: Vec<Template>,
    /// The resolved `argv[0]`.
    pub program: PathBuf,
    /// Working directory.
    pub cwd: PathBuf,
    /// What is written to stdin.
    pub stdin: StdinMode,
}

impl CommandSpec {
    /// The argv as an operator sees it in `GET api/v2/providers` — templates unrendered, so no
    /// secret substituted at run time can leak through the audit view (DESIGN §6.5.3).
    #[must_use]
    pub fn argv_source(&self) -> Vec<&str> {
        self.argv.iter().map(Template::as_str).collect()
    }
}

/// `[resolve]` (DESIGN §6.5.1).
#[derive(Debug)]
pub struct ResolveSpec {
    /// The command to run.
    pub command: CommandSpec,
    /// How its stdout is parsed.
    pub format: ResolveFormat,
}

/// `[download]` (DESIGN §6.5.1).
#[derive(Debug)]
pub struct DownloadSpec {
    /// The command to run.
    pub command: CommandSpec,
    /// How success is judged.
    pub expect_output: ExpectOutput,
    /// The extension `{out_path}` and `path_template` use.
    pub output_ext: Box<str>,
    /// Whether an existing `{out_path}` may be replaced.
    pub overwrite: bool,
}

/// `[env]` (DESIGN §6.5.1).
#[derive(Debug, Default)]
pub struct EnvSpec {
    /// Variable **names** inherited from the server process.
    pub pass: Vec<Box<str>>,
    /// Literal variables, `${VAR}` already interpolated at load time; values are still templates.
    pub set: Vec<(Box<str>, Template)>,
}

/// A validated `plugin.toml` (DESIGN §6.5.1).
#[derive(Debug)]
pub struct PluginManifest {
    /// The directory name, which is also the `command:<name>` suffix.
    pub dir_name: Box<str>,
    /// The plugin's own directory, absolute.
    pub dir: PathBuf,
    /// Always [`MANIFEST_VERSION`].
    pub manifest_version: u32,
    /// Display name.
    pub name: Box<str>,
    /// Shown to clients.
    pub version: Box<str>,
    /// Shown in the catalogue.
    pub description: Box<str>,
    /// Shown in the catalogue.
    pub homepage: Box<str>,
    /// Credits.
    pub authors: Vec<Box<str>>,
    /// `[match]`. `None` for a hook-only manifest.
    pub match_spec: Option<MatchSpec>,
    /// `[capabilities]`.
    pub capabilities: Capabilities,
    /// `[limits]`, clamped.
    pub limits: Limits,
    /// `[resolve]`. `Some` iff `capabilities.resolve`.
    pub resolve: Option<ResolveSpec>,
    /// `[download]`. `None` for a hook-only manifest.
    pub download: Option<DownloadSpec>,
    /// `[progress]`.
    pub progress: ProgressSpec,
    /// `[env]`.
    pub env: EnvSpec,
    /// `[headers]`, name → template.
    pub headers: Vec<(Box<str>, Template)>,
    /// The client-facing catalogue, derived from `[catalog]` or synthesised.
    pub catalog: Arc<FormatCatalog>,
    /// `[[hook]]` tables.
    pub hooks: Vec<HookSpec>,
    /// Non-fatal load problems: clamps and auto-anchors.
    pub warnings: Vec<Warning>,
    /// A hash of the manifest bytes, so a reload can tell "unchanged" from "updated".
    pub fingerprint: u64,
}

impl PluginManifest {
    /// This plugin's provider id: `command:<dirname>`.
    ///
    /// # Panics
    /// Never: the directory name was validated against [`PLUGIN_NAME_PATTERN`] at load time, and
    /// `command:` plus 32 such characters is always a legal [`ProviderId`].
    #[must_use]
    pub fn provider_id(&self) -> ProviderId {
        ProviderId::parse(&format!("command:{}", self.dir_name)).unwrap_or_else(|_| {
            // Unreachable: `dir_name` matched `^[a-z0-9][a-z0-9_-]{0,31}$` and `ProviderId`
            // accepts `[A-Za-z0-9._:-]{1,64}`. Kept total rather than panicking.
            ProviderId::parse("command:invalid").unwrap_or_else(|_| unreachable_id())
        })
    }

    /// Whether this manifest declares a provider at all (as opposed to hooks only).
    #[must_use]
    pub const fn declares_provider(&self) -> bool {
        self.match_spec.is_some() && self.download.is_some()
    }
}

/// The last-resort id. Split out so `provider_id` has no `expect` on a hot path.
fn unreachable_id() -> ProviderId {
    // `ProviderId::parse` only rejects the empty string, over-long input and disallowed bytes;
    // `"command"` is none of those, so this branch cannot be reached at run time.
    match ProviderId::parse("command") {
        Ok(id) => id,
        Err(_) => unreachable!("\"command\" is a valid provider id"),
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Reads and validates `<dir>/plugin.toml` (DESIGN §6.5.1, §6.5.2).
///
/// # Errors
/// [`ManifestError::Read`] when the file is absent or unreadable, [`ManifestError::Syntax`] when it
/// is not valid TOML, and [`ManifestError::Invalid`] for every semantic check in the §6.5.2 table.
pub fn load_manifest(dir: &Path) -> Result<PluginManifest, ManifestError> {
    load_manifest_with_env(dir, &|name| std::env::var(name).ok())
}

/// [`load_manifest`], with the `${VAR}` lookup injected so the interpolation is testable without
/// mutating the process environment.
///
/// # Errors
/// As [`load_manifest`].
pub fn load_manifest_with_env(
    dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PluginManifest, ManifestError> {
    let file = dir.join(MANIFEST_FILE);
    let bytes = std::fs::read(&file).map_err(|source| ManifestError::Read {
        file: file.clone(),
        source,
    })?;
    let text = String::from_utf8_lossy(&bytes);
    let raw: RawManifest = toml::from_str(&text).map_err(|e| ManifestError::Syntax {
        file: file.clone(),
        message: e.to_string(),
    })?;
    validate(dir, &raw, fingerprint(&bytes), env)
}

/// FNV-1a over the manifest bytes. Only ever compared for equality, never published.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Interpolates `${VAR}` from `env`, at load time (DESIGN §13.4).
///
/// An unset variable becomes the empty string; the caller records a [`Warning`] so a mistyped
/// `${PLEX_TOKEEN}` is visible rather than producing a silent 401 forever. `$${` escapes a literal
/// `${`.
#[must_use]
pub fn interpolate_env(s: &str, env: &dyn Fn(&str) -> Option<String>) -> (String, Vec<Box<str>>) {
    let mut out = String::with_capacity(s.len());
    let mut missing = Vec::new();
    let mut rest = s;
    while let Some(at) = rest.find("${") {
        if rest[..at].ends_with('$') {
            // `$${VAR}` — emit a literal `${VAR}`.
            out.push_str(&rest[..at - 1]);
            out.push_str("${");
            rest = &rest[at + 2..];
            continue;
        }
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match env(name) {
                    Some(v) => out.push_str(&v),
                    None => missing.push(Box::from(name)),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    (out, missing)
}

/// The whole of DESIGN §6.5.2.
fn validate(
    dir: &Path,
    raw: &RawManifest,
    fingerprint: u64,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PluginManifest, ManifestError> {
    let mut warnings = Vec::new();

    // --- identity ---
    let dir_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !is_plugin_name(&dir_name) {
        return Err(ManifestError::invalid(
            "plugin directory",
            format!("{dir_name:?} does not match {PLUGIN_NAME_PATTERN}"),
        ));
    }
    match raw.manifest_version {
        Some(MANIFEST_VERSION) => {}
        _ => {
            return Err(ManifestError::invalid(
                "manifest_version",
                "unsupported manifest_version",
            ));
        }
    }
    let name = non_empty(raw.name.as_deref(), "name")?;
    let version = non_empty(raw.version.as_deref(), "version")?;

    // --- the directory itself must be safe to execute out of ---
    check_dir_permissions(dir, &mut warnings)?;

    // --- capabilities ---
    let capabilities = Capabilities {
        resolve: raw.capabilities.resolve,
        playlists: raw.capabilities.playlists,
        streaming_resolve: raw
            .capabilities
            .streaming_resolve
            .unwrap_or(raw.capabilities.resolve),
        subtitles: raw.capabilities.subtitles,
        chapters: raw.capabilities.chapters,
        thumbnails: raw.capabilities.thumbnails,
        nfo_capable: raw.capabilities.nfo_capable,
        cancel: match raw.capabilities.cancel.as_deref() {
            None | Some("process_group") => CancelPolicy::ProcessGroup,
            Some("cooperative") => CancelPolicy::Cooperative,
            Some("none") => CancelPolicy::None,
            Some(other) => {
                return Err(ManifestError::invalid(
                    "capabilities.cancel",
                    format!("{other:?} is not one of process_group | cooperative | none"),
                ));
            }
        },
    };

    // --- limits (clamp + WARN) ---
    let limits = clamp_limits(&raw.limits, &mut warnings);

    // --- hooks (parsed even for a provider manifest) ---
    let hooks = hookspec::parse_hooks(&dir_name, &raw.hooks, env, &mut warnings)?;

    // --- provider or hook-only? ---
    let declares_provider = raw.match_.is_some() || raw.download.is_some() || raw.resolve.is_some();
    if !declares_provider {
        if hooks.is_empty() {
            return Err(ManifestError::invalid(
                "match",
                "manifest declares neither a provider ([match] + [download]) nor a [[hook]]",
            ));
        }
        // A hook-only manifest is valid and omits [match]/[download] (DESIGN §6.5.1).
        return Ok(PluginManifest {
            dir_name: dir_name.into(),
            dir: dir.to_path_buf(),
            manifest_version: MANIFEST_VERSION,
            name: name.into(),
            version: version.into(),
            description: raw.description.clone().into(),
            homepage: raw.homepage.clone().into(),
            authors: raw.authors.iter().map(|a| a.clone().into()).collect(),
            match_spec: None,
            capabilities,
            limits,
            resolve: None,
            download: None,
            progress: ProgressSpec::default(),
            env: EnvSpec::default(),
            headers: Vec::new(),
            catalog: Arc::new(hook_only_catalog()),
            hooks,
            warnings,
            fingerprint,
        });
    }

    // --- [match] ---
    let raw_match = raw.match_.as_ref().ok_or_else(|| {
        ManifestError::invalid("match", "a provider manifest needs a [match] section")
    })?;
    let match_spec = validate_match(raw_match, &mut warnings)?;

    // --- [headers] and [env], which the argv templates may reference ---
    let mut headers = Vec::with_capacity(raw.headers.len());
    for (k, v) in &raw.headers {
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
            return Err(ManifestError::invalid(
                format!("headers.{k}"),
                "a header name must be non-empty printable ASCII without a colon",
            ));
        }
        let (interpolated, missing) = interpolate_env(v, env);
        warn_missing(&mut warnings, &format!("headers.{k}"), &missing);
        headers.push((
            Box::from(k.as_str()),
            template(&interpolated, &format!("headers.{k}"), capabilities.resolve)?,
        ));
    }

    let mut env_set = Vec::with_capacity(raw.env.set.len());
    for (k, v) in &raw.env.set {
        let (interpolated, missing) = interpolate_env(v, env);
        warn_missing(&mut warnings, &format!("env.set.{k}"), &missing);
        env_set.push((
            Box::from(k.as_str()),
            template(&interpolated, &format!("env.set.{k}"), capabilities.resolve)?,
        ));
    }
    let env_spec = EnvSpec {
        pass: raw.env.pass.iter().map(|p| p.clone().into()).collect(),
        set: env_set,
    };

    // --- [resolve] ---
    let resolve = if capabilities.resolve {
        let raw_resolve = raw.resolve.as_ref().ok_or_else(|| {
            ManifestError::invalid(
                "resolve",
                "capabilities.resolve = true needs a [resolve] section",
            )
        })?;
        let command = validate_command(
            dir,
            &raw_resolve.command,
            raw_resolve.cwd.as_deref(),
            raw_resolve.stdin.as_deref(),
            "resolve",
            capabilities.resolve,
        )?;
        let format = match raw_resolve.format.as_deref() {
            None | Some("json_lines") => ResolveFormat::JsonLines,
            Some("json") => ResolveFormat::Json,
            Some(other) => {
                return Err(ManifestError::invalid(
                    "resolve.format",
                    format!("{other:?} is not one of json_lines | json"),
                ));
            }
        };
        Some(ResolveSpec { command, format })
    } else {
        if raw.resolve.is_some() {
            warnings.push(Warning::new(
                "resolve",
                "ignored: capabilities.resolve is false",
            ));
        }
        None
    };

    // --- [download] ---
    let raw_download = raw.download.as_ref().ok_or_else(|| {
        ManifestError::invalid("download", "a provider manifest needs a [download] section")
    })?;
    let command = validate_command(
        dir,
        &raw_download.command,
        raw_download.cwd.as_deref(),
        raw_download.stdin.as_deref(),
        "download",
        capabilities.resolve,
    )?;
    let expect_output = match raw_download.expect_output.as_deref() {
        None | Some("path_template") => ExpectOutput::PathTemplate,
        Some("result_frame") => ExpectOutput::ResultFrame,
        Some("newest_in_dir") => ExpectOutput::NewestInDir,
        Some(other) => {
            return Err(ManifestError::invalid(
                "download.expect_output",
                format!("{other:?} is not one of path_template | result_frame | newest_in_dir"),
            ));
        }
    };
    let output_ext = raw_download
        .output_ext
        .clone()
        .unwrap_or_else(|| "mp4".to_owned());
    if !output_ext.is_empty()
        && !output_ext
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(ManifestError::invalid(
            "download.output_ext",
            format!("{output_ext:?} is not a plain file extension"),
        ));
    }
    let download = DownloadSpec {
        command,
        expect_output,
        output_ext: output_ext.into(),
        overwrite: raw_download.overwrite.unwrap_or(true),
    };

    // --- [progress] ---
    let progress = ProgressSpec::validate(
        raw.progress.kind.as_deref(),
        raw.progress.source.as_deref(),
        raw.progress.strip_ansi,
        raw.progress.cr_as_newline,
        raw.progress.last_match_wins,
        raw.progress.min_interval_ms,
        &raw.progress.patterns,
        &raw.progress.units,
        &raw.progress.status_map,
    )?;

    // --- [catalog] ---
    let provider_id = ProviderId::parse(&format!("command:{dir_name}"))
        .map_err(|e| ManifestError::invalid("plugin directory", e.to_string()))?;
    let catalog = match &raw.catalog {
        Some(c) => validate_catalog(&provider_id, c)?,
        None => derived_catalog(&provider_id, &download.output_ext),
    };

    Ok(PluginManifest {
        dir_name: dir_name.into(),
        dir: dir.to_path_buf(),
        manifest_version: MANIFEST_VERSION,
        name: name.into(),
        version: version.into(),
        description: raw.description.clone().into(),
        homepage: raw.homepage.clone().into(),
        authors: raw.authors.iter().map(|a| a.clone().into()).collect(),
        match_spec: Some(match_spec),
        capabilities,
        limits,
        resolve,
        download: Some(download),
        progress,
        env: env_spec,
        headers,
        catalog: Arc::new(catalog),
        hooks,
        warnings,
        fingerprint,
    })
}

/// A best-effort `[match]` parse for a manifest that failed validation (DESIGN §6.4).
///
/// A broken plugin must still **claim its URLs**, so that an item routed to it fails with
/// `provider_degraded` and the reason rather than silently falling through to `ytdlp` and
/// downloading a login page. That needs a matcher, and the only honest source for one is the
/// manifest's own `[match]` section — parsed on its own, ignoring everything that was wrong with
/// the rest of the file. `None` when even `[match]` is unusable, in which case the plugin is
/// registered with [`crate::provider::DegradedProvider::unmatched`].
#[must_use]
pub fn partial_match(dir: &Path) -> Option<MatchSpec> {
    #[derive(Deserialize)]
    struct Only {
        #[serde(rename = "match")]
        match_: Option<RawMatch>,
    }
    let text = std::fs::read_to_string(dir.join(MANIFEST_FILE)).ok()?;
    let only: Only = toml::from_str(&text).ok()?;
    let raw = only.match_?;
    let mut ignored = Vec::new();
    validate_match(&raw, &mut ignored).ok()
}

/// Whether `s` matches [`PLUGIN_NAME_PATTERN`], without compiling a regex for it.
#[must_use]
pub fn is_plugin_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn non_empty(v: Option<&str>, key: &'static str) -> Result<String, ManifestError> {
    match v {
        Some(s) if !s.trim().is_empty() => Ok(s.to_owned()),
        _ => Err(ManifestError::invalid(key, "must be a non-empty string")),
    }
}

fn warn_missing(warnings: &mut Vec<Warning>, key: &str, missing: &[Box<str>]) {
    for name in missing {
        tracing::warn!(key, var = %name, "plugin manifest references an unset ${{VAR}}");
        warnings.push(Warning::new(
            key,
            format!("${{{name}}} is not set in the server environment; substituted \"\""),
        ));
    }
}

/// Refuses to execute out of a world-writable directory, or a directory holding a setuid/setgid
/// file (DESIGN §6.5.2).
fn check_dir_permissions(dir: &Path, _warnings: &mut Vec<Warning>) -> Result<(), ManifestError> {
    use std::os::unix::fs::PermissionsExt as _;

    let meta = std::fs::metadata(dir).map_err(|source| ManifestError::Read {
        file: dir.to_path_buf(),
        source,
    })?;
    let mode = meta.permissions().mode();
    if mode & 0o002 != 0 {
        return Err(ManifestError::invalid(
            "plugin directory",
            format!(
                "refusing to execute out of a world-writable directory (mode {:o})",
                mode & 0o7777
            ),
        ));
    }
    let entries = std::fs::read_dir(dir).map_err(|source| ManifestError::Read {
        file: dir.to_path_buf(),
        source,
    })?;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mode = meta.permissions().mode();
        if mode & 0o6000 != 0 {
            return Err(ManifestError::invalid(
                "plugin directory",
                format!(
                    "refusing to execute: {} is setuid/setgid (mode {:o})",
                    entry.file_name().to_string_lossy(),
                    mode & 0o7777
                ),
            ));
        }
        if mode & 0o002 != 0 {
            return Err(ManifestError::invalid(
                "plugin directory",
                format!(
                    "refusing to execute: {} is world-writable (mode {:o})",
                    entry.file_name().to_string_lossy(),
                    mode & 0o7777
                ),
            ));
        }
    }
    Ok(())
}

fn clamp_limits(raw: &RawLimits, warnings: &mut Vec<Warning>) -> Limits {
    let d = Limits::default();
    let mut clamp_concurrency = |v: Option<u32>, default: u32, key: &'static str| -> u32 {
        let v = v.unwrap_or(default);
        if v == 0 {
            warnings.push(Warning::new(key, "0 is not a concurrency; clamped to 1"));
            1
        } else if v > MAX_CONCURRENT_CAP {
            warnings.push(Warning::new(
                key,
                format!("{v} exceeds the hard cap; clamped to {MAX_CONCURRENT_CAP}"),
            ));
            MAX_CONCURRENT_CAP
        } else {
            v
        }
    };
    // Two closures cannot both borrow `warnings`, so concurrency is done first and the timeout
    // clamps run after.
    let max_concurrent = clamp_concurrency(
        raw.max_concurrent,
        d.max_concurrent,
        "limits.max_concurrent",
    );
    let max_concurrent_resolves = clamp_concurrency(
        raw.max_concurrent_resolves,
        d.max_concurrent_resolves,
        "limits.max_concurrent_resolves",
    );

    let mut clamp_secs = |v: Option<u64>, default: u64, key: &'static str| -> u64 {
        let v = v.unwrap_or(default);
        if v > MAX_TIMEOUT_SECS {
            warnings.push(Warning::new(
                key,
                format!("{v}s exceeds the 24 h hard cap; clamped to {MAX_TIMEOUT_SECS}"),
            ));
            MAX_TIMEOUT_SECS
        } else {
            v
        }
    };
    let resolve_timeout_secs = clamp_secs(
        raw.resolve_timeout_secs,
        d.resolve_timeout_secs,
        "limits.resolve_timeout_secs",
    );
    let download_stall_secs = clamp_secs(
        raw.download_stall_secs,
        d.download_stall_secs,
        "limits.download_stall_secs",
    );
    let download_hard_timeout_secs = clamp_secs(
        raw.download_hard_timeout_secs,
        d.download_hard_timeout_secs,
        "limits.download_hard_timeout_secs",
    );
    let cpu_secs = clamp_secs(raw.cpu_secs, d.cpu_secs, "limits.cpu_secs");

    let max_output_bytes = match raw.max_output_bytes {
        Some(v) if v < MIN_MAX_OUTPUT_BYTES => {
            warnings.push(Warning::new(
                "limits.max_output_bytes",
                format!("{v} is below the {MIN_MAX_OUTPUT_BYTES}-byte floor; clamped"),
            ));
            MIN_MAX_OUTPUT_BYTES
        }
        Some(v) => v,
        None => d.max_output_bytes,
    };

    Limits {
        max_concurrent,
        uses_global_slot: raw.uses_global_slot.unwrap_or(d.uses_global_slot),
        max_concurrent_resolves,
        min_request_interval_ms: raw
            .min_request_interval_ms
            .unwrap_or(d.min_request_interval_ms),
        resolve_timeout_secs,
        download_stall_secs,
        download_hard_timeout_secs,
        max_output_bytes,
        memory_bytes: raw.memory_bytes.unwrap_or(d.memory_bytes),
        cpu_secs,
        nofile: raw.nofile.unwrap_or(d.nofile),
        file_size_bytes: raw.file_size_bytes.unwrap_or(d.file_size_bytes),
    }
}

fn validate_match(raw: &RawMatch, warnings: &mut Vec<Warning>) -> Result<MatchSpec, ManifestError> {
    if raw.hosts.is_empty() && raw.host_regex.is_none() {
        return Err(ManifestError::invalid(
            "match",
            "at least one of match.hosts or match.host_regex is required",
        ));
    }
    let hosts: Vec<Box<str>> = raw
        .hosts
        .iter()
        .map(|h| h.trim().trim_start_matches('.').to_lowercase())
        .filter(|h| !h.is_empty())
        .map(|h| Box::from(h.as_str()))
        .collect();

    let host_regex = match &raw.host_regex {
        None => None,
        Some(src) => {
            let anchored = anchor(src);
            if anchored != *src {
                tracing::warn!(
                    pattern = %src,
                    "match.host_regex was not anchored; anchoring it"
                );
                warnings.push(Warning::new(
                    "match.host_regex",
                    format!("not anchored; compiled as {anchored:?}"),
                ));
            }
            Some(compile(&anchored, "match.host_regex")?)
        }
    };
    let path_regex = raw
        .path_regex
        .as_deref()
        .map(|s| compile(s, "match.path_regex"))
        .transpose()?;
    let exclude_path_regex = raw
        .exclude_path_regex
        .as_deref()
        .map(|s| compile(s, "match.exclude_path_regex"))
        .transpose()?;

    let schemes: Vec<Box<str>> = raw
        .schemes
        .clone()
        .unwrap_or_else(|| vec!["http".to_owned(), "https".to_owned()])
        .iter()
        .map(|s| Box::from(s.to_lowercase().as_str()))
        .collect();

    let priority = match raw.priority {
        None => None,
        Some(p) if (0..=255).contains(&p) => Some(u8::try_from(p).unwrap_or(u8::MAX)),
        Some(p) => {
            return Err(ManifestError::invalid(
                "match.priority",
                format!("{p} is outside 0..=255"),
            ));
        }
    };

    Ok(MatchSpec {
        hosts,
        host_regex,
        path_regex,
        exclude_path_regex,
        schemes,
        priority,
    })
}

/// Anchors a host regex at both ends, which is what "anchored regex over the normalised host"
/// means (DESIGN §6.5.1).
fn anchor(src: &str) -> String {
    let mut s = src.to_owned();
    if !s.starts_with('^') {
        s.insert(0, '^');
    }
    if !s.ends_with('$') || s.ends_with("\\$") {
        s.push('$');
    }
    s
}

fn compile(src: &str, key: &'static str) -> Result<Regex, ManifestError> {
    Regex::new(src).map_err(|e| {
        ManifestError::invalid(
            key,
            format!("{src:?} does not compile: {}", one_line(&e.to_string())),
        )
    })
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn template(src: &str, key: &str, resolve: bool) -> Result<Template, ManifestError> {
    let t = Template::parse(src, TokenScope::Provider)
        .map_err(|e| ManifestError::invalid(key, e.to_string()))?;
    if !resolve && let Some(tok) = t.tokens().find(|t| matches!(t, Token::StateField(_))) {
        return Err(ManifestError::invalid(
            key,
            TemplateError::StateNeedsResolve {
                token: tok.name().into(),
            }
            .to_string(),
        ));
    }
    Ok(t)
}

fn validate_command(
    dir: &Path,
    argv: &[String],
    cwd: Option<&str>,
    stdin: Option<&str>,
    section: &'static str,
    resolve: bool,
) -> Result<CommandSpec, ManifestError> {
    if argv.is_empty() {
        return Err(ManifestError::invalid(
            format!("{section}.command"),
            "must be a non-empty argv array",
        ));
    }
    let mut templates = Vec::with_capacity(argv.len());
    for (i, element) in argv.iter().enumerate() {
        templates.push(template(
            element,
            &format!("{section}.command[{i}]"),
            resolve,
        )?);
    }
    // `argv[0]` must be a fixed path, not a template: a program name that depends on the entry is
    // not auditable and cannot be checked at load time.
    if !templates[0].is_literal() {
        return Err(ManifestError::invalid(
            format!("{section}.command[0]"),
            "the program name may not contain a template token",
        ));
    }
    let program = resolve_program(dir, &argv[0]).ok_or_else(|| {
        ManifestError::invalid(
            format!("{section}.command[0]"),
            format!(
                "{:?} is not an existing executable in the plugin directory or on PATH",
                argv[0]
            ),
        )
    })?;

    let cwd = match cwd {
        None => dir.to_path_buf(),
        Some(c) => {
            let p = Path::new(c);
            let joined = if p.is_absolute() {
                p.to_path_buf()
            } else {
                dir.join(p)
            };
            if !joined.is_dir() {
                return Err(ManifestError::invalid(
                    format!("{section}.cwd"),
                    format!("{c:?} is not a directory"),
                ));
            }
            joined
        }
    };
    let stdin = match stdin {
        None | Some("none") => StdinMode::None,
        Some("json") => StdinMode::Json,
        Some(other) => {
            return Err(ManifestError::invalid(
                format!("{section}.stdin"),
                format!("{other:?} is not one of none | json"),
            ));
        }
    };
    Ok(CommandSpec {
        argv: templates,
        program,
        cwd,
        stdin,
    })
}

/// Resolves `argv[0]` against the plugin directory, then `PATH` (DESIGN §6.5.1).
#[must_use]
pub fn resolve_program(dir: &Path, program: &str) -> Option<PathBuf> {
    let is_exec = |p: &Path| -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return is_exec(candidate).then(|| candidate.to_path_buf());
    }
    let local = dir.join(candidate);
    if is_exec(&local) {
        return Some(local);
    }
    // A relative path with a separator is plugin-local only; only a bare name searches PATH.
    if program.contains('/') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| is_exec(p))
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// The catalogue a hook-only manifest gets: empty, since it downloads nothing.
fn hook_only_catalog() -> FormatCatalog {
    FormatCatalog {
        provider: unreachable_id(),
        version: 1,
        naming: NamingPolicy::Template,
        download_types: Vec::new(),
    }
}

/// The `[catalog]`-absent default: a single `video`/`<ext>`/`best` type with an honest notice
/// (DESIGN §6.5.1).
fn derived_catalog(provider: &ProviderId, output_ext: &str) -> FormatCatalog {
    let ext = if output_ext.is_empty() {
        "mp4"
    } else {
        output_ext
    };
    FormatCatalog {
        provider: provider.clone(),
        version: 1,
        naming: NamingPolicy::Template,
        download_types: vec![DownloadTypeSpec {
            id: "video".into(),
            label: "Video".into(),
            default_format: ext.into(),
            formats: vec![FormatSpec {
                id: ext.into(),
                label: ext.to_uppercase().into(),
                default_quality: "best".into(),
                qualities: vec![QualitySpec {
                    id: "best".into(),
                    label: "Best".into(),
                    notice: None,
                }],
                codecs: Vec::new(),
                notice: Some(
                    "This plugin declares no [catalog], so only its own default is offered.".into(),
                ),
                flags: FormatFlags::default(),
            }],
            options: Vec::new(),
        }],
    }
}

fn validate_catalog(
    provider: &ProviderId,
    raw: &RawCatalog,
) -> Result<FormatCatalog, ManifestError> {
    let naming = match raw.naming.as_deref() {
        None | Some("template") => NamingPolicy::Template,
        Some("provider") => NamingPolicy::Provider,
        Some(other) => {
            return Err(ManifestError::invalid(
                "catalog.naming",
                format!("{other:?} is not one of template | provider"),
            ));
        }
    };
    if raw.download_types.is_empty() {
        return Err(ManifestError::invalid(
            "catalog.download_types",
            "a [catalog] section must declare at least one download type",
        ));
    }
    let mut seen_types = BTreeSet::new();
    let mut download_types = Vec::with_capacity(raw.download_types.len());
    for dt in &raw.download_types {
        check_catalog_id(&dt.id, "catalog.download_types.id")?;
        if !seen_types.insert(dt.id.clone()) {
            return Err(ManifestError::invalid(
                "catalog.download_types.id",
                format!("{:?} is declared twice", dt.id),
            ));
        }
        if dt.formats.is_empty() {
            return Err(ManifestError::invalid(
                format!("catalog.download_types.{}.formats", dt.id),
                "must declare at least one format",
            ));
        }
        let mut seen_formats = BTreeSet::new();
        let mut formats = Vec::with_capacity(dt.formats.len());
        for f in &dt.formats {
            check_catalog_id(&f.id, "catalog.download_types.formats.id")?;
            if !seen_formats.insert(f.id.clone()) {
                return Err(ManifestError::invalid(
                    "catalog.download_types.formats.id",
                    format!("{:?} is declared twice", f.id),
                ));
            }
            let mut seen_qualities = BTreeSet::new();
            let mut qualities = Vec::with_capacity(f.qualities.len());
            for q in &f.qualities {
                check_catalog_id(&q.id, "catalog.download_types.formats.qualities.id")?;
                if !seen_qualities.insert(q.id.clone()) {
                    return Err(ManifestError::invalid(
                        "catalog.download_types.formats.qualities.id",
                        format!("{:?} is declared twice", q.id),
                    ));
                }
                qualities.push(QualitySpec {
                    id: q.id.clone().into(),
                    label: label_of(q.label.as_deref(), &q.id),
                    notice: q.notice.clone().map(Into::into),
                });
            }
            if qualities.is_empty() {
                qualities.push(QualitySpec {
                    id: "best".into(),
                    label: "Best".into(),
                    notice: None,
                });
            }
            let default_quality = pick_default(
                f.default_quality.as_deref(),
                qualities.iter().map(|q| &*q.id),
                "catalog.download_types.formats.default_quality",
            )?;
            let codecs = f
                .codecs
                .iter()
                .map(|c| {
                    check_catalog_id(&c.id, "catalog.download_types.formats.codecs.id").map(|()| {
                        CodecSpec {
                            id: c.id.clone().into(),
                            label: label_of(c.label.as_deref(), &c.id),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            formats.push(FormatSpec {
                id: f.id.clone().into(),
                label: label_of(f.label.as_deref(), &f.id),
                qualities,
                default_quality,
                codecs,
                notice: f.notice.clone().map(Into::into),
                flags: FormatFlags {
                    advisory: f.advisory,
                    requires_ffmpeg: f.requires_ffmpeg,
                    lossy_remux: f.lossy_remux,
                    slow: f.slow,
                },
            });
        }
        let default_format = pick_default(
            dt.default_format.as_deref(),
            formats.iter().map(|f| &*f.id),
            "catalog.download_types.default_format",
        )?;
        let mut options = Vec::with_capacity(dt.options.len());
        for o in &dt.options {
            check_catalog_id(&o.id, "catalog.download_types.options.id")?;
            options.push(OptionSpec {
                id: o.id.clone().into(),
                label: label_of(o.label.as_deref(), &o.id),
                kind: option_kind(o)?,
                default: o.default.clone(),
                choices: o
                    .choices
                    .iter()
                    .map(|c| Choice {
                        id: c.id.clone().into(),
                        label: label_of(c.label.as_deref(), &c.id),
                    })
                    .collect(),
                help: o.help.clone().map(Into::into),
            });
        }
        download_types.push(DownloadTypeSpec {
            id: dt.id.clone().into(),
            label: label_of(dt.label.as_deref(), &dt.id),
            formats,
            default_format,
            options,
        });
    }
    Ok(FormatCatalog {
        provider: provider.clone(),
        version: 1,
        naming,
        download_types,
    })
}

fn option_kind(o: &RawOption) -> Result<OptionKind, ManifestError> {
    Ok(match o.kind.as_str() {
        "" | "bool" => OptionKind::Bool,
        "int" => OptionKind::Int {
            min: o.min.unwrap_or(i64::MIN),
            max: o.max.unwrap_or(i64::MAX),
        },
        "enum" => OptionKind::Enum,
        "text" => OptionKind::Text {
            pattern: o.pattern.clone().map(Into::into),
        },
        "path" => OptionKind::Path,
        other => {
            return Err(ManifestError::invalid(
                "catalog.download_types.options.kind",
                format!("{other:?} is not one of bool | int | enum | text | path"),
            ));
        }
    })
}

fn label_of(label: Option<&str>, id: &str) -> Box<str> {
    match label {
        Some(l) if !l.is_empty() => l.into(),
        _ => id.into(),
    }
}

fn pick_default<'a>(
    declared: Option<&str>,
    mut ids: impl Iterator<Item = &'a str>,
    key: &'static str,
) -> Result<Box<str>, ManifestError> {
    let all: Vec<&str> = ids.by_ref().collect();
    match declared {
        Some(d) if all.contains(&d) => Ok(d.into()),
        Some(d) => Err(ManifestError::invalid(
            key,
            format!("{d:?} is not one of {all:?}"),
        )),
        None => all
            .first()
            .map(|first| Box::from(*first))
            .ok_or_else(|| ManifestError::invalid(key, "there is nothing to default to")),
    }
}

/// Catalog ids must match `^[a-z0-9_]+$` (DESIGN §6.5.2).
fn check_catalog_id(id: &str, key: &'static str) -> Result<(), ManifestError> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ManifestError::invalid(
            key,
            format!("{id:?} does not match ^[a-z0-9_]+$"),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plugin_names_follow_the_pattern() {
        for ok in ["bandcamp", "a", "0", "my_plugin-2", &"x".repeat(32)] {
            assert!(is_plugin_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "",
            "_x",
            "-x",
            "A",
            "Bandcamp",
            "a b",
            "a.b",
            &"x".repeat(33),
        ] {
            assert!(!is_plugin_name(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn env_interpolation_reports_what_was_missing() {
        let env = |name: &str| match name {
            "PLEX_TOKEN" => Some("abc123".to_owned()),
            _ => None,
        };
        let (out, missing) = interpolate_env("t=${PLEX_TOKEN}&u=${NOPE}", &env);
        assert_eq!(out, "t=abc123&u=");
        assert_eq!(missing, [Box::<str>::from("NOPE")]);
        // No interpolation at all.
        let (out, missing) = interpolate_env("plain", &env);
        assert_eq!(out, "plain");
        assert!(missing.is_empty());
        // `$${VAR}` is a literal.
        let (out, missing) = interpolate_env("$${PLEX_TOKEN}", &env);
        assert_eq!(out, "${PLEX_TOKEN}");
        assert!(missing.is_empty());
        // An unterminated `${` is literal text.
        let (out, _) = interpolate_env("${unterminated", &env);
        assert_eq!(out, "${unterminated");
    }

    #[test]
    fn host_regexes_are_auto_anchored() {
        assert_eq!(anchor("bandcamp\\.com"), "^bandcamp\\.com$");
        assert_eq!(anchor("^bandcamp\\.com$"), "^bandcamp\\.com$");
        assert_eq!(anchor("^a"), "^a$");
        assert_eq!(anchor("a$"), "^a$");
        // A trailing escaped `$` is data, not an anchor.
        assert_eq!(anchor("a\\$"), "^a\\$$");
    }

    #[test]
    fn limits_are_clamped_with_a_warning() {
        let mut w = Vec::new();
        let l = clamp_limits(
            &RawLimits {
                max_concurrent: Some(1000),
                max_concurrent_resolves: Some(0),
                resolve_timeout_secs: Some(999_999),
                max_output_bytes: Some(1),
                ..RawLimits::default()
            },
            &mut w,
        );
        assert_eq!(l.max_concurrent, MAX_CONCURRENT_CAP);
        assert_eq!(l.max_concurrent_resolves, 1);
        assert_eq!(l.resolve_timeout_secs, MAX_TIMEOUT_SECS);
        assert_eq!(l.max_output_bytes, MIN_MAX_OUTPUT_BYTES);
        assert_eq!(w.len(), 4, "{w:?}");
        // The defaults of the DESIGN §6.5.1 table.
        let mut none = Vec::new();
        let d = clamp_limits(&RawLimits::default(), &mut none);
        assert!(none.is_empty());
        assert_eq!(d.max_concurrent, 1);
        assert!(d.uses_global_slot);
        assert_eq!(d.resolve_timeout_secs, 60);
        assert_eq!(d.download_stall_secs, 600);
        assert_eq!(d.download_hard_timeout_secs, 0);
        assert_eq!(d.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(d.nofile, 1024);
    }

    #[test]
    fn catalog_ids_are_checked() {
        assert!(check_catalog_id("audio", "k").is_ok());
        assert!(check_catalog_id("mp3_320", "k").is_ok());
        for bad in ["", "Audio", "a-b", "a.b", "a b"] {
            let e = check_catalog_id(bad, "catalog.x").unwrap_err();
            assert!(e.to_string().starts_with("catalog.x: "), "{e}");
        }
    }

    #[test]
    fn a_reason_is_one_capped_line() {
        let e = ManifestError::invalid("download.command[0]", "nope\nreally");
        assert_eq!(&*e.reason(), "download.command[0]: nope really");
        let long = ManifestError::invalid("k", "x".repeat(1000));
        assert_eq!(long.reason().chars().count(), 400);
        assert!(e.has_partial_match());
        assert!(
            !ManifestError::Syntax {
                file: PathBuf::from("p"),
                message: "m".to_owned()
            }
            .has_partial_match()
        );
    }
}
