//! Entry compaction and rebuilding (DESIGN §7.5).
//!
//! The provider entry blob is the one unbounded field on an item, and it is also the only thing
//! that lets a download run after a restart: StreamingCommunity re-extracts a just-in-time m3u8
//! from it, a `command` plugin round-trips its own identifiers through it, and the yt-dlp output
//! template needs the playlist/channel fields back to resolve `%(playlist_title)s`.
//!
//! So there are two functions here and they are exact inverses over what the design says to keep:
//! [`compact_entry`] decides what is persisted, [`rebuild_entry`] turns a row back into the
//! [`MediaEntry`] a `download` call needs. The store applies the `AULOS_ENTRY_MAX_BYTES` cap on
//! top and substitutes `{"__truncated": true}`; [`rebuild_entry`] therefore has to work from the
//! row alone, and does.

use aulos_core::{EntryBlob, Item, ProviderId};
use aulos_provider::{EntryHints, MediaEntry};
use serde_json::{Map, Value};

/// The provider id whose partial files are worthless after a kill, and whose entry must survive
/// until the NFO hook has run (DESIGN §7.5, §8.7, §8.9).
///
/// Hard-coded because DESIGN §7.5/§8.7/§8.9 state the rule per provider id rather than as a
/// capability. See `docs/INTEGRATION-NOTES.md`, WP-12: an additive
/// `Provider::partials_resumable()` would let the engine ask instead of knowing.
pub const SC_PROVIDER: &str = "streamingcommunity";

/// The prefix every `command` plugin's provider id carries (DESIGN §6.1).
pub const COMMAND_PREFIX: &str = "command:";

/// The key [`compact_entry`] stores [`EntryHints`] under.
const HINTS_KEY: &str = "hints";

/// The key a `command` plugin's opaque state is stored under.
const STATE_KEY: &str = "state";

/// What is persisted in `items.entry_json`, per the DESIGN §7.5 table.
///
/// | Provider / case | Kept |
/// |---|---|
/// | `ytdlp` (and any plain provider), non-playlist child | nothing |
/// | `ytdlp`, playlist/channel child | the playlist/channel hints, so `outtmpl` survives a restart |
/// | `streamingcommunity` | the **whole** `state` object, in the §10.3 shape the importer also writes |
/// | `command:<name>` | the `state` object plus `media_id`/`title`/`url` |
#[must_use]
pub fn compact_entry(provider: &ProviderId, entry: &MediaEntry) -> Option<EntryBlob> {
    let id = provider.as_str();
    if id == SC_PROVIDER {
        // The state object verbatim: `write_sidecar`, the JIT m3u8 and the NFO hook all read it,
        // and the legacy importer writes exactly this shape.
        return (!entry.state.is_null()).then(|| EntryBlob::new(entry.state.clone()));
    }
    if id.starts_with(COMMAND_PREFIX) {
        let mut obj = Map::new();
        obj.insert(STATE_KEY.to_owned(), entry.state.clone());
        obj.insert(
            "media_id".to_owned(),
            Value::String(entry.media_id.to_string()),
        );
        obj.insert("title".to_owned(), Value::String(entry.title.to_string()));
        obj.insert("url".to_owned(), Value::String(entry.url.to_string()));
        if entry.hints != EntryHints::default() {
            obj.insert(HINTS_KEY.to_owned(), hints_json(&entry.hints));
        }
        return Some(EntryBlob::new(Value::Object(obj)));
    }
    // A plain provider: nothing at all unless the entry is part of a container, in which case the
    // output template needs its position back after a restart.
    let mut obj = Map::new();
    if entry.hints != EntryHints::default() {
        obj.insert(HINTS_KEY.to_owned(), hints_json(&entry.hints));
    }
    if !entry.state.is_null() {
        obj.insert(STATE_KEY.to_owned(), entry.state.clone());
    }
    (!obj.is_empty()).then(|| EntryBlob::new(Value::Object(obj)))
}

