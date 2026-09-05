//! The v2 REST surface (PROTOCOL §4).
//!
//! Every route here is mounted under `<p>api/v2/` except the four PROTOCOL §4.7 lists at the top
//! level (`healthz`, `livez`, `version`, `robots.txt`), which [`crate::router`] mounts outside the
//! auth layer.
//!
//! Two conventions hold everywhere and are enforced by the helpers in this module rather than by
//! discipline:
//!
//! - **A mutating request must say `Content-Type: application/json`** (PROTOCOL §1.2, DESIGN §16.6
//!   CSRF row). [`json_body`] is the only way a handler reads a body, so no route can forget.
//! - **An unknown request field is a `warnings` entry, never a 400** (PROTOCOL §4.1). There is no
//!   `deny_unknown_fields` anywhere in this crate: an App Store rollout runs mixed client and
//!   server versions for weeks, and the first client to send a new optional field must not have
//!   its whole add rejected.

pub mod actions;
pub mod cookies;
pub mod downloads;
pub mod meta;
pub mod query;
pub mod subs;

use aulos_core::ErrorCode;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Query};
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use axum::routing::{get, patch, post};
use axum::{Router, body::Bytes};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::ApiState;
use crate::error::ApiError;

/// A query-string extractor whose rejection is the **error envelope**.
///
/// `axum::extract::Query`'s own rejection is a `400` with a plain-text body, which would be the one
/// response in the whole surface that is not PROTOCOL §1.5-shaped. This wrapper is why `?since=abc`
/// answers with `{"error":{"code":"bad_request",…}}` like everything else.
#[derive(Clone, Copy, Debug)]
pub struct Q<T>(pub T);

impl<S, T> FromRequestParts<S> for Q<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(e) => Err(ApiError::bad_request(format!(
                "the query string is invalid: {}",
                e.body_text()
            ))),
        }
    }
}

/// The largest JSON request body a v2 route is expected to carry.
///
/// A 500-URL batch add with a full selection per item is ~120 KB, so 1 MiB is generous. The
/// enforced ceiling is [`cookies::BODY_LIMIT`], which has to leave room for the multipart cookie
/// upload's own 1 000 000-byte payload plus its part headers.
pub const MAX_JSON_BODY: usize = 1024 * 1024;

/// Every `<p>api/v2/*` route.
pub fn router(state: ApiState) -> Router {
    let p = state.cfg.url_prefix.clone();
    let r = |suffix: &str| p.route(&format!("api/v2/{suffix}"));

    Router::new()
        // --- the queue ---
        .route(&r("downloads"), post(downloads::add))
        .route(
            &r("downloads/cancel-resolve"),
            post(downloads::cancel_resolve),
        )
        .route(&r("items/actions"), post(actions::actions))
        .route(&r("items/clear"), post(actions::clear))
        .route(&r("items"), get(query::items))
        .route(
            &r("items/{id}"),
            get(query::item).delete(actions::delete_one),
        )
        .route(&r("items/{id}/file"), get(query::item_file))
        .route(&r("state"), get(query::state))
        // --- discovery ---
        .route(&r("capabilities"), get(meta::capabilities))
        .route(&r("catalog"), get(meta::catalog))
        .route(&r("presets"), get(meta::presets))
        .route(&r("providers"), get(meta::providers))
        .route(&r("plugins/reload"), post(meta::plugins_reload))
        .route(&r("resolve-preview"), get(meta::resolve_preview))
        .route(&r("custom-dirs"), get(meta::custom_dirs))
        .route(&r("import-report"), get(meta::import_report))
        // --- options ---
        .route(&r("ytdl-options"), get(meta::ytdl_options))
        .route(&r("ytdl-options/reload"), post(meta::ytdl_options_reload))
        .route(&r("debug/options"), get(meta::debug_options))
        // --- cookies ---
        .route(
            &r("cookies"),
            get(cookies::status)
                .post(cookies::upload)
                .delete(cookies::remove),
        )
        // --- subscriptions ---
        .route(&r("subscriptions"), get(subs::list).post(subs::create))
        .route(&r("subscriptions/check"), post(subs::check_many))
        .route(
            &r("subscriptions/{id}"),
            patch(subs::update).delete(subs::remove),
        )
        .route(&r("subscriptions/{id}/check"), post(subs::check_one))
        // One limit for the whole surface, sized for the largest legal body — the cookie upload,
        // whose own 1 000 000-byte cap is enforced in the handler so the answer is the legacy
        // `413` envelope rather than axum's bare rejection (DESIGN §16.6).
        .layer(DefaultBodyLimit::max(cookies::BODY_LIMIT))
        .with_state(state)
}

