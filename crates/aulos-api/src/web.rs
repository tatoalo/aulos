//! The embedded web UI: `GET <p>` (content-negotiated), `<p>assets/*` and
//! `<p>manifest.webmanifest`.
//!
//! Everything this module serves is compiled into the binary with `include_str!`/`include_bytes!`
//! from `crates/aulos-api/web/`, so a deployment is still one file and there is no static root to
//! mis-mount. The page is vanilla HTML/CSS/ES modules — no bundler, no npm, no external origin —
//! which is what lets the Content-Security-Policy below be as tight as it is.
//!
//! # Shape
//!
//! | Route | Body | Content-Type |
//! |---|---|---|
//! | `GET <p>` with `Accept: text/html` | the rendered `index.html` | `text/html; charset=utf-8` |
//! | `GET <p>` otherwise | the identity document, **unchanged** | `application/json` |
//! | `GET <p>assets/app.css` | the stylesheet | `text/css; charset=utf-8` |
//! | `GET <p>assets/app.js` | the ES module | `text/javascript; charset=utf-8` |
//! | `GET <p>assets/icon.svg` | the icon | `image/svg+xml` |
//! | `GET <p>assets/icon-180.png` | the Apple touch icon | `image/png` |
//! | `GET <p>manifest.webmanifest` | the PWA manifest | `application/manifest+json` |
//!
//! # Why these routes are outside the auth middleware
//!
//! [`crate::router`] mounts them on the *open* subtree, alongside `healthz` and the identity
//! document. They contain nothing secret — the page is the same bytes for every deployment — and
//! the alternative is a chicken-and-egg: a browser cannot send `Authorization: Bearer …` on a
//! document navigation, so an authenticated `index.html` would answer the very first request with
//! a 401 the user has no way to act on. The page instead treats a 401 from any **API** call as
//! "ask for the token", and every API route keeps its auth exactly as it was (PROTOCOL §1.4).
//!
//! # Templating
//!
//! `index.html` and the manifest are templates with two substitutions and no others:
//! [`PREFIX_PLACEHOLDER`] becomes `URL_PREFIX` (always `/`-delimited on both ends) and
//! [`THEME_PLACEHOLDER`] becomes `DEFAULT_THEME`. Both are server configuration; **no request
//! data and no user data ever reaches the HTML**, which is what makes rendering by string
//! replacement safe here and would not make it safe anywhere else.
//!
//! # Caching
//!
//! Every response carries a strong `ETag` — the hex SHA-256 of the body actually served — and
//! `Cache-Control: no-cache`, which means "revalidate every time" rather than "do not store": the
//! browser keeps the bytes and a reload costs one conditional request per file, answered with a
//! bodyless `304` — `If-None-Match` is matched against the weak form `W/"…"` too, because a
//! gzipping reverse proxy weakens the tag on its way out and the browser replays what it stored.
//! `GET <p>` additionally carries `Vary: Accept`, so an intermediary cannot serve one caller's
//! representation to the other. The static assets hash themselves once ([`ASSETS`]); the two templates are
//! rendered and hashed once per `(prefix, theme)` pair and memoised, because a process serves
//! exactly one such pair and a test process serves two.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

use aulos_core::{Prefix, Theme};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::ApiState;

// ---------------------------------------------------------------------------
// the embedded files
// ---------------------------------------------------------------------------

/// `index.html`, before substitution.
const INDEX_TEMPLATE: &str = include_str!("../web/index.html");
/// The manifest, before substitution.
const MANIFEST_TEMPLATE: &str = include_str!("../web/manifest.webmanifest");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
const ICON_SVG: &str = include_str!("../web/icon.svg");
const ICON_180_PNG: &[u8] = include_bytes!("../web/icon-180.png");

/// The token `URL_PREFIX` replaces.
pub const PREFIX_PLACEHOLDER: &str = "{{PREFIX}}";

/// The token `DEFAULT_THEME` replaces.
pub const THEME_PLACEHOLDER: &str = "{{THEME}}";

// ---------------------------------------------------------------------------
// media types and headers
// ---------------------------------------------------------------------------

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const SVG: &str = "image/svg+xml";
const PNG: &str = "image/png";
const WEBMANIFEST: &str = "application/manifest+json";

