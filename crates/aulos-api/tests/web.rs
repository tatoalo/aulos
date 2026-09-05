//! The embedded web UI: content negotiation at `GET <p>`, the assets and the manifest, their
//! caching and security headers, the auth exemption, and `AULOS_WEB_UI=false` — under both
//! prefixes.
//!
//! # Why so much of this is raw HTTP
//!
//! The two facts that matter most here are about the `Accept` header, and `reqwest` puts
//! `Accept: */*` on every request it sends. There is no way to ask it for *no* `Accept` header at
//! all, and a missing `Accept` is precisely what a hand-rolled client sends and what must keep
//! getting the identity JSON. [`raw`] therefore writes the request bytes itself, which also makes
//! `HEAD` and the exact response-header set observable without a client library in between.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::Value;
use support::{Rig, for_each_prefix};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The `Accept` a browser sends on a document navigation.
const BROWSER: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";

/// The exact policy the contract pins, byte for byte.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; manifest-src 'self'; font-src 'self'; base-uri 'none'; \
                   form-action 'none'; frame-ancestors 'none'";

/// The five UI routes, as `(suffix, content type)`.
const UI_ROUTES: [(&str, &str); 5] = [
    ("assets/app.css", "text/css; charset=utf-8"),
    ("assets/app.js", "text/javascript; charset=utf-8"),
    ("assets/icon.svg", "image/svg+xml"),
    ("assets/icon-180.png", "image/png"),
    ("manifest.webmanifest", "application/manifest+json"),
];

// ---------------------------------------------------------------------------
// a raw HTTP/1.1 client, so `Accept` is exactly what the test says it is
// ---------------------------------------------------------------------------

/// One response, parsed far enough to assert on.
struct Raw {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Raw {
    /// The first value of a header, lowercased name, or `None`.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body as UTF-8.
    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("a UTF-8 body")
    }

    /// The body parsed as JSON.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
}

/// Sends `method path` with exactly the headers given — no `Accept` unless one is in `headers`.
///
/// `Connection: close` makes the end of the body the end of the stream, so no chunked or
/// content-length bookkeeping is needed to read it.
async fn raw(rig: &Rig, method: &str, suffix: &str, headers: &[(&str, &str)]) -> Raw {
    let path = rig.cfg.url_prefix.route(suffix);
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        rig.addr
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");

    let mut socket = tokio::net::TcpStream::connect(rig.addr).await.unwrap();
    socket.write_all(request.as_bytes()).await.unwrap();
    socket.flush().await.unwrap();
    let mut buf = Vec::new();
    socket.read_to_end(&mut buf).await.unwrap();

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete header block");
    let head = std::str::from_utf8(&buf[..split]).expect("ASCII headers");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();

    Raw {
        status,
        headers,
        body: buf[split + 4..].to_vec(),
    }
}

/// The four headers every UI response carries, plus the absence of a CSP on everything but the
/// page.
fn assert_ui_headers(response: &Raw, content_type: &str, is_index: bool) {
    assert_eq!(response.header("content-type"), Some(content_type));
    assert_eq!(response.header("cache-control"), Some("no-cache"));
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(response.header("referrer-policy"), Some("no-referrer"));

    let etag = response.header("etag").expect("an ETag");
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "strong: {etag}"
    );
    assert_eq!(etag.len(), 66, "a quoted hex sha256: {etag}");
    assert!(
        etag[1..65]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "lowercase hex: {etag}"
    );

    let csp = response.header("content-security-policy");
    if is_index {
        assert_eq!(csp, Some(CSP));
    } else {
        assert_eq!(csp, None, "the CSP belongs on index.html and nowhere else");
    }
}

