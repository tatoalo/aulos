//! S1 (the site version) and S2 (an Inertia page), with the 30-minute single-flight cache and the
//! one-shot retry on a version rejection (DESIGN §10.2).
//!
//! The site is an Inertia app: every JSON route demands an `x-inertia-version` matching the asset
//! manifest of the deploy that is currently live, and that value rotates on every deploy. Legacy
//! fetched it once per extractor instance and simply failed the whole add when the site shipped
//! mid-scrape. Here the value is cached per base URL for 30 minutes, fetched under a per-base lock
//! so a 20-episode add cannot stampede it, and a `403`/`404`/`409` from an Inertia call forces
//! **exactly one** refresh-and-retry before giving up with
//! [`ScError::VersionRejected`](crate::ScError::VersionRejected).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use scraper::{Html, Selector};
use serde_json::Value;
use tokio::time::Instant;
use url::Url;

use crate::error::ScError;
use crate::http::{ScHttp, ScReq, ScRes};

/// How long a site version is trusted (DESIGN §10.2).
pub const VERSION_TTL: Duration = Duration::from_secs(30 * 60);

/// The statuses that mean "your `x-inertia-version` is stale" (DESIGN §10.2).
///
/// Inertia answers a version mismatch with a `409 Conflict`; the site's edge has also been seen to
/// answer `403` and `404` for the same cause, and legacy could not tell any of them apart.
pub const VERSION_REJECTED_STATUSES: [u16; 3] = [403, 404, 409];

/// `div#app`, compiled once.
///
/// `None` is unreachable for a literal selector; it is modelled rather than unwrapped because
/// `clippy::expect_used` is a workspace warning and a panic in a scraper is never the right answer.
static APP_DIV: LazyLock<Option<Selector>> = LazyLock::new(|| Selector::parse("div#app").ok());

/// The per-base site-version cache with single-flight (DESIGN §10.2).
///
/// One `tokio::sync::Mutex` per base URL is the whole single-flight mechanism: the first caller
/// holds it across the site-version probe, every other caller for the same base waits and then reads
/// the value it stored. There is no dogpile and no second request.
#[derive(Debug, Default)]
pub struct SiteVersions {
    slots: Mutex<HashMap<Box<str>, std::sync::Arc<Slot>>>,
}

type Slot = tokio::sync::Mutex<Option<Cached>>;

#[derive(Clone, Debug)]
struct Cached {
    site: Site,
    fetched: Instant,
}

impl SiteVersions {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, base: &Url) -> std::sync::Arc<Slot> {
        let key: Box<str> = base.as_str().into();
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        std::sync::Arc::clone(slots.entry(key).or_default())
    }

    /// The cached version, or one fresh site probe shared by concurrent callers.
    ///
    /// # Errors
    /// Whatever [`fetch_version`] returns.
    pub async fn version(&self, http: &dyn ScHttp, base: &Url) -> Result<Arc<str>, ScError> {
        Ok(self.site(http, base, false).await?.version)
    }

    /// Discards the cached version and fetches a new one, ignoring the TTL.
    ///
    /// # Errors
    /// Whatever [`fetch_version`] returns.
    pub async fn refresh(&self, http: &dyn ScHttp, base: &Url) -> Result<Arc<str>, ScError> {
        Ok(self.site(http, base, true).await?.version)
    }

    pub(crate) async fn site(
        &self,
        http: &dyn ScHttp,
        base: &Url,
        refresh: bool,
    ) -> Result<Site, ScError> {
        let slot = self.slot(base);
        let mut guard = slot.lock().await;
        if !refresh
            && let Some(c) = guard.as_ref()
            && c.fetched.elapsed() < VERSION_TTL
        {
            return Ok(c.site.clone());
        }
        let previous = guard.take();
        let mut site = match &previous {
            Some(c) => fetch_site(c.site.http(http), &c.site.base).await?,
            None => fetch_site(http, base).await?,
        };
        if site.session.is_none() {
            site.session = previous.and_then(|c| c.site.session);
        }
        *guard = Some(Cached {
            site: site.clone(),
            fetched: Instant::now(),
        });
        Ok(site)
    }

    /// Drops the cached version for one base without fetching.
    pub async fn invalidate(&self, base: &Url) {
        let slot = self.slot(base);
        let mut guard = slot.lock().await;
        *guard = None;
    }
}