/// Reads a **required** JSON object body, enforcing `Content-Type` (PROTOCOL §1.2).
///
/// # Errors
/// `400 bad_request` for a missing or wrong content type, an unparseable body, or a body that is
/// not a JSON object.
pub fn json_body(headers: &HeaderMap, body: &Bytes) -> Result<Map<String, Value>, ApiError> {
    require_json_content_type(headers)?;
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("the request body is not valid JSON: {e}")))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ApiError::bad_request(
            "the request body must be a JSON object",
        )),
    }
}

/// Reads an **optional** JSON object body: an empty body with no `Content-Type` at all is `{}`.
///
/// This is what the four "just do it" routes take — `plugins/reload`, `ytdl-options/reload`,
/// `subscriptions/{id}/check` and `items/clear` — because a POST with nothing to say should not
/// have to carry `Content-Type: application/json` and two bytes of body.
///
/// The shortcut is conditioned on the header being **absent**, not on the body being empty, and
/// that distinction is the CSRF gate this module's header comment promises. A browser can send a
/// cross-origin form POST with an empty body and `application/x-www-form-urlencoded` without a
/// preflight; skipping the content-type check on an empty body would have let such a page reach
/// `POST api/v2/items/clear` — which deletes every terminal record, and the files on disk when
/// `DELETE_FILE_ON_TRASHCAN=true`. `curl -X POST`, which sends no `Content-Type`, still works.
///
/// # Errors
/// `400 bad_request` for a present-but-wrong `Content-Type`, or when a **non-empty** body is not
/// a JSON object.
pub fn optional_json_body(
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Map<String, Value>, ApiError> {
    if body.is_empty() && headers.get(header::CONTENT_TYPE).is_none() {
        return Ok(Map::new());
    }
    require_json_content_type(headers)?;
    if body.is_empty() {
        return Ok(Map::new());
    }
    json_body(headers, body)
}

/// `400 bad_request` unless the request says it is sending JSON.
fn require_json_content_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let raw = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let mime = raw.split(';').next().unwrap_or_default().trim();
    if mime.eq_ignore_ascii_case("application/json") {
        return Ok(());
    }
    Err(ApiError::new(
        ErrorCode::BadRequest,
        "Content-Type: application/json is required",
        Some("Content-Type"),
    ))
}

/// Collects one `warnings` entry per key that is not in `known` (PROTOCOL §4.1).
pub fn unknown_fields(body: &Map<String, Value>, known: &[&str], warnings: &mut Vec<String>) {
    for key in body.keys() {
        if !known.contains(&key.as_str()) {
            warnings.push(format!("unknown field \"{key}\" ignored"));
        }
    }
}

/// A `bool` from JSON, accepting the legacy string tokens (PROTOCOL §4.1).
///
/// `true`/`false`/`1`/`0`/`on`/`off`, case-insensitively, plus the JSON booleans and the two
/// integers. Anything else is an error naming the field.
///
/// # Errors
/// `400 validation_failed` with `field` set.
pub fn parse_bool(field: &str, value: &Value) -> Result<bool, ApiError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => match n.as_i64() {
            Some(1) => Ok(true),
            Some(0) => Ok(false),
            _ => Err(bool_error(field)),
        },
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Ok(true),
            "false" | "0" | "off" | "no" => Ok(false),
            _ => Err(bool_error(field)),
        },
        _ => Err(bool_error(field)),
    }
}

fn bool_error(field: &str) -> ApiError {
    ApiError::invalid(field, format!("{field} must be a boolean"))
}

/// A `u32` from a JSON number.
///
/// # Errors
/// `400 validation_failed` with `field` set, and the legacy message when the field is
/// `playlist_item_limit` (DESIGN §11.7).
pub fn parse_u32(field: &str, value: &Value) -> Result<u32, ApiError> {
    let message = if field == "playlist_item_limit" {
        aulos_core::request::legacy::PLAYLIST_ITEM_LIMIT.to_owned()
    } else {
        format!("{field} must be a non-negative integer")
    };
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| ApiError::invalid(field, message))
}

/// A non-empty string from JSON.
///
/// # Errors
/// `400 validation_failed` with `field` set.
pub fn parse_str<'a>(field: &str, value: &'a Value) -> Result<&'a str, ApiError> {
    value
        .as_str()
        .ok_or_else(|| ApiError::invalid(field, format!("{field} must be a string")))
}

