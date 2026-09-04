//! `[[hook]]` tables: community post-completion hooks (DESIGN §13.4, BRIEF §13).
//!
//! Community hooks use the **same** `plugin.toml` file as providers, so a Plex, Emby or ntfy
//! integration is a directory with three lines of TOML and no `[match]` at all. This module owns
//! the schema, its validation and the [`HookFilter`] predicate; `aulos-hooks` (WP-11) owns the
//! execution, the debounce timers and the retries.
//!
//! Parsing here rather than in `aulos-hooks` is what keeps the manifest one file: a directory can
//! declare a provider **and** the hook that files its output, and there is exactly one loader,
//! one validation pass and one `Degraded` reason string for both.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use aulos_core::item::{Item, ItemView};
use aulos_core::selection::{DownloadType, ProviderId};
use aulos_core::status::TerminalStatus;
use serde::Deserialize;

use super::manifest::{ManifestError, Warning, interpolate_env};
use super::template::{Template, TokenScope};

/// The default `ordering` of a community hook: after `nfo` (20), before `jellyfin` (90)
/// (DESIGN §13).
pub const DEFAULT_ORDERING: i16 = 50;
/// The default per-attempt timeout (DESIGN §13.4).
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// The default number of retries after the first attempt (DESIGN §13.4).
pub const DEFAULT_RETRIES: u8 = 2;
/// The hard cap on `debounce_ms`: one hour (DESIGN §6.5.2).
pub const MAX_DEBOUNCE_MS: u64 = 3_600_000;
/// The hard cap on `timeout_ms`: one hour, so a hook cannot outlive a shutdown grace by design.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// The raw `[[hook]]` table, as `toml` sees it.
#[derive(Debug, Deserialize)]
pub(super) struct RawHook {
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) on: Vec<String>,
    pub(super) ordering: Option<i64>,
    pub(super) debounce_ms: Option<u64>,
    pub(super) max_wait_ms: Option<u64>,
    pub(super) timeout_ms: Option<u64>,
    pub(super) retries: Option<u32>,
    #[serde(default)]
    pub(super) when: RawWhen,
    pub(super) http: Option<RawHttp>,
    pub(super) command: Option<Vec<String>>,
    pub(super) cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RawWhen {
    #[serde(default)]
    pub(super) provider: Vec<String>,
    #[serde(default)]
    pub(super) download_type: Vec<String>,
    #[serde(default)]
    pub(super) folder_prefix: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RawHttp {
    pub(super) method: Option<String>,
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) headers: std::collections::BTreeMap<String, String>,
    pub(super) body: Option<String>,
}

/// The HTTP verbs a community hook may use.
///
/// A local enum rather than `http::Method`: DESIGN §13.4 writes `Method`, but DESIGN §3's
/// dependency row for `aulos-provider` budgets for no HTTP crate, and the six verbs below are all
/// a media-server refresh or a push notification needs. `aulos-hooks` maps this onto
/// `reqwest::Method` in one line. See `docs/INTEGRATION-NOTES.md`, WP-10.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum HttpMethod {
    /// `GET` — the Plex refresh shape.
    Get,
    /// `POST` — the default (DESIGN §13.4).
    #[default]
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
    /// `HEAD`.
    Head,
}

impl HttpMethod {
    /// The upper-case verb.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
        }
    }

    /// Parses a verb, case-insensitively.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            "HEAD" => Self::Head,
            _ => return None,
        })
    }
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a hook does when it fires (DESIGN §13.4).
#[derive(Debug)]
pub enum HookAction {
    /// An HTTP request. Enough for Plex, Emby, ntfy and any generic webhook.
    Http {
        /// The verb. Defaults to `POST`.
        method: HttpMethod,
        /// The URL template. Placeholders inside it are percent-encoded when rendered
        /// ([`super::template::Escape::Percent`]).
        url: Template,
        /// Header name → value template.
        headers: Vec<(String, Template)>,
        /// The body template. Empty by default.
        body: Template,
    },
    /// A local command, spawned exactly like a provider command: argv-level substitution, no
    /// shell, own process group, cleared environment (DESIGN §6.5.3).
    Command {
        /// The templated argv.
        argv: Vec<Template>,
        /// Working directory. Defaults to the plugin directory.
        cwd: PathBuf,
    },
}

