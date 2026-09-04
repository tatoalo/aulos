//! The v1 compatibility shim (DESIGN §11, PROTOCOL §10).
//!
//! Purpose: the **currently shipped** iOS build (`metube_ios` @ `8622a2f`), the README bookmarklet
//! and the iOS Shortcut keep working unchanged through cutover. This is a pure translation layer
//! over the v2 core — it builds [`aulos_core::DownloadRequest`]s and [`aulos_queue::EngineCmd`]s
//! and projects [`aulos_core::ItemView`] down to the legacy shape. It owns **no state**, and it is
//! mounted iff `AULOS_V1_ENABLED` (default `true`), so switching it off after the v2 client ships
//! is one environment variable and removing it is one line in [`crate::router`].
//!
//! # The two response shapes, and how to tell which a route uses
//!
//! Legacy had exactly two ways of failing, and the shim answers differently for each. This is the
//! single most important thing to understand about reading a v1 response:
//!
//! | Legacy | Shim | Routes |
//! |---|---|---|
//! | `web.HTTPBadRequest(reason='<message>')` — aiohttp put the message in the **status line** and served a `text/plain` body of `400: <message>` | the same status with the §1.5 **error envelope**, `error.message` carrying the reason verbatim | `add`, `subscribe`, `subscriptions/{update,delete,check}`, `delete`, `start` |
//! | `web.Response(status=400, text=json({'status':'error','msg':…}))` — a real JSON body | the **same JSON body**, unchanged | the three cookie routes |
//! | `web.Response(text=json({'status':'error','msg':…}))` at **200** — a *business* failure | the same body at 200 | `add`'s resolution failures, `subscribe`'s duplicate, `subscriptions/update`'s unknown id |
//!
//! The envelope in row 1 is a **deliberate** change of shape, not a mismatch: the message a client
//! renders is preserved byte for byte, and it moves from a status line no HTTP client library
//! exposes conveniently into a JSON field every one of them does. Row 3 is why the shipped
//! `AddResultClassifier` still works: it parses the body, not the status.
//!
//! # What is not here
//!
//! `<p>version`, `<p>robots.txt`, `<p>`, `<p>socket.io/*` and the two file routes are served by
//! [`crate::v2::meta`] and [`crate::files`] for **both** protocol versions — PROTOCOL §10.1 lists
//! them under v1 because a v1 client uses them, not because the shim re-implements them. Socket.IO
//! is the one accepted regression (DESIGN §11.6): `<p>socket.io/*` answers `501 socketio_removed`
//! so a stale client fails loudly instead of hanging on a handshake.
//!
//! # Auth and CORS
//!
//! Every route below sits behind the same [`crate::auth::require`] layer as v2 — with neither
//! `AULOS_API_TOKEN` nor `AULOS_TRUSTED_PROXY_AUTH_HEADER` set (the stock configuration) that is
//! open, and cookies flow through untouched. The two `GET /` redirects stay outside it: a redirect
//! carries no data and legacy's index needed no credentials.
//!
//! CORS is **legacy's**, not v2's: [`legacy_cors`] reproduces `on_prepare` exactly — if `Origin`
//! is present and allowed, set `Access-Control-Allow-Origin: <origin>` and
//! `Access-Control-Allow-Headers: Content-Type`, and nothing else. No methods header, no
//! `Vary`, no credentials, on every response including a 401 (DESIGN §11.6).

pub mod actions;
pub mod add;
pub mod cookies;
pub mod history;
pub mod legacy;
pub mod request;
pub mod subs;

use std::collections::BTreeSet;

use aulos_core::ItemId;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, options, post};
use axum::{Router, body::Bytes};
use serde_json::{Map, Value, json};

use crate::ApiState;
use crate::error::{ApiError, Json};

/// The nine routes legacy registered an `OPTIONS` handler for (`app/main.py:1011-1019`).
///
/// Not `history`, `delete`, `start`, `presets`, `version` or `cookie-status`: legacy did not
/// register those, and adding them would be inventing a preflight surface the Angular UI never
/// needed. `Access-Control-Allow-Origin` still reaches them through [`legacy_cors`], which is what
/// a simple cross-origin `GET` actually requires.
pub const OPTIONS_ROUTES: [&str; 9] = [
    "add",
    "cancel-add",
    "subscribe",
    "subscriptions",
    "subscriptions/update",
    "subscriptions/delete",
    "subscriptions/check",
    "upload-cookies",
    "delete-cookies",
];

