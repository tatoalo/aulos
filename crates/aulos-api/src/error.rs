//! The one HTTP error envelope (PROTOCOL §1.5, §1.6, DESIGN §5).
//!
//! Every non-2xx response the server produces — a validation failure, a 401, a 404, the
//! `socket.io` 501, a store timeout — is this object, and nothing else:
//!
//! ```json
//! { "error": { "code": "validation_failed", "message": "…", "field": "quality",
//!              "provider": null, "provider_code": null, "request_id": "01JB…" } }
//! ```
//!
//! `request_id` is stamped by [`crate::trace`] on the way out rather than passed into every
//! handler: the id is minted by the middleware that also writes the `X-Request-Id` header, and
//! threading it through forty call sites would be forty chances to forget it. An [`ApiError`]
//! therefore leaves `request_id` as `null` and marks its response with [`EnvelopeStamp`]; the
//! middleware fills it in and the two surfaces can never disagree.

use std::sync::Arc;

use aulos_core::{ErrorCode, WireError};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

/// The longest `message` the envelope carries (PROTOCOL §1.5).
pub const MAX_MESSAGE_CHARS: usize = 512;

/// A marker on a response whose body is an error envelope with a `null` `request_id`.
///
/// [`crate::trace::headers`] looks for it and rewrites the body. Nothing else produces one, so
/// the middleware can never rewrite a payload that merely looks like an envelope.
#[derive(Clone, Copy, Debug)]
pub struct EnvelopeStamp;

/// One HTTP failure, as the envelope's five fields.
///
/// Deviation from PLAN WP-14's `ApiError(pub ErrorCode, pub String, pub Option<&'static str>)`:
/// `field` has to be able to hold a **dynamic** name, because `EngineHandle::add` reports
/// validation failures as [`WireError`]s built from the catalog (`"quality"`, `"format"`, …) and
/// `aulos_core::SubError::Invalid` does the same. The tuple's three positions survive as
/// [`ApiError::new`]'s three arguments.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// The closed taxonomy code. Also decides the HTTP status.
    pub code: ErrorCode,
    /// Human text, already cleaned: no `"ERROR: "` prefix, no `\r`, at most 512 characters.
    pub message: Arc<str>,
    /// The offending request field, when there is one.
    pub field: Option<Arc<str>>,
    /// The provider that produced the failure, when one did.
    pub provider: Option<Arc<str>>,
    /// The provider's own error class, for diagnostics only.
    pub provider_code: Option<Arc<str>>,
}

impl ApiError {
    /// The PLAN's three positions: a code, a message and an optional field.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl AsRef<str>, field: Option<&str>) -> Self {
        Self {
            code,
            message: clean_message(message.as_ref()),
            field: field.map(Arc::from),
            provider: None,
            provider_code: None,
        }
    }

    /// A failure with no offending field.
    #[must_use]
    pub fn of(code: ErrorCode, message: impl AsRef<str>) -> Self {
        Self::new(code, message, None)
    }

    /// `400 bad_request`.
    #[must_use]
    pub fn bad_request(message: impl AsRef<str>) -> Self {
        Self::of(ErrorCode::BadRequest, message)
    }

    /// `400 validation_failed` naming the offending field.
    #[must_use]
    pub fn invalid(field: &str, message: impl AsRef<str>) -> Self {
        Self::new(ErrorCode::ValidationFailed, message, Some(field))
    }

    /// `404 not_found`.
    #[must_use]
    pub fn not_found(message: impl AsRef<str>) -> Self {
        Self::of(ErrorCode::NotFound, message)
    }

    /// `401 unauthorized` — the one message PROTOCOL §1.4 pins.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self::of(ErrorCode::Unauthorized, "authentication required")
    }

    /// `503 state_unavailable`, served with `Retry-After: 1`.
    #[must_use]
    pub fn unavailable(message: impl AsRef<str>) -> Self {
        Self::of(ErrorCode::StateUnavailable, message)
    }

    /// `500 internal`. The message is the request id; the detail belongs in the logs.
    #[must_use]
    pub fn internal(message: impl AsRef<str>) -> Self {
        Self::of(ErrorCode::Internal, message)
    }

    /// The HTTP status this failure answers with.
    ///
    /// An item-terminal code that reached the HTTP surface (`unsupported_url` on an add, say)
    /// keeps its documented status; anything with no mapping is a bug and answers 500.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        let raw = self.code.http_status().unwrap_or(500);
        StatusCode::from_u16(raw).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// The envelope body, with `request_id` still `null`.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        json!({ "error": {
            "code": self.code,
            "message": self.message,
            "field": self.field,
            "provider": self.provider,
            "provider_code": self.provider_code,
            "request_id": serde_json::Value::Null,
        }})
    }
}

