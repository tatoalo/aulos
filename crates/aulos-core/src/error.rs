//! The one shared wire taxonomy (DESIGN §5) plus the [`Redact`] secret newtype (DESIGN §16.5).

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Every HTTP error and every terminal item error carries a code from this list, so a client
/// branches on `bot_check` instead of regex-matching prose (DESIGN §5).
///
/// `#[non_exhaustive]`: a client must treat an unknown code as a generic failure and keep the
/// message.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// 400 — malformed body, unparseable field.
    BadRequest,
    /// 400 — field-level validation; `field` is set.
    ValidationFailed,
    /// 400, item-terminal — no provider matched and the scheme is unusable.
    UnsupportedUrl,
    /// 400 — `ALLOW_YTDL_OPTIONS_OVERRIDES=false`.
    OverridesDisabled,
    /// 400 — preset name not in the catalogue.
    UnknownPreset,
    /// 400 — containment violation, missing dir, or `CUSTOM_DIRS=false`.
    FolderInvalid,
    /// 401 — auth failure. **Never** a redirect.
    Unauthorized,
    /// 404 — unknown item / group / subscription id.
    NotFound,
    /// 409 — duplicate subscription URL; `AULOS_DEDUPE_MODE=strict` duplicate.
    Conflict,
    /// 413 — cookie upload over 1 000 000 bytes; batch add over `AULOS_MAX_BATCH_URLS`.
    PayloadTooLarge,
    /// Item-terminal — the provider needs credentials or cookies.
    AuthRequired,
    /// Item-terminal — YouTube bot check; the POT-sidecar signal.
    BotCheck,
    /// Item-terminal — blocked in this region.
    GeoRestricted,
    /// Item-terminal — removed, terminated, deleted.
    Unavailable,
    /// Item-terminal — `is_upcoming`. Also the pre-download-problem case on a `queued` item.
    NotYetLive,
    /// Item-terminal — the requested format is not available.
    NoFormat,
    /// Item-terminal — transport, 5xx, timeout. **Retryable.**
    Network,
    /// Item-terminal — HTTP 429. Retryable after 60 s.
    Throttled,
    /// Item-terminal — merge, remux or subtitle conversion failed.
    PostprocessingFailed,
    /// Item-terminal — `ENOSPC`.
    DiskFull,
    /// Item-terminal — ffmpeg / `N_m3u8DL-RE` / python3 absent.
    ToolMissing,
    /// Item-terminal — the selected provider is `Degraded` (DESIGN §6.4).
    ProviderDegraded,
    /// Item-terminal — resolve / job / stall deadline.
    Timeout,
    /// Item-terminal — user cancel.
    Canceled,
    /// Item-terminal — shim or plugin protocol violation.
    Contract,
    /// 501 — `GET <p>socket.io/*`. The only 501 in the taxonomy: it exists so a stale Socket.IO
    /// client fails loudly instead of hanging on a handshake (DESIGN §11.1).
    SocketioRemoved,
    /// 503 — SQLite busy or locked; served with `Retry-After: 1`.
    StateUnavailable,
    /// 500, item-terminal — a bug. The message is a request id; details only in the logs.
    Internal,
}

impl ErrorCode {
    /// The HTTP status this code maps to, or `None` for a code that is only ever an item error.
    #[must_use]
    pub const fn http_status(self) -> Option<u16> {
        match self {
            Self::BadRequest
            | Self::ValidationFailed
            | Self::UnsupportedUrl
            | Self::OverridesDisabled
            | Self::UnknownPreset
            | Self::FolderInvalid => Some(400),
            Self::Unauthorized => Some(401),
            Self::NotFound => Some(404),
            Self::Conflict => Some(409),
            Self::PayloadTooLarge => Some(413),
            Self::SocketioRemoved => Some(501),
            Self::StateUnavailable => Some(503),
            Self::Internal => Some(500),
            _ => None,
        }
    }

    /// Whether this code can appear as a terminal item error (DESIGN §5, column 3).
    #[must_use]
    pub const fn item_terminal(self) -> bool {
        matches!(
            self,
            Self::UnsupportedUrl
                | Self::AuthRequired
                | Self::BotCheck
                | Self::GeoRestricted
                | Self::Unavailable
                | Self::NotYetLive
                | Self::NoFormat
                | Self::Network
                | Self::Throttled
                | Self::PostprocessingFailed
                | Self::DiskFull
                | Self::ToolMissing
                | Self::ProviderDegraded
                | Self::Timeout
                | Self::Canceled
                | Self::Contract
                | Self::Internal
        )
    }

