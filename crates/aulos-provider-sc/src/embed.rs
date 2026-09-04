//! S3 (the embed iframe) and S4 (the vixcloud stream parameters) — DESIGN §10.2.
//!
//! This is the one part of the pipeline that has to be a *faithful* port rather than a better
//! one: the vixcloud page hands out a `masterPlaylist` URL whose query string carries
//! server-selection parameters (`ub`, `ab`, `b`) that must be preserved, plus a `token`/`expires`
//! pair that must be appended, plus an FHD flag that must only be set when the server says it can
//! serve FHD. Legacy got all four of those right and any deviation produces a 403 from vixcloud or
//! a lower-quality stream, so the query-string assembly here reproduces Python's
//! `parse_qs`/`urlencode` semantics exactly, down to `parse_qs` dropping blank values and
//! `quote_plus`'s safe set.
//!
//! Ported from `streamingcommunity.py:57-127`.

use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Selector};
use url::Url;

use crate::error::ScError;
use crate::http::{ScHttp, ScReq};

/// What S4 produces: the playable m3u8 and the `Referer` that must accompany it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StreamParams {
    /// The fully assembled master playlist URL.
    pub m3u8: Url,
    /// The vixcloud iframe URL, which is what legacy sent as `Referer`.
    pub referer: Url,
}

/// `iframe`, compiled once. See [`crate::inertia`] for why this is an `Option`.
static IFRAME: LazyLock<Option<Selector>> = LazyLock::new(|| Selector::parse("iframe").ok());
/// `script`, compiled once.
static SCRIPT: LazyLock<Option<Selector>> = LazyLock::new(|| Selector::parse("script").ok());

/// The five patterns S4 scrapes with, compiled once.
struct Patterns {
    token: Regex,
    expires: Regex,
    streams: Regex,
    url: Regex,
    can_play_fhd: Regex,
}

