//! The [`Provider`] trait, its borrowed context structs, match scoring and the error taxonomy
//! every provider maps its failures onto (DESIGN §6.1, §6.3, §5).
//!
//! A provider is the only thing in the workspace that knows how to talk to a site. It gets a URL
//! and a context, and it answers with entries or with a file — nothing else. It cannot see the
//! store, the queue or the API (DESIGN §3 rule A1), which is what keeps a provider replaceable and
//! testable on its own.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use aulos_core::catalog::FormatCatalog;
use aulos_core::error::{ErrorCode, WireError};
use aulos_core::health::ComponentStatus;
use aulos_core::id::ItemId;
use aulos_core::paths::Paths;
use aulos_core::request::DownloadRequest;
use aulos_core::selection::ProviderId;
use aulos_core::ytdl_options::YtdlOptions;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::entry::MediaEntry;
use crate::outcome::Outcome;
use crate::sink::ProgressSink;

/// How well a provider matches a URL (DESIGN §6.1).
///
/// The score is `0..=255`; the highest wins and ties break by registration order
/// ([`crate::registry::Registry::pick`]). The derived [`Ord`] is the selection order, which is why
/// the variants are declared worst-first: every [`Match::Strong`] outranks every [`Match::Weak`]
/// regardless of score, so the `ytdlp` fallback at `Weak(1)` can never beat a real match, and
/// [`Match::Forced`] (a `provider_hint`) outranks everything.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Match {
    /// This provider cannot handle the URL. Also the answer to an `exclude_path_regex` veto.
    No,
    /// A catch-all match. `ytdlp` returns `Weak(1)`.
    Weak(u8),
    /// A real match: the provider recognised the host and (usually) the path.
    Strong(u8),
    /// The request named this provider in `provider_hint`. Beats every score.
    Forced,
}

/// The score a `command` plugin gets from a `match.hosts` suffix hit (DESIGN §6.3).
pub const SCORE_HOST_SUFFIX: u8 = 100;
/// The score a `command` plugin gets from a `match.host_regex` hit (DESIGN §6.3).
pub const SCORE_HOST_REGEX: u8 = 150;
/// The score a `command` plugin gets when `match.path_regex` also matches (DESIGN §6.3).
pub const SCORE_PATH_REGEX: u8 = 250;
/// The score `streamingcommunity` returns on a dispatchable path (DESIGN §6.3, §10.2).
pub const SCORE_SC: u8 = 200;
/// The score `ytdlp`, the catch-all fallback, returns (DESIGN §6.3).
pub const SCORE_FALLBACK: u8 = 1;

impl Match {
    /// The numeric score, `0` for [`Match::No`] and `255` for [`Match::Forced`].
    ///
    /// This is the number the wire calls `match.score` (PROTOCOL §4.6).
    #[must_use]
    pub const fn score(self) -> u8 {
        match self {
            Self::No => 0,
            Self::Weak(n) | Self::Strong(n) => n,
            Self::Forced => u8::MAX,
        }
    }

    /// Whether this is a match at all.
    #[must_use]
    pub const fn is_match(self) -> bool {
        !matches!(self, Self::No)
    }

    /// The [`MatchReason`] this match reports on the wire, or `None` for [`Match::No`].
    ///
    /// The reason is derived from the shape and the score rather than reported by the provider,
    /// because the DESIGN §6.3 score table is a bijection onto the closed PROTOCOL §4.6 reason
    /// set:
    ///
    /// | `Match` | `reason` |
    /// |---|---|
    /// | [`Match::Forced`] | `forced` |
    /// | `Strong(250)` | `path_regex` |
    /// | `Strong(150)` | `host_regex` |
    /// | any other `Strong` | `host_contains` |
    /// | any `Weak` | `fallback` |
    #[must_use]
    pub const fn reason(self) -> Option<MatchReason> {
        match self {
            Self::No => None,
            Self::Forced => Some(MatchReason::Forced),
            Self::Weak(_) => Some(MatchReason::Fallback),
            Self::Strong(SCORE_PATH_REGEX) => Some(MatchReason::PathRegex),
            Self::Strong(SCORE_HOST_REGEX) => Some(MatchReason::HostRegex),
            Self::Strong(_) => Some(MatchReason::HostContains),
        }
    }
}