#[derive(Clone)]
pub(crate) struct Site {
    pub base: Url,
    version: Arc<str>,
    session: Option<Arc<dyn ScHttp>>,
}

impl std::fmt::Debug for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Site")
            .field("base", &self.base)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Site {
    pub fn http<'a>(&'a self, fallback: &'a dyn ScHttp) -> &'a dyn ScHttp {
        self.session.as_deref().unwrap_or(fallback)
    }
}

/// S1: probe the site root for `div#app[data-page]` → JSON → `.version`.
/// Only a page without `data-page` falls back to the legacy `/it` route.
///
/// # Errors
/// [`ScError::VersionProbe`] for HTTP or redirect failures, [`ScError::VersionUnreadable`]
/// for a missing or malformed version, plus transport errors. Probe failures are logged at WARN.
pub async fn fetch_version(http: &dyn ScHttp, base: &Url) -> Result<Arc<str>, ScError> {
    Ok(fetch_site(http, base).await?.version)
}

async fn fetch_site(http: &dyn ScHttp, base: &Url) -> Result<Site, ScError> {
    let result = probe_site(http, base).await;
    if let Err(error) = &result {
        tracing::warn!(provider = crate::PROVIDER_ID, %base, %error,
            "StreamingCommunity version probe failed");
    }
    result
}

fn probe_error(res: &ScRes, reason: &'static str) -> ScError {
    ScError::VersionProbe {
        url: res.url.to_string(),
        status: res.status,
        location: res.location.clone().unwrap_or_else(|| "(none)".to_owned()),
        reason,
    }
}

fn migration_host(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.strip_prefix("www.").unwrap_or(host);
        let Some((label, _)) = host.split_once('.') else {
            return false;
        };
        label
            .strip_prefix(crate::HOST_NEEDLE)
            .is_some_and(|suffix| suffix.bytes().all(|b| b.is_ascii_alphanumeric()))
    })
}

async fn probe_site(http: &dyn ScHttp, base: &Url) -> Result<Site, ScError> {
    let mut url = base.join("/").map_err(|_| ScError::VersionUnreadable {
        url: base.to_string(),
    })?;
    let mut session: Option<Arc<dyn ScHttp>> = None;
    let mut redirects = 0;
    let mut fallback = false;
    loop {
        let client = session.as_deref().unwrap_or(http);
        let res = client.get(ScReq::probe(url.clone())).await?;
        if matches!(res.status, 301 | 302 | 303 | 307 | 308) {
            if redirects == 3 {
                return Err(probe_error(&res, "version probe exceeded 3 redirects"));
            }
            let next = res
                .location
                .as_deref()
                .and_then(|location| url.join(location).ok())
                .ok_or_else(|| probe_error(&res, "missing or invalid redirect destination"))?;
            if !matches!(next.scheme(), "http" | "https")
                || !next.username().is_empty()
                || next.password().is_some()
                || (url.scheme() == "https" && next.scheme() != "https")
            {
                return Err(probe_error(&res, "unsafe redirect destination"));
            }
            if next.origin() != url.origin() {
                if next.host_str() != url.host_str() && !migration_host(&next) {
                    return Err(probe_error(
                        &res,
                        "redirect is not a StreamingCommunity host",
                    ));
                }
                session = Some(client.new_session().ok_or_else(|| {
                    probe_error(&res, "cannot isolate the migrated site's session")
                })?);
            }
            redirects += 1;
            url = next;
            continue;
        }
        if !res.is_success() {
            return Err(probe_error(&res, "could not fetch the site version"));
        }
        if let Some(version) = version_from_page(&res.body) {
            if url.host_str() != base.host_str() {
                tracing::warn!(
                    "StreamingCommunity moved to {}",
                    url.host_str().unwrap_or_default()
                );
            }
            return Ok(Site {
                base: url.join("/").map_err(|_| ScError::VersionUnreadable {
                    url: url.to_string(),
                })?,
                version,
                session,
            });
        }
        if !fallback && !has_data_page(&res.body) {
            fallback = true;
            url = url.join("/it").map_err(|_| ScError::VersionUnreadable {
                url: url.to_string(),
            })?;
        } else {
            return Err(ScError::VersionUnreadable {
                url: url.to_string(),
            });
        }
    }
}

