//! Just-in-time stream re-extraction, run at download time (DESIGN §10.5).
//!
//! vixcloud's `token`/`expires` pair is valid for minutes, so the m3u8 resolved during a scrape is
//! discarded and re-resolved here, immediately before the bytes start moving. Ported from
//! `streamingcommunity.py:399-477`, with the two changes DESIGN §10.5 calls for:
//!
//! - a **fresh site-version cache and a fresh cookie jar** per call, matching legacy's fresh
//!   extractor instance and its fresh `curl_cffi.Session` — a token minted against one deploy's
//!   assets must not be reused across one, and the `Cookie:` header handed to the external
//!   downloader must hold only this extraction's cookies (see [`crate::http`]);
//! - **no debug `GET` of the m3u8.** Legacy fetched the playlist on every single download purely
//!   to log its status code and the first 300 bytes, which cost a round trip, burned one use of a
//!   rate-limited token and logged stream contents at INFO.

use url::Url;

use crate::embed;
use crate::error::ScError;
use crate::http::{ScHttp, USER_AGENT};
use crate::inertia::{SiteVersions, inertia_get, props};

/// Everything an external downloader needs to fetch the stream.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StreamTarget {
    /// The master playlist URL, tokens included.
    pub m3u8: Url,
    /// `Referer`, `Origin` and `User-Agent`, in that order — legacy's `http_headers`
    /// (`streamingcommunity.py:468-472`).
    pub headers: Vec<(String, String)>,
    /// The session cookies as one `Cookie:` header value, or an empty string.
    pub cookies: String,
}

impl StreamTarget {
    /// One header value by name, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Re-runs S1–S4 for one watch URL and returns a playable target.
///
/// # Errors
/// [`ScError::NoEmbedUrl`], [`ScError::NoIframe`] or [`ScError::NoStream`] when the site no longer
/// serves the stream, plus whatever the transport and [`inertia_get`] can return.
pub async fn fresh_stream(
    http: &dyn ScHttp,
    base: &Url,
    watch_url: &Url,
) -> Result<StreamTarget, ScError> {
    // Legacy built a brand-new extractor here, so the site version was always refetched and the
    // session jar started empty. Keeping the first means a deploy between queueing and downloading
    // cannot poison the request; keeping the second means `target.cookies` holds only what *this*
    // extraction was handed, so two concurrent downloads cannot swap vixcloud session cookies and
    // 403 each other (`streamingcommunity.py:407,442`).
    let session = http.new_session();
    let http = session.as_deref().unwrap_or(http);
    let versions = SiteVersions::new();

    // Legacy computed the path as `watch_url.replace(base_url, "")`; path-plus-query is the same
    // string for every URL the site serves and does not break when `base` has a trailing slash.
    let mut path = watch_url.path().to_owned();
    if let Some(q) = watch_url.query() {
        path.push('?');
        path.push_str(q);
    }

    let page = inertia_get(http, &versions, base, &path).await?;
    let embed_url = props(&page)
        .get("embedUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ScError::NoEmbedUrl {
            url: watch_url.to_string(),
        })?;
    let embed_url = Url::parse(embed_url).map_err(|_| ScError::NoEmbedUrl {
        url: watch_url.to_string(),
    })?;

    let (iframe, params) = embed::stream_for_embed(http, &embed_url).await?;
    Ok(StreamTarget {
        m3u8: params.m3u8,
        headers: vec![
            ("Referer".to_owned(), params.referer.to_string()),
            ("Origin".to_owned(), origin_of(&iframe)),
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ],
        cookies: http.cookie_header(),
    })
}

/// `{scheme}://{netloc}` of the vixcloud iframe — legacy's `Origin`
/// (`streamingcommunity.py:437-438`).
#[must_use]
pub fn origin_of(iframe: &Url) -> String {
    let mut origin = format!("{}://", iframe.scheme());
    origin.push_str(iframe.host_str().unwrap_or_default());
    if let Some(port) = iframe.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    origin
}

#[cfg(test)]
mod tests {
    use crate::testing::MockHttp;

    use super::*;

    fn base() -> Url {
        Url::parse("https://sc.test").expect("url")
    }