/// Why a provider was selected, as `catalog?url=` and `resolve-preview` report it
/// (PROTOCOL §4.6 — a closed five-value enum).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchReason {
    /// The host matched a suffix or a substring rule.
    HostContains,
    /// The host matched an anchored regex.
    HostRegex,
    /// The host matched *and* the path regex matched, promoting the score.
    PathRegex,
    /// The request carried a `provider_hint`.
    Forced,
    /// Nothing better matched, so the catch-all took it.
    Fallback,
}

impl MatchReason {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostContains => "host_contains",
            Self::HostRegex => "host_regex",
            Self::PathRegex => "path_regex",
            Self::Forced => "forced",
            Self::Fallback => "fallback",
        }
    }
}

impl std::fmt::Display for MatchReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a provider can fail with (DESIGN §6.1).
///
/// The variants map 1:1 onto the [`ErrorCode`] item-terminal codes of DESIGN §5, and nothing in
/// the system inspects a variant except [`Self::code`], [`Self::retryable`] and the one documented
/// fall-through of DESIGN §6.4, which keys on [`Self::Unsupported`].
///
/// Every message-bearing variant carries text that is shown to the user verbatim in
/// `Item.error.message`, already cleaned by the provider (no `"ERROR: "` prefix, no ANSI, ≤ 512
/// characters — DESIGN §9.6).
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// This provider does not understand the URL after all. **The one retryable-through-the-
    /// runner-up variant** (DESIGN §6.4).
    ///
    /// The message is carried **verbatim**, with no prefix: DESIGN §8.4 and §11.7 require
    /// `Invalid/empty data was given.` and `Unsupported resource "<etype>"` byte-identical on the
    /// wire (the v1 shim echoes them as `{"status":"error","msg":…}` and the shipped iOS build
    /// matches on the first). [`Self::code`] is what says the failure was an unsupported URL.
    #[error("{0}")]
    Unsupported(String),
    /// Credentials or cookies are required (login, members-only, private).
    #[error("{0}")]
    AuthRequired(String),
    /// A bot check blocked the extraction. The POT-sidecar signal.
    #[error("{0}")]
    BotCheck(String),
    /// Blocked in this region.
    #[error("{0}")]
    GeoRestricted(String),
    /// Removed, terminated or deleted upstream.
    #[error("{0}")]
    Unavailable(String),
    /// A scheduled live stream that has not started.
    #[error("{0}")]
    NotYetLive(String),
    /// The requested format is not available.
    #[error("{0}")]
    NoFormat(String),
    /// Transport failure, a 5xx or a connect timeout. Retryable.
    #[error("{0}")]
    Network(String),
    /// HTTP 429. Retryable after 60 s.
    #[error("{0}")]
    Throttled(String),
    /// Merge, remux or subtitle conversion failed.
    #[error("{0}")]
    Postprocessing(String),
    /// `ENOSPC`.
    #[error("{0}")]
    Disk(String),
    /// A required external binary is absent from the image.
    #[error("required tool not found: {0}")]
    ToolMissing(&'static str),
    /// A resolve, job or stall deadline expired.
    #[error("{0}")]
    Timeout(String),
    /// The user cancelled, or the process group was killed on shutdown.
    #[error("canceled")]
    Canceled,
    /// The shim or a plugin violated its protocol: a bad frame, a missing terminator, an
    /// over-long line.
    #[error("{0}")]
    Contract(String),
    /// The selected provider is in [`crate::registry::ProviderState::Degraded`] (DESIGN §6.4).
    #[error("{0}")]
    Degraded(String),
    /// Anything else. For a `command` plugin this carries the last 2 KiB of stderr, which is what
    /// a plugin author needs to debug their script (DESIGN §6.5.3).
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// The wire code (DESIGN §5). Every value returned here satisfies
    /// [`ErrorCode::item_terminal`].
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Unsupported(_) => ErrorCode::UnsupportedUrl,
            Self::AuthRequired(_) => ErrorCode::AuthRequired,
            Self::BotCheck(_) => ErrorCode::BotCheck,
            Self::GeoRestricted(_) => ErrorCode::GeoRestricted,
            Self::Unavailable(_) => ErrorCode::Unavailable,
            Self::NotYetLive(_) => ErrorCode::NotYetLive,
            Self::NoFormat(_) => ErrorCode::NoFormat,
            Self::Network(_) => ErrorCode::Network,
            Self::Throttled(_) => ErrorCode::Throttled,
            Self::Postprocessing(_) => ErrorCode::PostprocessingFailed,
            Self::Disk(_) => ErrorCode::DiskFull,
            Self::ToolMissing(_) => ErrorCode::ToolMissing,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::Canceled => ErrorCode::Canceled,
            Self::Contract(_) => ErrorCode::Contract,
            Self::Degraded(_) => ErrorCode::ProviderDegraded,
            Self::Other(_) => ErrorCode::Internal,
        }
    }

    /// Whether the queue's retry policy (DESIGN §8.8) may re-queue an item that failed with this.
    ///
    /// Delegates to [`ErrorCode::retryable`] so there is exactly one retry table.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.code().retryable()
    }

    /// The user-facing message, cleaned and capped at [`Self::MAX_MESSAGE_CHARS`].
    ///
    /// Cleaning happens once, here: a leading `"ERROR: "` is stripped and carriage returns and
    /// ANSI escapes are removed, so no caller has to regex-match prose (DESIGN §5, §9.6).
    #[must_use]
    pub fn message(&self) -> String {
        clean_message(&self.to_string())
    }

    /// The cap [`Self::message`] applies, per DESIGN §9.6.
    pub const MAX_MESSAGE_CHARS: usize = 512;

    /// Rebuilds a failure from a wire code and a message — the inverse of [`Self::code`].
    ///
    /// This is what turns a `command` plugin's `{"t":"error","code":"unavailable"}` frame
    /// (DESIGN §6.5.3) and the Python shim's `code` field (DESIGN §9.6) into a typed error without
    /// either of them owning a copy of the mapping. A code that is not item-terminal — a `400`
    /// code a provider has no business producing — becomes [`Self::Contract`], because a provider
    /// claiming `validation_failed` *is* a protocol violation.
    #[must_use]
    pub fn from_code(code: ErrorCode, message: impl Into<String>) -> Self {
        let m = message.into();
        match code {
            ErrorCode::UnsupportedUrl => Self::Unsupported(m),
            ErrorCode::AuthRequired => Self::AuthRequired(m),
            ErrorCode::BotCheck => Self::BotCheck(m),
            ErrorCode::GeoRestricted => Self::GeoRestricted(m),
            ErrorCode::Unavailable => Self::Unavailable(m),
            ErrorCode::NotYetLive => Self::NotYetLive(m),
            ErrorCode::NoFormat => Self::NoFormat(m),
            ErrorCode::Network => Self::Network(m),
            ErrorCode::Throttled => Self::Throttled(m),
            ErrorCode::PostprocessingFailed => Self::Postprocessing(m),
            ErrorCode::DiskFull => Self::Disk(m),
            ErrorCode::ToolMissing => Self::ToolMissing("unknown"),
            ErrorCode::Timeout => Self::Timeout(m),
            ErrorCode::Canceled => Self::Canceled,
            ErrorCode::ProviderDegraded => Self::Degraded(m),
            ErrorCode::Internal => Self::Other(m),
            ErrorCode::Contract => Self::Contract(m),
            other => Self::Contract(format!("provider reported the non-item code {other}: {m}")),
        }
    }

    /// The [`WireError`] this failure becomes on the item and on the socket.
    ///
    /// `provider` is always attributed; `provider_code` is the provider's own error class (e.g.
    /// `"ExtractorError"`) when it has one.
    #[must_use]
    pub fn to_wire(&self, provider: &ProviderId, provider_code: Option<&str>) -> WireError {
        WireError::new(self.code(), self.message())
            .with_provider(provider.as_arc(), provider_code.map(Arc::from))
    }
}

