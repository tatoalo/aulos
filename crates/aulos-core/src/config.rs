//! Configuration loading (DESIGN §17.1, §17.3).
//!
//! Every legacy environment variable keeps its name, default and semantics (BRIEF §15, thesis T1):
//! the VPS `docker-compose.yml` must not need editing to cut over. New knobs are `AULOS_*` with
//! safe defaults, and an unknown `AULOS_*` variable is **fatal** — `AULOS_WS_BATCH_MS` versus
//! `AULOS_WS_BATCH_MSEC` is a silent no-op otherwise.
//!
//! [`load`] is a pure function of a [`RawEnv`] plus the current directory: it does no file IO, so
//! the whole §17.3 table is testable without a filesystem. Reading `YTDL_OPTIONS_FILE` is
//! [`crate::ytdl_options::YtdlOptions::load`], called separately by the binary.
//!
//! Field naming rule: the field is the variable name lower-cased, with the `AULOS_` namespace
//! marker stripped (`AULOS_WS_BATCH_MS` → [`Config::ws_batch_ms`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use regex::Regex;

use crate::error::{ErrorCode, Redact};
use crate::paths::Paths;
use crate::prefix::{Prefix, PrefixFixup};

/// A raw environment snapshot: every value is a string, nothing is defaulted yet.
#[derive(Clone, Debug, Default)]
pub struct RawEnv(BTreeMap<String, String>);

impl RawEnv {
    /// The process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    /// An explicit environment, for tests and for `check-config --env-file`.
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    /// One raw value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The keys present, sorted.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// The effective string map — `DEFAULTS` overlaid with this environment, `%%` resolved — with
    /// every secret-bearing value replaced by `«redacted»`.
    ///
    /// A [`SECRET_KEYS`] entry is redacted whole. A [`JSON_OPTION_KEYS`] entry is a JSON object of
    /// yt-dlp options, so it is redacted *per entry*: only the values whose key matches
    /// [`crate::error::SECRET_KEY_PATTERN`] (`password`, `proxy`, `cookiefile`, …) are replaced,
    /// which keeps the table useful without leaking the site password or the proxy credentials
    /// (DESIGN §16.5). A value that does not parse as JSON is redacted whole rather than printed
    /// blind.
    ///
    /// This is what `aulos-server check-config` prints, and what the boot log dumps.
    ///
    /// # Errors
    /// The same errors [`load`] reports for steps 1–2 (indirection).
    pub fn effective_redacted(&self) -> Result<BTreeMap<String, String>, Vec<ConfigError>> {
        let mut merged = self.merged();
        resolve_indirection(&mut merged)?;
        Ok(merged
            .into_iter()
            .map(|(k, v)| {
                let value = if SECRET_KEYS.contains(&k.as_str()) {
                    crate::error::REDACTED.to_owned()
                } else if JSON_OPTION_KEYS.contains(&k.as_str()) {
                    redact_json_options(&v)
                } else {
                    v
                };
                (k, value)
            })
            .collect())
    }

    /// `DEFAULTS` overlaid with this environment.
    fn merged(&self) -> BTreeMap<String, String> {
        let mut map: BTreeMap<String, String> = DEFAULTS
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        for (k, v) in &self.0 {
            if map.contains_key(k) {
                map.insert(k.clone(), v.clone());
            }
        }
        // `AULOS_VERSION` is a documented alias of `METUBE_VERSION`; whichever is set explicitly
        // wins, and `AULOS_VERSION` wins a tie because the Dockerfile sets both.
        if let Some(v) = self.0.get("AULOS_VERSION") {
            map.insert("METUBE_VERSION".to_owned(), v.clone());
        }
        // `PLUGINS_DIR` is the documented default for `AULOS_PLUGINS_DIR`.
        if !self.0.contains_key("AULOS_PLUGINS_DIR")
            && let Some(v) = self.0.get("PLUGINS_DIR")
        {
            map.insert("AULOS_PLUGINS_DIR".to_owned(), v.clone());
        }
        map
    }
}

/// The complete `DEFAULTS` table of DESIGN §17.3.
///
/// The order is the table's order, so a reader can diff the two by eye. Every key here is
/// recognised; an `AULOS_*` key that is **not** here (and not in [`ACCEPTED_IGNORED_PREFIXES`] or
/// [`ACCEPTED_IGNORED_KEYS`]) is a fatal typo.
pub const DEFAULTS: &[(&str, &str)] = &[
    // --- paths and file serving ---
    ("DOWNLOAD_DIR", "."),
    ("AUDIO_DOWNLOAD_DIR", "%%DOWNLOAD_DIR"),
    ("TEMP_DIR", "%%DOWNLOAD_DIR"),
    ("DOWNLOAD_DIRS_INDEXABLE", "false"),
    ("CUSTOM_DIRS", "true"),
    ("CREATE_CUSTOM_DIRS", "true"),
    ("CUSTOM_DIRS_EXCLUDE_REGEX", r"(^|/)[.@].*$"),
    ("DELETE_FILE_ON_TRASHCAN", "false"),
    ("STATE_DIR", "."),
    ("URL_PREFIX", ""),
    ("PUBLIC_HOST_URL", "download/"),
    ("PUBLIC_HOST_AUDIO_URL", "audio_download/"),
    // --- naming ---
    ("OUTPUT_TEMPLATE", "%(title)s.%(ext)s"),
    (
        "OUTPUT_TEMPLATE_CHAPTER",
        "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
    ),
    (
        "OUTPUT_TEMPLATE_PLAYLIST",
        "%(playlist_title)s/%(title)s.%(ext)s",
    ),
    ("OUTPUT_TEMPLATE_CHANNEL", "%(channel)s/%(title)s.%(ext)s"),
    // --- request and subscription defaults ---
    ("DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT", "0"),
    ("SUBSCRIPTION_DEFAULT_CHECK_INTERVAL", "60"),
    ("SUBSCRIPTION_SCAN_PLAYLIST_END", "50"),
    ("SUBSCRIPTION_MAX_SEEN_IDS", "50000"),
    ("CLEAR_COMPLETED_AFTER", "0"),
    // --- yt-dlp options ---
    ("YTDL_OPTIONS", "{}"),
    ("YTDL_OPTIONS_FILE", ""),
    ("YTDL_OPTIONS_PRESETS", "{}"),
    ("YTDL_OPTIONS_PRESETS_FILE", ""),
    ("ALLOW_YTDL_OPTIONS_OVERRIDES", "false"),
    // --- server ---
    ("CORS_ALLOWED_ORIGINS", ""),
    ("ROBOTS_TXT", ""),
    ("HOST", "0.0.0.0"),
    ("PORT", "8081"),
    ("HTTPS", "false"),
    ("CERTFILE", ""),
    ("KEYFILE", ""),
    ("BASE_DIR", ""),
    ("DEFAULT_THEME", "auto"),
    ("MAX_CONCURRENT_DOWNLOADS", "3"),
    ("LOGLEVEL", "INFO"),
    ("ENABLE_ACCESSLOG", "false"),
    // --- StreamingCommunity ---
    ("SC_THREAD_COUNT", "16"),
    ("SC_USE_FFMPEG", "false"),
    ("SC_MAX_CONCURRENT_DOWNLOADS", "1"),
    // --- Jellyfin ---
    ("JELLYFIN_SYNC_ENABLED", "false"),
    ("JELLYFIN_URL", ""),
    ("JELLYFIN_API_KEY", ""),
    ("JELLYFIN_SYNC_TIMEOUT_SECONDS", "20"),
    ("JELLYFIN_LIBRARY_ID", ""),
    ("JELLYFIN_METADATA_REFRESH_MODE", "Default"),
    ("JELLYFIN_IMAGE_REFRESH_MODE", "Default"),
    // --- Telegram ---
    ("TELEGRAM_BOT_ENABLED", "false"),
    ("TELEGRAM_BOT_TOKEN", ""),
    ("TELEGRAM_ALLOWED_CHAT_IDS", ""),
    ("TELEGRAM_STALL_TIMEOUT_SECONDS", "180"),
    ("TELEGRAM_HARD_TIMEOUT_SECONDS", "7200"),
    ("TELEGRAM_MAX_URLS_PER_MESSAGE", "10"),
    // --- identity and plugins ---
    ("METUBE_VERSION", "dev"),
    ("PLUGINS_DIR", "/config/plugins"),
    // --- AULOS_*: storage ---
    ("AULOS_DB_PATH", ""),
    ("AULOS_DB_READERS", "4"),
    ("AULOS_DB_FLUSH_MS", "200"),
    ("AULOS_DB_SYNCHRONOUS", "NORMAL"),
    // --- AULOS_*: realtime ---
    ("AULOS_WS_BATCH_MS", "250"),
    ("AULOS_WS_URGENT_MS", "25"),
    ("AULOS_WS_MAX_DELTAS_PER_FRAME", "200"),
    ("AULOS_WS_REPLAY_FRAMES", "512"),
    ("AULOS_WS_REPLAY_BYTES", "4194304"),
    ("AULOS_WS_MAX_CLIENTS", "64"),
    ("AULOS_WS_SEND_TIMEOUT_MS", "5000"),
    ("AULOS_MEM_DONE_ITEMS", "500"),
    ("AULOS_SNAPSHOT_GROUP_INLINE", "50"),
    // --- AULOS_*: queue ---
    ("AULOS_RESOLVE_CONCURRENCY", "4"),
    ("AULOS_RESOLVE_TIMEOUT_SECS", "120"),
    ("AULOS_RESOLVE_MAX_DEPTH", "3"),
    ("AULOS_RESOLVE_FALLTHROUGH", "true"),
    ("AULOS_SCHED_LOOKAHEAD", "32"),
    ("AULOS_MAX_BATCH_URLS", "500"),
    ("AULOS_DEDUPE_MODE", "active"),
    ("AULOS_JOB_STALL_SECS", "900"),
    ("AULOS_JOB_TIMEOUT_SECS", "0"),
    ("AULOS_KILL_GRACE_MS", "5000"),
    ("AULOS_AUTO_RETRY_MAX", "2"),
    ("AULOS_RESTART_POLICY", "resume"),
    ("AULOS_CLEAN_ORPHAN_TEMP", "false"),
    ("AULOS_ENTRY_MAX_BYTES", "262144"),
    ("AULOS_CUSTOM_DIRS_MAX_DEPTH", "8"),
    // --- AULOS_*: plugins and hooks ---
    ("AULOS_PLUGINS_DIR", "/config/plugins"),
    ("AULOS_PLUGINS_ENABLED", "true"),
    ("AULOS_PLUGIN_TIMEOUT_RESOLVE", "60"),
    ("AULOS_HOOKS_ENABLED", "true"),
    // --- AULOS_*: StreamingCommunity ---
    ("AULOS_SC_HTTP", "auto"),
    ("AULOS_SC_META_CONCURRENCY", "4"),
    ("AULOS_SC_EXTRA_HOSTS", ""),
    ("AULOS_SC_USE_OUTPUT_TEMPLATE", "false"),
    // --- AULOS_*: POT sidecar ---
    ("AULOS_POT_ENABLED", "true"),
    ("AULOS_POT_CMD", "bgutil-pot server"),
    ("AULOS_POT_URL", "http://127.0.0.1:4416"),
    ("AULOS_POT_MAX_RESTARTS", "10"),
    // --- AULOS_*: hooks ---
    ("AULOS_JELLYFIN_DEBOUNCE_SECS", "30"),
    ("AULOS_JELLYFIN_MAX_WAIT_SECS", "300"),
    ("AULOS_NFO_ENABLED", "true"),
    ("AULOS_NFO_DELETE_INFO_JSON", "false"),
    // --- AULOS_*: Telegram ---
    ("AULOS_TELEGRAM_BOARD", "board"),
    ("AULOS_TELEGRAM_EDIT_INTERVAL_MS", "3000"),
    ("AULOS_TELEGRAM_WATCH_ALL", "true"),
    // --- AULOS_*: subscriptions ---
    ("AULOS_SUB_CHECK_CONCURRENCY", "2"),
    ("AULOS_SUB_CHECK_TIMEOUT_SECS", "180"),
    ("AULOS_SUB_BACKOFF_MAX_SECS", "21600"),
    ("AULOS_SUB_FIRST_CHECK_DELAY_SECS", "10"),
    // --- AULOS_*: config watching and logging ---
    ("AULOS_CONFIG_DEBOUNCE_MS", "250"),
    ("AULOS_CONFIG_POLL_SECS", "30"),
    ("AULOS_LOG_FORMAT", "text"),
    // --- AULOS_*: API surface ---
    ("AULOS_V1_ENABLED", "true"),
    ("AULOS_API_TOKEN", ""),
    ("AULOS_TRUSTED_PROXY_AUTH_HEADER", ""),
    ("AULOS_ALLOW_PRIVATE_TARGETS", "true"),
    ("AULOS_METRICS_ENABLED", "false"),
    ("AULOS_SHUTDOWN_GRACE_SECS", "20"),
    ("AULOS_VERSION", "dev"),
    ("AULOS_IMPORT_ON_ERROR", "fail"),
    ("AULOS_V1_ADD_RESOLVE_WAIT_MS", "10000"),
    ("AULOS_V1_HISTORY_MAX", "0"),
];