impl HookAction {
    /// A one-line description for `healthz` and for `GET api/v2/providers`' audit view.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Http { method, url, .. } => format!("{method} {}", url.as_str()),
            Self::Command { argv, .. } => argv
                .iter()
                .map(Template::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// DESIGN §13.4 `when.*`. An empty vec means "no filter on this axis" (matches everything).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HookFilter {
    /// Allowed provider ids, e.g. `["streamingcommunity"]`.
    pub provider: Vec<Arc<str>>,
    /// Allowed download types.
    pub download_type: Vec<DownloadType>,
    /// Allowed prefixes of the item's `folder`.
    pub folder_prefix: Vec<Arc<str>>,
}

impl HookFilter {
    /// Whether `item` passes every declared axis.
    ///
    /// An empty axis matches everything, so a hook with no `when` table runs for every item. An
    /// item with no provider yet fails a `when.provider` filter rather than passing it — the
    /// filter names providers, and "unresolved" is not one of them.
    #[must_use]
    pub fn matches(&self, item: &Item) -> bool {
        self.passes(
            item.provider.as_ref().map(ProviderId::as_str),
            item.request.selection.download_type,
            item.request.folder.as_ref().map_or("", |f| f.as_str()),
        )
    }

    /// [`Self::matches`] against the wire projection of an item.
    ///
    /// `aulos-hooks` never holds an [`Item`]: the dispatcher's only event source is the event bus,
    /// whose payloads are `Arc<ItemView>` (DESIGN §13). Both entry points read the same three axes
    /// through the same predicate, so the two crates cannot drift apart on what `when` means.
    #[must_use]
    pub fn matches_view(&self, item: &ItemView) -> bool {
        self.passes(
            item.provider.as_deref(),
            item.selection.download_type,
            item.folder.as_deref().unwrap_or(""),
        )
    }

    /// The one implementation of DESIGN §13.4's `when.*` allow-lists.
    fn passes(&self, provider: Option<&str>, download_type: DownloadType, folder: &str) -> bool {
        if !self.provider.is_empty()
            && !provider.is_some_and(|p| self.provider.iter().any(|want| **want == *p))
        {
            return false;
        }
        if !self.download_type.is_empty() && !self.download_type.contains(&download_type) {
            return false;
        }
        if !self.folder_prefix.is_empty()
            && !self.folder_prefix.iter().any(|p| folder.starts_with(&**p))
        {
            return false;
        }
        true
    }

    /// Whether this filter constrains anything at all.
    #[must_use]
    pub fn is_unfiltered(&self) -> bool {
        self.provider.is_empty() && self.download_type.is_empty() && self.folder_prefix.is_empty()
    }
}

/// One validated `[[hook]]` table, ready for `aulos-hooks` to execute (DESIGN §13.4).
#[derive(Debug)]
pub struct HookSpec {
    /// `hook:<dir>/<id>` — unique across the whole plugin directory.
    pub id: Arc<str>,
    /// Which terminal statuses fire it. Non-empty, a subset of the closed three.
    pub on: Vec<TerminalStatus>,
    /// Lower runs first. Built-ins are 10 / 20 / 90.
    pub ordering: i16,
    /// `0` fires per event; `> 0` coalesces events in a trailing window.
    pub debounce_ms: u64,
    /// The hard cap on the debounce window. Defaults to `10 × debounce_ms`.
    pub max_wait_ms: u64,
    /// Per-attempt timeout.
    pub timeout_ms: u64,
    /// Attempts after the first, exponential 2 s / 8 s.
    pub retries: u8,
    /// The `when.*` allow-lists.
    pub when: HookFilter,
    /// What it does.
    pub action: HookAction,
}

impl HookSpec {
    /// Whether this hook fires for `status`.
    #[must_use]
    pub fn fires_on(&self, status: TerminalStatus) -> bool {
        self.on.contains(&status)
    }

