//! CORS: legacy origin reflection, and the methods v2 needs (DESIGN §11.6, §16.6).
//!
//! `CORS_ALLOWED_ORIGINS` keeps its legacy meaning — empty sends no header at all, `*` reflects
//! any origin, otherwise only the listed origins are reflected. Credentials are **never** granted,
//! which together with "every mutating v2 route requires `Content-Type: application/json`" is what
//! keeps a cross-origin form POST out of the queue (DESIGN §16.6).
//!
//! The one difference between the two surfaces is the method list: v1 answered `OPTIONS` with the
//! two verbs legacy used, while v2 has `PATCH` and `DELETE` routes and must advertise them.

use aulos_core::CorsOrigins;
use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// The request headers a browser client may send (`Authorization` for the bearer token,
/// `X-Request-Id` so a web client can correlate its own logs, and `X-Aulos-Client` /
/// `X-Aulos-Install` because PROTOCOL §1.3 promises neither header can make a request fail — a
/// preflight rejection would).
const ALLOWED_HEADERS: [HeaderName; 5] = [
    header::CONTENT_TYPE,
    header::AUTHORIZATION,
    crate::trace::REQUEST_ID,
    crate::v2::downloads::CLIENT_HEADER,
    crate::v2::downloads::INSTALL_HEADER,
];

/// The response headers a browser client must be able to read.
const EXPOSED_HEADERS: [HeaderName; 4] = [
    crate::trace::REQUEST_ID,
    crate::trace::AULOS_SEQ,
    header::ETAG,
    header::CONTENT_RANGE,
];

/// The v2 method set: everything the §4.7 table uses.
const V2_METHODS: [Method; 6] = [
    Method::GET,
    Method::HEAD,
    Method::POST,
    Method::PATCH,
    Method::DELETE,
    Method::OPTIONS,
];

/// The legacy method set: v1 is `GET` and `POST` only.
const V1_METHODS: [Method; 4] = [Method::GET, Method::HEAD, Method::POST, Method::OPTIONS];

/// How the configured origins map onto `Access-Control-Allow-Origin`.
fn allow_origin(origins: &CorsOrigins) -> Option<AllowOrigin> {
    match origins {
        CorsOrigins::None => None,
        CorsOrigins::Any => Some(AllowOrigin::any()),
        CorsOrigins::List(list) => {
            let values: Vec<HeaderValue> = list
                .iter()
                .filter_map(|o| HeaderValue::from_str(o).ok())
                .collect();
            Some(AllowOrigin::list(values))
        }
    }
}

/// The v2 CORS layer, or `None` when `CORS_ALLOWED_ORIGINS` is empty.
#[must_use]
pub fn v2(origins: &CorsOrigins) -> Option<CorsLayer> {
    Some(
        CorsLayer::new()
            .allow_origin(allow_origin(origins)?)
            .allow_methods(V2_METHODS)
            .allow_headers(ALLOWED_HEADERS)
            .expose_headers(EXPOSED_HEADERS),
    )
}

/// The legacy CORS layer, for the v1 shim's routes (WP-15 layers it on its own router).
#[must_use]
pub fn v1(origins: &CorsOrigins) -> Option<CorsLayer> {
    Some(
        CorsLayer::new()
            .allow_origin(allow_origin(origins)?)
            .allow_methods(V1_METHODS)
            .allow_headers(ALLOWED_HEADERS)
            .expose_headers(EXPOSED_HEADERS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_setting_installs_no_layer() {
        assert!(v2(&CorsOrigins::None).is_none());
        assert!(v1(&CorsOrigins::None).is_none());
    }

    #[test]
    fn a_star_and_a_list_both_install_one() {
        assert!(v2(&CorsOrigins::Any).is_some());
        assert!(v2(&CorsOrigins::List(vec!["https://a.test".into()])).is_some());
    }

    /// PROTOCOL §1.3 promises the two attribution headers can never make a request fail. From a
    /// browser that promise *is* the preflight: a header outside `Access-Control-Allow-Headers`
    /// never reaches the handler, so the page's add would fail before the server saw it.
    #[test]
    fn both_attribution_headers_survive_a_preflight() {
        assert!(ALLOWED_HEADERS.contains(&crate::v2::downloads::CLIENT_HEADER));
        assert!(ALLOWED_HEADERS.contains(&crate::v2::downloads::INSTALL_HEADER));
    }

    #[test]
    fn v2_advertises_the_verbs_its_routes_use() {
        // A documentation-grade assertion: PATCH and DELETE exist only in v2 (PROTOCOL §4.7), so
        // a client that talks to `api/v2/subscriptions/{id}` from a browser needs them.
        assert!(V2_METHODS.contains(&Method::PATCH));
        assert!(V2_METHODS.contains(&Method::DELETE));
        assert!(!V1_METHODS.contains(&Method::PATCH));
    }
}
