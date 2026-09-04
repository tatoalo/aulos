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
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use scraper::{Html, Selector};
use serde_json::Value;
use tokio::time::Instant;
use url::Url;

use crate::error::ScError;
use crate::http::{ScHttp, ScReq};

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
/// holds it across the `GET {base}/it`, every other caller for the same base waits and then reads
/// the value it stored. There is no dogpile and no second request.
#[derive(Debug, Default)]
pub struct SiteVersions {
    slots: Mutex<HashMap<Box<str>, std::sync::Arc<Slot>>>,
}

type Slot = tokio::sync::Mutex<Option<Cached>>;

#[derive(Clone, Debug)]
struct Cached {
    version: std::sync::Arc<str>,
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

    /// The cached version, or one fresh `GET {base}/it` shared by every concurrent caller.
    ///
    /// # Errors
    /// Whatever [`fetch_version`] returns.
    pub async fn version(
        &self,
        http: &dyn ScHttp,
        base: &Url,
    ) -> Result<std::sync::Arc<str>, ScError> {
        let slot = self.slot(base);
        let mut guard = slot.lock().await;
        if let Some(c) = guard.as_ref()
            && c.fetched.elapsed() < VERSION_TTL
        {
            return Ok(std::sync::Arc::clone(&c.version));
        }
        let version = fetch_version(http, base).await?;
        *guard = Some(Cached {
            version: std::sync::Arc::clone(&version),
            fetched: Instant::now(),
        });
        Ok(version)
    }

    /// Discards the cached version and fetches a new one, ignoring the TTL.
    ///
    /// This is the "the site deployed while we were scraping" path, and it is the only thing that
    /// runs between the first rejected Inertia call and its one retry.
    ///
    /// # Errors
    /// Whatever [`fetch_version`] returns.
    pub async fn refresh(
        &self,
        http: &dyn ScHttp,
        base: &Url,
    ) -> Result<std::sync::Arc<str>, ScError> {
        let slot = self.slot(base);
        let mut guard = slot.lock().await;
        *guard = None;
        let version = fetch_version(http, base).await?;
        *guard = Some(Cached {
            version: std::sync::Arc::clone(&version),
            fetched: Instant::now(),
        });
        Ok(version)
    }

    /// Drops the cached version for one base without fetching.
    pub async fn invalidate(&self, base: &Url) {
        let slot = self.slot(base);
        let mut guard = slot.lock().await;
        *guard = None;
    }
}

/// S1: `GET {base}/it`, then `div#app[data-page]` → JSON → `.version` (DESIGN §10.2).
///
/// Byte-for-byte the legacy extraction (`streamingcommunity.py:30-44`), including the
/// `raise_for_status()` before any parsing.
///
/// # Errors
/// [`ScError::Status`] for a non-2xx, [`ScError::VersionUnreadable`] when the page carries no
/// `div#app`, no `data-page`, unparseable JSON in it, or no `version` string.
pub async fn fetch_version(http: &dyn ScHttp, base: &Url) -> Result<std::sync::Arc<str>, ScError> {
    let url = base.join("/it").map_err(|_| ScError::VersionUnreadable {
        url: base.to_string(),
    })?;
    let res = http
        .get(ScReq::get(url.clone()))
        .await?
        .error_for_status()?;
    version_from_page(&res.body).ok_or(ScError::VersionUnreadable {
        url: url.to_string(),
    })
}

/// Pulls the Inertia asset version out of an `/it` page body.
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
    let url = base.join(path).map_err(|_| ScError::BadUrlShape {
        what: "inertia",
        url: format!("{base}{path}"),
    })?;
    let version = versions.version(http, base).await?;
    let res = http.get(ScReq::inertia(url.clone(), &version)).await?;
    if !VERSION_REJECTED_STATUSES.contains(&res.status) {
        return res.error_for_status()?.json();
    }

    tracing::info!(
        url = %url,
        status = res.status,
        "the site rejected the cached Inertia version; refreshing it once"
    );
    let fresh = versions.refresh(http, base).await?;
    let retry = http.get(ScReq::inertia(url.clone(), &fresh)).await?;
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
            "https://sc.test/it",
            200,
            include_str!("../tests/fixtures/sc/it_page_no_app_div.html"),
        );
        let err = fetch_version(&http, &base())
            .await
            .expect_err("a page with no div#app must fail");
        assert_eq!(err.code(), crate::ScErrorCode::VersionUnreadable);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cache_is_single_flight_and_ttl_bounded() {
        let http = Arc::new(MockHttp::new().on(
            "https://sc.test/it",
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
        assert_eq!(http.count("https://sc.test/it"), 1);

        // Still one after 29 minutes, two after 31.
        tokio::time::advance(Duration::from_secs(29 * 60)).await;
        let _ = versions.version(http.as_ref(), &base()).await.expect("v");
        assert_eq!(http.count("https://sc.test/it"), 1);
        tokio::time::advance(Duration::from_secs(2 * 60)).await;
        let _ = versions.version(http.as_ref(), &base()).await.expect("v");
        assert_eq!(http.count("https://sc.test/it"), 2);
    }

    #[tokio::test]
    async fn a_409_refreshes_the_version_once_and_then_succeeds() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/it",
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
        assert_eq!(http.count("https://sc.test/it"), 2);
        assert_eq!(http.count("https://sc.test/it/watch/123"), 2);
    }

    #[tokio::test]
    async fn a_second_rejection_fails_with_its_own_code_and_does_not_loop() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on("https://sc.test/it/watch/123", 409, "");
        let versions = SiteVersions::new();
        let err = inertia_get(&http, &versions, &base(), "/it/watch/123")
            .await
            .expect_err("two rejections must fail");
        assert_eq!(err.code(), crate::ScErrorCode::VersionRejected);
        assert_eq!(http.count("https://sc.test/it"), 2);
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
                    "https://sc.test/it",
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
            assert_eq!(http.count("https://sc.test/it"), 2, "status {status}");
        }
    }

    #[tokio::test]
    async fn a_non_rejection_error_is_reported_as_is() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on("https://sc.test/it/watch/1", 500, "");
        let versions = SiteVersions::new();
        let err = inertia_get(&http, &versions, &base(), "/it/watch/1")
            .await
            .expect_err("a 500 is not a version problem");
        assert_eq!(err.code(), crate::ScErrorCode::Status);
        assert_eq!(http.count("https://sc.test/it"), 1, "no refresh for a 500");
    }

    #[tokio::test]
    async fn the_inertia_request_sends_the_cached_version() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/it",
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
            "https://sc.test/it",
            200,
            include_str!("../tests/fixtures/sc/it_page.html"),
        );
        let versions = SiteVersions::new();
        let _ = versions.version(&http, &base()).await.expect("v");
        versions.invalidate(&base()).await;
        let _ = versions.version(&http, &base()).await.expect("v");
        assert_eq!(http.count("https://sc.test/it"), 2);
    }
}
