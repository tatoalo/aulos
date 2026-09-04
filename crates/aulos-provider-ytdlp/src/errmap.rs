//! Shim `code` → [`ProviderError`]: the mechanical half of the DESIGN §9.6 taxonomy.
//!
//! The *classification* — which exception class and which message regex produce which code —
//! lives in `python/ytdlp_runner.py`, because it has to version with the yt-dlp pin. This module
//! is the other half: a total function from a code string to a typed error, with no prose
//! matching anywhere. That split is the point. If yt-dlp renames an exception, the shim changes;
//! if the queue's retry policy changes, `aulos-core` changes; neither touches the other.
//!
//! An unrecognised code is a [`ProviderError::Contract`] naming it, not a silent
//! [`ProviderError::Other`]: a shim that invents a code is a protocol violation, and pretending
//! otherwise is how a taxonomy rots.

use aulos_provider::ProviderError;

use crate::frames::ErrorFrame;

/// The DESIGN §9.6 table, as data.
///
/// The strings are the shim's `code` values. `bad_job` is the one code with no matching
/// `ErrorCode`: request validation failing inside the shim *is* a contract violation, because
/// Rust built the job.
const CODES: [&str; 15] = [
    "canceled",
    "unsupported_url",
    "auth_required",
    "geo_restricted",
    "unavailable",
    "not_yet_live",
    "no_format",
    "bot_check",
    "network",
    "throttled",
    "postprocessing_failed",
    "disk_full",
    "timeout",
    "bad_job",
    "internal",
];

/// Every code the shim is allowed to emit, in DESIGN §9.6 table order.
#[must_use]
pub const fn known_codes() -> &'static [&'static str] {
    &CODES
}

/// Maps one shim code and its message onto a typed provider failure.
///
/// The message is passed through verbatim: the shim already stripped the `"ERROR: "` prefix,
/// removed ANSI escapes and capped the text at 512 characters, and
/// [`ProviderError::message`] applies the identical (idempotent) transform again on the way out.
#[must_use]
pub fn to_provider_error(code: &str, message: &str) -> ProviderError {
    let owned = || message.to_owned();
    match code {
        "canceled" => ProviderError::Canceled,
        "unsupported_url" => ProviderError::Unsupported(owned()),
        "auth_required" => ProviderError::AuthRequired(owned()),
        "geo_restricted" => ProviderError::GeoRestricted(owned()),
        "unavailable" => ProviderError::Unavailable(owned()),
        "not_yet_live" => ProviderError::NotYetLive(owned()),
        "no_format" => ProviderError::NoFormat(owned()),
        "bot_check" => ProviderError::BotCheck(owned()),
        "network" => ProviderError::Network(owned()),
        "throttled" => ProviderError::Throttled(owned()),
        "postprocessing_failed" => ProviderError::Postprocessing(owned()),
        "disk_full" => ProviderError::Disk(owned()),
        "timeout" => ProviderError::Timeout(owned()),
        "bad_job" => ProviderError::Contract(format!("the shim rejected the job: {message}")),
        "internal" => ProviderError::Other(owned()),
        other => ProviderError::Contract(format!(
            "the shim reported the unknown error code {other:?}: {message}"
        )),
    }
}

/// [`to_provider_error`] for a whole `error` frame, filling in a message when the shim sent none.
///
/// A code-only frame is legal — `canceled` carries nothing useful — so an empty message becomes
/// the code itself rather than an empty `error.message` on the item.
#[must_use]
pub fn from_frame(frame: &ErrorFrame) -> ProviderError {
    let message = if frame.message.trim().is_empty() {
        frame.code.clone()
    } else {
        frame.message.clone()
    };
    to_provider_error(&frame.code, &message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::error::ErrorCode;

    use super::*;

    #[test]
    fn every_code_in_the_design_table_maps_to_its_error_code() {
        // The DESIGN §9.6 right-hand column, in order.
        let expected = [
            ErrorCode::Canceled,
            ErrorCode::UnsupportedUrl,
            ErrorCode::AuthRequired,
            ErrorCode::GeoRestricted,
            ErrorCode::Unavailable,
            ErrorCode::NotYetLive,
            ErrorCode::NoFormat,
            ErrorCode::BotCheck,
            ErrorCode::Network,
            ErrorCode::Throttled,
            ErrorCode::PostprocessingFailed,
            ErrorCode::DiskFull,
            ErrorCode::Timeout,
            ErrorCode::Contract,
            ErrorCode::Internal,
        ];
        assert_eq!(known_codes().len(), expected.len());
        for (code, want) in known_codes().iter().zip(expected) {
            let got = to_provider_error(code, "boom");
            assert_eq!(got.code(), want, "{code} must map to {want}");
            assert!(got.code().item_terminal(), "{code} must be item-terminal");
        }
    }

    #[test]
    fn only_the_two_transport_codes_are_retryable() {
        let retryable: Vec<_> = known_codes()
            .iter()
            .filter(|c| to_provider_error(c, "x").retryable())
            .copied()
            .collect();
        assert_eq!(retryable, ["network", "throttled"]);
    }

    #[test]
    fn the_table_agrees_with_the_core_code_mapping() {
        // Every code except the two intentional translations must round-trip through
        // `ProviderError::from_code`, so the two tables cannot drift apart silently.
        for code in known_codes() {
            if matches!(*code, "bad_job") {
                continue;
            }
            let ours = to_provider_error(code, "m");
            let theirs = ProviderError::from_code(ours.code(), "m");
            assert_eq!(ours.code(), theirs.code(), "{code}");
        }
    }

    #[test]
    fn an_unknown_code_is_a_contract_violation() {
        let e = to_provider_error("teapot", "brewing");
        assert_eq!(e.code(), ErrorCode::Contract);
        assert!(e.message().contains("teapot"));
        assert!(e.message().contains("brewing"));
    }

    #[test]
    fn bad_job_is_a_contract_violation_and_says_so() {
        let e = to_provider_error("bad_job", "unknown coercion 'Nope'");
        assert_eq!(e.code(), ErrorCode::Contract);
        assert!(e.message().contains("unknown coercion"));
    }

    #[test]
    fn a_message_less_frame_falls_back_to_its_code() {
        let frame = ErrorFrame {
            code: "canceled".to_owned(),
            message: "   ".to_owned(),
            retryable: false,
            extractor: None,
            fatal: true,
            provider_code: None,
            traceback: None,
        };
        let e = from_frame(&frame);
        assert!(matches!(e, ProviderError::Canceled));

        let frame = ErrorFrame {
            code: "unavailable".to_owned(),
            message: String::new(),
            retryable: false,
            extractor: None,
            fatal: true,
            provider_code: None,
            traceback: None,
        };
        assert_eq!(from_frame(&frame).message(), "unavailable");
    }

    #[test]
    fn a_message_the_shim_already_cleaned_is_not_mangled_again() {
        let e = to_provider_error("bot_check", "Sign in to confirm you're not a bot");
        assert_eq!(e.message(), "Sign in to confirm you're not a bot");
    }
}
