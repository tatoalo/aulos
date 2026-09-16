//! The [`Provider`] implementation: `matches`, `resolve`, `own_slots` and `probe` (DESIGN §10.2).
//!
//! # `matches` is host **and** path, and that is a deliberate bug-for-bug port
//!
//! Legacy split detection from dispatch: `can_extract()` tested the hostname only, but `extract()`
//! dispatched on the path and returned `None` for anything that was not `/watch/`, `/titles/` or
//! `/season-` — at which point `__extract_info` handed the URL to yt-dlp
//! (`streamingcommunity.py:387-397`, legacy spec §9.1).
//!
//! A host-only `Strong(200)` here would therefore be a regression: an SC search page, a browse
//! page or a mirror's homepage would terminate with `unsupported_url` where the old server quietly
//! let yt-dlp try. So an SC host with a non-dispatchable path answers [`Match::No`], and the
//! remaining legacy case — a scrape that dispatches but yields nothing — is covered by
//! [`ProviderError::Unsupported`] and the engine's one runner-up retry (DESIGN §6.4).

use std::sync::Arc;

use async_trait::async_trait;
use aulos_core::catalog::FormatCatalog;
use aulos_core::config::Config;
use aulos_core::selection::ProviderId;
use aulos_provider::entry::{EntryKind, MediaEntry};
use aulos_provider::outcome::Outcome;
use aulos_provider::provider::{
    DownloadCtx, Match, Provider, ProviderError, ProviderHealth, ResolveCtx, SCORE_SC,
};
use aulos_provider::sink::ProgressSink;
use url::Url;

use crate::engines::EngineCfg;
use crate::error::{ScError, ScInitError};
use crate::http::{ScHttp, build_client};
use crate::inertia::SiteVersions;
use crate::{catalog, engines, season, watch};

/// The three path fragments legacy's `extract()` dispatched on, in its order
/// (`streamingcommunity.py:389-394`).
pub const DISPATCH_FRAGMENTS: [&str; 3] = ["/season-", "/watch/", "/titles/"];

/// DESIGN §10.2's `matches()`, as a free function.
///
/// Free rather than a method so the caller that has to register a
/// [`DegradedProvider`](aulos_provider::provider::DegradedProvider) — because the client would not
/// build — can hand it the *same* matcher the real provider would have used, which is what makes a
/// degraded SC provider fail loudly on SC URLs instead of leaking them to yt-dlp (DESIGN §6.4).
#[must_use]
pub fn sc_matches(url: &Url, extra_hosts: &[Box<str>]) -> Match {
    let Some(host) = url.host_str() else {
        return Match::No;
    };
    let host = host.to_lowercase();
    let host_ok = host.contains(crate::HOST_NEEDLE)
        || extra_hosts
            .iter()
            .any(|h| !h.is_empty() && host.contains(&h.to_lowercase()));
    if !host_ok {
        return Match::No;
    }
    if dispatch_fragment(url.path()).is_some() {
        Match::Strong(SCORE_SC)
    } else {
        // Legacy logged "Unsupported URL format" and returned `None`, so yt-dlp got the URL.
        Match::No
    }
}

/// Which of legacy's three dispatch fragments a path carries, in legacy's precedence order.
#[must_use]
pub fn dispatch_fragment(path: &str) -> Option<&'static str> {
    DISPATCH_FRAGMENTS.into_iter().find(|f| path.contains(f))
}

/// The StreamingCommunity provider.
pub struct ScProvider {
    http: Arc<dyn ScHttp>,
    versions: Arc<SiteVersions>,
    catalog: Arc<FormatCatalog>,
    extra_hosts: Vec<Box<str>>,
    meta_concurrency: usize,
    own_slots: usize,
    engine: EngineCfg,
}