fn has_data_page(html: &str) -> bool {
    APP_DIV.as_ref().is_some_and(|selector| {
        Html::parse_document(html)
            .select(selector)
            .any(|node| node.value().attr("data-page").is_some())
    })
}

/// Pulls the Inertia asset version out of a site page body.
///
/// Split out from [`fetch_version`] so the parsing is testable without a client, and so a
/// malformed page has one obvious place to be diagnosed.
#[must_use]
pub fn version_from_page(html: &str) -> Option<std::sync::Arc<str>> {
    let selector = APP_DIV.as_ref()?;
    let doc = Html::parse_document(html);
    let data_page = doc.select(selector).next()?.attr("data-page")?;
    let parsed: Value = serde_json::from_str(data_page).ok()?;
    let version = parsed.get("version")?.as_str()?;
    if version.is_empty() {
        return None;
    }
    Some(version.into())
}

/// S2: one Inertia `GET`, with the version-drift retry (DESIGN §10.2).
///
/// `path` is site-absolute and may carry a query, e.g. `/it/watch/123?e=456`.
///
/// # Errors
/// [`ScError::VersionRejected`] when the second attempt is rejected too, plus anything
/// [`fetch_version`] or the transport can produce.
pub async fn inertia_get(
    http: &dyn ScHttp,
    versions: &SiteVersions,
    base: &Url,
    path: &str,
) -> Result<Value, ScError> {
    let site = versions.site(http, base, false).await?;
    let url = site.base.join(path).map_err(|_| ScError::BadUrlShape {
        what: "inertia",
        url: format!("{}{path}", site.base),
    })?;
    let res = site
        .http(http)
        .get(ScReq::inertia(url.clone(), &site.version))
        .await?;
    if !VERSION_REJECTED_STATUSES.contains(&res.status) {
        return res.error_for_status()?.json();
    }

    tracing::info!(
        url = %url,
        status = res.status,
        "the site rejected the cached Inertia version; refreshing it once"
    );
    let fresh = versions.site(http, base, true).await?;
    let url = fresh.base.join(path).map_err(|_| ScError::BadUrlShape {
        what: "inertia",
        url: format!("{}{path}", fresh.base),
    })?;
    let retry = fresh
        .http(http)
        .get(ScReq::inertia(url.clone(), &fresh.version))
        .await?;
    if VERSION_REJECTED_STATUSES.contains(&retry.status) {
        return Err(ScError::VersionRejected {
            url: url.to_string(),
            status: retry.status,
        });
    }
    retry.error_for_status()?.json()
}