impl Patterns {
    fn compile() -> Option<Self> {
        Some(Self {
            token: Regex::new(r#"'token':\s*['"]([^'"]+)['"]"#).ok()?,
            expires: Regex::new(r#"'expires':\s*['"](\d+)['"]"#).ok()?,
            streams: Regex::new(r"(?s)window\.streams\s*=\s*(\[.*?\]);").ok()?,
            url: Regex::new(r#"url:\s*['"]([^'"]+)['"]"#).ok()?,
            can_play_fhd: Regex::new(r"window\.canPlayFHD\s*=\s*(true|false)").ok()?,
        })
    }
}

static PATTERNS: LazyLock<Option<Patterns>> = LazyLock::new(Patterns::compile);

/// The first `<iframe src=…>` in document order — BeautifulSoup's `soup.find("iframe")`.
///
/// A relative `src` is resolved against `page_url`; legacy left it relative and would have
/// produced a broken request, but the site has only ever served an absolute vixcloud URL here, so
/// resolving is strictly more correct and observationally identical.
#[must_use]
pub fn first_iframe_src(html: &str, page_url: &Url) -> Option<Url> {
    let selector = IFRAME.as_ref()?;
    let doc = Html::parse_document(html);
    let src = doc.select(selector).find_map(|el| el.attr("src"))?;
    if src.is_empty() {
        return None;
    }
    page_url.join(src).ok()
}

/// S3: fetch the embed page and pull the vixcloud iframe out of it.
///
/// # Errors
/// [`ScError::NoIframe`] when the page carries no usable `<iframe src>`, plus anything the
/// transport can produce. A non-2xx is **not** raised, matching legacy, which parsed the body of
/// whatever came back (`streamingcommunity.py:148-150`).
pub async fn embed_iframe(http: &dyn ScHttp, embed_url: &Url) -> Result<Url, ScError> {
    let res = http.get(ScReq::get(embed_url.clone())).await?;
    first_iframe_src(&res.body, &res.url).ok_or_else(|| ScError::NoIframe {
        url: embed_url.to_string(),
    })
}

/// S4: fetch the vixcloud page and assemble the master playlist URL.
///
/// # Errors
/// [`ScError::NoStream`] when no script carries a usable stream URL, plus anything the transport
/// can produce.
pub async fn stream_from_iframe(http: &dyn ScHttp, iframe: &Url) -> Result<StreamParams, ScError> {
    let res = http
        .get(ScReq::get(iframe.clone()))
        .await?
        .error_for_status()?;
    extract_stream(&res.body, iframe)
}

/// S3 + S4 in one hop: the embed URL from the watch props to a playable m3u8.
///
/// # Errors
/// Whatever [`embed_iframe`] or [`stream_from_iframe`] returns.
pub async fn stream_for_embed(
    http: &dyn ScHttp,
    embed_url: &Url,
) -> Result<(Url, StreamParams), ScError> {
    let iframe = embed_iframe(http, embed_url).await?;
    let params = stream_from_iframe(http, &iframe).await?;
    Ok((iframe, params))
}

/// The pure half of S4: everything scraped out of one vixcloud page body.
///
/// The port, step for step against `streamingcommunity.py:66-125`:
///
/// 1. the **first** `<script>` whose text contains `masterPlaylist` wins;
/// 2. `'token'` and `'expires'` come out of that script;
/// 3. `window.streams` is parsed as JSON and the `active` entry wins, else the first entry;
/// 4. failing that, the bare `url:` inside `masterPlaylist` is the fallback;
/// 5. the stream URL's own query parameters are preserved, `h=1` is added **only** when
///    `window.canPlayFHD` is `true`, and `token`/`expires` are appended.
///
/// # Errors
/// [`ScError::NoStream`] when steps 3 and 4 both come up empty.
pub fn extract_stream(html: &str, iframe: &Url) -> Result<StreamParams, ScError> {
    let no_stream = || ScError::NoStream {
        url: iframe.to_string(),
    };
    let (Some(script_sel), Some(pats)) = (SCRIPT.as_ref(), PATTERNS.as_ref()) else {
        return Err(no_stream());
    };
    let doc = Html::parse_document(html);
    let text = doc
        .select(script_sel)
        .map(|el| el.text().collect::<String>())
        .find(|t| t.contains("masterPlaylist"))
        .ok_or_else(no_stream)?;

    let token = pats.token.captures(&text).map(|c| c[1].to_owned());
    let expires = pats.expires.captures(&text).map(|c| c[1].to_owned());

    let mut stream_url = stream_url_from_window_streams(pats, &text);
    if stream_url.is_none() {
        stream_url = pats.url.captures(&text).map(|c| c[1].replace("\\/", "/"));
    }
    let stream_url = stream_url.filter(|s| !s.is_empty()).ok_or_else(no_stream)?;
    let parsed = Url::parse(&stream_url).map_err(|_| no_stream())?;

    // Python: `params = {k: v[0] for k, v in parse_qs(parsed.query).items()}`. `parse_qs` drops
    // blank values by default, and a dict keeps first-insertion order.
    let mut params = Params::new();
    for (k, v) in parsed.query_pairs() {
        if v.is_empty() {
            continue;
        }
        params.put_new(&k, &v);
    }
    if pats
        .can_play_fhd
        .captures(&text)
        .is_some_and(|c| &c[1] == "true")
    {
        params.put("h", "1");
    }
    if let Some(t) = token {
        params.put("token", &t);
    }
    if let Some(e) = expires {
        params.put("expires", &e);
    }

    let mut m3u8 = parsed.clone();
    m3u8.set_query(Some(&params.urlencode()));
    Ok(StreamParams {
        m3u8,
        referer: iframe.clone(),
    })
}

/// Step 3: the `active` entry of `window.streams`, else the first one.
///
/// A `window.streams` blob that is not valid JSON is a warning and a fall-through to the `url:`
/// fallback, exactly as legacy did (`streamingcommunity.py:90-91`).
fn stream_url_from_window_streams(pats: &Patterns, text: &str) -> Option<String> {
    let raw = pats.streams.captures(text)?.get(1)?.as_str();
    let streams: Vec<serde_json::Value> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse window.streams");
            return None;
        }
    };
    let pick = streams
        .iter()
        .find(|s| s.get("active").and_then(serde_json::Value::as_bool) == Some(true))
        .or_else(|| streams.first())?;
    let url = pick.get("url").and_then(serde_json::Value::as_str)?;
    let url = url.replace("\\/", "/");
    if url.is_empty() { None } else { Some(url) }
}

/// Python `dict` semantics for a query string: insertion-ordered, assignment replaces in place.
///
/// A `Vec` rather than a map because a vixcloud query has at most a handful of parameters, and
/// because the DESIGN §3 dependency budget for this crate does not carry `indexmap`.
#[derive(Debug, Default)]
struct Params(Vec<(String, String)>);

impl Params {
    fn new() -> Self {
        Self::default()
    }