/// Every v1 route, with the legacy CORS behaviour and the shared auth layer.
///
/// Mounted from [`crate::router`] **after** the v2 CORS layer, so these routes get legacy's two
/// headers rather than v2's method/`Vary`/`Max-Age` set.
pub fn router(state: ApiState) -> Router {
    let p = state.cfg.url_prefix.clone();

    let mut guarded: Router<ApiState> = Router::new()
        .route(&p.route("add"), post(add::add))
        .route(&p.route("cancel-add"), post(add::cancel_add))
        .route(&p.route("presets"), get(presets))
        .route(&p.route("history"), get(history_route))
        .route(&p.route("delete"), post(actions::delete))
        .route(&p.route("start"), post(actions::start))
        .route(&p.route("subscribe"), post(subs::subscribe))
        .route(&p.route("subscriptions"), get(subs::list))
        .route(&p.route("subscriptions/update"), post(subs::update))
        .route(&p.route("subscriptions/delete"), post(subs::delete))
        .route(&p.route("subscriptions/check"), post(subs::check))
        .route(&p.route("upload-cookies"), post(cookies::upload))
        .route(&p.route("delete-cookies"), post(cookies::remove))
        .route(&p.route("cookie-status"), get(cookies::status));

    for suffix in OPTIONS_ROUTES {
        guarded = guarded.route(&p.route(suffix), options(cors_preflight));
    }

    let guarded = guarded
        // Sized for the largest legal v1 body — the cookie upload's own 1 000 000 bytes plus its
        // multipart part headers — so a just-over-cap upload reaches the handler and gets the
        // legacy message instead of axum's bare rejection.
        .layer(DefaultBodyLimit::max(crate::v2::cookies::BODY_LIMIT))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require,
        ))
        .with_state(state.clone());

    // `GET /` and `GET /<prefix without its trailing slash>` → `302` to the prefix, exactly as
    // legacy's two `index_redirect_*` handlers did. Only when the prefix is not root, because
    // otherwise `GET /` is the identity document.
    let mut open: Router = Router::new();
    if !state.cfg.url_prefix.is_root() {
        let to = state.cfg.url_prefix.as_str().to_owned();
        let trimmed = to.trim_end_matches('/').to_owned();
        let root_target = to.clone();
        open = open
            .route(
                "/",
                get(move || {
                    let to = root_target.clone();
                    async move { redirect(&to) }
                }),
            )
            .route(
                &trimmed,
                get(move || {
                    let to = to.clone();
                    async move { redirect(&to) }
                }),
            );
    }

    guarded
        .merge(open)
        .layer(axum::middleware::from_fn_with_state(state, legacy_cors))
}

/// `302` to the prefix.
///
/// Legacy used `web.HTTPFound`, which is a **302**. `axum::response::Redirect` has helpers for
/// 303, 307 and 308 but not for 302, and the difference is observable: a client that follows a
/// 303 rewrites the method to `GET`, which is wrong for anything but this idempotent index hop.
fn redirect(to: &str) -> Response {
    match HeaderValue::from_str(to) {
        Ok(value) => (StatusCode::FOUND, [(header::LOCATION, value)]).into_response(),
        // Unreachable: the prefix is normalised to `/`-delimited ASCII by `Prefix::normalize`.
        Err(_) => StatusCode::FOUND.into_response(),
    }
}

/// `GET <p>presets` — `{"presets": ["a","b"]}`, sorted, exactly as legacy's
/// `sorted(config.YTDL_OPTIONS_PRESETS.keys())`.
pub async fn presets(State(state): State<ApiState>) -> Json<Value> {
    Json(json!({ "presets": crate::v2::meta::preset_names(&state) }))
}

/// `GET <p>history` — the three-array projection (DESIGN §11.4).
///
/// # Errors
/// `503 state_unavailable` when the read pool is busy.
pub async fn history_route(
    State(state): State<ApiState>,
) -> Result<Json<history::V1History>, ApiError> {
    Ok(Json(history::history(&state).await?))
}

/// The shared `OPTIONS` handler: `{"status":"ok"}` plus whatever [`legacy_cors`] adds.
async fn cors_preflight() -> Json<Value> {
    Json(status_ok())
}