/// The policy on `index.html`, and on nothing else.
///
/// `default-src 'none'` plus an explicit allowance per directive the page actually uses. There is
/// no `'unsafe-inline'` in either `script-src` or `style-src`, so every line of CSS and JS has to
/// live in the two `assets/` files — the constraint that keeps a future edit from quietly
/// introducing an inline handler. `img-src` allows `data:` for inline SVG-as-image and nothing
/// remote; `connect-src 'self'` covers both `fetch` and the WebSocket, which is same-origin by
/// construction (the page builds every URL from the prefix meta tag).
///
/// It is **not** applied to the API's JSON responses: a policy on `application/json` protects
/// nothing and only muddies what the header means when a browser does render one.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' \
                       data:; connect-src 'self'; manifest-src 'self'; font-src 'self'; base-uri \
                       'none'; form-action 'none'; frame-ancestors 'none'";

/// Every UI response carries these three besides `Content-Type` and `ETag`.
///
/// `no-cache` is revalidate-always, not do-not-store (see the module docs). `nosniff` matters most
/// for `app.js`: without it a proxy that rewrites `Content-Type` could get the module treated as
/// something else. `no-referrer` keeps a private instance's hostname out of any URL the page ever
/// links to.
const COMMON: [(HeaderName, &str); 3] = [
    (header::CACHE_CONTROL, "no-cache"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "no-referrer"),
];

// ---------------------------------------------------------------------------
// the static asset table
// ---------------------------------------------------------------------------

/// One embedded file that needs no templating: its bytes, its media type, and the strong ETag
/// computed once at first use.
pub struct Asset {
    /// The bytes served verbatim. `Bytes::from_static`, so a response body is a refcount bump.
    pub body: Bytes,
    /// The `Content-Type`.
    pub content_type: &'static str,
    /// The quoted strong entity tag, `"<64 hex digits>"`.
    pub etag: String,
}

impl Asset {
    fn new(body: &'static [u8], content_type: &'static str) -> Self {
        Self {
            body: Bytes::from_static(body),
            content_type,
            etag: etag_of(body),
        }
    }
}

/// `assets/app.css` in [`ASSETS`].
const APP_CSS_AT: usize = 0;
/// `assets/app.js` in [`ASSETS`].
const APP_JS_AT: usize = 1;
/// `assets/icon.svg` in [`ASSETS`].
const ICON_SVG_AT: usize = 2;
/// `assets/icon-180.png` in [`ASSETS`].
const ICON_180_AT: usize = 3;

/// The four non-templated assets, keyed by their route suffix.
///
/// The suffix rides along with the bytes so [`router`]'s path list and this table cannot drift
/// apart unnoticed; the handlers themselves index by the `*_AT` constants, which costs nothing and
/// cannot miss.
static ASSETS: LazyLock<[(&'static str, Asset); 4]> = LazyLock::new(|| {
    [
        ("assets/app.css", Asset::new(APP_CSS.as_bytes(), CSS)),
        ("assets/app.js", Asset::new(APP_JS.as_bytes(), JS)),
        ("assets/icon.svg", Asset::new(ICON_SVG.as_bytes(), SVG)),
        ("assets/icon-180.png", Asset::new(ICON_180_PNG, PNG)),
    ]
});

// ---------------------------------------------------------------------------
// the two templates
// ---------------------------------------------------------------------------

/// A rendered template plus the ETag of exactly those bytes.
struct Rendered {
    body: Bytes,
    etag: String,
}

impl Rendered {
    fn render(template: &str, prefix: &Prefix, theme: Theme) -> Self {
        let body = template
            .replace(PREFIX_PLACEHOLDER, prefix.as_str())
            .replace(THEME_PLACEHOLDER, theme.as_str());
        Self {
            etag: etag_of(body.as_bytes()),
            body: Bytes::from(body),
        }
    }

    /// The rendered text. Test-only: a response never needs it as `str`.
    #[cfg(test)]
    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or_default()
    }
}

/// The memo behind one template: what it renders to for each `(prefix, theme)` the process has
/// been asked for.
type RenderCache = LazyLock<RwLock<HashMap<(Box<str>, Theme), Arc<Rendered>>>>;

/// `index.html` per `(prefix, theme)`. A process has one pair; the test suite has four.
static INDEX_CACHE: RenderCache = LazyLock::new(|| RwLock::new(HashMap::new()));

