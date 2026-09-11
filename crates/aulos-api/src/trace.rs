//! Request ids, the two headers every response carries, the error-envelope stamp and the access
//! log (PROTOCOL §1.3, DESIGN §16.5).
//!
//! One middleware does all four because they are one decision per request:
//!
//! | Concern | Rule |
//! |---|---|
//! | `X-Request-Id` | echoed from the request when it is a sane token, else a fresh ULID |
//! | `X-Aulos-Seq` | [`EventHub::head`] at the moment the response was produced (PROTOCOL §6.4) |
//! | error bodies | a response stamped [`EnvelopeStamp`] gets its `error.request_id` filled in |
//! | the access log | one line per request at INFO when `ENABLE_ACCESSLOG`, else DEBUG |
//!
//! The logged path is [`redact_path`]-ed. `api/v2/devices/{token}` (DESIGN §25.8) is the only
//! route whose path segment is a secret, and the rule is per-segment rather than per-route so a
//! future one cannot be added without it.
//!
//! [`EventHub::head`]: aulos_queue::EventHub::head

use std::time::Instant;

use aulos_core::ItemId;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

use crate::ApiState;
use crate::error::EnvelopeStamp;

/// `X-Request-Id`, echoed or minted.
pub const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// `X-Aulos-Seq`: the frame cursor the response was produced at (PROTOCOL §1.3).
pub const AULOS_SEQ: HeaderName = HeaderName::from_static("x-aulos-seq");

/// The largest error body the envelope stamp will buffer. An envelope is ~300 bytes; anything
/// bigger is not one.
const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

/// A fresh request id.
///
/// PROTOCOL §1.3 says a ULID. `ulid` itself is not in `aulos-api`'s DESIGN §3 dependency row, so
/// the mint goes through the core newtype that wraps it — [`ItemId::new`] is a ULID generator with
/// a domain name on it, and no item is created here.
#[must_use]
pub fn new_request_id() -> String {
    ItemId::new().to_string()
}

/// Whether an inbound `X-Request-Id` is safe to echo: 1..=64 printable ASCII characters.
fn usable(raw: &str) -> bool {
    (1..=64).contains(&raw.len()) && raw.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// The request id for this request: the client's when it is usable, else a fresh one.
fn request_id_for(req: &Request) -> String {
    req.headers()
        .get(REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|raw| usable(raw))
        .map_or_else(new_request_id, ToOwned::to_owned)
}

/// The middleware. Wraps the whole router, v1 shim included.
pub async fn headers(State(state): State<ApiState>, mut req: Request, next: Next) -> Response {
    let started = Instant::now();
    let id = request_id_for(&req);
    let method = req.method().clone();
    let path = redact_path(req.uri().path());

    // Handlers that want the id (none do today) can read it from the extensions rather than
    // re-deriving it, so there is exactly one id per request.
    req.extensions_mut().insert(RequestId(id.clone()));

    let mut response = next.run(req).await;

    if response.extensions().get::<EnvelopeStamp>().is_some() {
        response = stamp_envelope(response, &id).await;
    }

    let seq = state.hub.head().0;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID, value);
    }
    response.headers_mut().insert(
        AULOS_SEQ,
        HeaderValue::from_str(&seq.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );

    let status = response.status().as_u16();
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    if state.cfg.enable_accesslog {
        tracing::info!(%method, %path, status, latency_ms, request_id = %id, "request");
    } else {
        tracing::debug!(%method, %path, status, latency_ms, request_id = %id, "request");
    }
    response
}

/// A path segment must be at least this long, and entirely hexadecimal, to be redacted. It is
/// the devices routes' own `TOKEN_MIN`, so anything they would accept as a token is redacted.
const SECRET_SEGMENT_MIN: usize = 32;

/// How much of a redacted segment survives: enough to correlate two lines, not enough to push to.
const SECRET_SEGMENT_KEEP: usize = 8;

/// The path as it may be logged: every long hexadecimal segment shortened to its first
/// [`SECRET_SEGMENT_KEEP`] characters.
///
/// `PUT api/v2/devices/9f3c…e21b` carries an APNs device token *in the URL*, and the access log
/// writes every path — at INFO with `ENABLE_ACCESSLOG`, and at DEBUG otherwise, which is exactly
/// the level an operator turns up to debug push. Nothing else on the v2 surface puts a secret in a
/// path, and a ULID is not hexadecimal, so no ordinary route is touched.
#[must_use]
pub fn redact_path(path: &str) -> String {
    if !path
        .split('/')
        .any(|s| s.len() >= SECRET_SEGMENT_MIN && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return path.to_owned();
    }
    path.split('/')
        .map(|segment| {
            if segment.len() >= SECRET_SEGMENT_MIN && segment.bytes().all(|b| b.is_ascii_hexdigit())
            {
                let mut short = segment[..SECRET_SEGMENT_KEEP].to_owned();
                short.push('\u{2026}');
                short
            } else {
                segment.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The request id, in the request extensions.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Rewrites `error.request_id` in a stamped envelope.
///
/// A failure to buffer or re-parse leaves the response alone: the header still carries the id, so
/// the worst case is an envelope whose `request_id` stays `null`, never a broken body.
async fn stamp_envelope(response: Response, id: &str) -> Response {
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ENVELOPE_BYTES).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Some(error) = value.get_mut("error").and_then(|e| e.as_object_mut()) {
        error.insert(
            "request_id".to_owned(),
            serde_json::Value::String(id.to_owned()),
        );
    }
    let Ok(text) = serde_json::to_vec(&value) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_request_id_is_a_ulid() {
        let id = new_request_id();
        assert_eq!(id.len(), 26, "{id}");
        assert!(usable(&id));
    }

    #[test]
    fn a_device_token_in_the_path_is_never_logged_in_full() {
        let token = "9f3c1a2b".repeat(8); // a 64-hex APNs device token
        let logged = redact_path(&format!("/api/v2/devices/{token}"));
        assert!(!logged.contains(&token), "{logged}");
        assert_eq!(logged, "/api/v2/devices/9f3c1a2b\u{2026}");

        let item = "01JBQ7Z5T9K3M2R8V4XW6Y0AAA";
        let logged = redact_path(&format!("/api/v2/devices/{token}/live-activities/{item}"));
        assert!(!logged.contains(&token), "{logged}");
        assert!(logged.ends_with(item), "a ULID is not a secret: {logged}");
    }

    #[test]
    fn an_ordinary_path_is_logged_verbatim() {
        for path in [
            "/api/v2/items",
            "/api/v2/items/01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
            "/healthz",
            "/",
            "",
        ] {
            assert_eq!(redact_path(path), path);
        }
    }

    #[test]
    fn only_sane_client_ids_are_echoed() {
        assert!(usable("01JBQ7Z5T9K3M2R8V4XW6Y0AAA"));
        assert!(!usable(""));
        assert!(!usable("has space"));
        assert!(!usable(&"x".repeat(65)));
        assert!(!usable("new\nline"));
    }
}
