//! The scrape error taxonomy: one variant per step that can fail, each with a stable code
//! (DESIGN §10.2, §5).
//!
//! Legacy collapsed every one of these into `return None` plus a log line, which is why a broken
//! SC download was undiagnosable from the outside. Every failure here carries a distinct
//! [`ScErrorCode`] that reaches the client as `error.provider_code`, and a
//! [`ProviderError`] mapping chosen so the queue does the legacy thing:
//!
//! - the four "the page did not carry what the scraper needs" cases become
//!   [`ProviderError::Unsupported`], the one variant the engine retries through the runner-up
//!   provider (DESIGN §6.4) — which is exactly what legacy's `extract() -> None` did when
//!   `__extract_info` fell through to yt-dlp;
//! - everything else terminates the item.

use aulos_provider::provider::ProviderError;

/// A stable, machine-readable class for every scrape failure.
///
/// Reported as `error.provider_code` so an operator can tell "the site rotated its Inertia
/// version" from "Cloudflare blocked us" from "the vixcloud page changed shape" without reading
/// prose.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum ScErrorCode {
    /// The transport failed before a response arrived.
    Transport,
    /// A request deadline expired.
    Timeout,
    /// A non-success HTTP status that is not a version rejection.
    Status,
    /// A body that should have been JSON was not.
    BadJson,
    /// `GET {base}/it` carried no readable `div#app[data-page]` version.
    VersionUnreadable,
    /// The Inertia call rejected the asset version twice, one forced refresh apart.
    VersionRejected,
    /// The URL is not one of the three dispatchable shapes.
    BadUrlShape,
    /// The watch page props carried no `embedUrl`.
    NoEmbedUrl,
    /// The embed page carried no `<iframe src=…>`.
    NoIframe,
    /// The vixcloud page carried no usable stream URL.
    NoStream,
    /// The scrape completed but produced no entry.
    NothingResolved,
    /// The job was cancelled.
    Canceled,
}

impl ScErrorCode {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "sc_transport",
            Self::Timeout => "sc_timeout",
            Self::Status => "sc_http_status",
            Self::BadJson => "sc_bad_json",
            Self::VersionUnreadable => "sc_version_unreadable",
            Self::VersionRejected => "sc_version_rejected",
            Self::BadUrlShape => "sc_bad_url",
            Self::NoEmbedUrl => "sc_no_embed_url",
            Self::NoIframe => "sc_no_iframe",
            Self::NoStream => "sc_no_stream",
            Self::NothingResolved => "sc_nothing_resolved",
            Self::Canceled => "sc_canceled",
        }
    }
}

impl std::fmt::Display for ScErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a StreamingCommunity scrape can fail with.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScError {
    /// The transport failed before a response arrived.
    #[error("request to {url} failed: {message}")]
    Transport {
        /// The URL that was being fetched.
        url: String,
        /// The client's own message.
        message: String,
    },
    /// A request deadline expired.
    #[error("request to {url} timed out")]
    Timeout {
        /// The URL that was being fetched.
        url: String,
    },
    /// A non-success HTTP status.
    #[error("{url} returned HTTP {status}")]
    Status {
        /// The URL that was being fetched.
        url: String,
        /// The status code.
        status: u16,
    },
    /// A body that should have been JSON was not.
    #[error("{url} did not return JSON: {message}")]
    BadJson {
        /// The URL that was being fetched.
        url: String,
        /// The parser's message.
        message: String,
    },
    /// `GET {base}/it` carried no `div#app[data-page]`, or the blob had no `version` string.
    #[error("could not read the site version from {url}")]
    VersionUnreadable {
        /// The `/it` URL.
        url: String,
    },
    /// The Inertia call rejected the asset version twice, one forced refresh apart.
    #[error("{url} rejected the site version twice (HTTP {status})")]
    VersionRejected {
        /// The Inertia URL.
        url: String,
        /// The status the second attempt returned.
        status: u16,
    },
    /// The URL is not one of the three dispatchable shapes.
    #[error("not a StreamingCommunity {what} url: {url}")]
    BadUrlShape {
        /// Which shape was expected: `"watch"`, `"season"` or `"title"`.
        what: &'static str,
        /// The URL as given.
        url: String,
    },
    /// The watch page props carried no `embedUrl`.
    #[error("no embed url in the watch page props of {url}")]
    NoEmbedUrl {
        /// The watch URL.
        url: String,
    },
    /// The embed page carried no `<iframe src=…>`.
    #[error("no iframe in the embed page {url}")]
    NoIframe {
        /// The embed URL.
        url: String,
    },
    /// The vixcloud page carried no usable stream URL.
    #[error("no stream url in {url}")]
    NoStream {
        /// The vixcloud iframe URL.
        url: String,
    },
    /// The scrape completed but produced no entry.
    #[error("nothing resolvable at {url}")]
    NothingResolved {
        /// The URL that was resolved.
        url: String,
    },
    /// The job was cancelled.
    #[error("canceled")]
    Canceled,
}

