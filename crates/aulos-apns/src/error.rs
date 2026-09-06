//! The one error type this crate returns (DESIGN §5, rule A5: a `thiserror` enum with
//! `code()`/`retryable()`, never `anyhow`).
//!
//! Note what is **not** in here: an APNs response is not an error. A `410 Unregistered` is a
//! perfectly ordinary answer that means "prune this token", and a `429` that survives the backoff
//! ladder is a push that did not land — both are [`crate::client::Outcome`] values. `ApnsError`
//! is reserved for the cases where the *server* is misconfigured or the client could not be built
//! at all, which is exactly the set that makes `healthz` report `apns: degraded`.

use std::path::PathBuf;

use aulos_core::error::ErrorCode;

/// Everything that can go wrong before a push reaches the network.
#[derive(Debug, thiserror::Error)]
pub enum ApnsError {
    /// `APNS_ENABLED=true` with `APNS_KEY_FILE` empty.
    #[error("APNS_ENABLED is true but APNS_KEY_FILE is empty")]
    MissingKeyFile,

    /// `APNS_ENABLED=true` with `APNS_KEY_ID` empty.
    #[error("APNS_ENABLED is true but APNS_KEY_ID is empty")]
    MissingKeyId,

    /// `APNS_ENABLED=true` with `APNS_TEAM_ID` empty.
    #[error("APNS_ENABLED is true but APNS_TEAM_ID is empty")]
    MissingTeamId,

    /// The `.p8` could not be read.
    #[error("the APNs signing key {path} could not be read: {source}")]
    KeyFile {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying IO failure.
        #[source]
        source: std::io::Error,
    },

    /// The `.p8` is not a PKCS#8 P-256 private key `jsonwebtoken` can sign ES256 with.
    #[error("the APNs signing key is not a usable ES256 PKCS#8 key: {0}")]
    KeyFormat(Box<str>),

    /// Signing the provider token failed.
    #[error("the APNs provider token could not be minted: {0}")]
    Mint(Box<str>),

    /// `APNS_BASE_URL_OVERRIDE` is not a usable base URL.
    #[error("APNS_BASE_URL_OVERRIDE is not a usable base URL ({0})")]
    BaseUrl(Box<str>),

    /// The `reqwest` client could not be built (a broken TLS backend, essentially).
    #[error("the APNs HTTP client could not be built: {0}")]
    Http(Box<str>),
}

impl ApnsError {
    /// The wire error code, for the taxonomy of DESIGN §5.
    ///
    /// Every variant is an operator mistake or a broken build, never a client mistake, so they all
    /// map to [`ErrorCode::Internal`] — nothing here is reachable from an HTTP request.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        ErrorCode::Internal
    }

    /// Whether retrying could plausibly succeed.
    ///
    /// Only [`Self::Mint`] is: it covers a transient signing failure with a key that already
    /// parsed. Everything else needs the operator to change something, so the notifier reports
    /// `degraded` and stops rather than spinning.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Mint(_))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_a_transient_mint_failure_is_retryable() {
        assert!(ApnsError::Mint("clock".into()).retryable());
        assert!(!ApnsError::MissingKeyFile.retryable());
        assert!(!ApnsError::KeyFormat("not pkcs8".into()).retryable());
        assert_eq!(ApnsError::MissingTeamId.code(), ErrorCode::Internal);
    }

    #[test]
    fn the_messages_name_the_env_var_an_operator_has_to_fix() {
        assert!(
            ApnsError::MissingKeyFile
                .to_string()
                .contains("APNS_KEY_FILE")
        );
        assert!(ApnsError::MissingKeyId.to_string().contains("APNS_KEY_ID"));
        assert!(
            ApnsError::MissingTeamId
                .to_string()
                .contains("APNS_TEAM_ID")
        );
        assert!(
            ApnsError::BaseUrl("nope".into())
                .to_string()
                .contains("APNS_BASE_URL_OVERRIDE")
        );
    }
}
