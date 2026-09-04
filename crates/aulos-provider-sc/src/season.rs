//! `/titles/…/season-n` and `/titles/…` resolution — **the one substantive improvement in the SC
//! port** (DESIGN §10.4).
//!
//! Legacy's `extract_season` called `extract_episode` per episode, and `extract_episode` did
//! watch → embed → iframe → stream *per episode* purely as a validity probe, then threw the m3u8
//! away because only the watch URL is persisted. A 20-episode season was therefore ~60 HTTP round
//! trips for information that `props.loadedSeason.episodes` already contains in full
//! (`streamingcommunity.py:129-242`).
//!
//! [`resolve_season`] performs **zero** embed or stream requests: the title page for the series
//! name, the season page for the episode list, and then one synthesised [`MediaEntry`] per
//! episode. Two requests. Nothing about the produced file changes, because the just-in-time
//! re-extraction at download time ([`crate::jit`]) is what actually resolves a stream, and that is
//! unchanged.
//!
//! [`resolve_title`] for a TV title fetches each season's JSON once — bounded by
//! `AULOS_SC_META_CONCURRENCY` — and emits one flattened playlist, exactly as legacy did.

use std::sync::LazyLock;

use aulos_provider::entry::{EntryHints, EntryKind, LiveStatus, MediaEntry};
use futures_util::StreamExt;
use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::error::ScError;
use crate::http::ScHttp;
use crate::inertia::{SiteVersions, inertia_get, props};
use crate::state::ScState;
use crate::watch::{base_string, episode_title, resolve_watch};

/// `/titles/(\d+)-([^/]+)/season-(\d+)` — legacy's season-URL matcher
/// (`streamingcommunity.py:195`).
static SEASON_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"/titles/(\d+)-([^/]+)/season-(\d+)").ok());

/// `/titles/(\d+)-([^/]+)$` — legacy's title-URL matcher (`streamingcommunity.py:328`).
static TITLE_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"/titles/(\d+)-([^/]+)$").ok());

/// A JSON string field, or a default — legacy's `d.get(k, default)` with its `"Unknown"`/`""`
/// defaults.
#[must_use]
pub fn str_or<'a>(v: Option<&'a Value>, default: &'a str) -> &'a str {
    v.and_then(Value::as_str).unwrap_or(default)
}

/// The `(title_id, slug, season_number)` a season URL carries.
#[must_use]
pub fn parse_season_url(url: &str) -> Option<(String, String, u32)> {
    let caps = SEASON_URL.as_ref()?.captures(url)?;
    Some((
        caps.get(1)?.as_str().to_owned(),
        caps.get(2)?.as_str().to_owned(),
        caps.get(3)?.as_str().parse().ok()?,
    ))
}

/// The `(title_id, slug)` a title URL carries. A trailing `/season-n` does **not** match, which is
/// what makes `extract()`'s dispatch order irrelevant.
#[must_use]
pub fn parse_title_url(url: &str) -> Option<(String, String)> {
    let caps = TITLE_URL.as_ref()?.captures(url)?;
    Some((
        caps.get(1)?.as_str().to_owned(),
        caps.get(2)?.as_str().to_owned(),
    ))
}

/// One season, as a playlist entry, in **exactly two** HTTP requests (plus S1, which is cached).
///
/// # Errors
/// [`ScError::BadUrlShape`] when the URL is not a season URL,
/// [`ScError::NothingResolved`] when the season carries no episodes, plus whatever
/// [`inertia_get`] returns.
pub async fn resolve_season(
    http: &dyn ScHttp,
    versions: &SiteVersions,
    base: &Url,
    url: &Url,
) -> Result<MediaEntry, ScError> {
    let (title_id, slug, season_num) =
        parse_season_url(url.as_str()).ok_or_else(|| ScError::BadUrlShape {
            what: "season",
            url: url.to_string(),
        })?;

    // Request 1: the title page, for the series name.
    let title_page = inertia_get(
        http,
        versions,
        base,
        &format!("/it/titles/{title_id}-{slug}"),
    )
    .await?;
    let title_name = str_or(
        props(&title_page).get("title").and_then(|t| t.get("name")),
        "Unknown",
    )
    .to_owned();

    // Request 2: the season page, for the episode list.
    let entries = season_entries(
        http,
        versions,
        base,
        &title_id,
        &slug,
        season_num,
        &title_name,
    )
    .await?;
    if entries.is_empty() {
        return Err(ScError::NothingResolved {
            url: url.to_string(),
        });
    }

    let playlist_title = format!("{title_name} Season {season_num}");
    Ok(playlist(
        format!("sc_{title_id}_s{season_num}"),
        playlist_title,
        url.clone(),
        entries,
    ))
}