/// Legacy's `on_prepare`, verbatim (DESIGN §11.6).
///
/// Two headers, no more: `Access-Control-Allow-Origin: <the request's Origin>` and
/// `Access-Control-Allow-Headers: Content-Type`, set only when an `Origin` is present *and*
/// `CORS_ALLOWED_ORIGINS` allows it. Deliberately not a `tower_http::cors::CorsLayer`: that layer
/// answers a real preflight itself and emits `Access-Control-Allow-Headers` only on one, while
/// legacy put both headers on **every** response — which is what a client written against the
/// Python server observed.
async fn legacy_cors(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut response = next.run(req).await;
    let Some(origin) = origin else {
        return response;
    };
    if !state.cfg.cors_allowed_origins.allows(&origin) {
        return response;
    }
    if let Ok(value) = HeaderValue::from_str(&origin) {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type"),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Reads a JSON **object** body the way `_read_json_request` did.
///
/// `Content-Type` is deliberately **not** enforced: `request.json()` ignored it, and the README
/// bookmarklet and a hand-rolled `curl` both exist in the wild without it. The v2 surface is
/// strict here (DESIGN §16.6's CSRF row) because it has no such installed base.
///
/// # Errors
/// `400 Invalid JSON request body` when the bytes are not JSON — an **empty** body included, since
/// Python raised `JSONDecodeError` on `b""` — and `400 JSON request body must be an object` when
/// they parse to anything but an object, `null` included.
pub fn read_json_object(body: &Bytes) -> Result<Map<String, Value>, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::bad_request(legacy::INVALID_JSON_BODY))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ApiError::bad_request(legacy::BODY_MUST_BE_OBJECT)),
    }
}

/// `{"status":"ok"}` — the body eight legacy routes answered with.
#[must_use]
pub fn status_ok() -> Value {
    json!({ "status": "ok" })
}

/// `{"status":"error","msg":…}` — a legacy *business* failure, at whatever status the route uses.
#[must_use]
pub fn status_error(message: &str) -> Value {
    json!({ "status": "error", "msg": message })
}

/// `POST <p>add`'s success body: `{"status":"ok"}` plus the additive `ids` old clients ignore.
#[must_use]
pub fn ok_body(ids: &[ItemId]) -> Value {
    json!({ "status": "ok", "ids": ids })
}

/// The configured preset names, as the set [`request::parse_download_options`] checks against.
#[must_use]
pub fn known_presets(state: &ApiState) -> BTreeSet<Box<str>> {
    crate::v2::meta::preset_names(state)
        .into_iter()
        .map(String::into_boxed_str)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::config::{RawEnv, load};

    #[test]
    fn an_empty_body_is_the_invalid_json_message_and_null_is_the_object_one() {
        let err = read_json_object(&Bytes::new()).expect_err("empty");
        assert_eq!(&*err.message, legacy::INVALID_JSON_BODY);
        let err = read_json_object(&Bytes::from_static(b"{")).expect_err("truncated");
        assert_eq!(&*err.message, legacy::INVALID_JSON_BODY);
        for raw in ["null", "[]", "\"x\"", "3", "true"] {
            let err = read_json_object(&Bytes::from(raw)).expect_err(raw);
            assert_eq!(&*err.message, legacy::BODY_MUST_BE_OBJECT, "{raw}");
        }
        assert!(
            read_json_object(&Bytes::from_static(b"{}"))
                .expect("ok")
                .is_empty()
        );
    }

    #[test]
    fn the_two_status_bodies_are_the_legacy_shapes() {
        assert_eq!(status_ok(), json!({ "status": "ok" }));
        assert_eq!(
            status_error("nope"),
            json!({ "status": "error", "msg": "nope" })
        );
    }

    #[test]
    fn the_nine_options_routes_are_the_ones_legacy_registered() {
        assert_eq!(OPTIONS_ROUTES.len(), 9);
        for expected in [
            "add",
            "cancel-add",
            "subscribe",
            "subscriptions",
            "subscriptions/update",
            "subscriptions/delete",
            "subscriptions/check",
            "upload-cookies",
            "delete-cookies",
        ] {
            assert!(OPTIONS_ROUTES.contains(&expected), "{expected}");
        }
        assert!(
            !OPTIONS_ROUTES.contains(&"history"),
            "legacy registered no OPTIONS for history"
        );
    }

    #[test]
    fn the_prefix_builds_every_route_through_the_newtype() {
        let cfg = load(&RawEnv::from_pairs(vec![(
            "URL_PREFIX".to_owned(),
            "metube".to_owned(),
        )]))
        .unwrap();
        assert_eq!(cfg.url_prefix.route("add"), "/metube/add");
        assert_eq!(
            cfg.url_prefix.route("subscriptions/update"),
            "/metube/subscriptions/update"
        );
        assert!(!cfg.url_prefix.is_root());
        assert_eq!(cfg.url_prefix.as_str().trim_end_matches('/'), "/metube");
    }
}
