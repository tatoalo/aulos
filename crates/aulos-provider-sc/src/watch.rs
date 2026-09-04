//! `/watch/` resolution: one entry, with **bit-for-bit legacy** `media_id`, `title` and `url`
//! (DESIGN §10.3).
//!
//! Ported from `streamingcommunity.py:244-324`. Three things here are load-bearing and are
//! asserted rather than assumed:
//!
//! - `media_id` is `sc_<title_id>` for a movie and `sc_<title_id>_<episode_id>` for an episode.
//!   Imported legacy rows carry exactly those strings, and dedupe, the NFO `uniqueid` and every
//!   existing `.info.json` key off them.
//! - `title` is `"<Name>"`, `"<Name> S01E02"` or `"<Name> S01E02 - <ep name>"`, and the file on
//!   disk is named from it. Changing the spacing would re-download everyone's library.
//! - the **episode branch is only taken when the site says `type == "tv"` and both numbers are
//!   truthy**. Legacy wrote `if title_type == "tv" and season_num and ep_num`, so a season or
//!   episode number of `0` falls into the movie branch. That looks like a bug and it is one, but a
//!   row already in someone's database has the movie-shaped id, so it is reproduced.
//!
//! Unlike season resolution (see [`crate::season`], DESIGN §10.4), a single `/watch/` URL still
//! performs the embed and stream hops. They are not wasted here: legacy used them as the validity
//! probe that decided between "queue this" and "hand the URL back to yt-dlp", and reproducing that
//! is what makes an unplayable watch page fail with
//! [`ProviderError::Unsupported`](aulos_provider::provider::ProviderError::Unsupported) instead of
//! queueing an item that can never download.

use std::sync::LazyLock;

use aulos_provider::entry::{EntryHints, MediaEntry};
use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::error::ScError;
use crate::http::ScHttp;
use crate::inertia::{SiteVersions, inertia_get, props};
use crate::state::ScState;
use crate::{embed, season};

/// `/watch/(\d+)(?:\?e=(\d+))?` — legacy's watch-URL matcher (`streamingcommunity.py:246`).
static WATCH_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"/watch/(\d+)(?:\?e=(\d+))?").ok());

/// The `(title_id, episode_id)` a `/watch/` URL carries.
///
/// Matched anywhere in the URL, exactly as legacy's `re.search` did, so a locale prefix or a
/// trailing fragment does not matter.
#[must_use]
pub fn parse_watch_url(url: &str) -> Option<(String, Option<String>)> {
    let re = WATCH_URL.as_ref()?;
    let caps = re.captures(url)?;
    let title_id = caps.get(1)?.as_str().to_owned();
    let episode_id = caps.get(2).map(|m| m.as_str().to_owned());
    Some((title_id, episode_id))
}

/// Legacy's episode display title: `"<Name> S01E02"`, plus `" - <ep name>"` when there is one.
#[must_use]
pub fn episode_title(series: &str, season: u32, episode: u32, episode_name: &str) -> String {
    let mut t = format!("{series} S{season:02}E{episode:02}");
    if !episode_name.is_empty() {
        t.push_str(" - ");
        t.push_str(episode_name);
    }
    t
}