    fn mock() -> MockHttp {
        MockHttp::new()
            .with_cookies("sid=abc; cf_clearance=zzz")
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/watch/9?e=456",
                200,
                include_str!("../tests/fixtures/sc/watch_episode.json"),
            )
            .on(
                "https://sc.test/embed/456",
                200,
                include_str!("../tests/fixtures/sc/embed.html"),
            )
            .on(
                "https://vixcloud.co/embed/98765?token=abc&referer=1",
                200,
                include_str!("../tests/fixtures/sc/vixcloud_streams_active.html"),
            )
    }

    #[tokio::test]
    async fn a_fresh_stream_carries_the_legacy_header_trio_and_the_cookie_jar() {
        let http = mock();
        let watch = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        let t = fresh_stream(&http, &base(), &watch)
            .await
            .expect("a target");
        assert_eq!(
            t.m3u8.as_str(),
            "https://vixcloud.co/playlist/98765?b=1&token=tok3n-value&expires=1759000000"
        );
        let names: Vec<&str> = t.headers.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Referer", "Origin", "User-Agent"]);
        assert_eq!(
            t.header("Referer"),
            Some("https://vixcloud.co/embed/98765?token=abc&referer=1")
        );
        assert_eq!(t.header("origin"), Some("https://vixcloud.co"));
        assert_eq!(t.header("User-Agent"), Some(USER_AGENT));
        assert_eq!(t.cookies, "sid=abc; cf_clearance=zzz");
    }

    #[tokio::test]
    async fn the_m3u8_is_never_fetched_and_the_version_is_always_refetched() {
        let http = mock();
        let watch = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        let _ = fresh_stream(&http, &base(), &watch)
            .await
            .expect("a target");
        assert!(
            !http.requested_anything_containing("/playlist/"),
            "the legacy debug GET of the m3u8 is removed (DESIGN §10.5)"
        );
        assert_eq!(http.count("https://sc.test/it"), 1);
        // Exactly four hops: version, watch, embed, iframe.
        assert_eq!(http.total(), 4, "{:?}", http.urls());
        // A second call re-fetches the version rather than trusting a cache.
        let _ = fresh_stream(&http, &base(), &watch)
            .await
            .expect("a target");
        assert_eq!(http.count("https://sc.test/it"), 2);
    }

    #[tokio::test]
    async fn the_extraction_runs_on_a_forked_session_so_the_cookies_are_this_downloads_own() {
        // The long-lived client is shared by the whole resolve pool and its jar is keyed on the
        // bare cookie name, so a concurrent extraction can overwrite the vixcloud session cookie
        // between S4 and the spawn of N_m3u8DL-RE. Legacy built a new `curl_cffi.Session` per
        // download; `fresh_stream` must fork one too and read *its* jar.
        let session = std::sync::Arc::new(mock().with_cookies("vix_session=mine"));
        let shared = MockHttp::new()
            .with_cookies("vix_session=someone-elses; stale=from-a-previous-download")
            .with_session(std::sync::Arc::clone(&session));

        let watch = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        let t = fresh_stream(&shared, &base(), &watch)
            .await
            .expect("a target");

        assert_eq!(t.cookies, "vix_session=mine");
        assert_eq!(
            shared.total(),
            0,
            "every hop must go through the forked session: {:?}",
            shared.urls()
        );
        assert_eq!(session.total(), 4, "{:?}", session.urls());
    }

    #[tokio::test]
    async fn a_client_with_no_separable_session_still_works() {
        // The default `new_session()` is `None`; the caller then reuses the client it has.
        let http = mock();
        let watch = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        let t = fresh_stream(&http, &base(), &watch)
            .await
            .expect("a target");
        assert_eq!(t.cookies, "sid=abc; cf_clearance=zzz");
        assert_eq!(http.total(), 4, "{:?}", http.urls());
    }

    #[tokio::test]
    async fn a_site_that_no_longer_serves_the_stream_fails_per_step() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/watch/9?e=456",
                200,
                include_str!("../tests/fixtures/sc/watch_no_embed.json"),
            );
        let watch = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        assert_eq!(
            fresh_stream(&http, &base(), &watch)
                .await
                .expect_err("no embed url")
                .code(),
            crate::ScErrorCode::NoEmbedUrl
        );
    }

    #[test]
    fn the_origin_keeps_a_non_default_port() {
        assert_eq!(
            origin_of(&Url::parse("https://vixcloud.co:8443/embed/1").expect("url")),
            "https://vixcloud.co:8443"
        );
        assert_eq!(
            origin_of(&Url::parse("https://vixcloud.co/embed/1").expect("url")),
            "https://vixcloud.co"
        );
    }
}