impl ScProvider {
    /// Builds the provider, including the HTTP client `AULOS_SC_HTTP` asks for.
    ///
    /// # Errors
    /// [`ScInitError::ImpersonateUnavailable`] when `AULOS_SC_HTTP=impersonate` and the
    /// `sc-impersonate` feature is off, and [`ScInitError::Client`] when no client will build. In
    /// both cases the caller registers a `DegradedProvider` built from [`sc_matches`] rather than
    /// dropping the provider (DESIGN §6.4).
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the DESIGN §10.1 / PLAN WP-08 constructor signature the binary is written against"
    )]
    pub fn new(cfg: Arc<Config>) -> Result<Self, ScInitError> {
        let http = build_client(&cfg)?;
        Ok(Self::with_http(&cfg, http))
    }

    /// Builds the provider around an existing client — the seam the tests inject a mock through.
    #[must_use]
    pub fn with_http(cfg: &Config, http: Arc<dyn ScHttp>) -> Self {
        if !http.impersonating() {
            tracing::warn!(
                "StreamingCommunity is running without Chrome TLS impersonation; \
                 Cloudflare may block extraction (AULOS_SC_HTTP, DESIGN §10.1)"
            );
        }
        Self {
            http,
            versions: Arc::new(SiteVersions::new()),
            catalog: catalog::sc_catalog(),
            extra_hosts: cfg.sc_extra_hosts.clone(),
            meta_concurrency: cfg.sc_meta_concurrency.max(1) as usize,
            own_slots: cfg.sc_max_concurrent_downloads.max(1) as usize,
            engine: EngineCfg::from_config(cfg),
        }
    }

    /// Replaces the engine configuration (WP-09).
    ///
    /// The one seam the engine tests need: it points `argv[0]` at a fixture script so the whole
    /// download path — argv, progress plumbing, the ffmpeg retry, cancellation and cleanup — is
    /// exercised without `N_m3u8DL-RE` installed.
    #[must_use]
    pub fn with_engine(mut self, engine: EngineCfg) -> Self {
        self.engine = engine;
        self
    }

    /// The binaries and knobs [`engines::download`] runs with.
    #[must_use]
    pub fn engine(&self) -> &EngineCfg {
        &self.engine
    }

    /// Whether the underlying client presents a Chrome fingerprint.
    ///
    /// Reported as `providers[].impersonating` in `healthz` and `GET api/v2/providers`
    /// (DESIGN §10.1, §16.3).
    #[must_use]
    pub fn impersonating(&self) -> bool {
        self.http.impersonating()
    }

    /// The HTTP client, for the download engines' just-in-time re-extraction.
    #[must_use]
    pub fn http(&self) -> &Arc<dyn ScHttp> {
        &self.http
    }

    /// `{scheme}://{host}[:port]` of a URL — legacy's `base_url`
    /// (`streamingcommunity.py:496-497`).
    #[must_use]
    pub fn base_of(url: &Url) -> Option<Url> {
        let host = url.host_str()?;
        let mut s = format!("{}://{host}", url.scheme());
        if let Some(port) = url.port() {
            s.push(':');
            s.push_str(&port.to_string());
        }
        Url::parse(&s).ok()
    }

    /// The scrape, dispatched exactly as legacy's `extract()` did.
    async fn scrape(&self, url: &Url) -> Result<MediaEntry, ScError> {
        let base = Self::base_of(url).ok_or_else(|| ScError::BadUrlShape {
            what: "site",
            url: url.to_string(),
        })?;
        match dispatch_fragment(url.path()) {
            Some("/season-") => {
                season::resolve_season(self.http.as_ref(), &self.versions, &base, url).await
            }
            Some("/watch/") => {
                watch::resolve_watch(self.http.as_ref(), &self.versions, &base, url).await
            }
            Some("/titles/") => {
                season::resolve_title(
                    self.http.as_ref(),
                    &self.versions,
                    &base,
                    url,
                    self.meta_concurrency,
                )
                .await
            }
            _ => Err(ScError::BadUrlShape {
                what: "watch, titles or season",
                url: url.to_string(),
            }),
        }
    }
}

#[async_trait]
impl Provider for ScProvider {
    fn id(&self) -> ProviderId {
        crate::provider_id()
    }

    fn matches(&self, url: &Url) -> Match {
        sc_matches(url, &self.extra_hosts)
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        url: &Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        if ctx.cancel.is_cancelled() {
            return Err(ProviderError::Canceled);
        }
        let scrape = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Err(ProviderError::Canceled),
            r = self.scrape(url) => r,
        };
        let mut entry = scrape.map_err(ScError::into_provider_error)?;

