//! `command` plugins and the `[[hook]]` manifest — the community extension format
//! (DESIGN §6.5, §13.4).
//!
//! One directory with one `plugin.toml` adds a site, a post-completion hook, or both, in any
//! language, with no recompile:
//!
//! ```text
//! $AULOS_PLUGINS_DIR/
//!   bandcamp/
//!     plugin.toml        required
//!     resolve.py         any executable or interpreted file
//!     download.py
//! ```
//!
//! # What this module guarantees
//!
//! - **A broken manifest is visible, not absent.** [`discover`] never fails as a whole: a
//!   directory that parses far enough to know which URLs it claims is registered
//!   [`crate::registry::ProviderState::Degraded`] with the reason, and a directory that does not
//!   is a [`aulos_core::reload::ReloadFailure`] in the report. Both surface in `healthz`.
//! - **Every mistake a manifest can make is caught at load time, with a span.** Unknown template
//!   tokens, uncompilable regexes, an `argv[0]` that is not on `PATH`, an unknown progress capture
//!   group, a duplicate catalog id, an `on` value outside the closed set — each one names the key
//!   it is about (DESIGN §6.5.2).
//! - **There is no shell.** Argv elements are templated one at a time and handed to `execvp`
//!   verbatim, so no title, URL or provider blob can add an argv element
//!   ([`template::render_argv`]).
//! - **Plugins are not a security boundary.** A plugin runs as the server user and can do anything
//!   that user can. What this module does provide is a *cleared* environment, resource limits, its
//!   own process group, `nice(5)`, a bounded output budget and a refusal to execute out of a
//!   world-writable directory — and `GET api/v2/providers` exposes every plugin's full argv so an
//!   operator can audit what is installed. The plugin directory is operator-controlled by
//!   definition (DESIGN §6.5.3).
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the complete `plugin.toml` schema, its validation and its clamps | [`manifest`] |
//! | the `{token}` language, argv-level substitution, `${ENV}` interpolation | [`template`] |
//! | the `json_lines` / `regex` progress grammar | [`progress`] |
//! | the `Provider` implementation: discovery, spawn, isolation, success criteria | [`provider`] |
//! | `[[hook]]` tables as `HookSpec`s for `aulos-hooks` to execute | [`hookspec`] |

pub mod hookspec;
pub mod manifest;
pub mod progress;
pub mod provider;
pub mod sha256;
pub mod template;

pub use hookspec::{
    DEFAULT_ORDERING, DEFAULT_RETRIES, DEFAULT_TIMEOUT_MS, HookAction, HookFilter, HookSpec,
    HttpMethod, MAX_DEBOUNCE_MS,
};
pub use manifest::{
    CancelPolicy, Capabilities, CommandSpec, DEFAULT_MAX_OUTPUT_BYTES, DownloadSpec, EnvSpec,
    ExpectOutput, Limits, MANIFEST_FILE, MANIFEST_VERSION, MAX_CONCURRENT_CAP, MAX_TIMEOUT_SECS,
    ManifestError, MatchSpec, PLUGIN_NAME_PATTERN, PluginManifest, ResolveFormat, ResolveSpec,
    STDERR_TAIL_BYTES, StdinMode, Warning, interpolate_env, is_plugin_name, load_manifest,
    load_manifest_with_env, resolve_program,
};
pub use progress::{
    DEFAULT_MIN_INTERVAL_MS, GROUP_NAMES, ProgressKind, ProgressParser, ProgressSource,
    ProgressSpec, ProgressUpdate, StatusTarget, Unit,
};
pub use provider::{
    CommandPluginLoader, CommandProvider, PLUGIN_TOOL, PluginEnv, Scan, discover, match_url, scan,
    scan_with,
};
pub use template::{
    Escape, Template, TemplateCtx, TemplateError, Token, TokenScope, render, render_argv,
};