/// S2–S4 for one `/watch/` URL.
///
/// `url` is echoed onto the entry verbatim — legacy persisted the watch URL, never the m3u8,
/// because vixcloud tokens expire in minutes (DESIGN §10.3).
///
/// # Errors
/// [`ScError::BadUrlShape`] when the URL is not a watch URL, [`ScError::NoEmbedUrl`] when the
/// props carry no `embedUrl`, and whatever [`embed::stream_for_embed`] and
/// [`inertia_get`] can return.
pub async fn resolve_watch(
    http: &dyn ScHttp,
    versions: &SiteVersions,
    base: &Url,
    url: &Url,
) -> Result<MediaEntry, ScError> {
    let (title_id, episode_param) =
        parse_watch_url(url.as_str()).ok_or_else(|| ScError::BadUrlShape {
            what: "watch",
            url: url.to_string(),
        })?;

    let mut path = format!("/it/watch/{title_id}");
    if let Some(e) = &episode_param {
        path.push_str("?e=");
        path.push_str(e);
    }
    let page = inertia_get(http, versions, base, &path).await?;
    let props = props(&page);

    let title = props.get("title");
    let title_name = season::str_or(title.and_then(|t| t.get("name")), "Unknown");
    let title_type = season::str_or(title.and_then(|t| t.get("type")), "movie");

    let embed_url = props
        .get("embedUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| ScError::NoEmbedUrl {
            url: url.to_string(),
        })?;
    let embed_url = Url::parse(embed_url).map_err(|_| ScError::NoEmbedUrl {
        url: url.to_string(),
    })?;

    let episode = props.get("episode").filter(|e| !e.is_null());
    let season_num = episode
        .and_then(|e| e.get("season"))
        .and_then(|s| s.get("number"))
        .and_then(Value::as_u64);
    let ep_num = episode
        .and_then(|e| e.get("number"))
        .and_then(Value::as_u64);
    let ep_name = season::str_or(episode.and_then(|e| e.get("name")), "");

    // The validity probe legacy performed, and the reason an unplayable page becomes
    // `Unsupported` rather than a queued item that can never download.
    let _stream = embed::stream_for_embed(http, &embed_url).await?;

    // Legacy truthiness: `season_num and ep_num`, so a zero falls into the movie branch.
    let tv = title_type == "tv";
    let numbers = match (season_num.filter(|n| *n != 0), ep_num.filter(|n| *n != 0)) {
        (Some(s), Some(e)) if tv => Some((s, e)),
        _ => None,
    };

    let (media_id, display_title) = match numbers {
        Some((s, e)) => {
            // Legacy: `sc_{title_id}_{episode_param or ep_num}`.
            let ep_id = episode_param.clone().unwrap_or_else(|| e.to_string());
            (
                format!("sc_{title_id}_{ep_id}"),
                episode_title(title_name, u32_of(s), u32_of(e), ep_name),
            )
        }
        None => (format!("sc_{title_id}"), title_name.to_owned()),
    };

    let mut state = match numbers {
        Some((s, e)) => ScState::episode(
            base_string(base),
            title_id.parse().ok(),
            episode_param
                .as_deref()
                .and_then(|p| p.parse().ok())
                .or(Some(e)),
            u32_of(s),
            u32_of(e),
            ep_name,
            title_name,
        ),
        None => ScState::movie(base_string(base), title_id.parse().ok()),
    };
    // Legacy set `series` from the title *type*, independently of the episode branch, and set the
    // raw season/episode numbers even for the movie branch (`streamingcommunity.py:313-316`).
    state.series = if tv {
        Some(title_name.to_owned())
    } else {
        None
    };
    state.season_number = season_num.map(u32_of);
    state.episode_number = ep_num.map(u32_of);
    state.episode = ep_name.to_owned();

    Ok(MediaEntry {
        media_id: media_id.into(),
        title: display_title.into(),
        url: url.clone(),
        kind: aulos_provider::entry::EntryKind::Video,
        pre_error: None,
        live: aulos_provider::entry::LiveStatus::NotLive,
        state: state.to_json(),
        hints: EntryHints {
            ext: Some("mp4".into()),
            ..EntryHints::default()
        },
    })
}

/// `{scheme}://{host}[:port]`, the shape legacy's `base_url` had — no trailing slash.
#[must_use]
pub fn base_string(base: &Url) -> &str {
    base.as_str().trim_end_matches('/')
}