/// Turns a persisted row back into the [`MediaEntry`] a `download` call needs (DESIGN §7.5).
///
/// Total by construction: `media_id`, `title` and `url` are all `NOT NULL` on the row (`title` is
/// the URL before resolution), so a missing, dropped or truncated blob costs the hints and the
/// state and nothing else.
#[must_use]
pub fn rebuild_entry(item: &Item) -> MediaEntry {
    let media_id = item
        .media_id
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| item.url.as_str());
    let mut entry = MediaEntry::video(media_id, &*item.title, item.url.clone());

    let Some(blob) = item.entry.as_ref().filter(|b| !b.is_truncated()) else {
        return entry;
    };
    let value = blob.as_value();
    let is_sc = item
        .provider
        .as_ref()
        .is_some_and(|p| p.as_str() == SC_PROVIDER);

    if is_sc {
        entry.state = value.clone();
    } else if let Some(state) = value.get(STATE_KEY) {
        entry.state = state.clone();
    }
    if let Some(hints) = value.get(HINTS_KEY).cloned()
        && let Ok(parsed) = serde_json::from_value::<EntryHints>(hints)
    {
        entry.hints = parsed;
    }
    entry
}

/// A child's best-effort byte total, for the group accumulator (DESIGN §8.6).
///
/// The real `size` once the download has produced one, otherwise the provider's
/// `filesize_approx`. `None` keeps the group on the count-weighted percent.
#[must_use]
pub fn size_hint(item: &Item) -> Option<u64> {
    item.size.or_else(|| size_hint_excluding_size(item))
}

/// The estimate half of [`size_hint`], ignoring any real `size`.
///
/// This is what [`crate::GroupAcc::on_child_finished`] needs as `previous_hint`: the value the
/// child contributed *before* it finished, so the accumulator can swap it for the exact size.
#[must_use]
pub fn size_hint_excluding_size(item: &Item) -> Option<u64> {
    let blob = item.entry.as_ref().filter(|b| !b.is_truncated())?;
    blob.as_value()
        .get(HINTS_KEY)
        .and_then(|h| h.get("filesize_approx"))
        .and_then(Value::as_u64)
        .filter(|b| *b > 0)
}