    /// Whether this hook applies to `item` — its `on` set and its `when` filters together.
    ///
    /// `item.status` must already be terminal; a non-terminal status never fires a community hook.
    #[must_use]
    pub fn applies(&self, item: &Item) -> bool {
        TerminalStatus::try_from(item.status)
            .is_ok_and(|s| self.fires_on(s) && self.when.matches(item))
    }
}

/// Parses and validates every `[[hook]]` table in one manifest (DESIGN §13.4, §6.5.2).
///
/// `${VAR}` is interpolated from `env` at load time; an unset variable becomes `""` and produces a
/// [`Warning`] rather than a silent 401 forever.
pub(super) fn parse_hooks(
    dir_name: &str,
    raw: &[RawHook],
    env: &dyn Fn(&str) -> Option<String>,
    warnings: &mut Vec<Warning>,
) -> Result<Vec<HookSpec>, ManifestError> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (i, h) in raw.iter().enumerate() {
        out.push(parse_hook(dir_name, i, h, env, warnings, &mut seen)?);
    }
    Ok(out)
}

fn parse_hook(
    dir_name: &str,
    index: usize,
    raw: &RawHook,
    env: &dyn Fn(&str) -> Option<String>,
    warnings: &mut Vec<Warning>,
    seen: &mut BTreeSet<String>,
) -> Result<HookSpec, ManifestError> {
    let at = |field: &str| format!("hook[{index}].{field}");

    let local_id = raw
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ManifestError::invalid(at("id"), "must be a non-empty string"))?;
    if !local_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        || local_id.len() > 48
    {
        return Err(ManifestError::invalid(
            at("id"),
            format!("{local_id:?} must be 1..=48 characters of [A-Za-z0-9._-]"),
        ));
    }
    if !seen.insert(local_id.to_owned()) {
        return Err(ManifestError::invalid(
            at("id"),
            format!("{local_id:?} is declared twice in this manifest"),
        ));
    }

    // --- on: the closed three-value set ---
    if raw.on.is_empty() {
        return Err(ManifestError::invalid(
            at("on"),
            "must list at least one of finished | error | canceled",
        ));
    }
    let mut on = Vec::with_capacity(raw.on.len());
    for value in &raw.on {
        let status = TerminalStatus::ALL
            .into_iter()
            .find(|s| s.as_str() == value)
            .ok_or_else(|| {
                ManifestError::invalid(
                    at("on"),
                    format!("{value:?} is not one of finished | error | canceled"),
                )
            })?;
        if !on.contains(&status) {
            on.push(status);
        }
    }

    // --- numbers ---
    let ordering = match raw.ordering {
        None => DEFAULT_ORDERING,
        Some(v) => i16::try_from(v).map_err(|_| {
            ManifestError::invalid(at("ordering"), format!("{v} is outside -32768..=32767"))
        })?,
    };
    let debounce_ms = raw.debounce_ms.unwrap_or(0);
    if debounce_ms > MAX_DEBOUNCE_MS {
        return Err(ManifestError::invalid(
            at("debounce_ms"),
            format!("{debounce_ms} exceeds the one-hour cap of {MAX_DEBOUNCE_MS}"),
        ));
    }
    let max_wait_ms = match raw.max_wait_ms {
        Some(v) => v,
        None => debounce_ms.saturating_mul(10),
    };
    let max_wait_ms = if debounce_ms > 0 && max_wait_ms < debounce_ms {
        warnings.push(Warning::new(
            at("max_wait_ms"),
            format!("{max_wait_ms} is below debounce_ms; raised to {debounce_ms}"),
        ));
        debounce_ms
    } else {
        max_wait_ms
    };
    let timeout_ms = match raw.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS) {
        0 => {
            warnings.push(Warning::new(
                at("timeout_ms"),
                format!("0 is not a timeout; clamped to {DEFAULT_TIMEOUT_MS}"),
            ));
            DEFAULT_TIMEOUT_MS
        }
        v if v > MAX_TIMEOUT_MS => {
            warnings.push(Warning::new(
                at("timeout_ms"),
                format!("{v} exceeds the one-hour cap; clamped to {MAX_TIMEOUT_MS}"),
            ));
            MAX_TIMEOUT_MS
        }
        v => v,
    };
    let retries = match raw.retries {
        None => DEFAULT_RETRIES,
        Some(v) if v <= u32::from(u8::MAX) => u8::try_from(v).unwrap_or(DEFAULT_RETRIES),
        Some(v) => {
            warnings.push(Warning::new(
                at("retries"),
                format!("{v} exceeds 255; clamped"),
            ));
            u8::MAX
        }
    };

    // --- when ---
    let mut download_type = Vec::with_capacity(raw.when.download_type.len());
    for value in &raw.when.download_type {
        let dt = DownloadType::from_str_exact(value).ok_or_else(|| {
            ManifestError::invalid(
                at("when.download_type"),
                format!("{value:?} is not one of video | audio | captions | thumbnail"),
            )
        })?;
        download_type.push(dt);
    }
    let when = HookFilter {
        provider: raw
            .when
            .provider
            .iter()
            .map(|p| Arc::from(p.as_str()))
            .collect(),
        download_type,
        folder_prefix: raw
            .when
            .folder_prefix
            .iter()
            .map(|p| Arc::from(p.as_str()))
            .collect(),
    };

    // --- action: exactly one of http / command ---
    let action = match (&raw.http, &raw.command) {
        (Some(_), Some(_)) => {
            return Err(ManifestError::invalid(
                at("http"),
                "declare exactly one of http or command, not both",
            ));
        }
        (None, None) => {
            return Err(ManifestError::invalid(
                at("http"),
                "declare exactly one of http or command",
            ));
        }
        (Some(http), None) => {
            let method = match http.method.as_deref() {
                None => HttpMethod::Post,
                Some(m) => HttpMethod::parse(m).ok_or_else(|| {
                    ManifestError::invalid(
                        at("http.method"),
                        format!("{m:?} is not a supported HTTP verb"),
                    )
                })?,
            };
            let raw_url = http
                .url
                .as_deref()
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| {
                    ManifestError::invalid(at("http.url"), "must be a non-empty URL template")
                })?;
            let (interpolated, missing) = interpolate_env(raw_url, env);
            warn_missing(warnings, &at("http.url"), &missing);
            let url = hook_template(&interpolated, &at("http.url"))?;
            check_url_shape(&url, &at("http.url"))?;

            let mut headers = Vec::with_capacity(http.headers.len());
            for (k, v) in &http.headers {
                if k.is_empty() || !k.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
                    return Err(ManifestError::invalid(
                        at(&format!("http.headers.{k}")),
                        "a header name must be non-empty printable ASCII without a colon",
                    ));
                }
                let (interpolated, missing) = interpolate_env(v, env);
                warn_missing(warnings, &at(&format!("http.headers.{k}")), &missing);
                headers.push((
                    k.clone(),
                    hook_template(&interpolated, &at(&format!("http.headers.{k}")))?,
                ));
            }
            let body = match http.body.as_deref() {
                None => Template::literal(""),
                Some(b) => {
                    let (interpolated, missing) = interpolate_env(b, env);
                    warn_missing(warnings, &at("http.body"), &missing);
                    hook_template(&interpolated, &at("http.body"))?
                }
            };
            HookAction::Http {
                method,
                url,
                headers,
                body,
            }
        }
        (None, Some(argv)) => {
            if argv.is_empty() {
                return Err(ManifestError::invalid(
                    at("command"),
                    "must be a non-empty argv array",
                ));
            }
            let mut templates = Vec::with_capacity(argv.len());
            for (i, element) in argv.iter().enumerate() {
                let (interpolated, missing) = interpolate_env(element, env);
                warn_missing(warnings, &at(&format!("command[{i}]")), &missing);
                templates.push(hook_template(&interpolated, &at(&format!("command[{i}]")))?);
            }
            if !templates[0].is_literal() {
                return Err(ManifestError::invalid(
                    at("command[0]"),
                    "the program name may not contain a template token",
                ));
            }
            HookAction::Command {
                argv: templates,
                cwd: raw.cwd.as_deref().map_or_else(PathBuf::new, PathBuf::from),
            }
        }
    };

    Ok(HookSpec {
        id: Arc::from(format!("hook:{dir_name}/{local_id}").as_str()),
        on,
        ordering,
        debounce_ms,
        max_wait_ms,
        timeout_ms,
        retries,
        when,
        action,
    })
}

