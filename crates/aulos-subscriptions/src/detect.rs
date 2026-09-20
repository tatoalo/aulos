//! Feed detection: the `is_media_entry` port and the classification of a resolve result
//! (DESIGN §14.3 steps 2–4).
//!
//! Legacy worked on raw yt-dlp dicts. Here resolution goes through the **provider registry**, so a
//! subscription works for a StreamingCommunity series or a `command` plugin feed and not only for
//! a yt-dlp source (DESIGN §14.3 step 1). The port therefore reads two things:
//!
//! - [`aulos_provider::MediaEntry`]'s typed fields — [`aulos_provider::EntryKind`] already carries
//!   the `_type`/`entries` distinction every provider makes, and `url` is a required field so
//!   legacy's `webpage_url or url` test can never fail;
//! - the provider-native `state` blob, for the two yt-dlp-specific signals legacy leaned on:
//!   `ie_key`/`extractor_key` and the seven media-hint fields.
//!
//! A provider that supplies no `state` (StreamingCommunity, a plugin) therefore degrades to
//! "anything that is not a container is media", which is exactly right: it has already done the
//! classification the hint fields exist to reconstruct.

use aulos_provider::{EntryKind, MediaEntry};
use serde_json::Value;
use url::Url;

/// The seven fields legacy accepted as proof that a tab-shaped entry is really a video
/// (`_MEDIA_HINT_FIELDS`, `app/subscriptions.py:24`).
pub const MEDIA_HINT_FIELDS: [&str; 7] = [
    "duration",
    "timestamp",
    "release_timestamp",
    "upload_date",
    "view_count",
    "live_status",
    "availability",
];

/// The `ie_key` substrings that mean "this is a listing, not a video" unless a hint field proves
/// otherwise.
const LISTING_TOKENS: [&str; 3] = ["playlist", "channel", "tab"];

/// The container `_type` values legacy rejected outright.
const CONTAINER_TYPES: [&str; 3] = ["playlist", "multi_video", "channel"];

/// The legacy `_entry_id` port: the provider's own id, or the URL when it has none.
#[must_use]
pub fn media_id_of(entry: &MediaEntry) -> Box<str> {
    if entry.media_id.trim().is_empty() {
        entry.url.as_str().into()
    } else {
        entry.media_id.clone()
    }
}

fn is_youtube_url(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "youtu.be"
            || host == "youtube.com"
            || host.ends_with(".youtube.com")
            || host == "youtube-nocookie.com"
            || host.ends_with(".youtube-nocookie.com")
    })
}

pub(crate) fn is_shorts_url(url: &Url) -> bool {
    is_youtube_url(url)
        && url
            .path_segments()
            .is_some_and(|mut parts| parts.any(|part| part == "shorts"))
}

pub(crate) fn is_short(entry: &MediaEntry) -> bool {
    is_shorts_url(&entry.url)
        || (is_youtube_url(&entry.url)
            && entry.state.get("media_type").and_then(Value::as_str) == Some("short"))
        || ["url", "webpage_url", "original_url"].iter().any(|key| {
            entry
                .state
                .get(*key)
                .and_then(Value::as_str)
                .and_then(|value| Url::parse(value).ok())
                .is_some_and(|url| is_shorts_url(&url))
        })
}