        // `playlist_item_limit` (DESIGN §8.4). The engine caps children too, but truncating here
        // means a 500-episode title never allocates 500 entries just to drop them.
        if let (Some(limit), EntryKind::Playlist { entries, .. }) =
            (ctx.playlist_end, &mut entry.kind)
            && entries.len() > limit as usize
        {
            entries.truncate(limit as usize);
        }
        if entry.is_playlist() && entry.children().is_empty() {
            return Err(ProviderError::Unsupported(format!(
                "nothing resolvable at {url}"
            )));
        }
        Ok(vec![entry])
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        engines::download(self, ctx, sink).await
    }

    /// `SC_MAX_CONCURRENT_DOWNLOADS`, acquired **instead of** the global slot — exactly legacy's
    /// `sc_semaphore` positioning (DESIGN §8.7, §10.5).
    fn own_slots(&self) -> Option<usize> {
        Some(self.own_slots)
    }

    /// Always `Ok`.
    ///
    /// A client without impersonation is a *degradation of quality*, not of readiness: DESIGN
    /// §10.1 is explicit that `auto` with the feature off registers as `Ready` with
    /// `impersonating: false`. The only `Degraded` SC registration comes from
    /// [`ScProvider::new`] failing outright, which the caller turns into a `DegradedProvider`.
    async fn probe(&self) -> ProviderHealth {
        ProviderHealth::Ok
    }
}

#[cfg(test)]
mod tests {
    use aulos_core::config::{self, RawEnv};
    use aulos_core::id::ItemId;
    use aulos_core::paths::Paths;
    use aulos_core::request::DownloadRequest;
    use aulos_core::ytdl_options::YtdlOptions;
    use tokio::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    use crate::testing::MockHttp;

    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Arc<Config> {
        Arc::new(config::load(&RawEnv::from_pairs(pairs.iter().copied())).expect("config"))
    }

    fn provider(http: MockHttp, pairs: &[(&str, &str)]) -> ScProvider {
        let c = cfg(pairs);
        ScProvider::with_http(&c, Arc::new(http))
    }