fn warn_missing(warnings: &mut Vec<Warning>, key: &str, missing: &[Box<str>]) {
    for name in missing {
        tracing::warn!(key, var = %name, "hook references an unset ${{VAR}}");
        warnings.push(Warning::new(
            key,
            format!("${{{name}}} is not set in the server environment; substituted \"\""),
        ));
    }
}

fn hook_template(src: &str, key: &str) -> Result<Template, ManifestError> {
    Template::parse(src, TokenScope::Hook).map_err(|e| ManifestError::invalid(key, e.to_string()))
}

/// Checks that `http.url` is a URL once its placeholders are filled in (DESIGN §6.5.2).
///
/// The tokens are replaced by a harmless literal first, because `{id}` in a path is not itself
/// valid URL syntax and rejecting it would rule out every useful template.
fn check_url_shape(url: &Template, key: &str) -> Result<(), ManifestError> {
    let mut probe = String::with_capacity(url.as_str().len());
    let mut rest = url.as_str();
    while let Some(open) = rest.find('{') {
        probe.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                probe.push('x');
                rest = &rest[open + close + 1..];
            }
            None => {
                rest = &rest[open + 1..];
            }
        }
    }
    probe.push_str(rest);
    let parsed = url::Url::parse(&probe)
        .map_err(|e| ManifestError::invalid(key, format!("{probe:?} is not a URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ManifestError::invalid(
            key,
            format!("scheme {:?} is not http or https", parsed.scheme()),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::id::ItemId;
    use aulos_core::item::Kind;
    use aulos_core::paths::RelDir;
    use aulos_core::request::DownloadRequest;
    use aulos_core::selection::{Codec, FormatId, ProviderId, QualityId, Selection};
    use aulos_core::source::{SourceKind, SourceRef};
    use aulos_core::status::Status;
    use url::Url;

    fn item(provider: &str, dt: DownloadType, folder: Option<&str>) -> Item {
        let url = Url::parse("https://example.test/x").unwrap();
        let selection = Selection::new(
            dt,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("best").unwrap(),
        );
        let mut request = DownloadRequest::new(url.clone(), selection);
        request.folder = folder.map(|f| RelDir::parse(f).unwrap());
        Item {
            id: ItemId::new(),
            kind: Kind::Item,
            group_id: None,
            group_index: None,
            ord: 1,
            url,
            canonical_key: "k".into(),
            provider: ProviderId::parse(provider).ok(),
            media_id: None,
            title: "t".into(),
            status: Status::Finished,
            auto_start: true,
            msg: None,
            error: None,
            request,
            entry: None,
            filename: None,
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: 0,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: SourceRef::bare(SourceKind::ApiV2),
            children_total: None,
            clear_after: None,
        }
    }

    /// The three `when` axes, judged from an `Item` and from its own wire projection, must agree —
    /// `aulos-hooks` only ever has the latter (see the WP-11 entry in `docs/INTEGRATION-NOTES.md`).
    #[test]
    fn the_item_and_the_view_forms_of_a_filter_agree() {
        let filters = [
            HookFilter::default(),
            HookFilter {
                provider: vec![Arc::from("streamingcommunity")],
                ..HookFilter::default()
            },
            HookFilter {
                download_type: vec![DownloadType::Audio],
                ..HookFilter::default()
            },
            HookFilter {
                folder_prefix: vec![Arc::from("Series/")],
                ..HookFilter::default()
            },
            HookFilter {
                provider: vec![Arc::from("ytdlp")],
                download_type: vec![DownloadType::Video],
                folder_prefix: vec![Arc::from("Series/")],
            },
        ];
        let items = [
            item("ytdlp", DownloadType::Video, None),
            item("ytdlp", DownloadType::Video, Some("Series/S01")),
            item("streamingcommunity", DownloadType::Video, Some("Movies")),
            item("ytdlp", DownloadType::Audio, Some("Series/S01")),
            unresolved(),
        ];
        for f in &filters {
            for it in &items {
                let view = ItemView::from_item(it, None, &aulos_core::item::ViewExtras::default());
                assert_eq!(
                    f.matches(it),
                    f.matches_view(&view),
                    "{f:?} disagrees on {:?}/{:?}",
                    it.provider,
                    it.request.folder
                );
            }
        }
    }

    /// An item that has not been resolved yet: no provider at all.
    fn unresolved() -> Item {
        let mut it = item("ytdlp", DownloadType::Video, Some("Series/S01"));
        it.provider = None;
        it
    }

    fn parse(toml_src: &str) -> Result<Vec<HookSpec>, ManifestError> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, rename = "hook")]
            hooks: Vec<RawHook>,
        }
        let w: Wrapper = toml::from_str(toml_src).expect("valid toml");
        let mut warnings = Vec::new();
        parse_hooks("media-server-hooks", &w.hooks, &|_| None, &mut warnings)
    }

    #[test]
    fn the_design_examples_all_load() {
        let hooks = parse(
            r#"
[[hook]]
id          = "plex"
on          = ["finished"]
debounce_ms = 30000
max_wait_ms = 300000
http = { method = "GET", url = "http://plex:32400/library/sections/3/refresh?X-Plex-Token=tok" }

[[hook]]
id          = "emby"
on          = ["finished"]
debounce_ms = 30000
http = { method = "POST", url = "http://emby:8096/Library/Refresh",
         headers = { "X-Emby-Token" = "t", Accept = "application/json" } }

[[hook]]
id = "ntfy"
on = ["finished", "error"]
http = { method = "POST", url = "https://ntfy.sh/my-aulos-topic",
         headers = { Title = "Aulos: {status}", Priority = "default" },
         body = "{title}\n{filename}{error_message}" }

[[hook]]
id       = "post-process"
on       = ["finished"]
when     = { download_type = ["video"], folder_prefix = ["Series/"] }
command  = ["/bin/sh", "{filename}", "{folder}", "{title}"]
timeout_ms = 60000
"#,
        )
        .expect("the DESIGN §13.4 examples must load");
        assert_eq!(hooks.len(), 4);
        let ids: Vec<&str> = hooks.iter().map(|h| &*h.id).collect();
        assert_eq!(
            ids,
            [
                "hook:media-server-hooks/plex",
                "hook:media-server-hooks/emby",
                "hook:media-server-hooks/ntfy",
                "hook:media-server-hooks/post-process"
            ]
        );

        let plex = &hooks[0];
        assert_eq!(plex.on, [TerminalStatus::Finished]);
        assert_eq!(plex.ordering, DEFAULT_ORDERING);
        assert_eq!(plex.debounce_ms, 30_000);
        assert_eq!(plex.max_wait_ms, 300_000);
        assert_eq!(plex.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(plex.retries, DEFAULT_RETRIES);
        assert!(plex.when.is_unfiltered());
        match &plex.action {
            HookAction::Http { method, url, .. } => {
                assert_eq!(*method, HttpMethod::Get);
                assert!(url.as_str().contains("X-Plex-Token=tok"));
            }
            other => panic!("expected an http hook, got {other:?}"),
        }

        // `max_wait_ms` defaults to ten times the debounce.
        assert_eq!(hooks[1].max_wait_ms, 300_000);
        // ntfy fires on two statuses and has no debounce.
        assert_eq!(
            hooks[2].on,
            [TerminalStatus::Finished, TerminalStatus::Error]
        );
        assert_eq!(hooks[2].debounce_ms, 0);
        assert_eq!(hooks[2].max_wait_ms, 0);
        // The command hook keeps its filters and its timeout.
        assert_eq!(hooks[3].timeout_ms, 60_000);
        assert_eq!(hooks[3].when.download_type, [DownloadType::Video]);
        assert_eq!(&*hooks[3].when.folder_prefix[0], "Series/");
        assert!(matches!(hooks[3].action, HookAction::Command { .. }));
        assert_eq!(
            hooks[3].action.summary(),
            "/bin/sh {filename} {folder} {title}"
        );
    }

    #[test]
    fn on_must_come_from_the_closed_set() {
        let e = parse(
            r#"
[[hook]]
id = "x"
on = ["downloading"]
http = { url = "http://h/x" }
"#,
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "hook[0].on: \"downloading\" is not one of finished | error | canceled"
        );
        // An empty `on` is also a rejection.
        assert!(
            parse("[[hook]]\nid=\"x\"\non=[]\nhttp={url=\"http://h/x\"}\n")
                .unwrap_err()
                .to_string()
                .contains("must list at least one")
        );
    }

    #[test]
    fn an_unparseable_url_is_rejected() {
        let e = parse(
            r#"
[[hook]]
id = "x"
on = ["finished"]
http = { url = "not a url at all" }
"#,
        )
        .unwrap_err();
        assert!(e.to_string().starts_with("hook[0].http.url: "), "{e}");
        assert!(e.to_string().contains("is not a URL"), "{e}");
        // A non-http scheme is rejected too — a hook is not a shell escape hatch.
        let e = parse("[[hook]]\nid=\"x\"\non=[\"finished\"]\nhttp={url=\"file:///etc/passwd\"}\n")
            .unwrap_err();
        assert!(e.to_string().contains("is not http or https"), "{e}");
        // But a URL made mostly of placeholders is fine.
        assert!(
            parse(
                "[[hook]]\nid=\"x\"\non=[\"finished\"]\nhttp={url=\"https://h/{id}/{status}\"}\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_debounce_over_an_hour_is_rejected() {
        let e = parse(
            r#"
[[hook]]
id = "x"
on = ["finished"]
debounce_ms = 3600001
http = { url = "http://h/x" }
"#,
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "hook[0].debounce_ms: 3600001 exceeds the one-hour cap of 3600000"
        );
        assert!(
            parse("[[hook]]\nid=\"x\"\non=[\"finished\"]\ndebounce_ms=3600000\nhttp={url=\"http://h/x\"}\n")
                .is_ok()
        );
    }

    #[test]
    fn env_interpolates_at_load_time() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, rename = "hook")]
            hooks: Vec<RawHook>,
        }
        let w: Wrapper = toml::from_str(
            "[[hook]]\nid=\"plex\"\non=[\"finished\"]\nhttp={method=\"GET\",url=\"http://plex:32400/library/sections/3/refresh?X-Plex-Token=${PLEX_TOKEN}\"}\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        let hooks = parse_hooks(
            "d",
            &w.hooks,
            &|name| (name == "PLEX_TOKEN").then(|| "s3cret".to_owned()),
            &mut warnings,
        )
        .unwrap();
        match &hooks[0].action {
            HookAction::Http { url, .. } => assert_eq!(
                url.as_str(),
                "http://plex:32400/library/sections/3/refresh?X-Plex-Token=s3cret"
            ),
            other => panic!("expected http, got {other:?}"),
        }
        assert!(warnings.is_empty());

        // An unset variable is an empty substitution plus a warning, never a hard failure.
        let mut warnings = Vec::new();
        let hooks = parse_hooks("d", &w.hooks, &|_| None, &mut warnings).unwrap();
        match &hooks[0].action {
            HookAction::Http { url, .. } => assert!(url.as_str().ends_with("X-Plex-Token=")),
            other => panic!("expected http, got {other:?}"),
        }
        assert_eq!(warnings.len(), 1);
        assert_eq!(&*warnings[0].key, "hook[0].http.url");
        assert!(warnings[0].message.contains("PLEX_TOKEN"));
    }

    #[test]
    fn exactly_one_action_is_required() {
        let both = parse(
            "[[hook]]\nid=\"x\"\non=[\"finished\"]\nhttp={url=\"http://h/x\"}\ncommand=[\"/bin/sh\"]\n",
        )
        .unwrap_err();
        assert!(both.to_string().contains("not both"), "{both}");
        let neither = parse("[[hook]]\nid=\"x\"\non=[\"finished\"]\n").unwrap_err();
        assert!(
            neither
                .to_string()
                .contains("exactly one of http or command"),
            "{neither}"
        );
    }

    #[test]
    fn ids_are_unique_and_shaped() {
        let dup = parse(
            "[[hook]]\nid=\"x\"\non=[\"finished\"]\nhttp={url=\"http://h/x\"}\n[[hook]]\nid=\"x\"\non=[\"error\"]\nhttp={url=\"http://h/y\"}\n",
        )
        .unwrap_err();
        assert!(dup.to_string().contains("declared twice"), "{dup}");
        let bad = parse("[[hook]]\nid=\"a b\"\non=[\"finished\"]\nhttp={url=\"http://h/x\"}\n")
            .unwrap_err();
        assert!(bad.to_string().contains("[A-Za-z0-9._-]"), "{bad}");
        let missing =
            parse("[[hook]]\non=[\"finished\"]\nhttp={url=\"http://h/x\"}\n").unwrap_err();
        assert!(missing.to_string().contains("hook[0].id"), "{missing}");
    }

    #[test]
    fn a_hook_template_may_not_use_provider_tokens() {
        let e =
            parse("[[hook]]\nid=\"x\"\non=[\"finished\"]\ncommand=[\"/bin/sh\",\"{out_dir}\"]\n")
                .unwrap_err();
        assert!(e.to_string().contains("not available to a hook"), "{e}");
        let e = parse("[[hook]]\nid=\"x\"\non=[\"finished\"]\ncommand=[\"/bin/sh\",\"{nope}\"]\n")
            .unwrap_err();
        assert!(e.to_string().contains("unknown token"), "{e}");
    }

    #[test]
    fn the_filter_axes_are_independent_allow_lists() {
        let hooks = parse(
            r#"
[[hook]]
id = "x"
on = ["finished"]
when = { provider = ["streamingcommunity"], download_type = ["video"], folder_prefix = ["Series/", "Film/"] }
http = { url = "http://h/x" }
"#,
        )
        .unwrap();
        let f = &hooks[0].when;
        assert!(!f.is_unfiltered());
        assert!(f.matches(&item(
            "streamingcommunity",
            DownloadType::Video,
            Some("Series/Foo")
        )));
        assert!(f.matches(&item(
            "streamingcommunity",
            DownloadType::Video,
            Some("Film/Bar")
        )));
        // Each axis vetoes on its own.
        assert!(!f.matches(&item("ytdlp", DownloadType::Video, Some("Series/Foo"))));
        assert!(!f.matches(&item(
            "streamingcommunity",
            DownloadType::Audio,
            Some("Series/Foo")
        )));
        assert!(!f.matches(&item(
            "streamingcommunity",
            DownloadType::Video,
            Some("Other")
        )));
        // No folder at all fails a folder filter.
        assert!(!f.matches(&item("streamingcommunity", DownloadType::Video, None)));

        // An empty filter matches everything, including an unresolved item.
        let empty = HookFilter::default();
        assert!(empty.is_unfiltered());
        let mut unresolved = item("ytdlp", DownloadType::Audio, None);
        unresolved.provider = None;
        assert!(empty.matches(&unresolved));
        // …but a provider filter does not pass an item with no provider.
        assert!(!f.matches(&unresolved));
    }

    #[test]
    fn applies_combines_on_and_when() {
        let hooks =
            parse("[[hook]]\nid=\"x\"\non=[\"error\"]\nhttp={url=\"http://h/x\"}\n").unwrap();
        let h = &hooks[0];
        let mut it = item("ytdlp", DownloadType::Video, None);
        assert!(
            !h.applies(&it),
            "a finished item must not fire an error hook"
        );
        it.status = Status::Error;
        assert!(h.applies(&it));
        it.status = Status::Downloading;
        assert!(!h.applies(&it), "a non-terminal item never fires a hook");
        assert!(h.fires_on(TerminalStatus::Error));
        assert!(!h.fires_on(TerminalStatus::Canceled));
    }

    #[test]
    fn http_methods_round_trip() {
        for m in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Patch,
            HttpMethod::Delete,
            HttpMethod::Head,
        ] {
            assert_eq!(HttpMethod::parse(m.as_str()), Some(m));
            assert_eq!(HttpMethod::parse(&m.as_str().to_lowercase()), Some(m));
            assert_eq!(m.to_string(), m.as_str());
        }
        assert_eq!(HttpMethod::parse("TRACE"), None);
        assert_eq!(HttpMethod::default(), HttpMethod::Post);
    }
}