/// A verbatim port of legacy `_is_media_entry` (`app/subscriptions.py:60`).
///
/// | Legacy test | Here |
/// |---|---|
/// | `_type in (playlist, multi_video, channel)` ⇒ no | [`EntryKind::Playlist`], or `state._type` |
/// | `entry["entries"]` non-empty ⇒ no | [`EntryKind::Playlist`], or `state.entries` |
/// | no `webpage_url` and no `url` ⇒ no | unreachable: [`MediaEntry::url`] is required |
/// | `ie_key` contains playlist/channel/tab ⇒ needs one hint field | `state.ie_key` / `state.extractor_key`, then `state` or [`aulos_provider::EntryHints`] |
#[must_use]
pub fn is_media_entry(entry: &MediaEntry) -> bool {
    if matches!(entry.kind, EntryKind::Playlist { .. }) {
        return false;
    }
    let Some(state) = entry.state.as_object() else {
        // No provider-native blob: the typed `kind` is the whole truth.
        return true;
    };

    if let Some(t) = state.get("_type").and_then(Value::as_str)
        && CONTAINER_TYPES.contains(&t.trim().to_ascii_lowercase().as_str())
    {
        return false;
    }
    if state
        .get("entries")
        .and_then(Value::as_array)
        .is_some_and(|e| !e.is_empty())
    {
        return false;
    }

    let ie_key = state
        .get("ie_key")
        .or_else(|| state.get("extractor_key"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if LISTING_TOKENS.iter().any(|t| ie_key.contains(t)) {
        return has_media_hint(entry);
    }
    true
}

/// Whether any of [`MEDIA_HINT_FIELDS`] is non-null, in the state blob or in the typed hints.
fn has_media_hint(entry: &MediaEntry) -> bool {
    if let Some(state) = entry.state.as_object()
        && MEDIA_HINT_FIELDS
            .iter()
            .any(|f| state.get(*f).is_some_and(|v| !v.is_null()))
    {
        return true;
    }
    // A provider that filled the typed hints instead of the blob has told us the same thing.
    entry.hints.duration.is_some()
}

/// What a resolve result turned out to be (DESIGN §14.3 steps 3–4).
#[derive(Clone, PartialEq, Debug)]
pub enum Classified {
    /// A channel, playlist or season: a name and the entries it listed, **unfiltered**.
    Feed {
        /// The container's title, when the provider supplied one.
        name: Option<Box<str>>,
        /// Everything the container listed, before [`is_media_entry`].
        entries: Vec<MediaEntry>,
    },
    /// The URL points somewhere else; resolution should restart there.
    Redirect(Url),
    /// A single video, or nothing at all — the [`crate::CheckFailure::VideoOnly`] case.
    SingleVideo,
}

/// Classifies a `resolve` result.
///
/// Two provider shapes are accepted, because both exist in this workspace: `aulos-provider-ytdlp`
/// returns **one** entry whose `kind` is [`EntryKind::Playlist`] for a container (its children
/// nested inside), while the fake provider and the `command` plugins return a **flat** vector of
/// videos. A flat vector of more than one video is a feed; a single video is not.
#[must_use]
pub fn classify(entries: Vec<MediaEntry>) -> Classified {
    let mut entries = entries;
    match entries.len() {
        0 => Classified::SingleVideo,
        1 => {
            let only = entries.remove(0);
            match only.kind {
                EntryKind::Redirect { url } => Classified::Redirect(url),
                EntryKind::Playlist { title, entries } => Classified::Feed {
                    name: feed_name(Some(&title), entries.first()),
                    entries,
                },
                EntryKind::Video => Classified::SingleVideo,
            }
        }
        _ => {
            let name = feed_name(None, entries.first());
            Classified::Feed { name, entries }
        }
    }
}

/// The legacy name ladder: `info.title | channel | playlist_title | uploader | url`, with the
/// container's own title first and the children's shared hints as the fallback.
fn feed_name(title: Option<&str>, first_child: Option<&MediaEntry>) -> Option<Box<str>> {
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(t.into());
    }
    let hints = first_child.map(|c| &c.hints)?;
    hints
        .playlist_title
        .as_deref()
        .or(hints.channel_title.as_deref())
        .or(hints.uploader.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(Into::into)
}

/// The maximum recursion depth (legacy `_depth < 1`).
pub const TAB_RECURSION_MAX_DEPTH: u32 = 1;

#[cfg(test)]
mod tests {
    use aulos_provider::{EntryHints, EntryKind, MediaEntry};
    use serde_json::json;

    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn video(id: &str) -> MediaEntry {
        MediaEntry::video(id, format!("Title {id}"), url("https://x.test/v/1"))
    }

    #[test]
    fn shorts_are_identified_by_urls_and_metadata_not_duration() {
        for address in [
            "https://www.youtube.com/shorts/abc",
            "https://m.youtube.com/@channel/shorts",
            "https://www.youtube.com/channel/UC123/shorts/",
        ] {
            assert!(is_shorts_url(&url(address)));
            assert!(is_short(&MediaEntry::video("abc", "Short", url(address))));
        }
        for address in [
            "https://youtube.com/watch?v=shorts",
            "https://notyoutube.com/shorts/abc",
        ] {
            assert!(!is_shorts_url(&url(address)));
        }
        let mut e = MediaEntry::video("abc", "Short", url("https://youtube.com/watch?v=abc"));
        e.state = json!({"media_type": "short"});
        assert!(is_short(&e));
        for key in ["url", "webpage_url", "original_url"] {
            e.state = json!({key: "https://youtube.com/shorts/abc"});
            assert!(is_short(&e));
        }
        e.state = json!({"media_type": "video", "duration": 10});
        e.hints.duration = Some(10.0);
        assert!(!is_short(&e));
    }

    fn with_state(id: &str, state: Value) -> MediaEntry {
        let mut e = video(id);
        e.state = state;
        e
    }

    #[test]
    fn a_plain_video_with_no_state_is_media() {
        assert!(is_media_entry(&video("a")));
    }

    #[test]
    fn a_playlist_kind_is_never_media() {
        let mut e = video("p");
        e.kind = EntryKind::Playlist {
            title: "Album".into(),
            entries: vec![video("c")],
        };
        assert!(!is_media_entry(&e));
    }

    #[test]
    fn a_container_type_in_the_state_blob_is_rejected() {
        for t in [
            "playlist",
            "multi_video",
            "channel",
            "CHANNEL",
            " playlist ",
        ] {
            assert!(
                !is_media_entry(&with_state("x", json!({ "_type": t }))),
                "{t}"
            );
        }
        assert!(is_media_entry(&with_state(
            "x",
            json!({ "_type": "video" })
        )));
        assert!(is_media_entry(&with_state("x", json!({ "_type": "url" }))));
    }

    #[test]
    fn a_non_empty_entries_array_in_the_state_blob_is_rejected() {
        assert!(!is_media_entry(&with_state(
            "x",
            json!({ "entries": [{ "id": "c" }] })
        )));
        assert!(is_media_entry(&with_state("x", json!({ "entries": [] }))));
        assert!(is_media_entry(&with_state(
            "x",
            json!({ "entries": Value::Null })
        )));
    }

    /// The rule that makes YouTube channel-of-tabs pages work: a tab-shaped extractor key only
    /// counts as media if one of the seven hint fields is filled in.
    #[test]
    fn a_tab_shaped_extractor_key_needs_a_media_hint() {
        for key in [
            "YoutubeTab",
            "youtube:playlist",
            "SomeChannelIE",
            "YOUTUBETAB",
        ] {
            assert!(
                !is_media_entry(&with_state("x", json!({ "ie_key": key }))),
                "{key} with no hint should be rejected"
            );
        }
        for field in MEDIA_HINT_FIELDS {
            let e = with_state("x", json!({ "ie_key": "YoutubeTab", field: 1 }));
            assert!(is_media_entry(&e), "{field} should rescue it");
        }
        // A null hint does not rescue it.
        assert!(!is_media_entry(&with_state(
            "x",
            json!({ "ie_key": "YoutubeTab", "duration": Value::Null })
        )));
        // `extractor_key` is the fallback spelling.
        assert!(!is_media_entry(&with_state(
            "x",
            json!({ "extractor_key": "YoutubeTab" })
        )));
    }

    #[test]
    fn a_typed_duration_hint_also_rescues_a_tab_entry() {
        let mut e = with_state("x", json!({ "ie_key": "YoutubeTab" }));
        assert!(!is_media_entry(&e));
        e.hints.duration = Some(12.5);
        assert!(is_media_entry(&e));
    }

    #[test]
    fn a_non_listing_extractor_key_needs_no_hint() {
        assert!(is_media_entry(&with_state(
            "x",
            json!({ "ie_key": "Youtube" })
        )));
    }

    #[test]
    fn the_media_id_falls_back_to_the_url() {
        assert_eq!(&*media_id_of(&video("abc")), "abc");
        let mut blank = video("abc");
        blank.media_id = "   ".into();
        assert_eq!(&*media_id_of(&blank), "https://x.test/v/1");
    }

    #[test]
    fn nothing_and_a_lone_video_are_both_the_single_video_case() {
        assert_eq!(classify(vec![]), Classified::SingleVideo);
        assert_eq!(classify(vec![video("a")]), Classified::SingleVideo);
    }

    #[test]
    fn a_nested_playlist_entry_is_a_feed_named_by_its_title() {
        let mut parent = video("p");
        parent.kind = EntryKind::Playlist {
            title: "Veritasium".into(),
            entries: vec![video("c1"), video("c2")],
        };
        match classify(vec![parent]) {
            Classified::Feed { name, entries } => {
                assert_eq!(name.as_deref(), Some("Veritasium"));
                assert_eq!(entries.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_flat_vector_of_videos_is_a_feed_named_by_the_shared_hints() {
        let mut a = video("c1");
        a.hints = EntryHints {
            playlist_title: Some("Lo-fi beats".into()),
            ..EntryHints::default()
        };
        match classify(vec![a, video("c2")]) {
            Classified::Feed { name, entries } => {
                assert_eq!(name.as_deref(), Some("Lo-fi beats"));
                assert_eq!(entries.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_name_ladder_falls_through_title_then_channel_then_uploader() {
        let mut a = video("c1");
        a.hints.channel_title = Some("A Channel".into());
        let Classified::Feed { name, .. } = classify(vec![a.clone(), video("c2")]) else {
            panic!("expected a feed");
        };
        assert_eq!(name.as_deref(), Some("A Channel"));

        let mut b = video("c1");
        b.hints.uploader = Some("An Uploader".into());
        let Classified::Feed { name, .. } = classify(vec![b, video("c2")]) else {
            panic!("expected a feed");
        };
        assert_eq!(name.as_deref(), Some("An Uploader"));

        let Classified::Feed { name, .. } = classify(vec![video("c1"), video("c2")]) else {
            panic!("expected a feed");
        };
        assert_eq!(name, None, "nothing to name it with");
    }

    #[test]
    fn a_redirect_entry_names_its_target() {
        let mut e = video("r");
        e.kind = EntryKind::Redirect {
            url: url("https://elsewhere.test/@chan"),
        };
        assert_eq!(
            classify(vec![e]),
            Classified::Redirect(url("https://elsewhere.test/@chan"))
        );
    }
}