// ---------------------------------------------------------------------------
// content negotiation at GET <p>
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_browser_gets_the_page_and_everyone_else_gets_the_identity_document() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        let page = raw(&rig, "GET", "", &[("Accept", BROWSER)]).await;
        assert_eq!(page.status, 200);
        assert_ui_headers(&page, "text/html; charset=utf-8", true);
        assert!(
            page.text().trim_start()[..15].eq_ignore_ascii_case("<!doctype html>"),
            "an HTML document, not the JSON"
        );
        // One URL, two representations: without this a shared cache is free to hand the page to
        // an API client and the JSON to a browser.
        assert_eq!(
            page.header("vary").map(str::to_ascii_lowercase).as_deref(),
            Some("accept"),
            "the HTML branch says what it varied on"
        );

        // The identity document, for the three shapes a non-browser client sends.
        let expected = raw(&rig, "GET", "", &[("Accept", "application/json")]).await;
        assert_eq!(expected.status, 200);
        assert_eq!(
            expected.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(expected.json()["name"], "aulos-server");
        assert_eq!(expected.json()["url_prefix"], prefix);
        assert_eq!(expected.json()["protocol"], "v2");

        for accept in [
            &[("Accept", "*/*")][..],
            &[("Accept", "application/json, */*;q=0.5")][..],
            &[][..],
        ] {
            let other = raw(&rig, "GET", "", accept).await;
            assert_eq!(other.status, 200, "Accept {accept:?}");
            assert_eq!(
                other.body, expected.body,
                "the identity JSON must be byte-for-byte identical for Accept {accept:?}"
            );
            assert_eq!(
                other.header("content-type"),
                Some("application/json; charset=utf-8")
            );
            assert_eq!(
                other.header("content-security-policy"),
                None,
                "no CSP on an API JSON response"
            );
            assert_eq!(
                other.header("vary").map(str::to_ascii_lowercase).as_deref(),
                Some("accept"),
                "and the JSON branch varies on Accept too, or the header protects neither"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn the_identity_document_is_unchanged_by_the_ui() {
    // A byte-level golden, so a future edit to the negotiation branch cannot quietly reshape the
    // document the iOS app and every script parse.
    let rig = Rig::start("/").await;
    let response = raw(&rig, "GET", "", &[]).await;
    assert_eq!(
        response.text(),
        r#"{"name":"aulos-server","protocol":"v2","url_prefix":"/","version":"2026.09.04"}"#
    );
}

#[tokio::test]
async fn the_page_carries_the_prefix_and_the_theme_and_no_leftover_placeholder() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("DEFAULT_THEME", "dark")
            .start()
            .await;
        let page = raw(&rig, "GET", "", &[("Accept", BROWSER)]).await;
        let html = page.text();

        // Asserted on the *values*, not on attribute order or spacing: the page is the
        // front-end's file and may be reformatted freely, but what it must carry is fixed.
        assert!(!html.contains("{{"), "every placeholder is substituted");
        assert!(
            html.contains("aulos-prefix"),
            "the prefix meta tag is present"
        );
        assert!(
            html.contains("aulos-theme"),
            "the theme meta tag is present"
        );
        assert!(
            html.contains(&format!(r#"content="{prefix}""#)),
            "the prefix is that tag's value"
        );
        assert!(
            html.contains(r#"content="dark""#),
            "DEFAULT_THEME is that tag's value"
        );
        for asset in [
            "assets/app.css",
            "assets/app.js",
            "assets/icon.svg",
            "assets/icon-180.png",
            "manifest.webmanifest",
        ] {
            assert!(
                html.contains(&format!("{prefix}{asset}")),
                "{asset} is referenced under the prefix"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn the_page_etag_moves_with_the_prefix_and_with_the_theme() {
    let etag = |rig_prefix: &'static str, theme: &'static str| async move {
        let rig = Rig::builder(rig_prefix)
            .env("DEFAULT_THEME", theme)
            .start()
            .await;
        raw(&rig, "GET", "", &[("Accept", BROWSER)])
            .await
            .header("etag")
            .expect("an ETag")
            .to_owned()
    };
    let root_auto = etag("/", "auto").await;
    assert_ne!(root_auto, etag("/metube/", "auto").await, "the prefix");
    assert_ne!(root_auto, etag("/", "dark").await, "the theme");
    assert_eq!(
        root_auto,
        etag("/", "auto").await,
        "and is otherwise stable"
    );
}

// ---------------------------------------------------------------------------
// the assets and the manifest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_asset_is_served_with_its_type_and_the_security_headers() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        for (suffix, content_type) in UI_ROUTES {
            let response = raw(&rig, "GET", suffix, &[]).await;
            assert_eq!(response.status, 200, "{suffix}");
            assert_ui_headers(&response, content_type, false);
            assert!(!response.body.is_empty(), "{suffix} has bytes");
        }
    })
    .await;
}

#[tokio::test]
async fn the_manifest_is_scoped_to_the_prefix() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let manifest = raw(&rig, "GET", "manifest.webmanifest", &[]).await;
        let body = manifest.json();

        assert_eq!(body["name"], "Aulos");
        assert_eq!(body["display"], "standalone");

        // The manifest is fetched from `<prefix>manifest.webmanifest`, so the two URLs that must
        // equal the prefix may be written either absolutely (`/metube/`, which is what
        // `{{PREFIX}}` substitution produces) or as the relative `./`, which resolves to exactly
        // the same thing. Both are accepted; anything else would scope the installed app wrong.
        for key in ["start_url", "scope"] {
            let value = body[key]
                .as_str()
                .unwrap_or_else(|| panic!("{key} is a string"));
            assert!(
                value == prefix || value == "./",
                "{key} is {value:?}, which does not resolve to {prefix:?}"
            );
        }
        assert_eq!(body["theme_color"], "#E07850");
        assert_eq!(body["background_color"], "#F2F2F7");

        let icons: Vec<&str> = body["icons"]
            .as_array()
            .expect("icons")
            .iter()
            .map(|icon| icon["src"].as_str().expect("a src"))
            .collect();
        // Either spelling resolves to the same URL — the manifest lives at
        // `<prefix>manifest.webmanifest`, so a bare `assets/…` is already relative to the prefix.
        for icon in ["assets/icon.svg", "assets/icon-180.png"] {
            assert!(
                icons.contains(&&*format!("{prefix}{icon}")) || icons.contains(&icon),
                "{icon} is not among {icons:?}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn if_none_match_answers_304_with_the_same_etag_and_no_body() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        let page = raw(&rig, "GET", "", &[("Accept", BROWSER)]).await;
        let etag = page.header("etag").expect("an ETag").to_owned();
        let again = raw(
            &rig,
            "GET",
            "",
            &[("Accept", BROWSER), ("If-None-Match", &etag)],
        )
        .await;
        assert_eq!(again.status, 304);
        assert_eq!(again.header("etag"), Some(etag.as_str()));
        assert_eq!(again.header("cache-control"), Some("no-cache"));
        assert!(again.body.is_empty(), "a 304 carries no body");

        for (suffix, _) in UI_ROUTES {
            let first = raw(&rig, "GET", suffix, &[]).await;
            let tag = first.header("etag").expect("an ETag").to_owned();

            let matched = raw(&rig, "GET", suffix, &[("If-None-Match", &tag)]).await;
            assert_eq!(matched.status, 304, "{suffix}");
            assert_eq!(matched.header("etag"), Some(tag.as_str()));
            assert!(matched.body.is_empty(), "{suffix}");

            let wildcard = raw(&rig, "GET", suffix, &[("If-None-Match", "*")]).await;
            assert_eq!(wildcard.status, 304, "{suffix}: `*` matches anything held");

            // nginx weakens the ETag on a gzipped response, so the browser replays `W/"…"`.
            // Comparing that byte-for-byte would 200 every conditional request forever.
            let weak = format!("W/{tag}");
            let weakened = raw(&rig, "GET", suffix, &[("If-None-Match", &weak)]).await;
            assert_eq!(
                weakened.status, 304,
                "{suffix}: a weak validator still matches"
            );
            assert!(weakened.body.is_empty(), "{suffix}");

            let many = format!("W/\"other\", {weak}");
            let listed = raw(&rig, "GET", suffix, &[("If-None-Match", &many)]).await;
            assert_eq!(listed.status, 304, "{suffix}: and it matches inside a list");

            let stale = raw(&rig, "GET", suffix, &[("If-None-Match", "\"stale\"")]).await;
            assert_eq!(stale.status, 200, "{suffix}: a miss re-sends the body");
            assert_eq!(stale.body, first.body);
        }
    })
    .await;
}

#[tokio::test]
async fn head_answers_like_get_without_a_body() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        let get = raw(&rig, "GET", "", &[("Accept", BROWSER)]).await;
        let head = raw(&rig, "HEAD", "", &[("Accept", BROWSER)]).await;
        assert_eq!(head.status, 200);
        assert_eq!(
            head.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(head.header("etag"), get.header("etag"));
        assert_eq!(head.header("content-security-policy"), Some(CSP));
        assert!(head.body.is_empty());

        // ...and a HEAD that does not want HTML still describes the identity document.
        let json = raw(&rig, "HEAD", "", &[("Accept", "application/json")]).await;
        assert_eq!(json.status, 200);
        assert_eq!(
            json.header("content-type"),
            Some("application/json; charset=utf-8")
        );

        for (suffix, content_type) in UI_ROUTES {
            let head = raw(&rig, "HEAD", suffix, &[]).await;
            assert_eq!(head.status, 200, "{suffix}");
            assert_eq!(head.header("content-type"), Some(content_type), "{suffix}");
            assert!(head.header("etag").is_some(), "{suffix}");
            assert!(head.body.is_empty(), "{suffix}");
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ui_is_open_while_every_api_route_stays_guarded() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;

        // No token anywhere on any of these.
        let page = raw(&rig, "GET", "", &[("Accept", BROWSER)]).await;
        assert_eq!(page.status, 200, "index.html is never behind the token");
        assert_eq!(
            page.header("content-type"),
            Some("text/html; charset=utf-8")
        );

        for (suffix, content_type) in UI_ROUTES {
            let response = raw(&rig, "GET", suffix, &[]).await;
            assert_eq!(response.status, 200, "{suffix} is never behind the token");
            assert_eq!(response.header("content-type"), Some(content_type));
        }

        // ...and nothing else moved.
        for suffix in [
            "api/v2/capabilities",
            "api/v2/state",
            "api/v2/items",
            "api/v2/subscriptions",
            "download/movie.mp4",
            "ws",
        ] {
            let response = raw(&rig, "GET", suffix, &[]).await;
            assert_eq!(response.status, 401, "{suffix} still needs the token");
            assert_eq!(response.json()["error"]["code"], "unauthorized", "{suffix}");
            assert!(
                response.header("location").is_none(),
                "{suffix}: a 401 never redirects (PROTOCOL §1.4)"
            );
        }

        let ok = raw(
            &rig,
            "GET",
            "api/v2/capabilities",
            &[("Authorization", "Bearer s3cret")],
        )
        .await;
        assert_eq!(ok.status, 200, "and the token still works");
    })
    .await;
}

#[tokio::test]
async fn the_trusted_proxy_header_does_not_gate_the_ui_either() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_TRUSTED_PROXY_AUTH_HEADER", "Remote-User")
            .start()
            .await;

        assert_eq!(
            raw(&rig, "GET", "", &[("Accept", BROWSER)]).await.status,
            200
        );
        assert_eq!(raw(&rig, "GET", "assets/app.js", &[]).await.status, 200);
        assert_eq!(
            raw(&rig, "GET", "manifest.webmanifest", &[]).await.status,
            200
        );
        assert_eq!(
            raw(&rig, "GET", "api/v2/capabilities", &[]).await.status,
            401
        );
        assert_eq!(
            raw(
                &rig,
                "GET",
                "api/v2/capabilities",
                &[("Remote-User", "ada")]
            )
            .await
            .status,
            200
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// CORS is untouched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cors_still_applies_to_the_api_and_reaches_the_ui_routes() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("CORS_ALLOWED_ORIGINS", "https://a.test")
            .start()
            .await;

        let api = raw(
            &rig,
            "GET",
            "api/v2/capabilities",
            &[("Origin", "https://a.test")],
        )
        .await;
        assert_eq!(api.status, 200);
        assert_eq!(
            api.header("access-control-allow-origin"),
            Some("https://a.test"),
            "the v2 layer is unchanged"
        );

        // The layer wraps `open.merge(guarded)`, so the UI routes inherit it. Harmless (they hold
        // nothing secret) and asserted so a future re-layering is a visible change, not a silent
        // one.
        let asset = raw(
            &rig,
            "GET",
            "assets/app.js",
            &[("Origin", "https://a.test")],
        )
        .await;
        assert_eq!(asset.status, 200);
        assert_eq!(
            asset.header("access-control-allow-origin"),
            Some("https://a.test")
        );

        // The exposed-header set the v2 layer has always sent is unchanged, and an origin that
        // is not on the list still gets nothing — the UI routes did not widen either.
        let exposed = api
            .header("access-control-expose-headers")
            .expect("the exposed-header list")
            .to_ascii_lowercase();
        for name in ["x-request-id", "x-aulos-seq", "etag", "content-range"] {
            assert!(exposed.contains(name), "{name} in {exposed}");
        }

        for suffix in ["api/v2/capabilities", "assets/app.js", ""] {
            let stranger = raw(&rig, "GET", suffix, &[("Origin", "https://evil.test")]).await;
            assert_eq!(
                stranger.header("access-control-allow-origin"),
                None,
                "{suffix}: an unlisted origin is never reflected"
            );
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// AULOS_WEB_UI=false
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabling_the_web_ui_restores_the_pre_ui_surface_exactly() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_WEB_UI", "false")
            .start()
            .await;

        // The root is the identity document for *every* Accept, browsers included.
        let baseline = raw(&rig, "GET", "", &[]).await;
        for accept in [
            &[("Accept", BROWSER)][..],
            &[("Accept", "text/html")][..],
            &[][..],
        ] {
            let response = raw(&rig, "GET", "", accept).await;
            assert_eq!(response.status, 200, "{accept:?}");
            assert_eq!(
                response.header("content-type"),
                Some("application/json; charset=utf-8"),
                "{accept:?}"
            );
            assert_eq!(response.body, baseline.body, "{accept:?}");
            assert_eq!(response.header("content-security-policy"), None);
        }

        // With one representation left there is nothing to vary on, and an unnecessary `Vary`
        // only fragments a cache.
        assert_eq!(baseline.header("vary"), None);

        // The assets and the manifest are ordinary 404s in the standard envelope.
        for (suffix, _) in UI_ROUTES {
            let response = raw(&rig, "GET", suffix, &[]).await;
            assert_eq!(response.status, 404, "{suffix}");
            let body = response.json();
            assert_eq!(body["error"]["code"], "not_found", "{suffix}");
            assert!(body["error"]["request_id"].as_str().is_some(), "{suffix}");
            assert!(body["error"]["message"].as_str().is_some(), "{suffix}");
        }

        // ...and the rest of the API is untouched.
        assert_eq!(
            raw(&rig, "GET", "api/v2/capabilities", &[]).await.status,
            200
        );
        assert_eq!(raw(&rig, "GET", "healthz", &[]).await.status, 200);
    })
    .await;
}

#[tokio::test]
async fn the_ui_lives_under_the_prefix_and_nowhere_else() {
    let rig = Rig::builder("/metube/").start().await;
    // The unprefixed paths must not resolve: `Rig::url` prefixes for us, so these go out raw.
    for path in [
        "/assets/app.js",
        "/manifest.webmanifest",
        "/assets/icon.svg",
    ] {
        let response = rig
            .http
            .get(format!("http://{}{path}", rig.addr))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 404, "{path}");
    }
    // ...while the prefixed ones do.
    for (suffix, _) in UI_ROUTES {
        assert_eq!(raw(&rig, "GET", suffix, &[]).await.status, 200, "{suffix}");
    }
}