/// A season or episode number, clamped into `u32` — the site's numbers are single digits.
fn u32_of(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use crate::testing::MockHttp;

    use super::*;

    fn base() -> Url {
        Url::parse("https://sc.test").expect("url")
    }

    /// The four pages a `/watch/` resolution touches, wired to one movie fixture.
    fn movie_mock() -> MockHttp {
        MockHttp::new()
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
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
            )
    }

    fn episode_mock(watch_fixture: &str) -> MockHttp {
        MockHttp::new()
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on("https://sc.test/it/watch/9?e=456", 200, watch_fixture)
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

    #[test]
    fn the_watch_url_matcher_is_legacys() {
        assert!(WATCH_URL.is_some());
        assert_eq!(
            parse_watch_url("https://sc.test/it/watch/123"),
            Some(("123".to_owned(), None))
        );
        assert_eq!(
            parse_watch_url("https://sc.test/it/watch/123?e=456"),
            Some(("123".to_owned(), Some("456".to_owned())))
        );
        // A different locale prefix still matches, as `re.search` did.
        assert_eq!(
            parse_watch_url("https://sc.test/en/watch/7?e=8"),
            Some(("7".to_owned(), Some("8".to_owned())))
        );
        assert_eq!(parse_watch_url("https://sc.test/titles/9-slug"), None);
    }

    #[test]
    fn the_episode_title_format_is_byte_identical_to_legacy() {
        assert_eq!(episode_title("Una serie", 1, 2, ""), "Una serie S01E02");
        assert_eq!(
            episode_title("Una serie", 1, 2, "Pilota"),
            "Una serie S01E02 - Pilota"
        );
        assert_eq!(episode_title("X", 10, 24, "Y"), "X S10E24 - Y");
    }

    #[tokio::test]
    async fn a_movie_watch_page_produces_the_legacy_id_and_title() {
        let http = movie_mock();
        let url = Url::parse("https://sc.test/it/watch/123").expect("url");
        let e = resolve_watch(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect("a movie");
        assert_eq!(&*e.media_id, "sc_123");
        assert_eq!(&*e.title, "Il Ladro di Giorni");
        assert_eq!(e.url, url);
        let s = ScState::from_json(&e.state).expect("state");
        assert_eq!(s.base_url, "https://sc.test");
        assert_eq!(s.title_id, Some(123));
        assert_eq!(s.episode_id, None);
        assert_eq!(s.series, None);
        assert!(s.needs_m3u8_extraction);
        assert_eq!(e.hints.ext.as_deref(), Some("mp4"));
    }

    #[tokio::test]
    async fn an_episode_with_a_name_produces_the_legacy_id_and_title() {
        let http = episode_mock(include_str!("../tests/fixtures/sc/watch_episode.json"));
        let url = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        let e = resolve_watch(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect("an episode");
        assert_eq!(&*e.media_id, "sc_9_456");
        assert_eq!(&*e.title, "Una Serie Qualunque S02E03 - Il Segreto");
        let s = ScState::from_json(&e.state).expect("state");
        assert_eq!(s.season_number, Some(2));
        assert_eq!(s.episode_number, Some(3));
        assert_eq!(s.episode, "Il Segreto");
        assert_eq!(s.series.as_deref(), Some("Una Serie Qualunque"));
        assert_eq!(s.episode_id, Some(456));
    }

    #[tokio::test]
    async fn an_episode_without_a_name_drops_the_dash() {
        let http = episode_mock(include_str!(
            "../tests/fixtures/sc/watch_episode_no_name.json"
        ));
        let url = Url::parse("https://sc.test/it/watch/9?e=456").expect("url");
        let e = resolve_watch(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect("an episode");
        assert_eq!(&*e.media_id, "sc_9_456");
        assert_eq!(&*e.title, "Una Serie Qualunque S02E03");
        assert_eq!(
            ScState::from_json(&e.state).expect("state").episode,
            "",
            "legacy stored an empty episode name, not null"
        );
    }

    #[tokio::test]
    async fn a_watch_page_with_no_embed_url_is_unsupported_not_an_internal_error() {
        let http = MockHttp::new()
            .on(
                "https://sc.test/it",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/watch/123",
                200,
                include_str!("../tests/fixtures/sc/watch_no_embed.json"),
            );
        let url = Url::parse("https://sc.test/it/watch/123").expect("url");
        let err = resolve_watch(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect_err("no embed url");
        assert_eq!(err.code(), crate::ScErrorCode::NoEmbedUrl);
        assert!(matches!(
            err.into_provider_error(),
            aulos_provider::provider::ProviderError::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn a_non_watch_url_never_makes_a_request() {
        let http = MockHttp::new();
        let url = Url::parse("https://sc.test/browse").expect("url");
        let err = resolve_watch(&http, &SiteVersions::new(), &base(), &url)
            .await
            .expect_err("not a watch url");
        assert_eq!(err.code(), crate::ScErrorCode::BadUrlShape);
        assert_eq!(http.total(), 0);
    }

    #[tokio::test]
    async fn the_m3u8_is_never_fetched_during_resolution() {
        let http = movie_mock();
        let url = Url::parse("https://sc.test/it/watch/123").expect("url");
        let _ = resolve_watch(&http, &SiteVersions::new(), &base(), &url).await;
        assert!(!http.requested_anything_containing("/playlist/"));
    }
}