/// The manifest per prefix. `{{THEME}}` is not meaningful in it, but substituting both tokens
/// through one code path is what stops the two renderers from diverging.
static MANIFEST_CACHE: RenderCache = LazyLock::new(|| RwLock::new(HashMap::new()));

/// The memoised render of `template` for this state's prefix and theme.
///
/// A poisoned lock is impossible in practice (nothing under it can panic) but must not take the
/// server down, so it is recovered rather than propagated.
fn rendered(cache: &RenderCache, template: &str, state: &ApiState) -> Arc<Rendered> {
    let key = (
        Box::from(state.cfg.url_prefix.as_str()),
        state.cfg.default_theme,
    );
    if let Some(hit) = cache
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Arc::clone(hit);
    }
    let value = Arc::new(Rendered::render(
        template,
        &state.cfg.url_prefix,
        state.cfg.default_theme,
    ));
    cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, Arc::clone(&value));
    value
}

// ---------------------------------------------------------------------------
// content negotiation
// ---------------------------------------------------------------------------

/// Whether this `Accept` header asks for HTML.
///
/// Deliberately simple, and deliberately **not** RFC 9110 §12.5.1 negotiation: `q` values are
/// parsed off and discarded, and the comma-separated list is treated as a **set of media ranges**.
/// It answers `true` when that set contains `text/html` or `text/*`, and `false` otherwise.
///
/// The judgement call is `*/*`, which does **not** count. Every browser puts a literal `text/html`
/// first on a document navigation (`text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8`),
/// so nothing is lost — and `*/*` is exactly what `curl` and `reqwest` send by default, and what a
/// client that never thought about the header sends. Treating it as a vote for HTML would change
/// the answer `GET <p>` has always given those callers, which is the one thing this route may not
/// do: the identity document is what `curl`, the iOS app, the container's own tooling and every
/// existing script read, and it comes back from them byte for byte.
///
/// A wildcard *after* a `text/html` member changes nothing, since membership is all that is
/// tested — which is what "`*/*` preceded by `text/html`" amounts to in practice.
#[must_use]
pub fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .filter_map(|entry| entry.split(';').next())
        .map(str::trim)
        .any(|range| {
            range.eq_ignore_ascii_case("text/html") || range.eq_ignore_ascii_case("text/*")
        })
}

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

/// The asset and manifest routes, mounted only when `AULOS_WEB_UI` is true.
///
/// `GET <p>` is **not** here: it exists in both postures and [`crate::router`] owns it, so that
/// disabling the UI cannot accidentally unmount the identity document.
pub fn router(state: ApiState) -> Router {
    let p = state.cfg.url_prefix.clone();
    Router::new()
        .route(&p.route("assets/app.css"), get(app_css))
        .route(&p.route("assets/app.js"), get(app_js))
        .route(&p.route("assets/icon.svg"), get(icon_svg))
        .route(&p.route("assets/icon-180.png"), get(icon_180_png))
        .route(&p.route("manifest.webmanifest"), get(manifest))
        .with_state(state)
}

/// `GET <p>` — the page for a browser, the identity document for everyone else.
///
/// `HEAD` is answered by axum's `get` router, which runs this and drops the body.
pub async fn root(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    // One URL, two representations: a shared cache that keys on the URL alone would otherwise be
    // free to replay the page to `curl` and the JSON to a browser. `Vary: Accept` is what makes
    // the negotiation visible to it, and it goes on *both* branches or it protects neither. When
    // `AULOS_WEB_UI` is false the route has exactly one representation again, so it is omitted:
    // an unnecessary `Vary` only fragments a cache.
    let negotiated = state.cfg.web_ui;
    let mut response = if negotiated && wants_html(&headers) {
        index(&state, &headers)
    } else {
        crate::v2::meta::identity(State(state))
            .await
            .into_response()
    };
    if negotiated {
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("accept"));
    }
    response
}

/// The rendered `index.html`, with the CSP.
fn index(state: &ApiState, headers: &HeaderMap) -> Response {
    let page = rendered(&INDEX_CACHE, INDEX_TEMPLATE, state);
    let mut response = serve(headers, page.body.clone(), HTML, &page.etag);
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    response
}

