//! What a resolution produces: [`MediaEntry`] and its parts (DESIGN §6.1).
//!
//! An entry is provider-native metadata plus one opaque `state` blob. The engine never looks
//! inside `state`; it compacts it (DESIGN §7.5) and hands it back at download time, which is what
//! lets StreamingCommunity re-extract a just-in-time m3u8 and a `command` plugin round-trip its
//! own identifiers without the core knowing anything about either.

use aulos_core::error::WireError;
use aulos_core::id::UnixMs;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// One resolved thing: a video, a container, or a pointer somewhere else.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct MediaEntry {
    /// The provider's own id — the legacy `id` field. Never empty; a provider with no natural id
    /// uses `sha256(url)[..16]` (DESIGN §6.5.3).
    pub media_id: Box<str>,
    /// Display title. Never empty: a provider with no title uses the URL.
    pub title: Box<str>,
    /// The canonical page URL — yt-dlp's `webpage_url or url`.
    pub url: Url,
    /// Video, container or redirect.
    pub kind: EntryKind,
    /// A problem that does not prevent queueing, e.g. "Live stream is scheduled to start at …".
    ///
    /// It lands on the item as `error` while the status stays `queued` (DESIGN §8.4), which is how
    /// an upcoming premiere is visible without being terminal.
    pub pre_error: Option<WireError>,
    /// Live status, from yt-dlp's `live_status` or the provider's equivalent.
    pub live: LiveStatus,
    /// The provider-native blob, handed back verbatim in [`crate::provider::DownloadCtx::entry`].
    pub state: Value,
    /// Fields the output-name templates and the group counters need.
    pub hints: EntryHints,
}

impl MediaEntry {
    /// A plain single video with no state and no hints.
    ///
    /// This is what a provider with `capabilities.resolve = false` produces from the URL alone
    /// (DESIGN §6.5.1), and what [`crate::fake::FakeProvider`] produces by default.
    #[must_use]
    pub fn video(media_id: impl Into<Box<str>>, title: impl Into<Box<str>>, url: Url) -> Self {
        Self {
            media_id: media_id.into(),
            title: title.into(),
            url,
            kind: EntryKind::Video,
            pre_error: None,
            live: LiveStatus::NotLive,
            state: Value::Null,
            hints: EntryHints::default(),
        }
    }

    /// Whether this entry is a container rather than something downloadable.
    #[must_use]
    pub const fn is_playlist(&self) -> bool {
        matches!(self.kind, EntryKind::Playlist { .. })
    }

    /// The children of a container, or an empty slice.
    #[must_use]
    pub fn children(&self) -> &[MediaEntry] {
        match &self.kind {
            EntryKind::Playlist { entries, .. } => entries,
            EntryKind::Video | EntryKind::Redirect { .. } => &[],
        }
    }
}

/// What an entry is (DESIGN §6.1).
///
/// Externally tagged, so the overwhelmingly common case is the bare string `"video"` in a stored
/// `entry_json` blob and only a container pays for a nested object. Internal tagging is not an
/// option here: [`EntryKind::Redirect`]'s `url` would collide with [`MediaEntry::url`] the moment
/// anyone flattened it.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Something downloadable.
    Video,
    /// A playlist, channel or season. The engine turns this into a `group` row plus children
    /// (DESIGN §21.2).
    Playlist {
        /// The container's own title, which may differ from the entry title.
        title: Box<str>,
        /// The children. May be empty when the provider streams them separately.
        entries: Vec<MediaEntry>,
    },
    /// The URL points somewhere else and resolution should restart there.
    Redirect {
        /// Where to go instead.
        url: Url,
    },
}

/// A stream's live status (DESIGN §6.1).
///
/// `IsUpcoming` is the case that produces `not_yet_live`; the timestamp is optional because a
/// provider often knows *that* a premiere is scheduled without knowing *when*.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveStatus {
    /// An ordinary recording.
    #[default]
    NotLive,
    /// A scheduled premiere that has not started.
    IsUpcoming {
        /// Scheduled start, unix ms, when the provider reports one.
        at: Option<UnixMs>,
    },
    /// Live right now.
    IsLive,
    /// A finished live stream.
    WasLive,
}

impl LiveStatus {
    /// Whether an item with this status must not be downloaded yet.
    #[must_use]
    pub const fn is_upcoming(self) -> bool {
        matches!(self, Self::IsUpcoming { .. })
    }
}