/// An array of ids from `{"ids": […]}`.
///
/// # Errors
/// `400 validation_failed` when the key is missing, is not an array, or holds a non-string.
pub fn parse_ids(body: &Map<String, Value>) -> Result<Vec<String>, ApiError> {
    let raw = body
        .get("ids")
        .ok_or_else(|| ApiError::invalid("ids", "ids is required"))?;
    let list = raw
        .as_array()
        .ok_or_else(|| ApiError::invalid("ids", "ids must be an array of item ids"))?;
    let mut out = Vec::with_capacity(list.len());
    for value in list {
        out.push(parse_str("ids", value)?.to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(content_type: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(ct) = content_type {
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(ct).expect("a header"),
            );
        }
        h
    }

    #[test]
    fn a_mutating_body_needs_the_json_content_type() {
        let body = Bytes::from_static(b"{}");
        assert!(json_body(&headers(Some("application/json")), &body).is_ok());
        assert!(json_body(&headers(Some("application/json; charset=utf-8")), &body).is_ok());
        let err = json_body(&headers(None), &body).expect_err("no content type");
        assert_eq!(err.code, ErrorCode::BadRequest);
        let err = json_body(&headers(Some("text/plain")), &body).expect_err("wrong type");
        assert_eq!(err.code, ErrorCode::BadRequest);
        let err = json_body(&headers(Some("application/x-www-form-urlencoded")), &body)
            .expect_err("a cross-origin form POST cannot reach a v2 route");
        assert_eq!(err.code, ErrorCode::BadRequest);
    }

    #[test]
    fn an_empty_body_is_an_empty_object_only_where_it_is_optional() {
        let empty = Bytes::new();
        assert!(
            optional_json_body(&headers(None), &empty)
                .expect("`curl -X POST` sends no content type")
                .is_empty()
        );
        assert!(
            optional_json_body(&headers(Some("application/json")), &empty)
                .expect("an explicit JSON content type is fine too")
                .is_empty()
        );
        assert!(json_body(&headers(Some("application/json")), &empty).is_err());
    }

    /// The CSRF gate: a browser can send `application/x-www-form-urlencoded` with an empty body
    /// cross-origin without a preflight, so the empty-body shortcut must not skip the check when
    /// a `Content-Type` is actually present (DESIGN §16.6, CSRF row).
    #[test]
    fn a_cross_origin_form_post_cannot_reach_an_optional_body_route() {
        let empty = Bytes::new();
        for raw in [
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "text/plain",
        ] {
            let err = optional_json_body(&headers(Some(raw)), &empty).expect_err(raw);
            assert_eq!(err.code, ErrorCode::BadRequest, "{raw}");
            assert_eq!(err.field.as_deref(), Some("Content-Type"), "{raw}");
        }
    }

    #[test]
    fn a_body_that_is_not_an_object_is_a_400() {
        for raw in ["[]", "\"x\"", "3", "null", "{"] {
            let err =
                json_body(&headers(Some("application/json")), &Bytes::from(raw)).expect_err(raw);
            assert_eq!(err.code, ErrorCode::BadRequest, "{raw}");
        }
    }

    #[test]
    fn unknown_fields_become_warnings() {
        let body: Map<String, Value> =
            serde_json::from_str(r#"{"url":"x","nope":1,"also":2}"#).expect("an object");
        let mut warnings = Vec::new();
        unknown_fields(&body, &["url"], &mut warnings);
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|w| w.contains("nope")));
    }

    #[test]
    fn booleans_accept_the_legacy_token_set() {
        for raw in ["true", "1", "on", "TRUE", "yes"] {
            assert!(parse_bool("auto_start", &Value::String(raw.into())).expect(raw));
        }
        for raw in ["false", "0", "off", "No"] {
            assert!(!parse_bool("auto_start", &Value::String(raw.into())).expect(raw));
        }
        assert!(parse_bool("auto_start", &Value::Bool(true)).expect("json true"));
        assert!(!parse_bool("auto_start", &serde_json::json!(0)).expect("json 0"));
        let err = parse_bool("auto_start", &Value::String("maybe".into())).expect_err("nonsense");
        assert_eq!(err.field.as_deref(), Some("auto_start"));
    }

    #[test]
    fn playlist_item_limit_keeps_the_legacy_message() {
        let err = parse_u32("playlist_item_limit", &Value::String("3".into()))
            .expect_err("a string is not an integer");
        assert_eq!(
            &*err.message,
            aulos_core::request::legacy::PLAYLIST_ITEM_LIMIT
        );
    }
}