/// One title: a movie delegates to the watch path, a series flattens every season into one
/// playlist.
///
/// # Errors
/// [`ScError::BadUrlShape`] when the URL is not a title URL, [`ScError::NothingResolved`] when the
/// series has no seasons or no episodes, plus whatever [`inertia_get`] and [`resolve_watch`]
/// return.
pub async fn resolve_title(
    http: &dyn ScHttp,
    versions: &SiteVersions,
    base: &Url,
    url: &Url,
    meta_concurrency: usize,
) -> Result<MediaEntry, ScError> {
    let (title_id, slug) = parse_title_url(url.as_str()).ok_or_else(|| ScError::BadUrlShape {
        what: "title",
        url: url.to_string(),
    })?;

    let page = inertia_get(
        http,
        versions,
        base,
        &format!("/it/titles/{title_id}-{slug}"),
    )
    .await?;
    let page_props = props(&page);
    let info = page_props.get("title");
    let title_name = str_or(info.and_then(|t| t.get("name")), "Unknown").to_owned();
    let title_type = str_or(info.and_then(|t| t.get("type")), "movie").to_owned();

    if title_type != "tv" {
        // Legacy: `extract_watch(f"{base_url}/it/watch/{title_id}")`.
        let watch_url =
            Url::parse(&format!("{}/it/watch/{title_id}", base_string(base))).map_err(|_| {
                ScError::BadUrlShape {
                    what: "watch",
                    url: url.to_string(),
                }
            })?;
        return resolve_watch(http, versions, base, &watch_url).await;
    }

    let season_numbers = declared_season_numbers(page_props);
    if season_numbers.is_empty() {
        return Err(ScError::NothingResolved {
            url: url.to_string(),
        });
    }

    // One request per season, `AULOS_SC_META_CONCURRENCY` at a time, results in season order.
    let concurrency = meta_concurrency.max(1);
    let mut all: Vec<MediaEntry> =
        futures_util::stream::iter(
            season_numbers.into_iter().map(|n| {
                let title_name = title_name.clone();
                let title_id = title_id.clone();
                let slug = slug.clone();
                async move {
                    season_entries(http, versions, base, &title_id, &slug, n, &title_name).await
                }
            }),
        )
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();

    if all.is_empty() {
        return Err(ScError::NothingResolved {
            url: url.to_string(),
        });
    }
    renumber(&mut all, &title_name);
    Ok(playlist(
        format!("sc_{title_id}"),
        title_name,
        url.clone(),
        all,
    ))
}