/// The handful of provider fields the rest of the system genuinely needs (DESIGN §6.1).
///
/// Everything else stays inside [`MediaEntry::state`]. These are broken out because the output
/// template (`aulos-provider-ytdlp::outtmpl`), the `command` plugin templates (DESIGN §6.5.1) and
/// the group counters all read them, and none of those may guess at a JSON key.
///
/// Every field is optional and `#[serde(default)]`, so a provider fills in what it knows and a
/// stored entry from an older build still deserialises.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EntryHints {
    /// 1-based position inside a playlist.
    pub playlist_index: Option<u32>,
    /// The declared number of playlist entries.
    pub playlist_count: Option<u32>,
    /// The playlist's title, for `{playlist_title}`.
    pub playlist_title: Option<Box<str>>,
    /// 1-based position inside a channel listing.
    pub channel_index: Option<u32>,
    /// The declared number of channel entries.
    pub channel_count: Option<u32>,
    /// The channel's title.
    pub channel_title: Option<Box<str>>,
    /// The expected container extension, without a dot.
    pub ext: Option<Box<str>>,
    /// Duration in seconds. Fractional, as every provider reports it.
    pub duration: Option<f64>,
    /// An approximate size, when the provider offers one.
    pub filesize_approx: Option<u64>,
    /// A thumbnail URL, for the NFO hook and for a future rich client.
    pub thumbnail: Option<Box<str>>,
    /// Uploader / channel name.
    pub uploader: Option<Box<str>>,
}

impl EntryHints {
    /// Whether this entry is part of a container, which is the test the output-name template
    /// swap uses (DESIGN §9.8: the playlist template replaces the default only when
    /// `playlist_index` is present).
    #[must_use]
    pub const fn in_playlist(&self) -> bool {
        self.playlist_index.is_some()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://example.com/watch/1").unwrap()
    }

    #[test]
    fn a_video_entry_round_trips() {
        let e = MediaEntry::video("abc", "A clip", url());
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"], "video");
        assert_eq!(json["media_id"], "abc");
        assert_eq!(json["live"], "not_live");
        assert_eq!(serde_json::from_value::<MediaEntry>(json).unwrap(), e);
        assert!(!e.is_playlist());
        assert!(e.children().is_empty());
    }

    #[test]
    fn a_playlist_entry_carries_children() {
        let child = MediaEntry::video("c1", "One", url());
        let mut e = MediaEntry::video("p1", "Album", url());
        e.kind = EntryKind::Playlist {
            title: "Album".into(),
            entries: vec![child.clone()],
        };
        assert!(e.is_playlist());
        assert_eq!(e.children(), [child]);
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"]["playlist"]["title"], "Album");
        assert_eq!(json["kind"]["playlist"]["entries"][0]["media_id"], "c1");
        assert_eq!(serde_json::from_value::<MediaEntry>(json).unwrap(), e);
    }

    #[test]
    fn live_status_tags_its_variant() {
        assert_eq!(
            serde_json::to_value(LiveStatus::IsUpcoming { at: Some(17) }).unwrap(),
            serde_json::json!({ "is_upcoming": { "at": 17 } })
        );
        assert_eq!(
            serde_json::to_value(LiveStatus::NotLive).unwrap(),
            serde_json::json!("not_live")
        );
        assert!(LiveStatus::IsUpcoming { at: None }.is_upcoming());
        assert!(!LiveStatus::IsLive.is_upcoming());
        assert_eq!(LiveStatus::default(), LiveStatus::NotLive);
    }

    #[test]
    fn hints_default_to_all_absent_and_tolerate_a_partial_blob() {
        let h = EntryHints::default();
        assert!(!h.in_playlist());
        let partial: EntryHints = serde_json::from_str(r#"{"playlist_index":3}"#).unwrap();
        assert_eq!(partial.playlist_index, Some(3));
        assert!(partial.in_playlist());
        assert_eq!(partial.ext, None);
        // An unknown key from a future build is ignored, not an error.
        let future: EntryHints = serde_json::from_str(r#"{"chapter_index":1}"#).unwrap();
        assert_eq!(future, EntryHints::default());
    }

    #[test]
    fn a_redirect_names_its_target() {
        let mut e = MediaEntry::video("r", "r", url());
        e.kind = EntryKind::Redirect {
            url: Url::parse("https://elsewhere.test/x").unwrap(),
        };
        assert!(e.children().is_empty());
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"]["redirect"]["url"], "https://elsewhere.test/x");
        assert_eq!(json["url"], "https://example.com/watch/1");
    }
}