impl ScError {
    /// The stable class of this failure.
    #[must_use]
    pub const fn code(&self) -> ScErrorCode {
        match self {
            Self::Transport { .. } => ScErrorCode::Transport,
            Self::Timeout { .. } => ScErrorCode::Timeout,
            Self::Status { .. } => ScErrorCode::Status,
            Self::BadJson { .. } => ScErrorCode::BadJson,
            Self::VersionUnreadable { .. } => ScErrorCode::VersionUnreadable,
            Self::VersionRejected { .. } => ScErrorCode::VersionRejected,
            Self::BadUrlShape { .. } => ScErrorCode::BadUrlShape,
            Self::NoEmbedUrl { .. } => ScErrorCode::NoEmbedUrl,
            Self::NoIframe { .. } => ScErrorCode::NoIframe,
            Self::NoStream { .. } => ScErrorCode::NoStream,
            Self::NothingResolved { .. } => ScErrorCode::NothingResolved,
            Self::Canceled => ScErrorCode::Canceled,
        }
    }

    /// The queue-visible failure this becomes (DESIGN §6.1, §6.4).
    ///
    /// The four "the page did not carry what the scraper needs" cases and a non-dispatchable URL
    /// map onto [`ProviderError::Unsupported`], because legacy's `extract() -> None` made
    /// `__extract_info` retry the URL through yt-dlp
    /// (`streamingcommunity.py:387-397`, legacy spec §9.1). Everything else means "the right
    /// scraper tried and failed", which terminates the item.
    #[must_use]
    pub fn into_provider_error(self) -> ProviderError {
        let message = self.to_string();
        match &self {
            Self::Transport { .. } => ProviderError::Network(message),
            Self::Timeout { .. } => ProviderError::Timeout(message),
            Self::Status { status, .. } => match *status {
                401 => ProviderError::AuthRequired(message),
                // Cloudflare fronts the site; a 403 here is a bot check, not a login wall.
                403 => ProviderError::BotCheck(message),
                404 | 410 => ProviderError::Unavailable(message),
                429 => ProviderError::Throttled(message),
                500..=599 => ProviderError::Network(message),
                _ => ProviderError::Other(message),
            },
            Self::BadUrlShape { .. }
            | Self::NoEmbedUrl { .. }
            | Self::NoIframe { .. }
            | Self::NoStream { .. }
            | Self::NothingResolved { .. } => ProviderError::Unsupported(message),
            Self::Canceled => ProviderError::Canceled,
            Self::BadJson { .. }
            | Self::VersionUnreadable { .. }
            | Self::VersionRejected { .. } => ProviderError::Other(message),
        }
    }
}

/// Why [`crate::provider::ScProvider::new`] could not build a client (DESIGN §10.1, §6.4).
///
/// The caller registers a
/// [`DegradedProvider`](aulos_provider::provider::DegradedProvider) rather than dropping the
/// provider, so an SC URL fails with `provider_degraded` and the reason instead of silently
/// falling through to yt-dlp and downloading a Cloudflare page.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScInitError {
    /// `AULOS_SC_HTTP=impersonate` but the crate was built without the `sc-impersonate` feature.
    #[error("sc-impersonate not compiled in")]
    ImpersonateUnavailable,
    /// The HTTP client itself refused to build.
    #[error("could not build the StreamingCommunity HTTP client: {0}")]
    Client(String),
}