/// Strips a `"ERROR: "` prefix, removes ANSI escapes and control characters, collapses whitespace
/// and truncates to [`ProviderError::MAX_MESSAGE_CHARS`] characters.
fn clean_message(raw: &str) -> String {
    let stripped = crate::proc::strip_ansi(raw);
    let mut s = stripped.trim();
    // Legacy prefixed every yt-dlp failure with "ERROR: "; iOS used to strip it client-side.
    while let Some(rest) = s.strip_prefix("ERROR: ") {
        s = rest.trim_start();
    }
    let mut out = String::with_capacity(s.len().min(ProviderError::MAX_MESSAGE_CHARS));
    let mut last_was_space = false;
    for c in s.chars() {
        if out.chars().count() >= ProviderError::MAX_MESSAGE_CHARS {
            break;
        }
        if c.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else if !c.is_control() {
            out.push(c);
            last_was_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// A provider's readiness, as `healthz` and the `Degraded` gate read it (DESIGN §6.1, §6.4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProviderHealth {
    /// Ready.
    Ok,
    /// Usable but impaired — a missing optional tool, an expired cache.
    Degraded(Box<str>),
    /// Not usable at all.
    Down(Box<str>),
}

impl ProviderHealth {
    /// The `healthz` component status this maps to.
    #[must_use]
    pub const fn status(&self) -> ComponentStatus {
        match self {
            Self::Ok => ComponentStatus::Ok,
            Self::Degraded(_) => ComponentStatus::Degraded,
            Self::Down(_) => ComponentStatus::Down,
        }
    }

    /// The reason string, when there is one.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Degraded(r) | Self::Down(r) => Some(r),
        }
    }

    /// Whether this reading should keep the provider selectable.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// The output-name templates handed to a download, with the playlist/channel fields already
/// resolved by yt-dlp's own `evaluate_outtmpl` (DESIGN §6.1, §9.8).
///
/// Declared here rather than in `aulos-provider-ytdlp` because [`DownloadCtx`] names it and
/// `aulos-provider` is upstream of every provider crate; `aulos_provider_ytdlp::outtmpl`
/// re-exports it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct OutTmpl {
    /// The primary output template, e.g. `%(title)s.%(ext)s`.
    pub default: String,
    /// The per-chapter template used when `split_by_chapters` is set.
    pub chapter: String,
}