/// Boolean keys: legacy's `_BOOLEAN` tuple plus the new `AULOS_*` booleans (DESIGN §17.1 step 3).
pub const BOOLEAN_KEYS: &[&str] = &[
    // legacy
    "DOWNLOAD_DIRS_INDEXABLE",
    "CUSTOM_DIRS",
    "CREATE_CUSTOM_DIRS",
    "DELETE_FILE_ON_TRASHCAN",
    "HTTPS",
    "ENABLE_ACCESSLOG",
    "ALLOW_YTDL_OPTIONS_OVERRIDES",
    "SC_USE_FFMPEG",
    "JELLYFIN_SYNC_ENABLED",
    "TELEGRAM_BOT_ENABLED",
    // new
    "AULOS_RESOLVE_FALLTHROUGH",
    "AULOS_CLEAN_ORPHAN_TEMP",
    "AULOS_PLUGINS_ENABLED",
    "AULOS_HOOKS_ENABLED",
    "AULOS_SC_USE_OUTPUT_TEMPLATE",
    "AULOS_POT_ENABLED",
    "AULOS_NFO_ENABLED",
    "AULOS_NFO_DELETE_INFO_JSON",
    "AULOS_TELEGRAM_WATCH_ALL",
    "AULOS_V1_ENABLED",
    "AULOS_ALLOW_PRIVATE_TARGETS",
    "AULOS_METRICS_ENABLED",
];

/// The exact accepted boolean token set (legacy `_BOOLEAN` validation).
pub const BOOLEAN_TOKENS: [&str; 8] = ["true", "false", "True", "False", "on", "off", "1", "0"];

/// The truthy subset of [`BOOLEAN_TOKENS`].
pub const TRUTHY_TOKENS: [&str; 4] = ["true", "True", "on", "1"];

/// `AULOS_*` names that are recognised and skipped rather than rejected (DESIGN §17.1).
///
/// `AULOS_E2E` is the `tests/e2e` harness marker of BRIEF §17, exported into the container by
/// `tests/e2e/run.sh`; a CI job that passes its whole environment would otherwise make the server
/// exit 2.
pub const ACCEPTED_IGNORED_KEYS: &[&str] = &["AULOS_E2E"];

/// `AULOS_*` prefixes that are recognised and skipped in full.
pub const ACCEPTED_IGNORED_PREFIXES: &[&str] = &["AULOS_E2E_"];

/// Keys whose value is a secret and is redacted everywhere (DESIGN §16.5).
pub const SECRET_KEYS: &[&str] = &["TELEGRAM_BOT_TOKEN", "JELLYFIN_API_KEY", "AULOS_API_TOKEN"];

/// Keys whose value is a JSON object of yt-dlp options, redacted entry by entry (DESIGN §16.5).
///
/// The value itself is not a secret — `format`, `username` and friends are exactly what an
/// operator wants to see in `check-config` — but individual entries (`password`, `proxy`,
/// `cookiefile`, …) are.
pub const JSON_OPTION_KEYS: &[&str] = &["YTDL_OPTIONS", "YTDL_OPTIONS_PRESETS"];

/// Redacts the secret-bearing entries of a JSON option object, leaving the rest legible.
///
/// `YTDL_OPTIONS_PRESETS` is a map of preset name → option object, so the walk recurses; the
/// key test is [`crate::error::is_secret_key`], the same one `GET /api/v2/debug/options` uses.
/// A value that is not valid JSON is redacted whole: `check-config` would otherwise print a
/// half-written options blob — the case most likely to still contain a password — verbatim.
fn redact_json_options(raw: &str) -> String {
    if raw.trim().is_empty() {
        return raw.to_owned();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return crate::error::REDACTED.to_owned();
    };
    redact_secret_entries(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| crate::error::REDACTED.to_owned())
}