#[cfg(test)]
mod tests {
    use aulos_core::error::ErrorCode;

    use super::*;

    #[test]
    fn every_code_has_a_distinct_wire_string() {
        let all = [
            ScErrorCode::Transport,
            ScErrorCode::Timeout,
            ScErrorCode::Status,
            ScErrorCode::BadJson,
            ScErrorCode::VersionUnreadable,
            ScErrorCode::VersionRejected,
            ScErrorCode::BadUrlShape,
            ScErrorCode::NoEmbedUrl,
            ScErrorCode::NoIframe,
            ScErrorCode::NoStream,
            ScErrorCode::NothingResolved,
            ScErrorCode::Canceled,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for c in all {
            assert!(seen.insert(c.as_str()), "{c} is not distinct");
            assert!(c.as_str().starts_with("sc_"));
            assert_eq!(c.to_string(), c.as_str());
        }
        assert_eq!(seen.len(), all.len());
    }

    #[test]
    fn a_malformed_version_page_has_its_own_code() {
        let e = ScError::VersionUnreadable {
            url: "https://sc.test/it".to_owned(),
        };
        assert_eq!(e.code(), ScErrorCode::VersionUnreadable);
        // Distinct from the drift failure, which is the whole point of the pair.
        let drift = ScError::VersionRejected {
            url: "https://sc.test/it/watch/1".to_owned(),
            status: 409,
        };
        assert_ne!(e.code(), drift.code());
        assert_eq!(drift.code().as_str(), "sc_version_rejected");
    }

    #[test]
    fn the_nothing_resolved_family_maps_to_unsupported() {
        for e in [
            ScError::NoEmbedUrl {
                url: "u".to_owned(),
            },
            ScError::NoIframe {
                url: "u".to_owned(),
            },
            ScError::NoStream {
                url: "u".to_owned(),
            },
            ScError::NothingResolved {
                url: "u".to_owned(),
            },
            ScError::BadUrlShape {
                what: "watch",
                url: "u".to_owned(),
            },
        ] {
            let p = e.clone().into_provider_error();
            assert!(
                matches!(p, ProviderError::Unsupported(_)),
                "{e:?} became {p:?}"
            );
            assert_eq!(p.code(), ErrorCode::UnsupportedUrl);
        }
    }

    #[test]
    fn http_statuses_map_onto_the_honest_codes() {
        let cases = [
            (401, ErrorCode::AuthRequired),
            (403, ErrorCode::BotCheck),
            (404, ErrorCode::Unavailable),
            (410, ErrorCode::Unavailable),
            (429, ErrorCode::Throttled),
            (502, ErrorCode::Network),
            (418, ErrorCode::Internal),
        ];
        for (status, code) in cases {
            let e = ScError::Status {
                url: "https://sc.test/it".to_owned(),
                status,
            };
            assert_eq!(e.into_provider_error().code(), code, "status {status}");
        }
    }

    #[test]
    fn transport_and_timeout_stay_retryable_and_cancel_stays_cancel() {
        assert!(
            ScError::Transport {
                url: "u".to_owned(),
                message: "reset".to_owned(),
            }
            .into_provider_error()
            .retryable()
        );
        assert_eq!(
            ScError::Timeout {
                url: "u".to_owned()
            }
            .into_provider_error()
            .code(),
            ErrorCode::Timeout
        );
        assert!(matches!(
            ScError::Canceled.into_provider_error(),
            ProviderError::Canceled
        ));
    }

    #[test]
    fn init_errors_read_as_the_design_reason_strings() {
        assert_eq!(
            ScInitError::ImpersonateUnavailable.to_string(),
            "sc-impersonate not compiled in"
        );
        assert!(
            ScInitError::Client("boom".to_owned())
                .to_string()
                .contains("boom")
        );
    }
}