/// [`EntryHints`] as JSON, dropping the absent fields so a plain playlist child costs ~60 bytes
/// rather than the eleven-key object `serde` emits with `Option`s.
fn hints_json(hints: &EntryHints) -> Value {
    let mut obj = match serde_json::to_value(hints) {
        Ok(Value::Object(o)) => o,
        // `EntryHints` is a plain struct of `Option`s, so this is unreachable; an empty object is
        // the honest fallback and costs the hints, never the download.
        _ => Map::new(),
    };
    obj.retain(|_, v| !v.is_null());
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use aulos_core::{
        Codec, DownloadRequest, DownloadType, FormatId, Item, ItemId, Kind, QualityId, Selection,
        SourceKind, SourceRef, Status,
    };
    use url::Url;

    use super::*;

    fn url() -> Url {
        Url::parse("https://example.test/watch/7").unwrap()
    }

    fn provider(id: &str) -> ProviderId {
        ProviderId::parse(id).unwrap()
    }

    fn row(provider_id: &str, entry: Option<EntryBlob>) -> Item {
        let selection = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("best").unwrap(),
        );
        Item {
            id: ItemId::new(),
            kind: Kind::Item,
            group_id: None,
            group_index: None,
            ord: 1,
            url: url(),
            canonical_key: "k".into(),
            provider: Some(provider(provider_id)),
            media_id: Some("m7".into()),
            title: "Episode 7".into(),
            status: Status::Queued,
            auto_start: true,
            msg: None,
            error: None,
            request: DownloadRequest::new(url(), selection),
            entry,
            filename: None,
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: 0,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: SourceRef::bare(SourceKind::ApiV2),
            children_total: None,
            clear_after: None,
        }
    }

    #[test]
    fn a_plain_single_video_persists_nothing() {
        let entry = MediaEntry::video("m7", "Episode 7", url());
        assert!(compact_entry(&provider("ytdlp"), &entry).is_none());
    }

    #[test]
    fn a_playlist_child_keeps_the_fields_outtmpl_needs() {
        let mut entry = MediaEntry::video("m7", "Episode 7", url());
        entry.hints = EntryHints {
            playlist_index: Some(3),
            playlist_count: Some(12),
            playlist_title: Some("Season 1".into()),
            ext: Some("mp4".into()),
            ..EntryHints::default()
        };
        let blob = compact_entry(&provider("ytdlp"), &entry).expect("a playlist child has hints");
        let json = blob.as_value();
        assert_eq!(json["hints"]["playlist_index"], 3);
        assert_eq!(json["hints"]["playlist_title"], "Season 1");
        assert!(
            json["hints"].as_object().unwrap().len() == 4,
            "the absent hints are not stored: {json}"
        );

        let rebuilt = rebuild_entry(&row("ytdlp", Some(blob)));
        assert_eq!(rebuilt.hints, entry.hints);
        assert!(rebuilt.hints.in_playlist());
        assert_eq!(&*rebuilt.media_id, "m7");
    }

    #[test]
    fn streamingcommunity_keeps_the_whole_state_verbatim() {
        let mut entry = MediaEntry::video("sc_7", "Episode 7", url());
        entry.state = serde_json::json!({
            "base_url": "https://sc.test", "title_id": 42, "episode_id": 7
        });
        let blob = compact_entry(&provider(SC_PROVIDER), &entry).expect("sc keeps its state");
        assert_eq!(
            blob.as_value(),
            &entry.state,
            "the blob IS the state object"
        );

        let rebuilt = rebuild_entry(&row(SC_PROVIDER, Some(blob)));
        assert_eq!(rebuilt.state, entry.state);
    }

    #[test]
    fn a_command_plugin_keeps_its_state_and_identity() {
        let mut entry = MediaEntry::video("plug-1", "A clip", url());
        entry.state = serde_json::json!({ "token": "abc" });
        let blob =
            compact_entry(&provider("command:demo"), &entry).expect("a command plugin has state");
        let json = blob.as_value();
        assert_eq!(json["state"]["token"], "abc");
        assert_eq!(json["media_id"], "plug-1");
        assert_eq!(json["url"], url().as_str());

        let rebuilt = rebuild_entry(&row("command:demo", Some(blob)));
        assert_eq!(rebuilt.state, entry.state);
    }

    #[test]
    fn a_missing_or_truncated_blob_still_rebuilds_a_usable_entry() {
        for blob in [None, Some(EntryBlob::truncated())] {
            let rebuilt = rebuild_entry(&row("ytdlp", blob));
            assert_eq!(&*rebuilt.media_id, "m7");
            assert_eq!(&*rebuilt.title, "Episode 7");
            assert_eq!(rebuilt.url, url());
            assert_eq!(rebuilt.hints, EntryHints::default());
            assert!(rebuilt.state.is_null());
        }
    }

    #[test]
    fn the_size_hint_prefers_the_real_size_and_falls_back_to_the_estimate() {
        let mut entry = MediaEntry::video("m7", "Episode 7", url());
        entry.hints = EntryHints {
            playlist_index: Some(1),
            filesize_approx: Some(4_000),
            ..EntryHints::default()
        };
        let blob = compact_entry(&provider("ytdlp"), &entry);
        let mut item = row("ytdlp", blob);
        assert_eq!(size_hint(&item), Some(4_000));
        assert_eq!(size_hint_excluding_size(&item), Some(4_000));
        item.size = Some(4_321);
        assert_eq!(size_hint(&item), Some(4_321));
        assert_eq!(
            size_hint_excluding_size(&item),
            Some(4_000),
            "the estimate the accumulator has to swap out"
        );

        let bare = row("ytdlp", None);
        assert_eq!(size_hint(&bare), None);
        assert_eq!(size_hint(&row("ytdlp", Some(EntryBlob::truncated()))), None);
    }

    #[test]
    fn an_unresolved_row_falls_back_to_its_url_as_the_media_id() {
        let mut item = row("ytdlp", None);
        item.media_id = None;
        item.provider = None;
        assert_eq!(&*rebuild_entry(&item).media_id, url().as_str());
        item.media_id = Some("".into());
        assert_eq!(&*rebuild_entry(&item).media_id, url().as_str());
    }
}