/// Everything [`Provider::resolve`] is allowed to see (DESIGN §6.1).
///
/// A borrowed context struct rather than five positional parameters, so a provider that needs one
/// more piece of context does not force a signature change on every other provider
/// (DESIGN Appendix B4).
pub struct ResolveCtx<'a> {
    /// The item being resolved. Only for logging and for the progress sink.
    pub item_id: ItemId,
    /// What the user asked for.
    pub request: &'a DownloadRequest,
    /// The layered yt-dlp options: env → file → presets → per-request overrides.
    pub ytdl_options: Arc<YtdlOptions>,
    /// The four filesystem roots.
    pub paths: &'a Paths,
    /// Subscription scan mode: extract flat, do not descend.
    pub flat: bool,
    /// Stop after this many playlist entries. `None` means unlimited.
    pub playlist_end: Option<u32>,
    /// Cancelled on user cancel and on shutdown. **Must** be observed.
    pub cancel: CancellationToken,
    /// The hard deadline for this resolution.
    pub deadline: Instant,
}

/// Everything [`Provider::download`] is allowed to see (DESIGN §6.1).
pub struct DownloadCtx<'a> {
    /// The item being downloaded.
    pub item_id: ItemId,
    /// The origin of this download.
    pub source: aulos_core::SourceKind,
    /// The resolved entry this download is for.
    pub entry: &'a MediaEntry,
    /// What the user asked for.
    pub request: &'a DownloadRequest,
    /// The layered yt-dlp options.
    pub ytdl_options: Arc<YtdlOptions>,
    /// The output directory: absolute, already created and containment-checked.
    pub out_dir: PathBuf,
    /// The scratch directory: absolute and already created.
    pub tmp_dir: PathBuf,
    /// The output-name templates, as the engine built them from the effective config.
    ///
    /// The `playlist*`/`channel*` **field pre-resolution** legacy also did is *not* applied here:
    /// evaluating a yt-dlp template needs yt-dlp, and `aulos-queue` may not depend on a provider
    /// crate (DESIGN §3). A provider that wants it does it itself —
    /// `aulos_provider_ytdlp::outtmpl_job` plus `YtdlpProvider::resolve_outtmpl` — and for a
    /// provider whose templates reference no such field the two are identical.
    pub outtmpl: OutTmpl,
    /// Cancelled on user cancel and on shutdown. **Must** be observed, and must kill the whole
    /// process group (DESIGN §6.5.3).
    pub cancel: CancellationToken,
}