/// Replaces every object value under a secret-looking key with [`crate::error::REDACTED`].
fn redact_secret_entries(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if crate::error::is_secret_key(key) {
                    *v = serde_json::Value::String(crate::error::REDACTED.to_owned());
                } else {
                    redact_secret_entries(v);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_secret_entries(item);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Small typed enums
// ---------------------------------------------------------------------------

/// Enum-valued settings share this parser, so every one of them gets the same error.
macro_rules! choice_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $key:literal, [ $( ($variant:ident, $text:literal) ),+ $(,)? ]
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum $name { $(
            #[doc = concat!("`", $text, "`")]
            $variant,
        )+ }

        impl $name {
            /// The accepted values, in declaration order.
            pub const CHOICES: &'static [&'static str] = &[$($text),+];

            /// The canonical string.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            /// Parses a value, case-insensitively.
            ///
            /// # Errors
            /// [`ConfigError::InvalidChoice`] naming every accepted value.
            pub fn parse(raw: &str) -> Result<Self, ConfigError> {
                $(if raw.eq_ignore_ascii_case($text) { return Ok(Self::$variant); })+
                Err(ConfigError::InvalidChoice {
                    key: $key,
                    value: raw.into(),
                    allowed: Self::CHOICES,
                })
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

choice_enum!(
    /// `DEFAULT_THEME`. Accepted and echoed in capabilities; no cookie is set (no web UI).
    Theme, "DEFAULT_THEME",
    [(Light, "light"), (Dark, "dark"), (Auto, "auto")]
);

choice_enum!(
    /// `AULOS_DB_SYNCHRONOUS`.
    DbSynchronous, "AULOS_DB_SYNCHRONOUS",
    [(Normal, "NORMAL"), (Full, "FULL")]
);

choice_enum!(
    /// `AULOS_LOG_FORMAT`.
    LogFormat, "AULOS_LOG_FORMAT",
    [(Text, "text"), (Json, "json")]
);

choice_enum!(
    /// `AULOS_DEDUPE_MODE` (DESIGN §8.5).
    DedupeMode, "AULOS_DEDUPE_MODE",
    [(Off, "off"), (Active, "active"), (Strict, "strict")]
);

choice_enum!(
    /// `AULOS_RESTART_POLICY` — what boot recovery does with in-flight items.
    RestartPolicy, "AULOS_RESTART_POLICY",
    [(Resume, "resume"), (Pause, "pause")]
);

choice_enum!(
    /// `AULOS_SC_HTTP` — which HTTP client the StreamingCommunity provider uses.
    ScHttpMode, "AULOS_SC_HTTP",
    [(Auto, "auto"), (Impersonate, "impersonate"), (Plain, "plain")]
);

choice_enum!(
    /// `AULOS_TELEGRAM_BOARD` — one live board per chat, or one message per job.
    TelegramBoard, "AULOS_TELEGRAM_BOARD",
    [(Board, "board"), (PerJob, "per_job")]
);

choice_enum!(
    /// `AULOS_IMPORT_ON_ERROR` — what a legacy *file* error does (DESIGN §7.6.1).
    ImportOnError, "AULOS_IMPORT_ON_ERROR",
    [(Fail, "fail"), (Skip, "skip")]
);

choice_enum!(
    /// `JELLYFIN_METADATA_REFRESH_MODE` / `JELLYFIN_IMAGE_REFRESH_MODE`.
    JellyfinRefreshMode, "JELLYFIN_REFRESH_MODE",
    [
        (None, "None"),
        (ValidationOnly, "ValidationOnly"),
        (Default, "Default"),
        (FullRefresh, "FullRefresh"),
    ]
);

/// `CORS_ALLOWED_ORIGINS`: empty = none, `*` = any, otherwise a comma list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CorsOrigins {
    /// No `Access-Control-Allow-Origin` is ever sent.
    None,
    /// Any origin is echoed back.
    Any,
    /// Only these origins are echoed back.
    List(Vec<Box<str>>),
}

impl CorsOrigins {
    /// Parses the comma list.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let items: Vec<&str> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if items.is_empty() {
            Self::None
        } else if items.contains(&"*") {
            Self::Any
        } else {
            Self::List(items.into_iter().map(Into::into).collect())
        }
    }

    /// Whether `origin` may be echoed back.
    #[must_use]
    pub fn allows(&self, origin: &str) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::List(l) => l.iter().any(|o| &**o == origin),
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The effective, typed configuration.
///
/// Secrets are held in [`Redact`], so `{:?}` on this struct is safe to log.
#[derive(Clone, Debug)]
pub struct Config {
    // --- paths and file serving ---
    /// `DOWNLOAD_DIR`, `AUDIO_DOWNLOAD_DIR`, `TEMP_DIR`, `STATE_DIR`.
    pub paths: Paths,
    /// `DOWNLOAD_DIRS_INDEXABLE` — a JSON listing on the file routes, not HTML.
    pub download_dirs_indexable: bool,
    /// `CUSTOM_DIRS` — allows `folder`; gates `api/v2/custom-dirs`.
    pub custom_dirs: bool,
    /// `CREATE_CUSTOM_DIRS` — `create_dir_all` a missing `folder` instead of erroring.
    pub create_custom_dirs: bool,
    /// `CUSTOM_DIRS_EXCLUDE_REGEX`. `None` when empty. An invalid regex is fatal at boot.
    pub custom_dirs_exclude_regex: Option<Regex>,
    /// `DELETE_FILE_ON_TRASHCAN` — also deletes chapter/subtitle/`.info.json`/`.nfo` siblings.
    pub delete_file_on_trashcan: bool,
    /// `URL_PREFIX`, normalised. The only thing allowed to build a path.
    pub url_prefix: Prefix,
    /// `PUBLIC_HOST_URL`, with a trailing `/` when non-empty.
    pub public_host_url: Box<str>,
    /// `PUBLIC_HOST_AUDIO_URL`, with a trailing `/` when non-empty.
    pub public_host_audio_url: Box<str>,

    // --- naming ---
    /// `OUTPUT_TEMPLATE` — yt-dlp `outtmpl.default`.
    pub output_template: Box<str>,
    /// `OUTPUT_TEMPLATE_CHAPTER` — `outtmpl.chapter`, and the default request `chapter_template`.
    pub output_template_chapter: Box<str>,
    /// `OUTPUT_TEMPLATE_PLAYLIST`. Empty keeps the default.
    pub output_template_playlist: Box<str>,
    /// `OUTPUT_TEMPLATE_CHANNEL`. Empty keeps the default.
    pub output_template_channel: Box<str>,

    // --- request and subscription defaults ---
    /// `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT`.
    pub default_option_playlist_item_limit: u32,
    /// The raw string, so the v1 shim can echo it as legacy did (a string, never coerced).
    pub default_option_playlist_item_limit_raw: Box<str>,
    /// `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL`, in minutes.
    pub subscription_default_check_interval: u32,
    /// The raw string, for the same reason.
    pub subscription_default_check_interval_raw: Box<str>,
    /// `SUBSCRIPTION_SCAN_PLAYLIST_END`.
    pub subscription_scan_playlist_end: u32,
    /// `SUBSCRIPTION_MAX_SEEN_IDS`.
    pub subscription_max_seen_ids: u32,
    /// `CLEAR_COMPLETED_AFTER`, in seconds. `0` = never. Invalid parses to `0` with a warning.
    pub clear_completed_after: u64,

    // --- yt-dlp options ---
    /// `YTDL_OPTIONS`, still raw JSON text.
    pub ytdl_options: Box<str>,
    /// `YTDL_OPTIONS_FILE`, absolutised. `None` when empty.
    pub ytdl_options_file: Option<PathBuf>,
    /// `YTDL_OPTIONS_PRESETS`, still raw JSON text.
    pub ytdl_options_presets: Box<str>,
    /// `YTDL_OPTIONS_PRESETS_FILE`, absolutised. `None` when empty.
    pub ytdl_options_presets_file: Option<PathBuf>,
    /// `ALLOW_YTDL_OPTIONS_OVERRIDES`.
    pub allow_ytdl_options_overrides: bool,

    // --- server ---
    /// `CORS_ALLOWED_ORIGINS`.
    pub cors_allowed_origins: CorsOrigins,
    /// `ROBOTS_TXT`, resolved under `BASE_DIR`. `None` when empty.
    pub robots_txt: Option<PathBuf>,
    /// `HOST`.
    pub host: Box<str>,
    /// `PORT`. Invalid is fatal.
    pub port: u16,
    /// `HTTPS`.
    pub https: bool,
    /// `CERTFILE`. `None` when empty.
    pub certfile: Option<PathBuf>,
    /// `KEYFILE`. `None` when empty.
    pub keyfile: Option<PathBuf>,
    /// `BASE_DIR` — now used **only** to resolve `ROBOTS_TXT`.
    pub base_dir: Option<PathBuf>,
    /// `DEFAULT_THEME`.
    pub default_theme: Theme,
    /// `MAX_CONCURRENT_DOWNLOADS`, at least 1. Invalid is fatal.
    pub max_concurrent_downloads: u32,
    /// `LOGLEVEL`. An unknown value becomes `INFO` with a warning.
    pub loglevel: Box<str>,
    /// `ENABLE_ACCESSLOG` — request span at INFO versus DEBUG.
    pub enable_accesslog: bool,

    // --- StreamingCommunity ---
    /// `SC_THREAD_COUNT` — `N_m3u8DL-RE --thread-count`.
    pub sc_thread_count: u32,
    /// `SC_USE_FFMPEG` — force ffmpeg for StreamingCommunity.
    pub sc_use_ffmpeg: bool,
    /// `SC_MAX_CONCURRENT_DOWNLOADS`, at least 1. Acquired **instead of** a global slot.
    pub sc_max_concurrent_downloads: u32,
    /// `AULOS_SC_HTTP`.
    pub sc_http: ScHttpMode,
    /// `AULOS_SC_META_CONCURRENCY`.
    pub sc_meta_concurrency: u32,
    /// `AULOS_SC_EXTRA_HOSTS` — additional mirror host substrings.
    pub sc_extra_hosts: Vec<Box<str>>,
    /// `AULOS_SC_USE_OUTPUT_TEMPLATE`.
    pub sc_use_output_template: bool,

    // --- Jellyfin ---
    /// `JELLYFIN_SYNC_ENABLED` — arms the jellyfin hook.
    pub jellyfin_sync_enabled: bool,
    /// `JELLYFIN_URL`, trailing `/` stripped.
    pub jellyfin_url: Box<str>,
    /// `JELLYFIN_API_KEY`.
    pub jellyfin_api_key: Redact<String>,
    /// `JELLYFIN_SYNC_TIMEOUT_SECONDS`. Invalid warns and falls back to 20.
    pub jellyfin_sync_timeout_seconds: f64,
    /// `JELLYFIN_LIBRARY_ID` — empty means refresh all libraries.
    pub jellyfin_library_id: Box<str>,
    /// `JELLYFIN_METADATA_REFRESH_MODE`.
    pub jellyfin_metadata_refresh_mode: JellyfinRefreshMode,
    /// `JELLYFIN_IMAGE_REFRESH_MODE`.
    pub jellyfin_image_refresh_mode: JellyfinRefreshMode,
    /// `AULOS_JELLYFIN_DEBOUNCE_SECS`.
    pub jellyfin_debounce_secs: u64,
    /// `AULOS_JELLYFIN_MAX_WAIT_SECS` — the debounce cap.
    pub jellyfin_max_wait_secs: u64,

    // --- Telegram ---
    /// `TELEGRAM_BOT_ENABLED`.
    pub telegram_bot_enabled: bool,
    /// `TELEGRAM_BOT_TOKEN`. Empty means the bot logs an error and does not start.
    pub telegram_bot_token: Redact<String>,
    /// `TELEGRAM_ALLOWED_CHAT_IDS`. Empty means the bot refuses to start (kept from legacy).
    pub telegram_allowed_chat_ids: Vec<i64>,
    /// `TELEGRAM_STALL_TIMEOUT_SECONDS`.
    pub telegram_stall_timeout_seconds: u64,
    /// `TELEGRAM_HARD_TIMEOUT_SECONDS`.
    pub telegram_hard_timeout_seconds: u64,
    /// `TELEGRAM_MAX_URLS_PER_MESSAGE`.
    pub telegram_max_urls_per_message: u32,
    /// `AULOS_TELEGRAM_BOARD`.
    pub telegram_board: TelegramBoard,
    /// `AULOS_TELEGRAM_EDIT_INTERVAL_MS` — the per-chat edit budget.
    pub telegram_edit_interval_ms: u64,
    /// `AULOS_TELEGRAM_WATCH_ALL` — report web and subscription jobs too. Default `true`: the
    /// legacy blind spot is a bug, not a feature (BRIEF scope trims).
    pub telegram_watch_all: bool,

    // --- identity ---
    /// `METUBE_VERSION`, with `AULOS_VERSION` as an accepted alias.
    pub version: Box<str>,

    // --- storage ---
    /// `AULOS_DB_PATH`. Defaults to `<STATE_DIR>/aulos.db` so cutover needs no compose change.
    pub db_path: PathBuf,
    /// `AULOS_DB_READERS`.
    pub db_readers: u32,
    /// `AULOS_DB_FLUSH_MS` — the write-batching window.
    pub db_flush_ms: u64,
    /// `AULOS_DB_SYNCHRONOUS`.
    pub db_synchronous: DbSynchronous,

    // --- realtime ---
    /// `AULOS_WS_BATCH_MS`, 50..=5000.
    pub ws_batch_ms: u64,
    /// `AULOS_WS_URGENT_MS`, 0..=1000.
    pub ws_urgent_ms: u64,
    /// `AULOS_WS_MAX_DELTAS_PER_FRAME`.
    pub ws_max_deltas_per_frame: u32,
    /// `AULOS_WS_REPLAY_FRAMES`.
    pub ws_replay_frames: u32,
    /// `AULOS_WS_REPLAY_BYTES`.
    pub ws_replay_bytes: u64,
    /// `AULOS_WS_MAX_CLIENTS`.
    pub ws_max_clients: u32,
    /// `AULOS_WS_SEND_TIMEOUT_MS` — close a wedged socket with 1013.
    pub ws_send_timeout_ms: u64,
    /// `AULOS_MEM_DONE_ITEMS` — the in-memory completed window.
    pub mem_done_items: u32,
    /// `AULOS_SNAPSHOT_GROUP_INLINE`.
    ///
    /// v1.0: not implemented, see BRIEF — the snapshot always carries every non-terminal child.
    /// Parsed so a compose file that sets it does not fail the boot.
    pub snapshot_group_inline: u32,

    // --- queue ---
    /// `AULOS_RESOLVE_CONCURRENCY`.
    pub resolve_concurrency: u32,
    /// `AULOS_RESOLVE_TIMEOUT_SECS`.
    pub resolve_timeout_secs: u64,
    /// `AULOS_RESOLVE_MAX_DEPTH`.
    pub resolve_max_depth: u32,
    /// `AULOS_RESOLVE_FALLTHROUGH` (DESIGN §6.4).
    pub resolve_fallthrough: bool,
    /// `AULOS_SCHED_LOOKAHEAD`.
    pub sched_lookahead: u32,
    /// `AULOS_MAX_BATCH_URLS`.
    pub max_batch_urls: u32,
    /// `AULOS_DEDUPE_MODE`.
    pub dedupe_mode: DedupeMode,
    /// `AULOS_JOB_STALL_SECS`. `0` = off.
    pub job_stall_secs: u64,
    /// `AULOS_JOB_TIMEOUT_SECS`. `0` = off.
    pub job_timeout_secs: u64,
    /// `AULOS_KILL_GRACE_MS` — SIGTERM to SIGKILL grace for a process group.
    pub kill_grace_ms: u64,
    /// `AULOS_AUTO_RETRY_MAX`. `0` = off.
    pub auto_retry_max: u32,
    /// `AULOS_RESTART_POLICY`.
    pub restart_policy: RestartPolicy,
    /// `AULOS_CLEAN_ORPHAN_TEMP`.
    pub clean_orphan_temp: bool,
    /// `AULOS_ENTRY_MAX_BYTES` — the entry-blob hard cap.
    pub entry_max_bytes: u64,
    /// `AULOS_CUSTOM_DIRS_MAX_DEPTH`.
    pub custom_dirs_max_depth: u32,

    // --- plugins and hooks ---
    /// `AULOS_PLUGINS_DIR`, defaulting to `PLUGINS_DIR`.
    pub plugins_dir: PathBuf,
    /// `AULOS_PLUGINS_ENABLED`.
    pub plugins_enabled: bool,
    /// `AULOS_PLUGIN_TIMEOUT_RESOLVE`.
    pub plugin_timeout_resolve: u64,
    /// `AULOS_HOOKS_ENABLED`.
    pub hooks_enabled: bool,
    /// `AULOS_NFO_ENABLED`.
    pub nfo_enabled: bool,
    /// `AULOS_NFO_DELETE_INFO_JSON`.
    pub nfo_delete_info_json: bool,

    // --- POT sidecar ---
    /// `AULOS_POT_ENABLED`.
    pub pot_enabled: bool,
    /// `AULOS_POT_CMD`, as an argv.
    pub pot_cmd: Vec<Box<str>>,
    /// `AULOS_POT_URL` — the health probe target.
    pub pot_url: Box<str>,
    /// `AULOS_POT_MAX_RESTARTS` per 10 minutes.
    pub pot_max_restarts: u32,

    // --- subscriptions ---
    /// `AULOS_SUB_CHECK_CONCURRENCY`.
    pub sub_check_concurrency: u32,
    /// `AULOS_SUB_CHECK_TIMEOUT_SECS`.
    pub sub_check_timeout_secs: u64,
    /// `AULOS_SUB_BACKOFF_MAX_SECS`.
    pub sub_backoff_max_secs: u64,
    /// `AULOS_SUB_FIRST_CHECK_DELAY_SECS`.
    pub sub_first_check_delay_secs: u64,

    // --- config watching and logging ---
    /// `AULOS_CONFIG_DEBOUNCE_MS`.
    pub config_debounce_ms: u64,
    /// `AULOS_CONFIG_POLL_SECS`. `0` = off.
    pub config_poll_secs: u64,
    /// `AULOS_LOG_FORMAT`.
    pub log_format: LogFormat,

    // --- API surface ---
    /// `AULOS_V1_ENABLED`.
    pub v1_enabled: bool,
    /// `AULOS_API_TOKEN`. Empty = disabled.
    pub api_token: Redact<String>,
    /// `AULOS_TRUSTED_PROXY_AUTH_HEADER`. Empty = no proxy-auth requirement.
    pub trusted_proxy_auth_header: Box<str>,
    /// `AULOS_ALLOW_PRIVATE_TARGETS` — the SSRF guard for API adds. Telegram is always guarded.
    pub allow_private_targets: bool,
    /// `AULOS_METRICS_ENABLED`.
    ///
    /// v1.0: not implemented, see BRIEF — the Prometheus endpoint is CUT. Parsed so an existing
    /// compose file that sets it still boots.
    pub metrics_enabled: bool,
    /// `AULOS_SHUTDOWN_GRACE_SECS`.
    pub shutdown_grace_secs: u64,
    /// `AULOS_IMPORT_ON_ERROR`.
    pub import_on_error: ImportOnError,
    /// `AULOS_V1_ADD_RESOLVE_WAIT_MS`. `0` = fully async v1 add.
    pub v1_add_resolve_wait_ms: u64,
    /// `AULOS_V1_HISTORY_MAX`. `0` = unlimited, reproducing legacy exactly.
    pub v1_history_max: u32,
}

impl Config {
    /// The effective `chapter_template` a request with none of its own gets.
    #[must_use]
    pub fn default_chapter_template(&self) -> &str {
        &self.output_template_chapter
    }

    /// The `PUBLIC_HOST_*` prefix for a download type.
    #[must_use]
    pub fn public_host_prefix(&self, download_type: crate::selection::DownloadType) -> &str {
        match download_type {
            crate::selection::DownloadType::Audio => &self.public_host_audio_url,
            _ => &self.public_host_url,
        }
    }
}

/// Loads and types the whole §17.3 table, collecting **every** error.
///
/// Steps, per DESIGN §17.1: defaults overlaid with the environment (1), `%%` indirection with a
/// cycle check (2), the exact boolean token set (3), `URL_PREFIX` normalisation with a WARN for a
/// missing leading `/` (4), `PUBLIC_HOST_*` trailing slashes (5), `.`-relative option paths
/// absolutised (6), numbers with per-key leniency (7), and unknown-`AULOS_*` rejection.
///
/// Reading `YTDL_OPTIONS_FILE` (step 8) is deliberately **not** here: keeping [`load`] IO-free is
/// what makes the whole table testable without a filesystem. The caller runs
/// [`crate::ytdl_options::YtdlOptions::load`] next and adds its error to the same report.
///
/// # Errors
/// Every failure, so the binary can print one table and exit 2 rather than fixing typos one boot
/// at a time (DESIGN §17.1 step 9).
pub fn load(env: &RawEnv) -> Result<Config, Vec<ConfigError>> {
    let (result, warnings) = load_inner(env);
    for w in &warnings {
        tracing::warn!("{w}");
    }
    result
}

/// Loads the configuration and returns the non-fatal warnings alongside it.
///
/// `aulos-server check-config` renders them in its table; [`load`] logs them at WARN.
///
/// # Errors
/// The same report as [`load`].
pub fn load_with_warnings(env: &RawEnv) -> Result<(Config, Vec<ConfigWarning>), Vec<ConfigError>> {
    let (result, warnings) = load_inner(env);
    result.map(|cfg| (cfg, warnings))
}

/// The single pass that produces both halves of the report.
#[allow(clippy::too_many_lines)] // one arm per env var; splitting it would only hide the table
fn load_inner(env: &RawEnv) -> (Result<Config, Vec<ConfigError>>, Vec<ConfigWarning>) {
    let mut errs: Vec<ConfigError> = Vec::new();
    let mut warnings: Vec<ConfigWarning> = Vec::new();

    // Step: unknown `AULOS_*` rejection. Done first so a typo is reported even if a later value
    // also fails; unknown non-`AULOS_` variables are ignored (a container inherits a lot).
    let known: BTreeSet<&str> = DEFAULTS.iter().map(|(k, _)| *k).collect();
    for key in env.keys() {
        if !key.starts_with("AULOS_")
            || known.contains(key)
            || ACCEPTED_IGNORED_KEYS.contains(&key)
            || ACCEPTED_IGNORED_PREFIXES.iter().any(|p| key.starts_with(p))
        {
            continue;
        }
        errs.push(ConfigError::UnknownVariable { key: key.into() });
    }

    // Steps 1–2.
    let mut map = env.merged();
    if let Err(mut e) = resolve_indirection(&mut map) {
        errs.append(&mut e);
        return (Err(errs), warnings);
    }

    let mut g = Getter {
        map: &map,
        errs: &mut errs,
        warnings: &mut warnings,
    };

    // Step 3 (booleans, validated against the exact token set) happens inline below: every key in
    // `BOOLEAN_KEYS` is read exactly once by `g.bool` while the struct is built, and `Getter::bool`
    // reports `InvalidBoolean` there. A separate validation pre-pass would report one typo twice —
    // `every_boolean_key_is_reported_exactly_once` pins both halves of that.

    // Step 4.
    let (url_prefix, fixups) = Prefix::normalize(g.str("URL_PREFIX"));
    if fixups.contains(&PrefixFixup::AddedLeadingSlash) {
        g.warnings.push(ConfigWarning::PrefixLeadingSlash {
            normalised: url_prefix.as_str().into(),
        });
    }

    // Step 7 and the rest.
    let download = PathBuf::from(g.str("DOWNLOAD_DIR"));
    let state = PathBuf::from(g.str("STATE_DIR"));
    let paths = Paths {
        download: download.clone(),
        audio_download: PathBuf::from(g.str("AUDIO_DOWNLOAD_DIR")),
        temp: PathBuf::from(g.str("TEMP_DIR")),
        state: state.clone(),
    };

    let base_dir = g.opt_path("BASE_DIR");
    let db_path_raw = g.str("AULOS_DB_PATH");
    let db_path = if db_path_raw.is_empty() {
        state.join("aulos.db")
    } else {
        PathBuf::from(db_path_raw)
    };

    let cfg = Config {
        paths,
        download_dirs_indexable: g.bool("DOWNLOAD_DIRS_INDEXABLE"),
        custom_dirs: g.bool("CUSTOM_DIRS"),
        create_custom_dirs: g.bool("CREATE_CUSTOM_DIRS"),
        custom_dirs_exclude_regex: g.opt_regex("CUSTOM_DIRS_EXCLUDE_REGEX"),
        delete_file_on_trashcan: g.bool("DELETE_FILE_ON_TRASHCAN"),
        url_prefix,
        public_host_url: with_trailing_slash(g.str("PUBLIC_HOST_URL")),
        public_host_audio_url: with_trailing_slash(g.str("PUBLIC_HOST_AUDIO_URL")),

        output_template: g.str("OUTPUT_TEMPLATE").into(),
        output_template_chapter: g.str("OUTPUT_TEMPLATE_CHAPTER").into(),
        output_template_playlist: g.str("OUTPUT_TEMPLATE_PLAYLIST").into(),
        output_template_channel: g.str("OUTPUT_TEMPLATE_CHANNEL").into(),

        default_option_playlist_item_limit: g.u32("DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT"),
        default_option_playlist_item_limit_raw: g.str("DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT").into(),
        subscription_default_check_interval: g.u32("SUBSCRIPTION_DEFAULT_CHECK_INTERVAL"),
        subscription_default_check_interval_raw: g
            .str("SUBSCRIPTION_DEFAULT_CHECK_INTERVAL")
            .into(),
        subscription_scan_playlist_end: g.u32("SUBSCRIPTION_SCAN_PLAYLIST_END"),
        subscription_max_seen_ids: g.u32("SUBSCRIPTION_MAX_SEEN_IDS"),
        clear_completed_after: g.lenient_u64("CLEAR_COMPLETED_AFTER", 0),

        ytdl_options: g.str("YTDL_OPTIONS").into(),
        ytdl_options_file: g.opt_dot_relative_path("YTDL_OPTIONS_FILE"),
        ytdl_options_presets: g.str("YTDL_OPTIONS_PRESETS").into(),
        ytdl_options_presets_file: g.opt_dot_relative_path("YTDL_OPTIONS_PRESETS_FILE"),
        allow_ytdl_options_overrides: g.bool("ALLOW_YTDL_OPTIONS_OVERRIDES"),

        cors_allowed_origins: CorsOrigins::parse(g.str("CORS_ALLOWED_ORIGINS")),
        robots_txt: g
            .opt_path("ROBOTS_TXT")
            .map(|p| resolve_under(base_dir.as_deref(), &p)),
        host: g.str("HOST").into(),
        port: g.fatal_u16("PORT"),
        https: g.bool("HTTPS"),
        certfile: g.opt_path("CERTFILE"),
        keyfile: g.opt_path("KEYFILE"),
        base_dir,
        default_theme: g.choice("DEFAULT_THEME", Theme::parse, Theme::Auto),
        max_concurrent_downloads: g.fatal_u32_min("MAX_CONCURRENT_DOWNLOADS", 1),
        loglevel: g.loglevel(),
        enable_accesslog: g.bool("ENABLE_ACCESSLOG"),

        sc_thread_count: g.u32("SC_THREAD_COUNT"),
        sc_use_ffmpeg: g.bool("SC_USE_FFMPEG"),
        sc_max_concurrent_downloads: g.fatal_u32_min("SC_MAX_CONCURRENT_DOWNLOADS", 1),
        sc_http: g.choice("AULOS_SC_HTTP", ScHttpMode::parse, ScHttpMode::Auto),
        sc_meta_concurrency: g.u32("AULOS_SC_META_CONCURRENCY"),
        sc_extra_hosts: comma_list(g.str("AULOS_SC_EXTRA_HOSTS")),
        sc_use_output_template: g.bool("AULOS_SC_USE_OUTPUT_TEMPLATE"),

        jellyfin_sync_enabled: g.bool("JELLYFIN_SYNC_ENABLED"),
        jellyfin_url: g.str("JELLYFIN_URL").trim_end_matches('/').into(),
        jellyfin_api_key: Redact::new(g.str("JELLYFIN_API_KEY").to_owned()),
        jellyfin_sync_timeout_seconds: g.lenient_f64("JELLYFIN_SYNC_TIMEOUT_SECONDS", 20.0),
        jellyfin_library_id: g.str("JELLYFIN_LIBRARY_ID").into(),
        jellyfin_metadata_refresh_mode: g.choice(
            "JELLYFIN_METADATA_REFRESH_MODE",
            JellyfinRefreshMode::parse,
            JellyfinRefreshMode::Default,
        ),
        jellyfin_image_refresh_mode: g.choice(
            "JELLYFIN_IMAGE_REFRESH_MODE",
            JellyfinRefreshMode::parse,
            JellyfinRefreshMode::Default,
        ),
        jellyfin_debounce_secs: g.u64("AULOS_JELLYFIN_DEBOUNCE_SECS"),
        jellyfin_max_wait_secs: g.u64("AULOS_JELLYFIN_MAX_WAIT_SECS"),

        telegram_bot_enabled: g.bool("TELEGRAM_BOT_ENABLED"),
        telegram_bot_token: Redact::new(g.str("TELEGRAM_BOT_TOKEN").to_owned()),
        telegram_allowed_chat_ids: g.chat_ids("TELEGRAM_ALLOWED_CHAT_IDS"),
        telegram_stall_timeout_seconds: g.u64("TELEGRAM_STALL_TIMEOUT_SECONDS"),
        telegram_hard_timeout_seconds: g.u64("TELEGRAM_HARD_TIMEOUT_SECONDS"),
        telegram_max_urls_per_message: g.u32("TELEGRAM_MAX_URLS_PER_MESSAGE"),
        telegram_board: g.choice(
            "AULOS_TELEGRAM_BOARD",
            TelegramBoard::parse,
            TelegramBoard::Board,
        ),
        telegram_edit_interval_ms: g.u64("AULOS_TELEGRAM_EDIT_INTERVAL_MS"),
        telegram_watch_all: g.bool("AULOS_TELEGRAM_WATCH_ALL"),

        version: g.str("METUBE_VERSION").into(),

        db_path,
        db_readers: g.u32("AULOS_DB_READERS"),
        db_flush_ms: g.u64("AULOS_DB_FLUSH_MS"),
        db_synchronous: g.choice(
            "AULOS_DB_SYNCHRONOUS",
            DbSynchronous::parse,
            DbSynchronous::Normal,
        ),

        ws_batch_ms: g.ranged_u64("AULOS_WS_BATCH_MS", 50, 5_000),
        ws_urgent_ms: g.ranged_u64("AULOS_WS_URGENT_MS", 0, 1_000),
        ws_max_deltas_per_frame: g.u32("AULOS_WS_MAX_DELTAS_PER_FRAME"),
        ws_replay_frames: g.u32("AULOS_WS_REPLAY_FRAMES"),
        ws_replay_bytes: g.u64("AULOS_WS_REPLAY_BYTES"),
        ws_max_clients: g.u32("AULOS_WS_MAX_CLIENTS"),
        ws_send_timeout_ms: g.u64("AULOS_WS_SEND_TIMEOUT_MS"),
        mem_done_items: g.u32("AULOS_MEM_DONE_ITEMS"),
        snapshot_group_inline: g.u32("AULOS_SNAPSHOT_GROUP_INLINE"),

        resolve_concurrency: g.u32("AULOS_RESOLVE_CONCURRENCY"),
        resolve_timeout_secs: g.u64("AULOS_RESOLVE_TIMEOUT_SECS"),
        resolve_max_depth: g.u32("AULOS_RESOLVE_MAX_DEPTH"),
        resolve_fallthrough: g.bool("AULOS_RESOLVE_FALLTHROUGH"),
        sched_lookahead: g.u32("AULOS_SCHED_LOOKAHEAD"),
        max_batch_urls: g.u32("AULOS_MAX_BATCH_URLS"),
        dedupe_mode: g.choice("AULOS_DEDUPE_MODE", DedupeMode::parse, DedupeMode::Active),
        job_stall_secs: g.u64("AULOS_JOB_STALL_SECS"),
        job_timeout_secs: g.u64("AULOS_JOB_TIMEOUT_SECS"),
        kill_grace_ms: g.u64("AULOS_KILL_GRACE_MS"),
        auto_retry_max: g.u32("AULOS_AUTO_RETRY_MAX"),
        restart_policy: g.choice(
            "AULOS_RESTART_POLICY",
            RestartPolicy::parse,
            RestartPolicy::Resume,
        ),
        clean_orphan_temp: g.bool("AULOS_CLEAN_ORPHAN_TEMP"),
        entry_max_bytes: g.u64("AULOS_ENTRY_MAX_BYTES"),
        custom_dirs_max_depth: g.u32("AULOS_CUSTOM_DIRS_MAX_DEPTH"),

        plugins_dir: PathBuf::from(g.str("AULOS_PLUGINS_DIR")),
        plugins_enabled: g.bool("AULOS_PLUGINS_ENABLED"),
        plugin_timeout_resolve: g.u64("AULOS_PLUGIN_TIMEOUT_RESOLVE"),
        hooks_enabled: g.bool("AULOS_HOOKS_ENABLED"),
        nfo_enabled: g.bool("AULOS_NFO_ENABLED"),
        nfo_delete_info_json: g.bool("AULOS_NFO_DELETE_INFO_JSON"),

        pot_enabled: g.bool("AULOS_POT_ENABLED"),
        pot_cmd: g
            .str("AULOS_POT_CMD")
            .split_whitespace()
            .map(Into::into)
            .collect(),
        pot_url: g.str("AULOS_POT_URL").trim_end_matches('/').into(),
        pot_max_restarts: g.u32("AULOS_POT_MAX_RESTARTS"),

        sub_check_concurrency: g.u32("AULOS_SUB_CHECK_CONCURRENCY"),
        sub_check_timeout_secs: g.u64("AULOS_SUB_CHECK_TIMEOUT_SECS"),
        sub_backoff_max_secs: g.u64("AULOS_SUB_BACKOFF_MAX_SECS"),
        sub_first_check_delay_secs: g.u64("AULOS_SUB_FIRST_CHECK_DELAY_SECS"),

        config_debounce_ms: g.u64("AULOS_CONFIG_DEBOUNCE_MS"),
        config_poll_secs: g.u64("AULOS_CONFIG_POLL_SECS"),
        log_format: g.choice("AULOS_LOG_FORMAT", LogFormat::parse, LogFormat::Text),

        v1_enabled: g.bool("AULOS_V1_ENABLED"),
        api_token: Redact::new(g.str("AULOS_API_TOKEN").to_owned()),
        trusted_proxy_auth_header: g.str("AULOS_TRUSTED_PROXY_AUTH_HEADER").into(),
        allow_private_targets: g.bool("AULOS_ALLOW_PRIVATE_TARGETS"),
        metrics_enabled: g.bool("AULOS_METRICS_ENABLED"),
        shutdown_grace_secs: g.u64("AULOS_SHUTDOWN_GRACE_SECS"),
        import_on_error: g.choice(
            "AULOS_IMPORT_ON_ERROR",
            ImportOnError::parse,
            ImportOnError::Fail,
        ),
        v1_add_resolve_wait_ms: g.u64("AULOS_V1_ADD_RESOLVE_WAIT_MS"),
        v1_history_max: g.u32("AULOS_V1_HISTORY_MAX"),
    };

    let result = if errs.is_empty() { Ok(cfg) } else { Err(errs) };
    (result, warnings)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Resolves `%%KEY` indirection iteratively, with a cycle check (DESIGN §17.1 step 2).
fn resolve_indirection(map: &mut BTreeMap<String, String>) -> Result<(), Vec<ConfigError>> {
    let mut errs = Vec::new();
    let keys: Vec<String> = map.keys().cloned().collect();

    for key in keys {
        let mut seen: Vec<String> = vec![key.clone()];
        let mut current = key.clone();

        while let Some(value) = map.get(&current).cloned() {
            let Some(target) = value.strip_prefix("%%") else {
                // Resolved: copy the terminal value back onto the original key.
                if current != key {
                    map.insert(key.clone(), value);
                }
                break;
            };
            let target = target.to_owned();

            if !map.contains_key(&target) {
                errs.push(ConfigError::UnknownIndirection {
                    key: key.clone().into_boxed_str(),
                    target: target.into_boxed_str(),
                });
                break;
            }
            if seen.contains(&target) {
                errs.push(ConfigError::IndirectionCycle {
                    chain: {
                        seen.push(target);
                        seen.join(" -> ").into_boxed_str()
                    },
                });
                break;
            }
            seen.push(target.clone());
            current = target;
        }
    }

    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

/// `val + '/'` when `val` is non-empty and unterminated (DESIGN §17.1 step 5).
fn with_trailing_slash(raw: &str) -> Box<str> {
    if raw.is_empty() || raw.ends_with('/') {
        raw.into()
    } else {
        format!("{raw}/").into_boxed_str()
    }
}

/// A comma list, trimmed, empties dropped.
fn comma_list(raw: &str) -> Vec<Box<str>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Into::into)
        .collect()
}

/// Python's `Path(p).resolve()` on a possibly non-existent path: absolutise against the current
/// directory and normalise `.`/`..` **lexically**, with no filesystem access.
fn absolutise(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `ROBOTS_TXT` is resolved under `BASE_DIR` when it is relative.
fn resolve_under(base: Option<&Path>, path: &Path) -> PathBuf {
    match base {
        Some(b) if path.is_relative() => b.join(path),
        _ => path.to_path_buf(),
    }
}

/// Typed accessors over the merged string map, accumulating errors and warnings.
struct Getter<'a> {
    map: &'a BTreeMap<String, String>,
    errs: &'a mut Vec<ConfigError>,
    warnings: &'a mut Vec<ConfigWarning>,
}

impl Getter<'_> {
    /// The raw value. A key not in `DEFAULTS` is a programmer error, not a config error, so this
    /// returns `""` rather than failing the boot.
    fn str(&self, key: &'static str) -> &str {
        self.map.get(key).map_or("", String::as_str)
    }

    fn bool(&mut self, key: &'static str) -> bool {
        let raw = self.str(key);
        if !BOOLEAN_TOKENS.contains(&raw) {
            self.errs.push(ConfigError::InvalidBoolean {
                key,
                value: raw.into(),
            });
            return false;
        }
        TRUTHY_TOKENS.contains(&raw)
    }

    fn u32(&mut self, key: &'static str) -> u32 {
        self.parse_num(key, 0)
    }

    fn u64(&mut self, key: &'static str) -> u64 {
        self.parse_num(key, 0)
    }

    fn parse_num<T: std::str::FromStr>(&mut self, key: &'static str, fallback: T) -> T {
        let raw: Box<str> = self.str(key).into();
        match raw.trim().parse::<T>() {
            Ok(v) => v,
            Err(_) => {
                self.errs
                    .push(ConfigError::InvalidNumber { key, value: raw });
                fallback
            }
        }
    }

    /// `PORT` — invalid is fatal, because legacy crashed on it anyway.
    fn fatal_u16(&mut self, key: &'static str) -> u16 {
        self.parse_num(key, 0)
    }

    /// `MAX_CONCURRENT_DOWNLOADS` / `SC_MAX_CONCURRENT_DOWNLOADS` — fatal, and at least `min`.
    fn fatal_u32_min(&mut self, key: &'static str, min: u32) -> u32 {
        let v = self.parse_num::<u32>(key, min);
        if v < min {
            self.errs.push(ConfigError::OutOfRange {
                key,
                value: self.str(key).into(),
                min: i64::from(min),
                max: i64::from(u32::MAX),
            });
            return min;
        }
        v
    }

    fn ranged_u64(&mut self, key: &'static str, min: u64, max: u64) -> u64 {
        let v = self.parse_num::<u64>(key, min);
        if v < min || v > max {
            self.errs.push(ConfigError::OutOfRange {
                key,
                value: self.str(key).into(),
                min: i64::try_from(min).unwrap_or(i64::MAX),
                max: i64::try_from(max).unwrap_or(i64::MAX),
            });
            return v.clamp(min, max);
        }
        v
    }

    /// `CLEAR_COMPLETED_AFTER`: an invalid value logs and becomes the fallback (legacy parity).
    fn lenient_u64(&mut self, key: &'static str, fallback: u64) -> u64 {
        let raw: Box<str> = self.str(key).into();
        match raw.trim().parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                self.warnings.push(ConfigWarning::LenientNumber {
                    key,
                    value: raw,
                    fallback: fallback.to_string().into_boxed_str(),
                });
                fallback
            }
        }
    }

    /// `JELLYFIN_SYNC_TIMEOUT_SECONDS`: an invalid value warns and becomes the fallback.
    fn lenient_f64(&mut self, key: &'static str, fallback: f64) -> f64 {
        let raw: Box<str> = self.str(key).into();
        match raw.trim().parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                self.warnings.push(ConfigWarning::LenientNumber {
                    key,
                    value: raw,
                    fallback: fallback.to_string().into_boxed_str(),
                });
                fallback
            }
        }
    }

    fn choice<T>(
        &mut self,
        key: &'static str,
        parse: fn(&str) -> Result<T, ConfigError>,
        fallback: T,
    ) -> T {
        match parse(self.str(key)) {
            Ok(v) => v,
            Err(e) => {
                // The macro's error names the enum's canonical key; re-point it at the real one so
                // the two Jellyfin refresh-mode variables report their own names.
                self.errs.push(match e {
                    ConfigError::InvalidChoice { value, allowed, .. } => {
                        ConfigError::InvalidChoice {
                            key,
                            value,
                            allowed,
                        }
                    }
                    other => other,
                });
                fallback
            }
        }
    }

    /// `LOGLEVEL`: an unknown value becomes `INFO` with a warning (DESIGN §17.3).
    fn loglevel(&mut self) -> Box<str> {
        const LEVELS: [&str; 6] = ["TRACE", "DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"];
        let raw = self.str("LOGLEVEL");
        let upper = raw.trim().to_ascii_uppercase();
        if LEVELS.contains(&upper.as_str()) {
            return upper.into_boxed_str();
        }
        self.warnings.push(ConfigWarning::LenientNumber {
            key: "LOGLEVEL",
            value: raw.into(),
            fallback: "INFO".into(),
        });
        "INFO".into()
    }

    fn opt_path(&mut self, key: &'static str) -> Option<PathBuf> {
        let raw = self.str(key);
        if raw.is_empty() {
            None
        } else {
            Some(PathBuf::from(raw))
        }
    }

    /// `YTDL_OPTIONS_FILE` / `YTDL_OPTIONS_PRESETS_FILE`: a value starting with `.` is
    /// absolutised, as legacy's `Path().resolve()` did (DESIGN §17.1 step 6).
    fn opt_dot_relative_path(&mut self, key: &'static str) -> Option<PathBuf> {
        let raw = self.str(key);
        if raw.is_empty() {
            return None;
        }
        Some(if raw.starts_with('.') {
            absolutise(Path::new(raw))
        } else {
            PathBuf::from(raw)
        })
    }

    fn opt_regex(&mut self, key: &'static str) -> Option<Regex> {
        let raw = self.str(key);
        if raw.is_empty() {
            return None;
        }
        match Regex::new(raw) {
            Ok(r) => Some(r),
            Err(e) => {
                self.errs.push(ConfigError::InvalidRegex {
                    key,
                    value: raw.into(),
                    reason: e.to_string().into_boxed_str(),
                });
                None
            }
        }
    }

    /// `TELEGRAM_ALLOWED_CHAT_IDS`: legacy logged and skipped a bad entry rather than failing.
    fn chat_ids(&mut self, key: &'static str) -> Vec<i64> {
        let raw: Box<str> = self.str(key).into();
        let mut out = Vec::new();
        for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match entry.parse::<i64>() {
                Ok(v) => out.push(v),
                Err(_) => self.warnings.push(ConfigWarning::IgnoredListEntry {
                    key,
                    value: entry.into(),
                }),
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

// ---------------------------------------------------------------------------
// Errors and warnings
// ---------------------------------------------------------------------------

/// A fatal configuration problem. Every one of these is collected and printed as a table before
/// the process exits 2 (DESIGN §17.1 step 9).
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ConfigError {
    /// An `AULOS_*` variable that is not in `DEFAULTS` — almost always a typo.
    #[error("unknown variable {key}: no such AULOS_* setting (typo?)")]
    UnknownVariable {
        /// The offending name.
        key: Box<str>,
    },
    /// `%%TARGET` named a key that does not exist.
    #[error("{key} refers to %%{target}, which is not a known setting")]
    UnknownIndirection {
        /// The key holding the `%%` value.
        key: Box<str>,
        /// The name it pointed at.
        target: Box<str>,
    },
    /// A `%%` chain loops.
    #[error("%% indirection cycle: {chain}")]
    IndirectionCycle {
        /// The chain, `a -> b -> a`.
        chain: Box<str>,
    },
    /// A boolean was not one of the eight accepted tokens.
    #[error("environment variable \"{key}\" is set to a non-boolean value \"{value}\"")]
    InvalidBoolean {
        /// The key.
        key: &'static str,
        /// The offending value.
        value: Box<str>,
    },
    /// A numeric value did not parse and the key has no leniency rule.
    #[error("{key}: \"{value}\" is not a valid number")]
    InvalidNumber {
        /// The key.
        key: &'static str,
        /// The offending value.
        value: Box<str>,
    },
    /// A numeric value parsed but is outside the documented range.
    #[error("{key}: {value} is outside {min}..={max}")]
    OutOfRange {
        /// The key.
        key: &'static str,
        /// The offending value.
        value: Box<str>,
        /// Inclusive lower bound.
        min: i64,
        /// Inclusive upper bound.
        max: i64,
    },
    /// An enum-valued setting got something outside its set.
    #[error("{key}: \"{value}\" must be one of {allowed:?}")]
    InvalidChoice {
        /// The key.
        key: &'static str,
        /// The offending value.
        value: Box<str>,
        /// The accepted values.
        allowed: &'static [&'static str],
    },
    /// `CUSTOM_DIRS_EXCLUDE_REGEX` did not compile. Now fatal at boot, not at first request.
    #[error("{key}: \"{value}\" is not a valid regex: {reason}")]
    InvalidRegex {
        /// The key.
        key: &'static str,
        /// The offending value.
        value: Box<str>,
        /// The regex crate's message.
        reason: Box<str>,
    },
}

impl ConfigError {
    /// The wire error code, for the rare case a config failure is reported over HTTP
    /// (a `POST api/v2/ytdl-options/reload`, for instance).
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        ErrorCode::ValidationFailed
    }

    /// Configuration failures are never retryable: the operator has to change something.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        false
    }

    /// The variable this error is about.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::UnknownVariable { key } | Self::UnknownIndirection { key, .. } => key,
            Self::IndirectionCycle { chain } => chain,
            Self::InvalidBoolean { key, .. }
            | Self::InvalidNumber { key, .. }
            | Self::OutOfRange { key, .. }
            | Self::InvalidChoice { key, .. }
            | Self::InvalidRegex { key, .. } => key,
        }
    }
}