async fn manifest(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    let page = rendered(&MANIFEST_CACHE, MANIFEST_TEMPLATE, &state);
    serve(&headers, page.body.clone(), WEBMANIFEST, &page.etag)
}

async fn app_css(headers: HeaderMap) -> Response {
    static_asset(&headers, APP_CSS_AT)
}

async fn app_js(headers: HeaderMap) -> Response {
    static_asset(&headers, APP_JS_AT)
}

async fn icon_svg(headers: HeaderMap) -> Response {
    static_asset(&headers, ICON_SVG_AT)
}

async fn icon_180_png(headers: HeaderMap) -> Response {
    static_asset(&headers, ICON_180_AT)
}

fn static_asset(headers: &HeaderMap, at: usize) -> Response {
    let (_, entry) = &ASSETS[at];
    serve(headers, entry.body.clone(), entry.content_type, &entry.etag)
}

// ---------------------------------------------------------------------------
// the one response builder
// ---------------------------------------------------------------------------

/// The body, or a `304` when `If-None-Match` already has it.
///
/// A `304` carries the same `ETag` and the same cache directives as the `200` would — a bare 304
/// makes some caches forget both — and no body.
fn serve(headers: &HeaderMap, body: Bytes, content_type: &str, etag: &str) -> Response {
    let tag = HeaderValue::from_str(etag).unwrap_or(HeaderValue::from_static("\"0\""));
    let matched = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .flat_map(|value| value.to_str().into_iter())
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        // nginx has weakened the ETag on a gzipped response since 1.7.3, so the browser stores and
        // replays `W/"<sha>"`. Comparing that byte-for-byte against the strong tag never matches
        // and every conditional request re-sends the whole body — which is the entire point of the
        // `no-cache` + strong-ETag design. The weak prefix is stripped before comparing: these
        // bodies are byte-identical whenever the tag is, so weak and strong comparison agree.
        .map(|candidate| candidate.strip_prefix("W/").unwrap_or(candidate))
        .any(|candidate| candidate == etag || candidate == "*");

    let mut response = if matched {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (StatusCode::OK, body).into_response()
    };
    let out = response.headers_mut();
    out.insert(header::ETAG, tag);
    if let Ok(value) = HeaderValue::from_str(content_type) {
        out.insert(header::CONTENT_TYPE, value);
    }
    for (name, value) in COMMON {
        out.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// `"<hex sha256>"` — a strong entity tag, quoted, as `ETag` requires.
fn etag_of(body: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(body);
    let mut out = String::with_capacity(2 + digest.len() * 2);
    out.push('"');
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn a_browser_gets_html_and_a_json_client_does_not() {
        // What Safari, Chrome and Firefox actually send on a document navigation.
        assert!(wants_html(&accept(&[
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        ])));
        assert!(wants_html(&accept(&["text/html"])));
        assert!(wants_html(&accept(&["TEXT/HTML"])));
        assert!(wants_html(&accept(&["text/html;q=0.9"])));
        assert!(wants_html(&accept(&["text/*"])));
        assert!(wants_html(&accept(&["application/json", "text/html"])));
        assert!(wants_html(&accept(&["text/html, */*"])), "a browser's tail");

        // The three shapes a non-browser client sends, and the one the contract pins hardest:
        // `*/*` is what `curl` and `reqwest` default to, and it must keep meaning "the JSON".
        assert!(!wants_html(&accept(&["*/*"])));
        assert!(!wants_html(&accept(&["application/json"])));
        assert!(!wants_html(&accept(&["application/json, */*;q=0.5"])));
        assert!(!wants_html(&accept(&[""])));
        assert!(!wants_html(&HeaderMap::new()), "no Accept at all");
    }

    #[test]
    fn a_text_html_substring_is_not_a_match() {
        // `application/vnd.text/html+json` is not a thing, but substring matching would take it,
        // and so would `text/htmlx`.
        assert!(!wants_html(&accept(&["text/htmlx"])));
        assert!(!wants_html(&accept(&["application/text/html"])));
    }

    #[test]
    fn the_table_and_the_index_constants_agree() {
        for (suffix, entry) in ASSETS.iter() {
            assert!(entry.etag.starts_with('"'), "{suffix} etag is quoted");
            assert_eq!(entry.etag.len(), 66, "{suffix} etag is a quoted sha256");
        }
        for (at, suffix) in [
            (APP_CSS_AT, "assets/app.css"),
            (APP_JS_AT, "assets/app.js"),
            (ICON_SVG_AT, "assets/icon.svg"),
            (ICON_180_AT, "assets/icon-180.png"),
        ] {
            assert_eq!(ASSETS[at].0, suffix);
        }
    }

    #[test]
    fn the_templates_carry_both_placeholders_and_nothing_else() {
        assert!(INDEX_TEMPLATE.contains(PREFIX_PLACEHOLDER));
        assert!(INDEX_TEMPLATE.contains(THEME_PLACEHOLDER));
        // No third token can creep in: `{{` only ever opens one of the two.
        for template in [INDEX_TEMPLATE, MANIFEST_TEMPLATE] {
            for (index, _) in template.match_indices("{{") {
                let rest = &template[index..];
                assert!(
                    rest.starts_with(PREFIX_PLACEHOLDER) || rest.starts_with(THEME_PLACEHOLDER),
                    "unknown template token at byte {index}: {}",
                    &rest[..rest.len().min(24)]
                );
            }
        }
    }

    #[test]
    fn rendering_leaves_no_placeholder_behind() {
        let (prefix, _) = Prefix::normalize("/metube/");
        let page = Rendered::render(INDEX_TEMPLATE, &prefix, Theme::Dark);
        assert!(!page.text().contains("{{"));
        assert!(page.text().contains("/metube/assets/app.js"));
        assert!(page.text().contains(r#"content="/metube/""#));
        assert!(page.text().contains(r#"content="dark""#));
    }

    #[test]
    fn the_rendered_html_element_carries_the_theme() {
        // `DEFAULT_THEME` has to reach the *CSS*, not just a meta tag app.js reads: the stylesheet
        // keys the palette off `<html data-mode>`, so without this attribute a `dark` deployment
        // paints the light palette until the module has parsed and run.
        let (prefix, _) = Prefix::normalize("/");
        for theme in [Theme::Auto, Theme::Light, Theme::Dark] {
            let page = Rendered::render(INDEX_TEMPLATE, &prefix, theme);
            assert!(
                page.text()
                    .contains(&format!(r#"data-mode="{}""#, theme.as_str())),
                "the <html> element carries data-mode={theme:?}"
            );
            assert!(
                page.text()
                    .contains(&format!(r#"content="{}""#, theme.as_str())),
                "and the meta tag still carries it too"
            );
        }
    }

    #[test]
    fn the_etag_changes_with_the_prefix_and_with_the_theme() {
        let (root, _) = Prefix::normalize("/");
        let (nested, _) = Prefix::normalize("/metube/");
        let a = Rendered::render(INDEX_TEMPLATE, &root, Theme::Auto);
        let b = Rendered::render(INDEX_TEMPLATE, &nested, Theme::Auto);
        let c = Rendered::render(INDEX_TEMPLATE, &root, Theme::Dark);
        assert_ne!(a.etag, b.etag, "the prefix is in the bytes");
        assert_ne!(a.etag, c.etag, "so is the theme");
    }

    #[test]
    fn the_csp_is_the_exact_string_the_contract_pins() {
        assert_eq!(
            CSP,
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; manifest-src 'self'; font-src 'self'; base-uri 'none'; \
             form-action 'none'; frame-ancestors 'none'"
        );
        assert!(!CSP.contains("unsafe-inline"));
    }

    #[test]
    fn the_page_stays_inside_its_size_budget() {
        // A hard rule of the brief: no bundler, no dependency, and `app.js` + `app.css` together
        // under 70 KB unminified. It is asserted here because the only way it ever regresses is
        // one more "just a few lines" at a time.
        const BUDGET: usize = 70 * 1024;
        let total = APP_JS.len() + APP_CSS.len();
        assert!(
            total <= BUDGET,
            "app.js ({}) + app.css ({}) = {total} bytes, over the {BUDGET}-byte budget",
            APP_JS.len(),
            APP_CSS.len()
        );
    }

    #[test]
    fn the_etag_is_a_quoted_lowercase_sha256() {
        let tag = etag_of(b"");
        assert_eq!(
            tag,
            "\"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\""
        );
    }
}