/// What every downloader implements (DESIGN §6.1).
///
/// The two fallible methods must observe `ctx.cancel` and must not block the runtime: everything
/// that shells out goes through [`crate::proc`], everything that reports progress goes through
/// [`ProgressSink`].
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// This provider's stable id: `"ytdlp"`, `"streamingcommunity"`, `"command:<name>"`, `"fake"`.
    fn id(&self) -> ProviderId;

    /// How well this provider matches `url` (DESIGN §6.3).
    ///
    /// Must be cheap and side-effect-free: it is called for every add, for every
    /// `GET api/v2/catalog?url=` and for `resolve-preview`.
    fn matches(&self, url: &Url) -> Match;

    /// The client-facing format/quality catalogue for this provider (DESIGN §6.6).
    ///
    /// Returned by `Arc` because it is served by two cached HTTP endpoints and is immutable for
    /// the life of the provider instance.
    fn catalog(&self) -> Arc<FormatCatalog>;

    /// Metadata only — no bytes are written.
    ///
    /// Returns one entry for a single video, several for a playlist, or an [`crate::entry::
    /// EntryKind::Redirect`] when the URL turned out to point somewhere else.
    ///
    /// # Errors
    /// Any [`ProviderError`]. [`ProviderError::Unsupported`] is special: it is the only variant
    /// the engine retries through the runner-up provider (DESIGN §6.4).
    async fn resolve(
        &self,
        url: &Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError>;

    /// Performs the download, reporting through `sink` and honouring `ctx.cancel`.
    ///
    /// # Errors
    /// Any [`ProviderError`].
    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError>;

    /// An optional provider-specific concurrency cap, acquired **instead of** the global slot.
    ///
    /// This is how `SC_MAX_CONCURRENT_DOWNLOADS` bypasses `MAX_CONCURRENT_DOWNLOADS` exactly as
    /// legacy did (DESIGN §8.7).
    fn own_slots(&self) -> Option<usize> {
        None
    }

    /// Readiness, for `healthz` and for the `Degraded` gate (DESIGN §6.4).
    async fn probe(&self) -> ProviderHealth {
        ProviderHealth::Ok
    }
}

/// A stand-in for a provider that could not be constructed (DESIGN §6.4).
///
/// A malformed `plugin.toml`, a missing binary or an `ScHttp` implementation compiled out is
/// registered as this rather than dropped, so that:
///
/// - it appears in `GET api/v2/providers` and in `healthz` with its reason, and
/// - it **still matches** its URLs, and an item routed to it fails immediately with
///   `provider_degraded` — it does not silently fall through to `ytdlp`, which would download a
///   login page and call it a success.
///
/// The caller supplies the matcher, because only the failed constructor knows which URLs the real
/// provider would have claimed. Register it with
/// [`crate::registry::Registry::register_degraded`].
pub struct DegradedProvider {
    id: ProviderId,
    reason: Box<str>,
    catalog: Arc<FormatCatalog>,
    matcher: Box<dyn Fn(&Url) -> Match + Send + Sync>,
}