    fn ctx<'a>(
        request: &'a DownloadRequest,
        paths: &'a Paths,
        cancel: CancellationToken,
        playlist_end: Option<u32>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            item_id: ItemId::new(),
            request,
            ytdl_options: Arc::new(YtdlOptions::default()),
            paths,
            flat: false,
            playlist_end,
            cancel,
            deadline: Instant::now() + Duration::from_secs(120),
        }
    }

    fn request() -> DownloadRequest {
        use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
        DownloadRequest::new(
            Url::parse("https://sc.test/it/watch/1").expect("url"),
            Selection::new(
                DownloadType::Video,
                Codec::Auto,
                FormatId::parse("mp4").expect("format"),
                QualityId::parse("best").expect("quality"),
            ),
        )
    }

    fn paths() -> Paths {
        let c = cfg(&[]);
        c.paths.clone()
    }

    fn m(url: &str) -> Match {
        sc_matches(&Url::parse(url).expect("url"), &[])
    }

    #[test]
    fn a_dispatchable_path_on_an_sc_host_is_strong_200() {
        for url in [
            "https://streamingcommunity.example/it/watch/123",
            "https://streamingcommunity.example/it/watch/123?e=456",
            "https://streamingcommunity.example/it/titles/9-slug",
            "https://streamingcommunity.example/it/titles/9-slug/season-2",
            "https://www.streamingcommunityz.net/en/titles/1-x",
            "https://STREAMINGCOMMUNITY.example/it/watch/1",
        ] {
            assert_eq!(m(url), Match::Strong(200), "{url}");
            assert_eq!(Match::Strong(SCORE_SC).score(), 200);
        }
    }

    #[test]
    fn an_sc_host_with_any_other_path_is_no_so_ytdlp_gets_it_exactly_as_legacy_did() {
        for url in [
            "https://streamingcommunity.example/search?q=x",
            "https://streamingcommunity.example/",
            "https://streamingcommunity.example/browse",
            "https://streamingcommunity.example/it",
            "https://streamingcommunity.example/it/browse/genre",
        ] {
            assert_eq!(m(url), Match::No, "{url}");
        }
    }

    #[test]
    fn a_non_sc_host_is_never_matched() {
        for url in [
            "https://youtube.com/watch?v=abc",
            "https://example.com/it/watch/123",
            "https://example.com/it/titles/9-slug/season-1",
        ] {
            assert_eq!(m(url), Match::No, "{url}");
        }
    }

    #[test]
    fn an_extra_host_needs_a_dispatchable_path_too() {
        let extra: Vec<Box<str>> = vec!["scmirror.test".into()];
        let mirror = Url::parse("https://scmirror.test/it/watch/5").expect("url");
        assert_eq!(sc_matches(&mirror, &extra), Match::Strong(200));
        let home = Url::parse("https://scmirror.test/").expect("url");
        assert_eq!(sc_matches(&home, &extra), Match::No);
        // Without the config entry the mirror is not ours.
        assert_eq!(sc_matches(&mirror, &[]), Match::No);
        // An empty entry (a trailing comma in the env var) must not match everything.
        let empty: Vec<Box<str>> = vec![String::new().into()];
        assert_eq!(
            sc_matches(
                &Url::parse("https://youtube.com/watch/1").expect("url"),
                &empty
            ),
            Match::No
        );
    }

    #[test]
    fn extra_hosts_are_read_from_the_config() {
        let p = provider(
            MockHttp::new(),
            &[("AULOS_SC_EXTRA_HOSTS", "scmirror.test,alt.test")],
        );
        assert_eq!(
            p.matches(&Url::parse("https://alt.test/it/watch/5").expect("url")),
            Match::Strong(200)
        );
    }

    #[test]
    fn the_dispatch_order_is_legacys() {
        // A season URL contains both `/titles/` and `/season-`; legacy checked `/season-` first.
        assert_eq!(
            dispatch_fragment("/it/titles/9-slug/season-2"),
            Some("/season-")
        );
        assert_eq!(dispatch_fragment("/it/watch/1"), Some("/watch/"));
        assert_eq!(dispatch_fragment("/it/titles/9-slug"), Some("/titles/"));
        assert_eq!(dispatch_fragment("/search"), None);
    }

    #[test]
    fn the_base_url_is_scheme_and_host() {
        assert_eq!(
            ScProvider::base_of(&Url::parse("https://sc.test/it/watch/1?e=2").expect("url"))
                .map(|u| u.to_string()),
            Some("https://sc.test/".to_owned())
        );
        assert_eq!(
            ScProvider::base_of(&Url::parse("http://sc.test:8080/it/watch/1").expect("url"))
                .map(|u| u.to_string()),
            Some("http://sc.test:8080/".to_owned())
        );
    }

    #[test]
    fn the_provider_reports_its_id_slots_and_catalog() {
        let p = provider(MockHttp::new(), &[("SC_MAX_CONCURRENT_DOWNLOADS", "3")]);
        assert_eq!(p.id().as_str(), "streamingcommunity");
        assert_eq!(p.own_slots(), Some(3));
        assert_eq!(p.catalog().provider.as_str(), "streamingcommunity");
        assert!(!p.impersonating());
    }

    #[tokio::test]
    async fn probe_stays_ok_without_impersonation() {
        let p = provider(MockHttp::new(), &[]);
        assert_eq!(p.probe().await, ProviderHealth::Ok);
    }

    #[tokio::test]
    async fn the_degraded_registration_still_claims_sc_urls_and_fails_loudly() {
        // What the binary does with an `ScInitError` (DESIGN §6.4): register a stand-in built from
        // the *same* matcher, so an SC URL fails with `provider_degraded` and the reason instead of
        // leaking to yt-dlp, which would download a Cloudflare page and call it a success.
        use aulos_provider::provider::DegradedProvider;
        let extra: Vec<Box<str>> = vec!["scmirror.test".into()];
        let d = DegradedProvider::new(
            crate::provider_id(),
            ScInitError::ImpersonateUnavailable.to_string(),
            catalog::sc_catalog(),
            Box::new(move |u: &Url| sc_matches(u, &extra)),
        );
        assert_eq!(d.reason(), "sc-impersonate not compiled in");
        assert_eq!(
            d.matches(&Url::parse("https://streamingcommunity.example/it/watch/1").expect("url")),
            Match::Strong(200),
        );
        assert_eq!(
            d.matches(&Url::parse("https://scmirror.test/it/titles/9-x/season-1").expect("url")),
            Match::Strong(200),
        );
        // ...and it still hands a non-dispatchable path to yt-dlp.
        assert_eq!(
            d.matches(&Url::parse("https://streamingcommunity.example/search").expect("url")),
            Match::No,
        );
        assert_eq!(
            d.probe().await,
            ProviderHealth::Down("sc-impersonate not compiled in".into())
        );
        let (r, pa) = (request(), paths());
        let err = d
            .resolve(
                &Url::parse("https://streamingcommunity.example/it/watch/1").expect("url"),
                ctx(&r, &pa, CancellationToken::new(), None),
            )
            .await
            .expect_err("a degraded provider never resolves");
        assert_eq!(err.code(), aulos_core::error::ErrorCode::ProviderDegraded);
    }

    #[tokio::test]
    async fn resolving_a_season_yields_one_playlist_entry() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-2",
                200,
                include_str!("../tests/fixtures/sc/season_20.json"),
            );
        let p = provider(http, &[]);
        let (r, pa) = (request(), paths());
        let out = p
            .resolve(
                &Url::parse("https://sc.test/it/titles/9-una-serie/season-2").expect("url"),
                ctx(&r, &pa, CancellationToken::new(), None),
            )
            .await
            .expect("a playlist");
        assert_eq!(out.len(), 1);
        assert!(out[0].is_playlist());
        assert_eq!(out[0].children().len(), 20);
    }

    #[tokio::test]
    async fn playlist_end_truncates_the_children() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-2",
                200,
                include_str!("../tests/fixtures/sc/season_20.json"),
            );
        let p = provider(http, &[]);
        let (r, pa) = (request(), paths());
        let out = p
            .resolve(
                &Url::parse("https://sc.test/it/titles/9-una-serie/season-2").expect("url"),
                ctx(&r, &pa, CancellationToken::new(), Some(3)),
            )
            .await
            .expect("a playlist");
        assert_eq!(out[0].children().len(), 3);
    }

    #[tokio::test]
    async fn a_scrape_that_resolves_nothing_is_unsupported_so_the_runner_up_gets_a_turn() {
        // A dispatchable path whose season page is empty: legacy returned `None` here and
        // `__extract_info` retried through yt-dlp (DESIGN §6.4).
        let http = MockHttp::new()
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-2",
                200,
                r#"{"props":{"loadedSeason":{"episodes":[]}}}"#,
            );
        let p = provider(http, &[]);
        let (r, pa) = (request(), paths());
        let err = p
            .resolve(
                &Url::parse("https://sc.test/it/titles/9-una-serie/season-2").expect("url"),
                ctx(&r, &pa, CancellationToken::new(), None),
            )
            .await
            .expect_err("nothing resolvable");
        assert!(
            matches!(err, ProviderError::Unsupported(_)),
            "got {err:?}, which the engine would not retry"
        );
    }

    #[tokio::test]
    async fn an_already_cancelled_resolve_makes_no_request() {
        let http = MockHttp::new();
        let p = provider(http, &[]);
        let (r, pa) = (request(), paths());
        let token = CancellationToken::new();
        token.cancel();
        let err = p
            .resolve(
                &Url::parse("https://sc.test/it/watch/1").expect("url"),
                ctx(&r, &pa, token, None),
            )
            .await
            .expect_err("cancelled");
        assert!(matches!(err, ProviderError::Canceled));
    }

    #[tokio::test]
    async fn a_non_dispatchable_url_reaching_resolve_is_unsupported() {
        let p = provider(MockHttp::new(), &[]);
        let (r, pa) = (request(), paths());
        let err = p
            .resolve(
                &Url::parse("https://sc.test/search?q=x").expect("url"),
                ctx(&r, &pa, CancellationToken::new(), None),
            )
            .await
            .expect_err("not dispatchable");
        assert!(matches!(err, ProviderError::Unsupported(_)));
    }

    #[test]
    fn an_impersonate_only_config_without_the_feature_degrades_rather_than_panicking() {
        let c = cfg(&[("AULOS_SC_HTTP", "impersonate")]);
        match ScProvider::new(c) {
            Ok(p) => assert!(
                p.impersonating(),
                "with the feature on, impersonate mode must actually impersonate"
            ),
            Err(e) => assert_eq!(
                e,
                ScInitError::ImpersonateUnavailable,
                "with the feature off, the caller must get the Degraded reason, not a panic"
            ),
        }
    }
}
