//! Auth: cookie passthrough, an optional trusted proxy header, an optional bearer token — and
//! **never a redirect** (PROTOCOL §1.4, DESIGN §16.6).
//!
//! The server has no user model. Three mechanisms compose:
//!
//! | Configured | Effect |
//! |---|---|
//! | neither `AULOS_API_TOKEN` nor `AULOS_TRUSTED_PROXY_AUTH_HEADER` | open; cookies flow through untouched and the proxy in front decides |
//! | `AULOS_TRUSTED_PROXY_AUTH_HEADER=Remote-User` | that header must be present and non-empty |
//! | `AULOS_API_TOKEN=…` | `Authorization: Bearer <token>` must match |
//! | both | **either** satisfies the request |
//!
//! A failure is `401` with the JSON envelope. No `Location` header is ever written, so a client
//! never has to distinguish "expired session" from "the login page came back as a 200".
//!
//! The WebSocket upgrade accepts the same token two extra ways, because a reverse proxy that
//! cannot forward cookies to `<p>ws` is a real deployment (DESIGN §16.6):
//! `Sec-WebSocket-Protocol: aulos.v2, bearer.<token>` and `?token=<token>`.

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::ApiState;
use crate::error::ApiError;

/// The `bearer.` prefix a token takes when it rides in the WebSocket subprotocol list.
pub const SUBPROTOCOL_BEARER_PREFIX: &str = "bearer.";

/// The subprotocol every v2 client offers (PROTOCOL §5.1).
pub const SUBPROTOCOL: &str = "aulos.v2";

/// Constant-time string equality.
///
/// Both sides are hashed first so the comparison is independent of length as well as of content:
/// a naive `a == b` leaks the token's length through the timing of the length check, and a
/// byte-wise loop over unequal lengths has to decide what to do about the tail.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let left = Sha256::digest(a.as_bytes());
    let right = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for (x, y) in left.iter().zip(right.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Every place a bearer token can arrive on one request.
fn presented_tokens(req: &Request) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(raw) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let trimmed = raw.trim();
        // `Bearer` is case-insensitive per RFC 6750; a bare token is accepted too, because the
        // README bookmarklet and `curl -H "Authorization: <token>"` both exist in the wild.
        let value = trimmed
            .split_once(' ')
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map_or(trimmed, |(_, rest)| rest.trim());
        out.push(value.to_owned());
    }
    for offered in subprotocols(req) {
        if let Some(token) = offered.strip_prefix(SUBPROTOCOL_BEARER_PREFIX) {
            out.push(token.to_owned());
        }
    }
    if let Some(token) = query_token(req) {
        out.push(token);
    }
    out
}

/// The `Sec-WebSocket-Protocol` list, trimmed.
#[must_use]
pub fn subprotocols(req: &Request) -> Vec<String> {
    req.headers()
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `?token=` from the query string.
fn query_token(req: &Request) -> Option<String> {
    let query = req.uri().query()?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            return Some(
                percent_encoding::percent_decode_str(value)
                    .decode_utf8_lossy()
                    .into_owned(),
            );
        }
    }
    None
}

/// Whether this request satisfies the configured auth posture.
#[must_use]
pub fn authorized(state: &ApiState, req: &Request) -> bool {
    let token = state.cfg.api_token.expose().as_str();
    let proxy_header = &*state.cfg.trusted_proxy_auth_header;
    if token.is_empty() && proxy_header.is_empty() {
        return true; // cookie passthrough: the proxy decides, the server validates nothing.
    }

    if !token.is_empty()
        && presented_tokens(req)
            .iter()
            .any(|candidate| constant_time_eq(candidate, token))
    {
        return true;
    }

    !proxy_header.is_empty()
        && req
            .headers()
            .get(proxy_header)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.trim().is_empty())
}

/// The middleware, layered on every authenticated subtree: v2, the WebSocket, the file routes and
/// (when it is mounted) the v1 shim.
///
/// `healthz`, `livez`, `robots.txt`, the identity document and the `socket.io` 501 are deliberately
/// **outside** it: the container's `HEALTHCHECK` holds no token, and a stale Socket.IO client must
/// get its 501 rather than a 401 it cannot act on.
pub async fn require(State(state): State<ApiState>, req: Request, next: Next) -> Response {
    if authorized(&state, &req) {
        return next.run(req).await;
    }
    let response = ApiError::unauthorized().into_response();
    debug_assert!(
        !response.headers().contains_key(header::LOCATION),
        "a 401 must never redirect (PROTOCOL §1.4)"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_is_still_equality() {
        assert!(constant_time_eq("hunter2", "hunter2"));
        assert!(!constant_time_eq("hunter2", "hunter3"));
        assert!(!constant_time_eq("hunter2", "hunter2 "));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    fn req(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(axum::body::Body::empty()).expect("a request")
    }

    #[test]
    fn a_token_is_found_in_all_three_places() {
        assert_eq!(
            presented_tokens(&req("/", &[("authorization", "Bearer t0k")])),
            ["t0k"]
        );
        assert_eq!(
            presented_tokens(&req("/", &[("authorization", "bearer t0k")])),
            ["t0k"]
        );
        assert_eq!(
            presented_tokens(&req("/", &[("authorization", "t0k")])),
            ["t0k"]
        );
        assert_eq!(
            presented_tokens(&req(
                "/ws",
                &[("sec-websocket-protocol", "aulos.v2, bearer.t0k")]
            )),
            ["t0k"]
        );
        assert_eq!(presented_tokens(&req("/ws?token=t0k", &[])), ["t0k"]);
        assert_eq!(
            presented_tokens(&req("/ws?done=false&token=t%20k", &[])),
            ["t k"]
        );
        assert!(presented_tokens(&req("/", &[])).is_empty());
    }

    #[test]
    fn the_subprotocol_list_is_split_and_trimmed() {
        let r = req(
            "/ws",
            &[("sec-websocket-protocol", " aulos.v2 , bearer.x ")],
        );
        assert_eq!(subprotocols(&r), ["aulos.v2", "bearer.x"]);
    }
}