impl DegradedProvider {
    /// A stand-in that claims whatever `matcher` claims and fails every operation.
    #[must_use]
    pub fn new(
        id: ProviderId,
        reason: impl Into<Box<str>>,
        catalog: Arc<FormatCatalog>,
        matcher: Box<dyn Fn(&Url) -> Match + Send + Sync>,
    ) -> Self {
        Self {
            id,
            reason: reason.into(),
            catalog,
            matcher,
        }
    }

    /// A stand-in that claims nothing — for a plugin whose `[match]` section could not even be
    /// parsed, so there is no honest matcher to build.
    #[must_use]
    pub fn unmatched(
        id: ProviderId,
        reason: impl Into<Box<str>>,
        catalog: Arc<FormatCatalog>,
    ) -> Self {
        Self::new(id, reason, catalog, Box::new(|_| Match::No))
    }

    /// Why this provider is degraded. Shown verbatim in `healthz`.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[async_trait]
impl Provider for DegradedProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn matches(&self, url: &Url) -> Match {
        (self.matcher)(url)
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        _url: &Url,
        _ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        Err(ProviderError::Degraded(self.reason.to_string()))
    }

    async fn download(
        &self,
        _ctx: DownloadCtx<'_>,
        _sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        Err(ProviderError::Degraded(self.reason.to_string()))
    }

    async fn probe(&self) -> ProviderHealth {
        ProviderHealth::Down(self.reason.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn selection_order_is_the_derived_ord() {
        assert!(Match::Forced > Match::Strong(255));
        assert!(Match::Strong(250) > Match::Strong(200));
        assert!(Match::Strong(0) > Match::Weak(255));
        assert!(Match::Weak(1) > Match::No);
        // The DESIGN §6.3 table, as an ordering.
        let mut v = vec![
            Match::Weak(SCORE_FALLBACK),
            Match::Strong(SCORE_PATH_REGEX),
            Match::Strong(SCORE_SC),
            Match::Strong(SCORE_HOST_SUFFIX),
        ];
        v.sort_unstable();
        assert_eq!(
            v,
            vec![
                Match::Weak(1),
                Match::Strong(100),
                Match::Strong(200),
                Match::Strong(250)
            ]
        );
    }

    #[test]
    fn scores_and_reasons_follow_the_design_table() {
        assert_eq!(Match::No.score(), 0);
        assert_eq!(Match::Forced.score(), 255);
        assert_eq!(Match::Strong(200).score(), 200);
        assert_eq!(Match::No.reason(), None);
        assert_eq!(Match::Forced.reason(), Some(MatchReason::Forced));
        assert_eq!(
            Match::Strong(SCORE_PATH_REGEX).reason(),
            Some(MatchReason::PathRegex)
        );
        assert_eq!(
            Match::Strong(SCORE_HOST_REGEX).reason(),
            Some(MatchReason::HostRegex)
        );
        // PROTOCOL §4.6's worked example: streamingcommunity, score 200, reason host_contains.
        assert_eq!(
            Match::Strong(SCORE_SC).reason(),
            Some(MatchReason::HostContains)
        );
        assert_eq!(
            Match::Weak(SCORE_FALLBACK).reason(),
            Some(MatchReason::Fallback)
        );
        assert!(!Match::No.is_match());
        assert!(Match::Weak(0).is_match());
    }

    #[test]
    fn match_reason_serialises_snake_case() {
        for (r, s) in [
            (MatchReason::HostContains, "host_contains"),
            (MatchReason::HostRegex, "host_regex"),
            (MatchReason::PathRegex, "path_regex"),
            (MatchReason::Forced, "forced"),
            (MatchReason::Fallback, "fallback"),
        ] {
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{s}\""));
            assert_eq!(r.as_str(), s);
        }
    }