    /// `params[key] = value` — replaces in place, keeping the original position.
    fn put(&mut self, key: &str, value: &str) {
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value.to_owned();
        } else {
            self.0.push((key.to_owned(), value.to_owned()));
        }
    }

    /// `parse_qs`'s first-value-wins: a repeated key keeps the value it was first seen with.
    fn put_new(&mut self, key: &str, value: &str) {
        if !self.0.iter().any(|(k, _)| k == key) {
            self.0.push((key.to_owned(), value.to_owned()));
        }
    }

    /// Python's `urllib.parse.urlencode`: `quote_plus` on both halves, `&`-joined.
    fn urlencode(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// Python's `urllib.parse.quote_plus(s, safe='')`.
///
/// Hand-written rather than `form_urlencoded` because that crate leaves `*` unescaped and escapes
/// `~`, and this string is signed by the far end: `token` and `expires` have to survive byte for
/// byte.
fn quote_plus(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 0x0f)] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::testing::MockHttp;

    use super::*;

    fn iframe_url() -> Url {
        Url::parse("https://vixcloud.co/embed/555?token=x").expect("url")
    }

    #[test]
    fn the_literal_selectors_and_patterns_compile() {
        assert!(IFRAME.is_some());
        assert!(SCRIPT.is_some());
        assert!(PATTERNS.is_some());
    }

    #[test]
    fn quote_plus_matches_pythons_safe_set() {
        assert_eq!(quote_plus("abcXYZ019_.-~"), "abcXYZ019_.-~");
        assert_eq!(quote_plus("a b"), "a+b");
        assert_eq!(quote_plus("a/b"), "a%2Fb");
        assert_eq!(quote_plus("*"), "%2A");
        assert_eq!(quote_plus("é"), "%C3%A9");
    }

    #[test]
    fn the_first_iframe_wins_and_relative_srcs_resolve() {
        let html = include_str!("../tests/fixtures/sc/embed.html");
        assert_eq!(
            first_iframe_src(html, &Url::parse("https://sc.test/embed/1").expect("url"))
                .map(Url::into),
            Some("https://vixcloud.co/embed/98765?token=abc&referer=1".to_owned())
        );
        let two =
            r#"<html><body><iframe src="/a"></iframe><iframe src="/b"></iframe></body></html>"#;
        assert_eq!(
            first_iframe_src(two, &Url::parse("https://sc.test/embed/1").expect("url"))
                .map(|u| u.path().to_owned()),
            Some("/a".to_owned())
        );
        assert!(
            first_iframe_src(
                "<html><body>no frames</body></html>",
                &Url::parse("https://sc.test/").expect("url")
            )
            .is_none()
        );
    }

    #[test]
    fn window_streams_picks_the_active_server_and_keeps_its_query_params() {
        let html = include_str!("../tests/fixtures/sc/vixcloud_streams_active.html");
        let got = extract_stream(html, &iframe_url()).expect("a stream");
        assert_eq!(
            got.m3u8.as_str(),
            "https://vixcloud.co/playlist/98765?b=1&token=tok3n-value&expires=1759000000"
        );
        assert_eq!(got.referer, iframe_url());
    }

    #[test]
    fn a_single_inactive_server_is_still_used() {
        // Legacy's "fallback to the first server" branch: nothing is marked active.
        let html = include_str!("../tests/fixtures/sc/vixcloud_streams_inactive_only.html");
        let got = extract_stream(html, &iframe_url()).expect("a stream");
        assert_eq!(
            got.m3u8.as_str(),
            "https://vixcloud.co/playlist/98765?ub=1&ab=1&h=1&token=tok3n-value&expires=1759000000"
        );
    }