    /// Whether the queue engine's retry policy (DESIGN §8.8) may re-queue an item that failed with
    /// this code.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::Network | Self::Throttled | Self::StateUnavailable
        )
    }

    /// The wire string, without going through serde.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::ValidationFailed => "validation_failed",
            Self::UnsupportedUrl => "unsupported_url",
            Self::OverridesDisabled => "overrides_disabled",
            Self::UnknownPreset => "unknown_preset",
            Self::FolderInvalid => "folder_invalid",
            Self::Unauthorized => "unauthorized",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PayloadTooLarge => "payload_too_large",
            Self::AuthRequired => "auth_required",
            Self::BotCheck => "bot_check",
            Self::GeoRestricted => "geo_restricted",
            Self::Unavailable => "unavailable",
            Self::NotYetLive => "not_yet_live",
            Self::NoFormat => "no_format",
            Self::Network => "network",
            Self::Throttled => "throttled",
            Self::PostprocessingFailed => "postprocessing_failed",
            Self::DiskFull => "disk_full",
            Self::ToolMissing => "tool_missing",
            Self::ProviderDegraded => "provider_degraded",
            Self::Timeout => "timeout",
            Self::Canceled => "canceled",
            Self::Contract => "contract",
            Self::SocketioRemoved => "socketio_removed",
            Self::StateUnavailable => "state_unavailable",
            Self::Internal => "internal",
        }
    }

    /// Every code, in DESIGN §5 table order. The label set of `aulos_http_errors_total{code}`.
    pub const ALL: [Self; 28] = [
        Self::BadRequest,
        Self::ValidationFailed,
        Self::UnsupportedUrl,
        Self::OverridesDisabled,
        Self::UnknownPreset,
        Self::FolderInvalid,
        Self::Unauthorized,
        Self::NotFound,
        Self::Conflict,
        Self::PayloadTooLarge,
        Self::AuthRequired,
        Self::BotCheck,
        Self::GeoRestricted,
        Self::Unavailable,
        Self::NotYetLive,
        Self::NoFormat,
        Self::Network,
        Self::Throttled,
        Self::PostprocessingFailed,
        Self::DiskFull,
        Self::ToolMissing,
        Self::ProviderDegraded,
        Self::Timeout,
        Self::Canceled,
        Self::Contract,
        Self::SocketioRemoved,
        Self::StateUnavailable,
        Self::Internal,
    ];
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The one error struct on the wire.
///
/// The HTTP envelope is this plus `request_id`; `Item.error` is this verbatim (DESIGN §4.6, §5,
/// PROTOCOL §1.5, §2.3). That is why `field` is a member here and not of the envelope alone: one
/// Rust struct and one Swift struct decode both surfaces.
///
/// No `skip_serializing_if` anywhere: all five keys are always present, `None` as `null`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WireError {
    /// The closed taxonomy code.
    pub code: ErrorCode,
    /// Already-cleaned human text. Never carries a `"ERROR: "` prefix.
    pub message: Arc<str>,
    /// The offending request field. `null` on item errors, set on a `400 validation_failed`.
    pub field: Option<Arc<str>>,
    /// The provider that produced the failure, when one did.
    pub provider: Option<Arc<str>>,
    /// The provider's own error class, e.g. `"ExtractorError"`.
    pub provider_code: Option<Arc<str>>,
}

impl WireError {
    /// An item error: a code and a message, no `field`.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            provider: None,
            provider_code: None,
        }
    }

    /// A `400 validation_failed`-shaped error naming the offending field.
    #[must_use]
    pub fn field(
        code: ErrorCode,
        field: impl Into<Arc<str>>,
        message: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            field: Some(field.into()),
            provider: None,
            provider_code: None,
        }
    }

    /// Attaches provider attribution.
    #[must_use]
    pub fn with_provider(
        mut self,
        provider: impl Into<Arc<str>>,
        provider_code: Option<Arc<str>>,
    ) -> Self {
        self.provider = Some(provider.into());
        self.provider_code = provider_code;
        self
    }

    /// Whether the queue engine may auto-retry an item that failed with this error.
    #[must_use]
    pub fn retryable(&self) -> bool {
        self.code.retryable()
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A value that must never reach a log line, a `Debug` dump or `check-config` in the clear
/// (DESIGN §16.5).
///
/// `TELEGRAM_BOT_TOKEN`, `JELLYFIN_API_KEY`, `AULOS_API_TOKEN` and any `YTDL_OPTIONS` value whose
/// key matches [`SECRET_KEY_PATTERN`] are held in one of these. `Debug` and `Display` print
/// [`REDACTED`]; the real value is reachable only through [`Redact::expose`], which is easy to
/// grep for.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Redact<T>(T);

/// What a redacted value renders as, everywhere.
pub const REDACTED: &str = "«redacted»";

/// The case-insensitive pattern that marks an option **key** as secret-bearing (DESIGN §16.5).
pub const SECRET_KEY_PATTERN: &str = "(?i)(cookie|password|passwd|token|key|secret|proxy)";

impl<T> Redact<T> {
    /// Wraps a secret.
    pub const fn new(v: T) -> Self {
        Self(v)
    }

    /// Reads the secret. Every call site is an audit point.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// Unwraps the secret, consuming the guard.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl Redact<String> {
    /// Whether the secret is empty — the "not configured" test, which needs no exposure.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T> fmt::Debug for Redact<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Redact<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T: Serialize> Serialize for Redact<T> {
    /// Serialises as [`REDACTED`]. A secret is never on the wire, so there is no round trip.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(REDACTED)
    }
}