/// A non-fatal configuration observation, logged at WARN.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ConfigWarning {
    /// `URL_PREFIX` was missing its leading `/`. Legacy did not add one, producing routes like
    /// `metubeadd`.
    #[error("URL_PREFIX was missing a leading '/'; normalised to \"{normalised}\"")]
    PrefixLeadingSlash {
        /// The normalised value.
        normalised: Box<str>,
    },
    /// A key with a documented leniency rule got an unusable value.
    #[error("{key}: \"{value}\" is not usable; falling back to {fallback}")]
    LenientNumber {
        /// The key.
        key: &'static str,
        /// The offending value.
        value: Box<str>,
        /// What was used instead.
        fallback: Box<str>,
    },
    /// One entry of a comma list could not be parsed and was skipped (legacy parity).
    #[error("ignoring invalid {key} entry: {value}")]
    IgnoredListEntry {
        /// The key.
        key: &'static str,
        /// The offending entry.
        value: Box<str>,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn env(pairs: &[(&str, &str)]) -> RawEnv {
        RawEnv::from_pairs(pairs.iter().copied())
    }

    fn ok(pairs: &[(&str, &str)]) -> Config {
        load(&env(pairs)).expect("config should load")
    }

    fn err(pairs: &[(&str, &str)]) -> Vec<ConfigError> {
        load(&env(pairs)).expect_err("config should fail")
    }

    #[test]
    fn the_defaults_table_has_no_duplicate_keys() {
        let mut seen = BTreeSet::new();
        for (k, _) in DEFAULTS {
            assert!(seen.insert(*k), "{k} appears twice in DEFAULTS");
        }
        for k in BOOLEAN_KEYS {
            assert!(seen.contains(k), "{k} is boolean but not in DEFAULTS");
        }
        for k in SECRET_KEYS {
            assert!(seen.contains(k), "{k} is secret but not in DEFAULTS");
        }
        for k in JSON_OPTION_KEYS {
            assert!(
                seen.contains(k),
                "{k} is a JSON option map but not in DEFAULTS"
            );
        }
    }

    #[test]
    fn every_default_parses() {
        let c = ok(&[]);
        assert_eq!(c.port, 8081);
        assert_eq!(c.host.as_ref(), "0.0.0.0");
        assert_eq!(c.max_concurrent_downloads, 3);
        assert_eq!(c.url_prefix.as_str(), "/");
        assert_eq!(c.public_host_url.as_ref(), "download/");
        assert_eq!(c.public_host_audio_url.as_ref(), "audio_download/");
        assert_eq!(c.default_theme, Theme::Auto);
        assert_eq!(c.loglevel.as_ref(), "INFO");
        assert_eq!(c.clear_completed_after, 0);
        assert_eq!(c.ws_batch_ms, 250);
        assert_eq!(c.ws_urgent_ms, 25);
        assert_eq!(c.mem_done_items, 500);
        assert_eq!(c.dedupe_mode, DedupeMode::Active);
        assert_eq!(c.restart_policy, RestartPolicy::Resume);
        assert_eq!(c.sc_http, ScHttpMode::Auto);
        assert_eq!(c.telegram_board, TelegramBoard::Board);
        assert!(c.telegram_watch_all, "BRIEF: default true");
        assert_eq!(c.import_on_error, ImportOnError::Fail);
        assert_eq!(c.log_format, LogFormat::Text);
        assert_eq!(c.db_synchronous, DbSynchronous::Normal);
        assert_eq!(c.version.as_ref(), "dev");
        assert_eq!(c.v1_history_max, 0);
        assert_eq!(c.v1_add_resolve_wait_ms, 10_000);
        assert!(c.v1_enabled);
        assert!(!c.metrics_enabled);
        assert!(c.custom_dirs && c.create_custom_dirs);
        assert!(!c.allow_ytdl_options_overrides);
        assert_eq!(c.cors_allowed_origins, CorsOrigins::None);
        assert_eq!(c.plugins_dir, PathBuf::from("/config/plugins"));
        assert_eq!(
            c.jellyfin_metadata_refresh_mode,
            JellyfinRefreshMode::Default
        );
        assert_eq!(c.jellyfin_sync_timeout_seconds, 20.0);
        assert!(c.ytdl_options_file.is_none());
        assert_eq!(c.entry_max_bytes, 262_144);
        assert_eq!(c.subscription_max_seen_ids, 50_000);
    }

    #[test]
    fn indirection_defaults_point_audio_and_temp_at_download_dir() {
        let c = ok(&[("DOWNLOAD_DIR", "/downloads")]);
        assert_eq!(c.paths.download, PathBuf::from("/downloads"));
        assert_eq!(c.paths.audio_download, PathBuf::from("/downloads"));
        assert_eq!(c.paths.temp, PathBuf::from("/downloads"));
    }

    #[test]
    fn indirection_resolves_two_hops() {
        // TEMP_DIR -> %%AUDIO_DOWNLOAD_DIR -> %%DOWNLOAD_DIR
        let c = ok(&[
            ("DOWNLOAD_DIR", "/d"),
            ("AUDIO_DOWNLOAD_DIR", "%%DOWNLOAD_DIR"),
            ("TEMP_DIR", "%%AUDIO_DOWNLOAD_DIR"),
        ]);
        assert_eq!(c.paths.temp, PathBuf::from("/d"));
    }

    #[test]
    fn an_indirection_cycle_is_fatal() {
        let e = err(&[
            ("DOWNLOAD_DIR", "%%TEMP_DIR"),
            ("TEMP_DIR", "%%DOWNLOAD_DIR"),
        ]);
        assert!(
            e.iter()
                .any(|x| matches!(x, ConfigError::IndirectionCycle { .. })),
            "{e:?}"
        );
    }

    #[test]
    fn an_unknown_indirection_target_is_fatal() {
        let e = err(&[("TEMP_DIR", "%%NOPE")]);
        assert!(
            e.iter().any(|x| matches!(
                x,
                ConfigError::UnknownIndirection { target, .. } if &**target == "NOPE"
            )),
            "{e:?}"
        );
    }

    #[rstest]
    #[case("true", true)]
    #[case("True", true)]
    #[case("on", true)]
    #[case("1", true)]
    #[case("false", false)]
    #[case("False", false)]
    #[case("off", false)]
    #[case("0", false)]
    fn every_accepted_boolean_token_parses(#[case] token: &str, #[case] expected: bool) {
        assert_eq!(ok(&[("CUSTOM_DIRS", token)]).custom_dirs, expected);
    }

    #[rstest]
    #[case("yes")]
    #[case("TRUE")]
    #[case("no")]
    #[case("")]
    #[case("2")]
    fn a_rejected_boolean_token_is_fatal(#[case] token: &str) {
        let e = err(&[("CUSTOM_DIRS", token)]);
        assert!(
            e.iter().any(|x| matches!(
                x,
                ConfigError::InvalidBoolean { key, .. } if *key == "CUSTOM_DIRS"
            )),
            "{token:?} -> {e:?}"
        );
    }

    #[test]
    fn the_legacy_boolean_error_string_is_preserved() {
        let e = err(&[("HTTPS", "yes")]);
        assert!(e.iter().any(|x| x.to_string()
            == "environment variable \"HTTPS\" is set to a non-boolean value \"yes\""));
    }

    #[rstest]
    #[case("", "/")]
    #[case("/", "/")]
    #[case("metube", "/metube/")]
    #[case("/metube", "/metube/")]
    #[case("/metube/", "/metube/")]
    fn url_prefix_is_normalised(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(ok(&[("URL_PREFIX", raw)]).url_prefix.as_str(), expected);
    }

    #[test]
    fn public_host_urls_gain_a_trailing_slash_only_when_non_empty() {
        let c = ok(&[
            ("PUBLIC_HOST_URL", "https://cdn.example"),
            ("PUBLIC_HOST_AUDIO_URL", ""),
        ]);
        assert_eq!(c.public_host_url.as_ref(), "https://cdn.example/");
        assert_eq!(c.public_host_audio_url.as_ref(), "");
    }

    #[test]
    fn an_unknown_aulos_variable_is_fatal_and_a_random_one_is_ignored() {
        let e = err(&[("AULOS_WS_BATCH_MSEC", "300")]);
        assert!(
            e.iter().any(|x| matches!(
                x,
                ConfigError::UnknownVariable { key } if &**key == "AULOS_WS_BATCH_MSEC"
            )),
            "{e:?}"
        );
        assert!(load(&env(&[("RANDOM_VAR", "x"), ("PATH", "/bin")])).is_ok());
    }

    #[test]
    fn the_e2e_markers_are_accepted_and_ignored() {
        assert!(
            load(&env(&[
                ("AULOS_E2E", "1"),
                ("AULOS_E2E_IMAGE", "ghcr.io/x:y"),
                ("AULOS_E2E_ANYTHING_AT_ALL", "z"),
            ]))
            .is_ok()
        );
    }

    #[test]
    fn version_aliases_and_plugins_dir_default_chaining() {
        assert_eq!(
            ok(&[("METUBE_VERSION", "2026.09.04")]).version.as_ref(),
            "2026.09.04"
        );
        assert_eq!(
            ok(&[("AULOS_VERSION", "from-alias")]).version.as_ref(),
            "from-alias"
        );
        assert_eq!(
            ok(&[("PLUGINS_DIR", "/opt/p")]).plugins_dir,
            PathBuf::from("/opt/p")
        );
        assert_eq!(
            ok(&[("PLUGINS_DIR", "/opt/p"), ("AULOS_PLUGINS_DIR", "/opt/q")]).plugins_dir,
            PathBuf::from("/opt/q"),
            "the AULOS_ name wins when both are set"
        );
    }

    #[test]
    fn db_path_defaults_inside_state_dir() {
        let c = ok(&[("STATE_DIR", "/downloads/.metube")]);
        assert_eq!(c.db_path, PathBuf::from("/downloads/.metube/aulos.db"));
        let c = ok(&[("STATE_DIR", "/s"), ("AULOS_DB_PATH", "/elsewhere/x.db")]);
        assert_eq!(c.db_path, PathBuf::from("/elsewhere/x.db"));
    }

    #[test]
    fn fatal_numbers_are_fatal_and_lenient_ones_are_lenient() {
        assert!(
            err(&[("PORT", "eighty")])
                .iter()
                .any(|x| matches!(x, ConfigError::InvalidNumber { key, .. } if *key == "PORT"))
        );
        assert!(
            err(&[("MAX_CONCURRENT_DOWNLOADS", "0")])
                .iter()
                .any(|x| matches!(x, ConfigError::OutOfRange { key, .. }
                                  if *key == "MAX_CONCURRENT_DOWNLOADS"))
        );
        assert!(
            err(&[("SC_MAX_CONCURRENT_DOWNLOADS", "0")])
                .iter()
                .any(|x| matches!(x, ConfigError::OutOfRange { .. }))
        );
        // Lenient: no error, documented fallback.
        assert_eq!(
            ok(&[("CLEAR_COMPLETED_AFTER", "banana")]).clear_completed_after,
            0
        );
        assert_eq!(
            ok(&[("JELLYFIN_SYNC_TIMEOUT_SECONDS", "banana")]).jellyfin_sync_timeout_seconds,
            20.0
        );
        assert_eq!(ok(&[("LOGLEVEL", "shouty")]).loglevel.as_ref(), "INFO");
        assert_eq!(ok(&[("LOGLEVEL", "debug")]).loglevel.as_ref(), "DEBUG");
    }

    #[test]
    fn ranged_values_report_their_bounds() {
        assert!(err(&[("AULOS_WS_BATCH_MS", "10")]).iter().any(
            |x| matches!(x, ConfigError::OutOfRange { key, min, max, .. }
                                  if *key == "AULOS_WS_BATCH_MS" && *min == 50 && *max == 5000)
        ));
        assert!(err(&[("AULOS_WS_URGENT_MS", "2000")]).len() == 1);
        assert_eq!(ok(&[("AULOS_WS_BATCH_MS", "50")]).ws_batch_ms, 50);
        assert_eq!(ok(&[("AULOS_WS_URGENT_MS", "0")]).ws_urgent_ms, 0);
    }

    #[rstest]
    #[case("DEFAULT_THEME", "chartreuse")]
    #[case("AULOS_DB_SYNCHRONOUS", "MAYBE")]
    #[case("AULOS_LOG_FORMAT", "yaml")]
    #[case("AULOS_DEDUPE_MODE", "sometimes")]
    #[case("AULOS_RESTART_POLICY", "reboot")]
    #[case("AULOS_SC_HTTP", "curl")]
    #[case("AULOS_TELEGRAM_BOARD", "billboard")]
    #[case("AULOS_IMPORT_ON_ERROR", "shrug")]
    #[case("JELLYFIN_METADATA_REFRESH_MODE", "Sometimes")]
    #[case("JELLYFIN_IMAGE_REFRESH_MODE", "Sometimes")]
    fn an_invalid_choice_names_its_own_key(#[case] key: &str, #[case] value: &str) {
        let e = err(&[(key, value)]);
        assert!(
            e.iter()
                .any(|x| matches!(x, ConfigError::InvalidChoice { key: k, .. } if *k == key)),
            "{key}={value} -> {e:?}"
        );
    }

    #[test]
    fn choices_are_case_insensitive_where_that_is_harmless() {
        assert_eq!(
            ok(&[("AULOS_DB_SYNCHRONOUS", "full")]).db_synchronous,
            DbSynchronous::Full
        );
        assert_eq!(ok(&[("DEFAULT_THEME", "DARK")]).default_theme, Theme::Dark);
        assert_eq!(
            ok(&[("JELLYFIN_METADATA_REFRESH_MODE", "fullrefresh")]).jellyfin_metadata_refresh_mode,
            JellyfinRefreshMode::FullRefresh
        );
    }

    #[test]
    fn an_invalid_custom_dirs_regex_is_fatal_at_boot() {
        assert!(
            err(&[("CUSTOM_DIRS_EXCLUDE_REGEX", "([")])
                .iter()
                .any(|x| matches!(x, ConfigError::InvalidRegex { .. }))
        );
        let c = ok(&[("CUSTOM_DIRS_EXCLUDE_REGEX", "")]);
        assert!(c.custom_dirs_exclude_regex.is_none());
        let c = ok(&[]);
        let re = c.custom_dirs_exclude_regex.unwrap();
        assert!(re.is_match(".hidden"));
        assert!(re.is_match("a/@junk"));
        assert!(!re.is_match("Music"));
    }

    #[test]
    fn cors_parses_none_any_and_a_list() {
        assert_eq!(ok(&[]).cors_allowed_origins, CorsOrigins::None);
        assert_eq!(
            ok(&[("CORS_ALLOWED_ORIGINS", "*")]).cors_allowed_origins,
            CorsOrigins::Any
        );
        let c = ok(&[(
            "CORS_ALLOWED_ORIGINS",
            "https://a.example, https://b.example",
        )]);
        assert!(c.cors_allowed_origins.allows("https://a.example"));
        assert!(c.cors_allowed_origins.allows("https://b.example"));
        assert!(!c.cors_allowed_origins.allows("https://c.example"));
        assert!(!CorsOrigins::None.allows("https://a.example"));
    }

    #[test]
    fn chat_ids_skip_bad_entries_the_way_legacy_did() {
        let c = ok(&[("TELEGRAM_ALLOWED_CHAT_IDS", "12, oops, -100345, 12")]);
        assert_eq!(c.telegram_allowed_chat_ids, vec![-100_345, 12]);
        assert!(ok(&[]).telegram_allowed_chat_ids.is_empty());
    }

    #[test]
    fn secrets_never_appear_in_debug_or_the_effective_table() {
        let e = env(&[
            ("TELEGRAM_BOT_TOKEN", "123:abcdef"),
            ("JELLYFIN_API_KEY", "jf-key"),
            ("AULOS_API_TOKEN", "bearer-me"),
        ]);
        let c = load(&e).unwrap();
        let dump = format!("{c:?}");
        for secret in ["123:abcdef", "jf-key", "bearer-me"] {
            assert!(!dump.contains(secret), "{secret} leaked into Debug");
        }
        assert_eq!(c.telegram_bot_token.expose(), "123:abcdef");

        let table = e.effective_redacted().unwrap();
        assert_eq!(table["TELEGRAM_BOT_TOKEN"], crate::error::REDACTED);
        assert_eq!(table["JELLYFIN_API_KEY"], crate::error::REDACTED);
        assert_eq!(table["AULOS_API_TOKEN"], crate::error::REDACTED);
        assert_eq!(table["PORT"], "8081");
        assert_eq!(table["AUDIO_DOWNLOAD_DIR"], ".", "%% is resolved");
    }

    #[test]
    fn ytdl_options_secrets_are_redacted_entry_by_entry() {
        let e = env(&[
            (
                "YTDL_OPTIONS",
                r#"{"username":"me","password":"hunter2","proxy":"http://u:p@proxy:3128","cookiefile":"/etc/cookies.txt","format":"bv+ba"}"#,
            ),
            (
                "YTDL_OPTIONS_PRESETS",
                r#"{"private":{"username":"me","password":"hunter2","format":"best"}}"#,
            ),
        ]);
        let table = e.effective_redacted().unwrap();
        let dump = format!("{table:?}");
        for secret in ["hunter2", "u:p@proxy", "/etc/cookies.txt"] {
            assert!(
                !dump.contains(secret),
                "{secret} leaked into the effective table: {dump}"
            );
        }
        // The non-secret options survive: the table exists so the operator can see what resolved.
        assert!(table["YTDL_OPTIONS"].contains(r#""username":"me""#));
        assert!(table["YTDL_OPTIONS"].contains("bv+ba"));
        assert_eq!(
            table["YTDL_OPTIONS"]
                .matches(crate::error::REDACTED)
                .count(),
            3,
            "password, proxy and cookiefile"
        );
        // Presets are a map of name -> option object, so the walk has to recurse.
        assert!(table["YTDL_OPTIONS_PRESETS"].contains(r#""private""#));
        assert!(table["YTDL_OPTIONS_PRESETS"].contains(r#""format":"best""#));
        assert_eq!(
            table["YTDL_OPTIONS_PRESETS"]
                .matches(crate::error::REDACTED)
                .count(),
            1
        );
    }

    #[test]
    fn unparseable_ytdl_options_are_redacted_whole() {
        // A half-written options blob is the case most likely to still hold a password.
        let e = env(&[("YTDL_OPTIONS", r#"{"password": "hunter2""#)]);
        let table = e.effective_redacted().unwrap();
        assert_eq!(table["YTDL_OPTIONS"], crate::error::REDACTED);
        // The empty default stays legible rather than becoming a scary «redacted».
        let default = env(&[]).effective_redacted().unwrap();
        assert_eq!(default["YTDL_OPTIONS"], "{}");
        assert_eq!(default["YTDL_OPTIONS_PRESETS"], "{}");
    }

    #[test]
    fn every_boolean_key_is_reported_exactly_once() {
        // One typo must be one error. The loader used to validate `BOOLEAN_KEYS` in a pre-pass and
        // again while building the struct, so `check-config` printed each bad boolean twice and
        // `serve` doubled the "(N errors)" count.
        for key in BOOLEAN_KEYS {
            let errs = err(&[(key, "yes")]);
            let reported = errs
                .iter()
                .filter(|e| matches!(e, ConfigError::InvalidBoolean { key: k, .. } if k == key))
                .count();
            assert_eq!(reported, 1, "{key} reported {reported} times: {errs:?}");
        }
    }

    #[test]
    fn dot_relative_option_paths_are_absolutised() {
        let c = ok(&[("YTDL_OPTIONS_FILE", "./opts.json")]);
        let p = c.ytdl_options_file.unwrap();
        assert!(p.is_absolute(), "{p:?}");
        assert!(p.ends_with("opts.json"));
        // A non-dot relative path is left alone, exactly as legacy did.
        let c = ok(&[("YTDL_OPTIONS_FILE", "config/opts.json")]);
        assert_eq!(
            c.ytdl_options_file.unwrap(),
            PathBuf::from("config/opts.json")
        );
    }

    #[test]
    fn robots_txt_is_resolved_under_base_dir() {
        let c = ok(&[("BASE_DIR", "/app"), ("ROBOTS_TXT", "robots.txt")]);
        assert_eq!(c.robots_txt.unwrap(), PathBuf::from("/app/robots.txt"));
        let c = ok(&[("BASE_DIR", "/app"), ("ROBOTS_TXT", "/etc/robots.txt")]);
        assert_eq!(c.robots_txt.unwrap(), PathBuf::from("/etc/robots.txt"));
        assert!(ok(&[]).robots_txt.is_none());
    }

    #[test]
    fn jellyfin_and_pot_urls_lose_their_trailing_slash() {
        let c = ok(&[
            ("JELLYFIN_URL", "https://jf.example/"),
            ("AULOS_POT_URL", "http://127.0.0.1:4416/"),
        ]);
        assert_eq!(c.jellyfin_url.as_ref(), "https://jf.example");
        assert_eq!(c.pot_url.as_ref(), "http://127.0.0.1:4416");
    }

    #[test]
    fn pot_cmd_is_split_into_an_argv() {
        let c = ok(&[]);
        assert_eq!(
            c.pot_cmd.iter().map(|s| &**s).collect::<Vec<_>>(),
            ["bgutil-pot", "server"]
        );
    }

    #[test]
    fn every_error_is_collected_rather_than_the_first() {
        let e = err(&[
            ("PORT", "nope"),
            ("HTTPS", "maybe"),
            ("AULOS_DEDUPE_MODE", "sometimes"),
            ("AULOS_NOT_A_THING", "1"),
        ]);
        assert!(e.len() >= 4, "expected four errors, got {e:?}");
        let keys: BTreeSet<&str> = e.iter().map(ConfigError::key).collect();
        for k in ["PORT", "HTTPS", "AULOS_DEDUPE_MODE", "AULOS_NOT_A_THING"] {
            assert!(keys.contains(k), "{k} missing from {keys:?}");
        }
    }

    #[test]
    fn warnings_are_reported_separately_from_errors() {
        let (c, warnings) = load_with_warnings(&env(&[
            ("URL_PREFIX", "metube"),
            ("CLEAR_COMPLETED_AFTER", "soon"),
            ("TELEGRAM_ALLOWED_CHAT_IDS", "1,bad"),
        ]))
        .unwrap();
        assert_eq!(c.url_prefix.as_str(), "/metube/");
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, ConfigWarning::PrefixLeadingSlash { .. })),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                ConfigWarning::LenientNumber { key, .. } if *key == "CLEAR_COMPLETED_AFTER"
            )),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w, ConfigWarning::IgnoredListEntry { .. })),
            "{warnings:?}"
        );
    }

    #[test]
    fn helpers_expose_the_derived_values_other_crates_need() {
        use crate::selection::DownloadType;
        let c = ok(&[]);
        assert_eq!(
            c.default_chapter_template(),
            "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s"
        );
        assert_eq!(c.public_host_prefix(DownloadType::Audio), "audio_download/");
        assert_eq!(c.public_host_prefix(DownloadType::Video), "download/");
        assert_eq!(c.public_host_prefix(DownloadType::Captions), "download/");
    }
}
