//! What a hook can fail with (DESIGN §13).
//!
//! A `HookError` never changes an item's status: it is logged, counted, and surfaced in `healthz`
//! (DESIGN §13). The wire codes exist anyway, because [`HookError::code`] is what a future
//! `notice` frame would carry and because the retry decision in [`crate::jellyfin`] and
//! [`crate::manifest_hook`] is driven by [`HookError::retryable`] rather than by string matching.

use std::time::Duration;

use aulos_core::error::ErrorCode;
use aulos_core::ports::PortError;

/// A hook failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HookError {
    /// A precondition of the hook's own configuration is not met.
    ///
    /// Carries the message verbatim, because the two Jellyfin preconditions are byte-identical to
    /// legacy (`JELLYFIN_URL is required`, `JELLYFIN_API_KEY is required`, DESIGN §13.1).
    #[error("{0}")]
    Config(Box<str>),

    /// A non-2xx HTTP response, or a transport failure talking to the target.
    ///
    /// The message is pre-rendered by the hook so the legacy Jellyfin shapes survive verbatim.
    #[error("{message}")]
    Http {
        /// The already-rendered message.
        message: Box<str>,
        /// The status code, when there was a response at all.
        status: Option<u16>,
        /// Whether another attempt could plausibly succeed.
        retryable: bool,
    },

    /// The hook exceeded its own or the dispatcher's deadline.
    #[error("the hook timed out after {0:?}")]
    Timeout(Duration),

    /// Shutdown was requested while the hook was running.
    #[error("canceled")]
    Canceled,

    /// A required external tool is not installed.
    #[error("required tool {0} not found")]
    ToolMissing(&'static str),

    /// A spawned tool failed.
    #[error("{tool} failed: {detail}")]
    Tool {
        /// The tool's canonical name.
        tool: &'static str,
        /// Exit status plus the tail of stderr, or the I/O failure.
        detail: Box<str>,
    },

    /// A filesystem failure.
    #[error("{what}: {source}")]
    Io {
        /// What was being attempted.
        what: Box<str>,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The [`aulos_core::ports::HookStore`] port refused a read or a write.
    #[error("store: {0}")]
    Store(#[from] PortError),

    /// A manifest template could not be rendered — a bug in the caller, not in the manifest,
    /// since every token is validated at load time (DESIGN §13.4).
    #[error("template: {0}")]
    Template(Box<str>),

    /// The hook panicked. Caught by the dispatcher, counted, and never fatal.
    #[error("the hook panicked: {0}")]
    Panic(Box<str>),

    /// Anything else.
    #[error("{0}")]
    Other(Box<str>),
}

impl HookError {
    /// A configuration precondition failure.
    #[must_use]
    pub fn config(message: impl Into<Box<str>>) -> Self {
        Self::Config(message.into())
    }

    /// A non-2xx response. Retryable for 5xx and 429, final for every other 4xx.
    #[must_use]
    pub fn http_status(message: impl Into<Box<str>>, status: u16) -> Self {
        Self::Http {
            message: message.into(),
            status: Some(status),
            retryable: status >= 500 || status == 429,
        }
    }

    /// A transport failure. Always retryable.
    #[must_use]
    pub fn transport(message: impl Into<Box<str>>) -> Self {
        Self::Http {
            message: message.into(),
            status: None,
            retryable: true,
        }
    }

    /// Anything else.
    #[must_use]
    pub fn other(message: impl Into<Box<str>>) -> Self {
        Self::Other(message.into())
    }

    /// An I/O failure, labelled with what was being attempted.
    #[must_use]
    pub fn io(what: impl Into<Box<str>>, source: std::io::Error) -> Self {
        Self::Io {
            what: what.into(),
            source,
        }
    }

    /// The wire error code (DESIGN §5).
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Config(_) => ErrorCode::ValidationFailed,
            Self::Http { .. } => ErrorCode::Network,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::Canceled => ErrorCode::Canceled,
            Self::ToolMissing(_) => ErrorCode::ToolMissing,
            Self::Tool { .. } => ErrorCode::PostprocessingFailed,
            Self::Store(e) => e.code(),
            Self::Template(_) => ErrorCode::Contract,
            Self::Io { .. } | Self::Panic(_) | Self::Other(_) => ErrorCode::Internal,
        }
    }

    /// Whether another attempt could plausibly succeed.
    ///
    /// A misconfiguration, a missing tool, a bad template and a panic are all permanent for the
    /// life of the process; a 5xx, a transport failure and a busy store are not.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        match self {
            Self::Http { retryable, .. } => *retryable,
            Self::Store(e) => e.retryable(),
            Self::Timeout(_) | Self::Io { .. } | Self::Tool { .. } => true,
            Self::Config(_)
            | Self::Canceled
            | Self::ToolMissing(_)
            | Self::Template(_)
            | Self::Panic(_)
            | Self::Other(_) => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::id::ItemId;

    #[test]
    fn http_retryability_follows_the_status_class() {
        assert!(HookError::http_status("x", 500).retryable());
        assert!(HookError::http_status("x", 503).retryable());
        assert!(HookError::http_status("x", 429).retryable());
        assert!(!HookError::http_status("x", 404).retryable());
        assert!(!HookError::http_status("x", 400).retryable());
        assert!(HookError::transport("x").retryable());
    }

    #[test]
    fn every_variant_has_a_code_and_a_retry_verdict() {
        let cases: Vec<HookError> = vec![
            HookError::config("JELLYFIN_URL is required"),
            HookError::http_status("boom", 500),
            HookError::Timeout(Duration::from_secs(1)),
            HookError::Canceled,
            HookError::ToolMissing("ffmpeg"),
            HookError::Tool {
                tool: "ffmpeg",
                detail: "exit 1".into(),
            },
            HookError::io("write nfo", std::io::Error::other("nope")),
            HookError::Store(PortError::NotFound(ItemId::new())),
            HookError::Template("unknown token".into()),
            HookError::Panic("assertion failed".into()),
            HookError::other("odd"),
        ];
        for e in cases {
            // Every variant renders a non-empty message and answers both questions.
            assert!(!e.to_string().is_empty(), "{e:?}");
            let _ = e.code();
            let _ = e.retryable();
        }
    }

    #[test]
    fn the_config_message_is_verbatim() {
        assert_eq!(
            HookError::config("JELLYFIN_API_KEY is required").to_string(),
            "JELLYFIN_API_KEY is required"
        );
        assert_eq!(
            HookError::config("x").code(),
            ErrorCode::ValidationFailed,
            "a misconfiguration is not an internal error"
        );
    }

    #[test]
    fn a_missing_row_is_not_retryable_but_a_busy_store_is() {
        assert!(!HookError::Store(PortError::NotFound(ItemId::new())).retryable());
        assert!(HookError::Store(PortError::Unavailable).retryable());
    }
}