    #[test]
    fn every_error_maps_to_an_item_terminal_code() {
        let all = [
            ProviderError::Unsupported("u".into()),
            ProviderError::AuthRequired("a".into()),
            ProviderError::BotCheck("b".into()),
            ProviderError::GeoRestricted("g".into()),
            ProviderError::Unavailable("v".into()),
            ProviderError::NotYetLive("l".into()),
            ProviderError::NoFormat("f".into()),
            ProviderError::Network("n".into()),
            ProviderError::Throttled("t".into()),
            ProviderError::Postprocessing("p".into()),
            ProviderError::Disk("d".into()),
            ProviderError::ToolMissing("ffmpeg"),
            ProviderError::Timeout("to".into()),
            ProviderError::Canceled,
            ProviderError::Contract("c".into()),
            ProviderError::Degraded("dg".into()),
            ProviderError::Other("o".into()),
        ];
        for e in &all {
            assert!(
                e.code().item_terminal(),
                "{e:?} maps to {} which is not item-terminal",
                e.code()
            );
        }
        // Only the two retryable transport failures are retryable.
        let retryable: Vec<_> = all
            .iter()
            .filter(|e| e.retryable())
            .map(ProviderError::code)
            .collect();
        assert_eq!(retryable, vec![ErrorCode::Network, ErrorCode::Throttled]);
    }

    #[test]
    fn messages_are_cleaned_once() {
        let e = ProviderError::BotCheck(
            "ERROR: \u{1b}[0;31mSign in to confirm you're not a bot\u{1b}[0m\r\n".into(),
        );
        assert_eq!(e.message(), "Sign in to confirm you're not a bot");
        let long = ProviderError::Other("x".repeat(1000));
        assert_eq!(long.message().chars().count(), 512);
        assert_eq!(ProviderError::Canceled.message(), "canceled");
        // A doubled prefix, as a wrapped yt-dlp DownloadError produces.
        assert_eq!(
            ProviderError::Network("ERROR: ERROR: HTTP Error 503".into()).message(),
            "HTTP Error 503"
        );
    }

    /// DESIGN §8.4 / §11.7: these two strings reach the v1 shim byte-identical, so no variant may
    /// decorate them. A `#[error("unsupported url: {0}")]` on `Unsupported` broke exactly this.
    #[test]
    fn the_verbatim_legacy_messages_are_undecorated() {
        for text in [
            "Invalid/empty data was given.",
            "Unsupported resource \"unknown\"",
        ] {
            let e = ProviderError::Unsupported(text.into());
            assert_eq!(e.message(), text);
            assert_eq!(e.to_string(), text);
            let id = ProviderId::parse("ytdlp").unwrap();
            assert_eq!(&*e.to_wire(&id, None).message, text);
        }
    }

    #[test]
    fn to_wire_attributes_the_provider() {
        let id = ProviderId::parse("ytdlp").unwrap();
        let w = ProviderError::BotCheck("nope".into()).to_wire(&id, Some("ExtractorError"));
        assert_eq!(w.code, ErrorCode::BotCheck);
        assert_eq!(&*w.message, "nope");
        assert_eq!(w.provider.as_deref(), Some("ytdlp"));
        assert_eq!(w.provider_code.as_deref(), Some("ExtractorError"));
        assert_eq!(w.field, None);
    }

    #[test]
    fn provider_health_maps_onto_component_status() {
        assert_eq!(ProviderHealth::Ok.status(), ComponentStatus::Ok);
        assert_eq!(ProviderHealth::Ok.reason(), None);
        assert!(ProviderHealth::Ok.is_ok());
        let d = ProviderHealth::Degraded("stale".into());
        assert_eq!(d.status(), ComponentStatus::Degraded);
        assert_eq!(d.reason(), Some("stale"));
        assert!(!d.is_ok());
        assert_eq!(
            ProviderHealth::Down("gone".into()).status(),
            ComponentStatus::Down
        );
    }
}