impl<T> From<T> for Redact<T> {
    fn from(v: T) -> Self {
        Self(v)
    }
}

/// Whether an option key looks like it carries a secret, per [`SECRET_KEY_PATTERN`].
///
/// Implemented by substring search rather than a regex so it is allocation-free and cannot fail.
#[must_use]
pub fn is_secret_key(key: &str) -> bool {
    const NEEDLES: [&str; 7] = [
        "cookie", "password", "passwd", "token", "key", "secret", "proxy",
    ];
    let lower = key.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_code_serialises_snake_case_and_round_trips() {
        for c in ErrorCode::ALL {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(json, format!("\"{}\"", c.as_str()));
            assert_eq!(serde_json::from_str::<ErrorCode>(&json).unwrap(), c);
            assert!(
                !c.as_str().contains(char::is_uppercase),
                "{c} must be snake_case"
            );
        }
    }

    #[test]
    fn socketio_removed_is_in_the_enum_and_is_the_only_501() {
        assert_eq!(ErrorCode::SocketioRemoved.as_str(), "socketio_removed");
        assert_eq!(ErrorCode::SocketioRemoved.http_status(), Some(501));
        assert_eq!(
            ErrorCode::ALL
                .iter()
                .filter(|c| c.http_status() == Some(501))
                .count(),
            1
        );
    }

    #[test]
    fn wire_error_serialises_all_five_keys() {
        let e = WireError::new(ErrorCode::BotCheck, "Sign in to confirm you're not a bot")
            .with_provider("ytdlp", Some("ExtractorError".into()));
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5);
        for k in ["code", "message", "field", "provider", "provider_code"] {
            assert!(obj.contains_key(k), "{k} must be present");
        }
        assert!(obj["field"].is_null());
        assert_eq!(serde_json::from_value::<WireError>(v).unwrap(), e);
    }

    #[test]
    fn wire_error_decodes_an_http_envelope_error_minus_request_id() {
        // The §5 envelope's `error` object, with `request_id` stripped by the caller.
        let raw = r#"{"code":"validation_failed","message":"bad quality","field":"quality",
                      "provider":null,"provider_code":null}"#;
        let e: WireError = serde_json::from_str(raw).unwrap();
        assert_eq!(e.code, ErrorCode::ValidationFailed);
        assert_eq!(e.field.as_deref(), Some("quality"));
    }

    #[test]
    fn redact_never_prints_the_secret() {
        let r = Redact::new("hunter2".to_owned());
        assert_eq!(format!("{r}"), REDACTED);
        assert_eq!(format!("{r:?}"), REDACTED);
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            format!("\"{REDACTED}\"")
        );
        assert_eq!(r.expose(), "hunter2");
    }

    #[test]
    fn secret_key_detection_matches_the_documented_pattern() {
        for k in [
            "cookiefile",
            "COOKIESFROMBROWSER",
            "password",
            "videopassword",
            "passwd",
            "api_token",
            "apikey",
            "client_secret",
            "proxy",
        ] {
            assert!(is_secret_key(k), "{k} must be treated as secret");
        }
        for k in ["format", "outtmpl", "writesubtitles", "paths"] {
            assert!(!is_secret_key(k), "{k} must not be treated as secret");
        }
    }

    #[test]
    fn retryable_is_exactly_network_throttled_and_state_unavailable() {
        let retryable: Vec<_> = ErrorCode::ALL
            .iter()
            .filter(|c| c.retryable())
            .map(|c| c.as_str())
            .collect();
        assert_eq!(retryable, ["network", "throttled", "state_unavailable"]);
    }
}