/// The season numbers a title page declares, in order.
///
/// Legacy read `title.seasons`, then fell back to wrapping `props.loadedSeason` as a one-element
/// list (`streamingcommunity.py:349-354`); both are reproduced, and a season with no `number` is
/// skipped rather than being resolved as season `0`.
fn declared_season_numbers(page_props: &Value) -> Vec<u32> {
    let from_seasons = page_props
        .get("title")
        .and_then(|t| t.get("seasons"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut numbers: Vec<u32> = from_seasons.iter().filter_map(season_number).collect();
    if numbers.is_empty()
        && let Some(n) = page_props.get("loadedSeason").and_then(season_number)
    {
        numbers.push(n);
    }
    numbers
}

fn season_number(v: &Value) -> Option<u32> {
    v.get("number")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// One `GET /it/titles/{id}-{slug}/season-{n}`, turned into one entry per episode.
///
/// This is the whole of DESIGN §10.4: `props.loadedSeason.episodes` already carries the id, the
/// number and the name of every episode, so no per-episode request is needed.
async fn season_entries(
    http: &dyn ScHttp,
    versions: &SiteVersions,
    base: &Url,
    title_id: &str,
    slug: &str,
    season_num: u32,
    title_name: &str,
) -> Result<Vec<MediaEntry>, ScError> {
    let page = inertia_get(
        http,
        versions,
        base,
        &format!("/it/titles/{title_id}-{slug}/season-{season_num}"),
    )
    .await?;
    let episodes = props(&page)
        .get("loadedSeason")
        .and_then(|s| s.get("episodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let total = u32::try_from(episodes.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(episodes.len());
    for (i, ep) in episodes.iter().enumerate() {
        let Some(ep_id) = ep.get("id").and_then(Value::as_u64) else {
            tracing::warn!(
                season = season_num,
                "skipping a StreamingCommunity episode with no id"
            );
            continue;
        };
        let ep_num = ep
            .get("number")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or_else(|| u32::try_from(i + 1).unwrap_or(u32::MAX));
        let ep_name = str_or(ep.get("name"), "");

        let watch = Url::parse(&format!(
            "{}/it/watch/{title_id}?e={ep_id}",
            base_string(base)
        ))
        .map_err(|_| ScError::BadUrlShape {
            what: "watch",
            url: format!("{}/it/watch/{title_id}?e={ep_id}", base_string(base)),
        })?;

        let state = ScState::episode(
            base_string(base),
            title_id.parse().ok(),
            Some(ep_id),
            season_num,
            ep_num,
            ep_name,
            title_name,
        );
        out.push(MediaEntry {
            media_id: format!("sc_{title_id}_{ep_id}").into(),
            title: episode_title(title_name, season_num, ep_num, ep_name).into(),
            url: watch,
            kind: EntryKind::Video,
            pre_error: None,
            live: LiveStatus::NotLive,
            state: state.to_json(),
            hints: EntryHints {
                playlist_index: u32::try_from(i + 1).ok(),
                playlist_count: Some(total),
                playlist_title: Some(title_name.into()),
                ext: Some("mp4".into()),
                ..EntryHints::default()
            },
        });
    }
    Ok(out)
}

/// Re-stamps `playlist_index`/`playlist_count` across a flattened multi-season playlist, so a
/// group's progress counters and the output template see one 1..=N sequence rather than N per
/// season.
fn renumber(entries: &mut [MediaEntry], playlist_title: &str) {
    let total = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    for (i, e) in entries.iter_mut().enumerate() {
        e.hints.playlist_index = u32::try_from(i + 1).ok();
        e.hints.playlist_count = Some(total);
        e.hints.playlist_title = Some(playlist_title.into());
    }
}

/// Wraps children in a playlist entry with legacy's container id and title.
fn playlist(media_id: String, title: String, url: Url, entries: Vec<MediaEntry>) -> MediaEntry {
    MediaEntry {
        media_id: media_id.into(),
        title: title.clone().into(),
        url,
        kind: EntryKind::Playlist {
            title: title.into(),
            entries,
        },
        pre_error: None,
        live: LiveStatus::NotLive,
        state: Value::Null,
        hints: EntryHints::default(),
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::MockHttp;

    use super::*;

    fn base() -> Url {
        Url::parse("https://sc.test").expect("url")
    }

    fn version_mock() -> MockHttp {
        MockHttp::new().on(
            "https://sc.test/it",
            200,
            include_str!("../tests/fixtures/sc/it_page.html"),
        )
    }

    #[test]
    fn the_url_matchers_are_legacys() {
        assert!(SEASON_URL.is_some() && TITLE_URL.is_some());
        assert_eq!(
            parse_season_url("https://sc.test/it/titles/9-una-serie/season-2"),
            Some(("9".to_owned(), "una-serie".to_owned(), 2))
        );
        assert_eq!(
            parse_title_url("https://sc.test/it/titles/9-una-serie"),
            Some(("9".to_owned(), "una-serie".to_owned()))
        );
        // A season URL is not a title URL: the `$` anchor is what keeps the dispatch unambiguous.
        assert_eq!(
            parse_title_url("https://sc.test/it/titles/9-una-serie/season-2"),
            None
        );
        assert_eq!(parse_season_url("https://sc.test/it/watch/1"), None);
    }

    #[tokio::test]
    async fn a_twenty_episode_season_resolves_in_exactly_two_requests() {
        let http = version_mock()
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
        let url = Url::parse("https://sc.test/it/titles/9-una-serie/season-2").expect("url");
        let e = resolve_season(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect("a season");

        // S1 is cached and is not one of "the two"; the two are the title page and the season page.
        assert_eq!(http.count("https://sc.test/it/titles/9-una-serie"), 1);
        assert_eq!(
            http.count("https://sc.test/it/titles/9-una-serie/season-2"),
            1
        );
        assert_eq!(
            http.total(),
            3,
            "one version fetch plus exactly two page fetches, not ~60: {:?}",
            http.urls()
        );
        assert!(!http.requested_anything_containing("/embed"));
        assert!(!http.requested_anything_containing("vixcloud"));

        assert_eq!(&*e.media_id, "sc_9_s2");
        assert_eq!(&*e.title, "Una Serie Qualunque Season 2");
        assert_eq!(e.url, url);
        let kids = e.children();
        assert_eq!(kids.len(), 20);
        assert_eq!(&*kids[0].media_id, "sc_9_1001");
        assert_eq!(&*kids[0].title, "Una Serie Qualunque S02E01 - Episodio 1");
        assert_eq!(kids[0].url.as_str(), "https://sc.test/it/watch/9?e=1001");
        assert_eq!(&*kids[19].media_id, "sc_9_1020");
        assert_eq!(&*kids[19].title, "Una Serie Qualunque S02E20 - Episodio 20");
        assert_eq!(kids[19].hints.playlist_index, Some(20));
        assert_eq!(kids[19].hints.playlist_count, Some(20));

        let s = ScState::from_json(&kids[4].state).expect("state");
        assert_eq!(s.season_number, Some(2));
        assert_eq!(s.episode_number, Some(5));
        assert_eq!(s.episode_id, Some(1005));
        assert_eq!(s.title_id, Some(9));
        assert_eq!(s.series.as_deref(), Some("Una Serie Qualunque"));
        assert!(s.needs_m3u8_extraction);
    }

    #[tokio::test]
    async fn an_episode_without_a_name_keeps_the_bare_title() {
        let http = version_mock()
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-1",
                200,
                include_str!("../tests/fixtures/sc/season_mixed.json"),
            );
        let url = Url::parse("https://sc.test/it/titles/9-una-serie/season-1").expect("url");
        let e = resolve_season(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect("a season");
        let kids = e.children();
        assert_eq!(kids.len(), 2, "the id-less episode is skipped");
        assert_eq!(&*kids[0].title, "Una Serie Qualunque S01E01");
        assert_eq!(&*kids[1].title, "Una Serie Qualunque S01E02 - Con nome");
    }

    #[tokio::test]
    async fn an_empty_season_is_nothing_resolved() {
        let http = version_mock()
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-3",
                200,
                r#"{"props":{"loadedSeason":{"episodes":[]}}}"#,
            );
        let url = Url::parse("https://sc.test/it/titles/9-una-serie/season-3").expect("url");
        let err = resolve_season(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect_err("no episodes");
        assert_eq!(err.code(), crate::ScErrorCode::NothingResolved);
        assert!(matches!(
            err.into_provider_error(),
            aulos_provider::provider::ProviderError::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn a_movie_title_delegates_to_the_watch_path() {
        let http = version_mock()
            .on(
                "https://sc.test/it/titles/123-il-ladro",
                200,
                include_str!("../tests/fixtures/sc/title_movie.json"),
            )
            .on(
                "https://sc.test/it/watch/123",
                200,
                include_str!("../tests/fixtures/sc/watch_movie.json"),
            )
            .on(
                "https://sc.test/embed/123",
                200,
                include_str!("../tests/fixtures/sc/embed.html"),
            )
            .on(
                "https://vixcloud.co/embed/98765?token=abc&referer=1",
                200,
                include_str!("../tests/fixtures/sc/vixcloud_streams_active.html"),
            );
        let url = Url::parse("https://sc.test/it/titles/123-il-ladro").expect("url");
        let e = resolve_title(&http, &SiteVersions::new(), &base(), &url, 4)
            .await
            .expect("a movie");
        assert!(!e.is_playlist());
        assert_eq!(&*e.media_id, "sc_123");
        assert_eq!(
            e.url.as_str(),
            "https://sc.test/it/watch/123",
            "legacy persisted the synthesised watch url, not the title url"
        );
    }

    #[tokio::test]
    async fn a_tv_title_flattens_every_season_into_one_playlist() {
        let http = version_mock()
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-1",
                200,
                include_str!("../tests/fixtures/sc/season_mixed.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-2",
                200,
                include_str!("../tests/fixtures/sc/season_20.json"),
            );
        let url = Url::parse("https://sc.test/it/titles/9-una-serie").expect("url");
        let e = resolve_title(&http, &SiteVersions::new(), &base(), &url, 4)
            .await
            .expect("a series");
        assert_eq!(&*e.media_id, "sc_9");
        assert_eq!(&*e.title, "Una Serie Qualunque");
        let kids = e.children();
        assert_eq!(kids.len(), 22, "2 from season 1 plus 20 from season 2");
        // Season order is preserved even though the fetches are concurrent.
        assert_eq!(kids[0].hints.playlist_index, Some(1));
        assert_eq!(&*kids[0].title, "Una Serie Qualunque S01E01");
        assert_eq!(&*kids[2].title, "Una Serie Qualunque S02E01 - Episodio 1");
        // And the index is one global 1..=N run.
        assert_eq!(kids[21].hints.playlist_index, Some(22));
        assert!(kids.iter().all(|k| k.hints.playlist_count == Some(22)));
        // One request per season, plus the title page, plus S1.
        assert_eq!(http.total(), 4, "{:?}", http.urls());
    }

    #[tokio::test]
    async fn a_series_with_no_declared_seasons_falls_back_to_loaded_season() {
        let http = version_mock()
            .on(
                "https://sc.test/it/titles/9-una-serie",
                200,
                include_str!("../tests/fixtures/sc/title_tv_loaded_season_only.json"),
            )
            .on(
                "https://sc.test/it/titles/9-una-serie/season-1",
                200,
                include_str!("../tests/fixtures/sc/season_mixed.json"),
            );
        let url = Url::parse("https://sc.test/it/titles/9-una-serie").expect("url");
        let e = resolve_title(&http, &SiteVersions::new(), &base(), &url, 4)
            .await
            .expect("a series");
        assert_eq!(e.children().len(), 2);
    }

    #[tokio::test]
    async fn a_series_with_no_seasons_at_all_is_nothing_resolved() {
        let http = version_mock().on(
            "https://sc.test/it/titles/9-una-serie",
            200,
            r#"{"props":{"title":{"name":"X","type":"tv"}}}"#,
        );
        let url = Url::parse("https://sc.test/it/titles/9-una-serie").expect("url");
        let err = resolve_title(&http, &SiteVersions::new(), &base(), &url, 4)
            .await
            .expect_err("no seasons");
        assert_eq!(err.code(), crate::ScErrorCode::NothingResolved);
    }

    #[tokio::test]
    async fn a_non_title_url_never_makes_a_request() {
        let http = MockHttp::new();
        let url = Url::parse("https://sc.test/search?q=x").expect("url");
        assert_eq!(
            resolve_title(&http, &SiteVersions::new(), &base(), &url, 4)
                .await
                .expect_err("bad shape")
                .code(),
            crate::ScErrorCode::BadUrlShape
        );
        assert_eq!(http.total(), 0);
    }
}