impl From<WireError> for ApiError {
    fn from(e: WireError) -> Self {
        Self {
            code: e.code,
            message: clean_message(&e.message),
            field: e.field,
            provider: e.provider,
            provider_code: e.provider_code,
        }
    }
}

impl From<aulos_store::StoreError> for ApiError {
    /// A busy or locked database is `503 state_unavailable`; anything else is a bug.
    fn from(e: aulos_store::StoreError) -> Self {
        let code = e.code();
        Self::of(code, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // `Json` rather than `axum::Json`, so an error body carries the same
        // `application/json; charset=utf-8` PROTOCOL §1.2 promises for every response.
        let mut response = (status, Json(self.body())).into_response();
        response.extensions_mut().insert(EnvelopeStamp);
        if status == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

/// A successful JSON payload, with the `Content-Type` PROTOCOL §1.2 requires.
///
/// `axum::Json` already emits `application/json`; this exists so the charset is explicit on every
/// success as well as on every error, which is what the header snapshot tests assert.
#[derive(Clone, Copy, Debug)]
pub struct Json<T>(pub T);

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        let mut response = axum::Json(self.0).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        response
    }
}

/// Cleans a message for the wire: no `"ERROR: "` prefix, no `\r`, at most 512 characters
/// (PROTOCOL §1.5).
#[must_use]
pub fn clean_message(raw: &str) -> Arc<str> {
    let mut text = raw.trim();
    while let Some(rest) = text.strip_prefix("ERROR: ") {
        text = rest.trim_start();
    }
    let cleaned: String = text.chars().filter(|c| *c != '\r').collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= MAX_MESSAGE_CHARS {
        return Arc::from(trimmed);
    }
    Arc::from(
        trimmed
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect::<String>()
            .as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_maps_to_its_documented_status() {
        for code in ErrorCode::ALL {
            let err = ApiError::of(code, "x");
            let expected = code.http_status().unwrap_or(500);
            assert_eq!(err.status().as_u16(), expected, "{code}");
        }
    }

    #[test]
    fn the_envelope_always_carries_all_six_keys() {
        let body = ApiError::invalid("quality", "nope").body();
        let error = body.get("error").expect("an error object");
        for key in [
            "code",
            "message",
            "field",
            "provider",
            "provider_code",
            "request_id",
        ] {
            assert!(error.get(key).is_some(), "{key} must be present");
        }
        assert_eq!(error["field"], "quality");
        assert!(error["request_id"].is_null(), "stamped by the middleware");
    }

    #[test]
    fn messages_are_cleaned() {
        assert_eq!(&*clean_message("ERROR: nope\r\n"), "nope");
        assert_eq!(&*clean_message("ERROR: ERROR: nope"), "nope");
        assert_eq!(clean_message(&"x".repeat(600)).chars().count(), 512);
    }

    #[test]
    fn a_503_carries_retry_after_and_every_body_carries_the_charset() {
        use axum::http::header;

        let response = ApiError::unavailable("the database is busy").into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json; charset=utf-8"
        );
        assert!(
            response.extensions().get::<EnvelopeStamp>().is_some(),
            "so the middleware fills in request_id"
        );

        let response = ApiError::not_found("nope").into_response();
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
        assert_eq!(
            Json(json!({ "ok": true })).into_response().headers()[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn a_wire_error_becomes_an_envelope_without_losing_a_field() {
        let wire = WireError::field(ErrorCode::UnknownPreset, "ytdl_options_presets", "nope")
            .with_provider("ytdlp", Some(Arc::from("ExtractorError")));
        let err = ApiError::from(wire);
        assert_eq!(err.code, ErrorCode::UnknownPreset);
        assert_eq!(err.field.as_deref(), Some("ytdl_options_presets"));
        assert_eq!(err.provider.as_deref(), Some("ytdlp"));
        assert_eq!(err.provider_code.as_deref(), Some("ExtractorError"));
        assert_eq!(err.status().as_u16(), 400);
    }
}