    #[test]
    fn the_master_playlist_url_field_is_the_fallback() {
        let html = include_str!("../tests/fixtures/sc/vixcloud_masterplaylist_url.html");
        let got = extract_stream(html, &iframe_url()).expect("a stream");
        assert_eq!(
            got.m3u8.as_str(),
            "https://vixcloud.co/playlist/98765?h=1&token=tok3n-value&expires=1759000000"
        );
    }

    #[test]
    fn h_is_added_only_when_can_play_fhd_is_true() {
        let fhd = include_str!("../tests/fixtures/sc/vixcloud_streams_inactive_only.html");
        assert!(
            extract_stream(fhd, &iframe_url())
                .expect("a stream")
                .m3u8
                .query()
                .is_some_and(|q| q.contains("h=1"))
        );
        let no_fhd = include_str!("../tests/fixtures/sc/vixcloud_streams_active.html");
        assert!(
            !extract_stream(no_fhd, &iframe_url())
                .expect("a stream")
                .m3u8
                .query()
                .is_some_and(|q| q.contains("h=1"))
        );
    }

    #[test]
    fn a_page_with_no_master_playlist_script_is_no_stream() {
        for html in [
            include_str!("../tests/fixtures/sc/vixcloud_no_stream.html"),
            "<html><body></body></html>",
        ] {
            let err = extract_stream(html, &iframe_url()).expect_err("must fail");
            assert_eq!(err.code(), crate::ScErrorCode::NoStream);
        }
    }

    #[test]
    fn a_malformed_window_streams_blob_falls_through_to_the_url_field() {
        let html = include_str!("../tests/fixtures/sc/vixcloud_bad_streams_json.html");
        let got = extract_stream(html, &iframe_url()).expect("the url: fallback");
        assert_eq!(
            got.m3u8.as_str(),
            "https://vixcloud.co/playlist/98765?token=tok3n-value&expires=1759000000"
        );
    }

    #[test]
    fn escaped_slashes_are_unescaped_exactly_as_legacy_did() {
        let html = r#"<html><script>
            window.masterPlaylist = { params: { 'token': 'tk', 'expires': '99' },
                url: 'https:\/\/vixcloud.co\/playlist\/1' };
        </script></html>"#;
        let got = extract_stream(html, &iframe_url()).expect("a stream");
        assert_eq!(
            got.m3u8.as_str(),
            "https://vixcloud.co/playlist/1?token=tk&expires=99"
        );
    }

    #[test]
    fn blank_query_values_are_dropped_the_way_parse_qs_drops_them() {
        let html = r#"<html><script>
            window.masterPlaylist = { params: { 'token': 'tk', 'expires': '99' } };
            window.streams = [{"active":true,"url":"https://vixcloud.co/p/1?ub=&ab=1"}];
        </script></html>"#;
        let got = extract_stream(html, &iframe_url()).expect("a stream");
        assert_eq!(
            got.m3u8.as_str(),
            "https://vixcloud.co/p/1?ab=1&token=tk&expires=99"
        );
    }

    #[tokio::test]
    async fn the_embed_hop_reports_a_missing_iframe_distinctly() {
        let http = MockHttp::new().on("https://sc.test/embed/1", 200, "<html></html>");
        let err = embed_iframe(&http, &Url::parse("https://sc.test/embed/1").expect("url"))
            .await
            .expect_err("no iframe");
        assert_eq!(err.code(), crate::ScErrorCode::NoIframe);
    }

    #[tokio::test]
    async fn the_two_hops_never_fetch_the_m3u8_itself() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/embed/1",
                200,
                include_str!("../tests/fixtures/sc/embed.html"),
            )
            .on(
                "https://vixcloud.co/embed/98765?token=abc&referer=1",
                200,
                include_str!("../tests/fixtures/sc/vixcloud_streams_active.html"),
            );
        let (iframe, params) =
            stream_for_embed(&http, &Url::parse("https://sc.test/embed/1").expect("url"))
                .await
                .expect("a stream");
        assert_eq!(iframe.host_str(), Some("vixcloud.co"));
        assert!(params.m3u8.path().contains("/playlist/"));
        assert_eq!(
            http.total(),
            2,
            "exactly the embed page and the iframe page"
        );
        assert!(
            !http.requested_anything_containing("/playlist/"),
            "the debug GET of the m3u8 is removed (DESIGN §10.5)"
        );
    }
}