/// `props` of an Inertia response, or an empty object — legacy's `data.get("props", {})`.
#[must_use]
pub fn props(page: &Value) -> &Value {
    static EMPTY: LazyLock<Value> = LazyLock::new(|| Value::Object(serde_json::Map::new()));
    page.get("props").unwrap_or(&EMPTY)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::testing::MockHttp;

    use super::*;

    fn base() -> Url {
        Url::parse("https://sc.test").expect("url")
    }

    #[test]
    fn the_literal_selector_compiles() {
        assert!(APP_DIV.is_some());
    }

    #[test]
    fn the_version_comes_out_of_a_real_it_page() {
        let html = include_str!("../tests/fixtures/sc/it_page.html");
        assert_eq!(
            version_from_page(html).as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
    }

    #[test]
    fn a_malformed_page_yields_nothing_rather_than_a_wrong_version() {
        for html in [
            include_str!("../tests/fixtures/sc/it_page_no_app_div.html"),
            include_str!("../tests/fixtures/sc/it_page_bad_data_page.html"),
            "",
            "<html><body>hi</body></html>",
        ] {
            assert!(version_from_page(html).is_none());
        }
    }

    #[tokio::test]
    async fn a_malformed_page_is_its_own_error_code() {
        let http = MockHttp::new().on(
            "https://sc.test/",
            200,
            include_str!("../tests/fixtures/sc/it_page_bad_data_page.html"),
        );
        let err = fetch_version(&http, &base())
            .await
            .expect_err("a page with no div#app must fail");
        assert_eq!(err.code(), crate::ScErrorCode::VersionUnreadable);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cache_is_single_flight_and_ttl_bounded() {
        let http = Arc::new(MockHttp::new().on(
            "https://sc.test/",
            200,
            include_str!("../tests/fixtures/sc/it_page.html"),
        ));
        let versions = SiteVersions::new();

        // Eight concurrent callers, one request: the per-base lock is the single flight.
        let versions = Arc::new(versions);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let http = Arc::clone(&http);
            let versions = Arc::clone(&versions);
            tasks.push(tokio::spawn(async move {
                versions.version(http.as_ref(), &base()).await
            }));
        }
        let mut seen = Vec::new();
        for t in tasks {
            seen.push(t.await.expect("join").expect("version"));
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(http.count("https://sc.test/"), 1);

        // Still one after 29 minutes, two after 31.
        tokio::time::advance(Duration::from_secs(29 * 60)).await;
        let _ = versions.version(http.as_ref(), &base()).await.expect("v");
        assert_eq!(http.count("https://sc.test/"), 1);
        tokio::time::advance(Duration::from_secs(2 * 60)).await;
        let _ = versions.version(http.as_ref(), &base()).await.expect("v");
        assert_eq!(http.count("https://sc.test/"), 2);
    }

    #[tokio::test]
    async fn a_409_refreshes_the_version_once_and_then_succeeds() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on_sequence(
                "https://sc.test/it/watch/123",
                vec![
                    (409, String::new()),
                    (200, r#"{"props":{"ok":true}}"#.to_owned()),
                ],
            );
        let versions = SiteVersions::new();
        let page = inertia_get(&http, &versions, &base(), "/it/watch/123")
            .await
            .expect("the retry must succeed");
        assert_eq!(props(&page)["ok"], true);
        // S1 ran exactly twice: once for the first attempt, once for the forced refresh.
        assert_eq!(http.count("https://sc.test/"), 2);
        assert_eq!(http.count("https://sc.test/it/watch/123"), 2);
    }

    #[tokio::test]
    async fn a_second_rejection_fails_with_its_own_code_and_does_not_loop() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on("https://sc.test/it/watch/123", 409, "");
        let versions = SiteVersions::new();
        let err = inertia_get(&http, &versions, &base(), "/it/watch/123")
            .await
            .expect_err("two rejections must fail");
        assert_eq!(err.code(), crate::ScErrorCode::VersionRejected);
        assert_eq!(http.count("https://sc.test/"), 2);
        assert_eq!(
            http.count("https://sc.test/it/watch/123"),
            2,
            "exactly one retry, not a loop"
        );
    }

    #[tokio::test]
    async fn every_rejection_status_takes_the_refresh_path() {
        for status in VERSION_REJECTED_STATUSES {
            let http = MockHttp::new()
                .on(
                    "https://sc.test/",
                    200,
                    include_str!("../tests/fixtures/sc/it_page.html"),
                )
                .on_sequence(
                    "https://sc.test/it/watch/1",
                    vec![(status, String::new()), (200, "{\"props\":{}}".to_owned())],
                );
            let versions = SiteVersions::new();
            assert!(
                inertia_get(&http, &versions, &base(), "/it/watch/1")
                    .await
                    .is_ok(),
                "status {status} must trigger the refresh"
            );
            assert_eq!(http.count("https://sc.test/"), 2, "status {status}");
        }
    }

    #[tokio::test]
    async fn a_non_rejection_error_is_reported_as_is() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on("https://sc.test/it/watch/1", 500, "");
        let versions = SiteVersions::new();
        let err = inertia_get(&http, &versions, &base(), "/it/watch/1")
            .await
            .expect_err("a 500 is not a version problem");
        assert_eq!(err.code(), crate::ScErrorCode::Status);
        assert_eq!(http.count("https://sc.test/"), 1, "no refresh for a 500");
    }

    #[tokio::test]
    async fn the_inertia_request_sends_the_cached_version() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on("https://sc.test/it/watch/1", 200, "{\"props\":{}}");
        let versions = SiteVersions::new();
        let _ = inertia_get(&http, &versions, &base(), "/it/watch/1").await;
        let sent = http.headers_for("https://sc.test/it/watch/1");
        assert_eq!(
            sent,
            vec![
                ("x-inertia".to_owned(), "true".to_owned()),
                (
                    "x-inertia-version".to_owned(),
                    "d41d8cd98f00b204e9800998ecf8427e".to_owned()
                ),
                ("accept".to_owned(), "application/json".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn invalidate_forces_the_next_read_to_refetch() {
        let http = MockHttp::new().on(
            "https://sc.test/",
            200,
            include_str!("../tests/fixtures/sc/it_page.html"),
        );
        let versions = SiteVersions::new();
        let _ = versions.version(&http, &base()).await.expect("v");
        versions.invalidate(&base()).await;
        let _ = versions.version(&http, &base()).await.expect("v");
        assert_eq!(http.count("https://sc.test/"), 2);
    }
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_domain_move_uses_a_fresh_session_and_caches_the_new_base() {
        use tracing::instrument::WithSubscriber as _;
        let migrated = Arc::new(
            MockHttp::new()
                .with_cookies("new_session=clean")
                .on(
                    "https://streamingcommunityz.new/",
                    200,
                    include_str!("../tests/fixtures/sc/it_page.html"),
                )
                .on_sequence(
                    "https://streamingcommunityz.new/it/watch/1?e=2",
                    vec![
                        (409, String::new()),
                        (200, r#"{"props":{"ok":true}}"#.to_owned()),
                    ],
                ),
        );
        let http = MockHttp::new()
            .with_cookies("secret=old")
            .with_session(Arc::clone(&migrated))
            .on_redirect("https://sc.test/", 301, "https://streamingcommunityz.new/");
        let versions = SiteVersions::new();
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        async {
            for _ in 0..2 {
                let page = inertia_get(&http, &versions, &base(), "/it/watch/1?e=2")
                    .await
                    .expect("migrated watch page");
                assert_eq!(props(&page)["ok"], true);
            }
        }
        .with_subscriber(subscriber)
        .await;
        assert_eq!(http.urls(), ["https://sc.test/"]);
        assert_eq!(migrated.count("https://streamingcommunityz.new/"), 2);
        assert_eq!(
            migrated.count("https://streamingcommunityz.new/it/watch/1?e=2"),
            3
        );
        let site = versions
            .site(&http, &base(), false)
            .await
            .expect("cached site");
        assert_eq!(site.base.as_str(), "https://streamingcommunityz.new/");
        assert_eq!(site.http(&http).cookie_header(), "new_session=clean");
        let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("utf8");
        assert!(output.contains("WARN"), "{output}");
        assert!(
            output.contains("StreamingCommunity moved to streamingcommunityz.new"),
            "{output}"
        );
    }

    #[tokio::test]
    async fn unsafe_redirects_are_not_requested_and_warn_with_the_location() {
        use tracing::instrument::WithSubscriber as _;
        for location in [
            "https://unrelated.test/",
            "https://streaming-community.test/",
            "https://evilstreamingcommunity.test/",
            "http://sc.test/",
            "https://user:pass@sc.test/",
            "file:///tmp/version",
            "https://[",
        ] {
            let http = MockHttp::new().on_redirect("https://sc.test/", 301, location);
            let logs = Logs::default();
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            let error = fetch_version(&http, &base())
                .with_subscriber(subscriber)
                .await
                .expect_err("unsafe redirect must fail");
            assert!(error.to_string().contains(location));
            assert_eq!(
                error.into_provider_error().code(),
                aulos_core::error::ErrorCode::Network
            );
            assert_eq!(http.total(), 1);
            let output = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("utf8");
            assert!(
                output.contains("WARN") && output.contains(location),
                "{output}"
            );
        }
    }

    #[tokio::test]
    async fn a_domain_move_cannot_reuse_a_client_that_cannot_isolate_its_session() {
        let http = MockHttp::new().with_cookies("secret=old").on_redirect(
            "https://sc.test/",
            301,
            "https://streamingcommunity.new/",
        );
        let error = fetch_version(&http, &base())
            .await
            .expect_err("no fresh session");
        assert!(error.to_string().contains("cannot isolate"));
        assert_eq!(http.total(), 1);
    }
}
