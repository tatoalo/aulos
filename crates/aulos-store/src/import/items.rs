//! Turning one legacy record into one `items` row: identity, status, request, entry and error
//! (DESIGN §7.6.2, §7.6.3, §7.6.3a, §7.5).

use std::time::{Duration, SystemTime};

use aulos_core::{
    DownloadRequest, EntryBlob, ErrorCode, FileRef, Item, ItemId, Kind, Ord0, ProviderId, RelDir,
    RelPath, SourceKind, SourceRef, Status, SubtitleLang, SubtitleMode, UnixMs, WireError,
    normalize_download_selection,
};
use serde_json::{Map, Value};
use ulid::Ulid;

use crate::import::legacy_model::{Collection, LegacyRecord};
use crate::import::report::{Warning, WarningCode};
use crate::import::sc;
use crate::json::from_sql_string;

/// The provider every non-StreamingCommunity legacy row is attributed to.
///
/// Legacy had exactly two download paths — yt-dlp and its StreamingCommunity extractor — and every
/// persisted record had already been through `extract_info`, so it *is* resolved: `media_id` and
/// `title` are populated. Leaving `provider` null would instead say "never resolved", which is a
/// different thing and would make the imported row's `canonical_key` disagree with the one a fresh
/// add computes for the same video.
pub(crate) const DEFAULT_PROVIDER: &str = "ytdlp";

/// The `msg` a restarted in-flight download carries (DESIGN §7.6.3).
pub(crate) const RESTARTED_MSG: &str = "Restarted after upgrade";

/// The extra keys `_compact_persisted_entry` kept beyond the `playlist`/`channel` prefixes
/// (`app/ytdl.py:455`); DESIGN §7.5 keeps the same set, because `outtmpl` needs them after a
/// restart.
const COMPACT_ENTRY_EXTRA_KEYS: [&str; 2] = ["n_entries", "__last_playlist_index"];

/// What the caller must tell the item builder about the effective configuration.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ItemOpts {
    /// `CLEAR_COMPLETED_AFTER`, in seconds. `0` disables the timer.
    pub clear_completed_after_s: u64,
    /// The import's own timestamp, used for records with no usable legacy `timestamp`.
    pub fallback_ms: UnixMs,
}

/// One record staged for import, with everything the ordering and the warnings need.
pub(crate) struct Staged {
    /// The migrated record.
    pub record: LegacyRecord,
    /// Which file it came from.
    pub collection: Collection,
    /// Its index in that file's `items` array, for warning details.
    pub index: usize,
    /// The legacy `timestamp`, or the file order when it had none (DESIGN §7.6.2 step 1).
    pub sort_key: (i64, u8, usize),
}

impl Staged {
    /// How advanced this record is, for duplicate-URL resolution: terminal > active > pending
    /// (DESIGN §7.6.2 step 6).
    pub fn advancement(&self) -> u8 {
        match (&*self.record.status, self.collection) {
            (_, Collection::Completed) => 3,
            ("preparing" | "downloading", _) => 2,
            _ => 1,
        }
    }

    /// The status this record will be imported as, for the `duplicate_url` warning text.
    pub fn imported_status(&self) -> Status {
        map_status(&self.record.status, self.collection).status
    }
}

/// The result of the DESIGN §7.6.3 status table for one record.
pub(crate) struct MappedStatus {
    /// The imported status.
    pub status: Status,
    /// `auto_start` — for `queued`, whether the scheduler owns it.
    pub auto_start: bool,
    /// `1` for a record the upgrade interrupted, `0` otherwise.
    pub attempt: u16,
    /// A `msg` the mapping itself imposes.
    pub msg: Option<Box<str>>,
    /// Whether the legacy status was outside the documented set.
    pub unknown: bool,
}

/// The DESIGN §7.6.3 status table, verbatim.
///
/// | Legacy `status` | Source file | Imported status |
/// |---|---|---|
/// | `pending` | `pending.json` | `queued`, `auto_start=false` |
/// | `pending` | `queue.json` | `queued`, `auto_start=true` |
/// | `preparing`, `downloading` | `queue.json` | `queued`, `auto_start=true`, `attempt+=1`, `msg="Restarted after upgrade"` |
/// | `finished` | `completed.json` | `finished` |
/// | `error` | `completed.json` | `error` |
/// | anything else / absent | any | `queued` if in queue/pending, else `error` |
///
/// It never guesses `finished`.
pub(crate) fn map_status(status: &str, collection: Collection) -> MappedStatus {
    let queued = |auto_start: bool, unknown: bool| MappedStatus {
        status: Status::Queued,
        auto_start,
        attempt: 0,
        msg: None,
        unknown,
    };

    match (status, collection) {
        ("pending", Collection::Pending) => queued(false, false),
        ("pending", Collection::Queue) => queued(true, false),
        ("preparing" | "downloading", Collection::Queue) => MappedStatus {
            status: Status::Queued,
            auto_start: true,
            attempt: 1,
            msg: Some(RESTARTED_MSG.into()),
            unknown: false,
        },
        ("finished", Collection::Completed) => MappedStatus {
            status: Status::Finished,
            auto_start: true,
            attempt: 0,
            msg: None,
            unknown: false,
        },
        ("error", Collection::Completed) => MappedStatus {
            status: Status::Error,
            auto_start: true,
            attempt: 0,
            msg: None,
            unknown: false,
        },
        // Anything else in a live collection is queued, with the file's own `auto_start` …
        (_, Collection::Pending | Collection::Queue) => queued(collection.auto_start(), true),
        // … and anything else in `completed.json` is an error that says so.
        (other, Collection::Completed) => MappedStatus {
            status: Status::Error,
            auto_start: true,
            attempt: 0,
            msg: Some(format!("Imported with unknown legacy status: {other}").into_boxed_str()),
            unknown: true,
        },
    }
}

/// One built row plus the warnings building it produced.
pub(crate) struct Built {
    /// The row, ready for `WriteOp::InsertItems`.
    pub item: Item,
    /// Non-fatal findings (a dropped folder, unresolvable SC ids, an unknown status).
    pub warnings: Vec<Warning>,
}

/// Builds one `items` row.
///
/// # Errors
/// A message for the `record_skipped` warning when the record cannot become a row at all: the only
/// such case is an unusable `url`, because every other field has a documented default.
pub(crate) fn build(staged: &Staged, ord: Ord0, opts: ItemOpts) -> Result<Built, Box<str>> {
    let r = &staged.record;
    let mut warnings = Vec::new();
    let where_ = format!("{}[{}]", staged.collection, staged.index);

    // --- identity -----------------------------------------------------------------------------
    let url = from_sql_string(&r.url, "items.url")
        .map_err(|e| Box::<str>::from(format!("url {:?} is not usable: {e}", r.url)))?;
    let created_at = r.timestamp_ms.unwrap_or(opts.fallback_ms);
    let id = ItemId::from_ulid(Ulid::from_datetime(
        SystemTime::UNIX_EPOCH + Duration::from_millis(created_at.unsigned_abs()),
    ));

    // --- status -------------------------------------------------------------------------------
    let mapped = map_status(&r.status, staged.collection);
    if mapped.unknown {
        warnings.push(Warning::new(
            WarningCode::UnknownStatus,
            format!(
                "{where_} status={:?} → {}",
                r.status,
                mapped.status.as_str()
            ),
        ));
    }

    // --- provider and entry -------------------------------------------------------------------
    let is_sc = r.is_streamingcommunity();
    let provider_id = if is_sc {
        sc::PROVIDER
    } else {
        DEFAULT_PROVIDER
    };
    let provider = ProviderId::parse(provider_id).ok();
    let entry = if is_sc {
        let translated = sc::translate(r.entry.as_ref(), r.media_id.as_deref(), &r.url);
        if translated.ids_unresolved {
            warnings.push(Warning::new(
                WarningCode::ScIdsUnresolved,
                format!(
                    "{where_} could not derive title_id from id={:?} or url {:?}; \
                     the extractor will re-derive them at download time",
                    r.media_id.as_deref().unwrap_or(""),
                    r.url
                ),
            ));
        }
        Some(EntryBlob::new(translated.state))
    } else {
        compact_entry(r.entry.as_ref()).map(EntryBlob::new)
    };

    // --- request ------------------------------------------------------------------------------
    let selection = normalize_download_selection(&r.format, &r.quality, &r.download_type, &r.codec);
    let mut request = DownloadRequest::new(url, selection);
    request.folder = match &*r.folder {
        "" => None,
        folder => match RelDir::parse(folder) {
            Ok(dir) => Some(dir),
            Err(e) => {
                warnings.push(Warning::new(
                    WarningCode::FieldDropped,
                    format!("{where_} folder {folder:?} is not usable ({e}); using the base dir"),
                ));
                None
            }
        },
    };
    request.custom_name_prefix = r.custom_name_prefix.clone();
    request.playlist_item_limit = r.playlist_item_limit;
    request.auto_start = mapped.auto_start;
    request.split_by_chapters = r.split_by_chapters;
    // `""` already means "use the configured default" in v2 (`SubscriptionRecord.chapter_template`,
    // `DownloadRequest::new`), so it is carried across as-is rather than materialised here.
    request.chapter_template = r.chapter_template.clone();
    request.subtitle_language =
        SubtitleLang::parse(&r.subtitle_language).unwrap_or_else(|_| SubtitleLang::english());
    request.subtitle_mode =
        SubtitleMode::from_str_exact(&r.subtitle_mode).unwrap_or(SubtitleMode::PreferManual);
    request.ytdl_options_presets = r.ytdl_options_presets.clone();
    request.ytdl_options_overrides = r.ytdl_options_overrides.clone();

    // --- output -------------------------------------------------------------------------------
    let filename = match r.filename.as_deref() {
        None => None,
        Some(f) => match RelPath::parse(f) {
            Ok(p) => Some(p),
            Err(e) => {
                warnings.push(Warning::new(
                    WarningCode::FieldDropped,
                    format!("{where_} filename {f:?} is not usable ({e}); dropped"),
                ));
                None
            }
        },
    };
    let chapter_files = r
        .chapter_files
        .iter()
        .map(|f| FileRef {
            filename: (*f.filename).into(),
            size: f.size,
            // Both are derived at serve time from `PUBLIC_HOST_URL` and the sidecar scan
            // (DESIGN §4.6.2); legacy persisted neither.
            download_url: None,
            lang: None,
        })
        .collect();

    // --- error and timers ---------------------------------------------------------------------
    let error = r.error.as_deref().map(classify_error);
    // The importer is the one `msg` writer that bypasses the engine's terminal write, so it has to
    // keep that rule itself: a `finished` row's `msg` is always `null` (PROTOCOL §2.3, §3.1).
    // Legacy overloaded the field with the live progress line and persisted whatever was on the
    // record when it completed, so `completed.json` really does carry lines like `"MoveFiles…"` on
    // finished entries — importing them verbatim reproduces the exact bug on the imported half of
    // the history. A `mapped.msg` note ("unknown legacy status", "Restarted after upgrade") is a
    // terminal reason rather than a live line and is never written on `finished`, so nothing is
    // lost here.
    let msg = if mapped.status == Status::Finished {
        None
    } else {
        mapped.msg.or_else(|| r.msg.clone())
    };
    let finished_at = mapped.status.is_terminal().then_some(created_at);
    let clear_after = finished_at
        .filter(|_| opts.clear_completed_after_s > 0)
        .map(|at| {
            at.saturating_add(
                i64::try_from(opts.clear_completed_after_s.saturating_mul(1_000))
                    .unwrap_or(i64::MAX),
            )
        });

    let item = Item {
        id,
        kind: Kind::Item,
        // Legacy expanded playlists into flat records, so an imported row is never a group and
        // never a child of one (DESIGN §8.6 groups are created by v2 resolution only).
        group_id: None,
        group_index: None,
        ord,
        url: request.url.clone(),
        canonical_key: crate::import::canonical::canonical_key(
            provider_id,
            &r.url,
            r.media_id.as_deref(),
        ),
        provider,
        media_id: r.media_id.clone(),
        title: r.title.clone().unwrap_or_else(|| r.url.clone()),
        status: mapped.status,
        auto_start: mapped.auto_start,
        msg,
        error,
        request,
        entry,
        filename,
        size: r.size,
        chapter_files,
        // Legacy deliberately did not persist `subtitle_files` (legacy spec §5.2).
        subtitle_files: Vec::new(),
        created_at,
        // Unknowable: legacy persisted no start time. Inventing one would put a lie on the wire.
        started_at: None,
        finished_at,
        attempt: mapped.attempt,
        // Legacy persisted no attribution at all. `api_v1` is the honest answer — every legacy
        // record entered through the v1 surface, whatever ultimately called it.
        source: SourceRef::bare(SourceKind::ApiV1),
        children_total: None,
        clear_after,
    };

    Ok(Built { item, warnings })
}

/// `_compact_persisted_entry` for a non-StreamingCommunity entry (DESIGN §7.5, legacy
/// `app/ytdl.py:458`): the `playlist*`/`channel*` keys plus `n_entries` and
/// `__last_playlist_index`, or nothing at all.
fn compact_entry(entry: Option<&Map<String, Value>>) -> Option<Value> {
    let entry = entry?;
    let compact: Map<String, Value> = entry
        .iter()
        .filter(|(k, _)| {
            k.starts_with("playlist")
                || k.starts_with("channel")
                || COMPACT_ENTRY_EXTRA_KEYS.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!compact.is_empty()).then_some(Value::Object(compact))
}

/// The legacy `error` string as a [`WireError`] (DESIGN §7.6.3).
///
/// `internal` unless the text matches the DESIGN §6 taxonomy. The matching is a small ordered
/// substring table rather than the shim's regex table (DESIGN §9.6): `aulos-store` does not budget
/// for `regex`, and the inputs here are already-rendered yt-dlp messages, not exception classes.
/// Getting a code wrong only affects the retry hint on a row that has already failed.
pub(crate) fn classify_error(text: &str) -> WireError {
    let cleaned = clean_error_text(text);
    let lower = cleaned.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    let code = if has(&["http error 429", "too many requests"]) {
        ErrorCode::Throttled
    } else if has(&[
        "sign in",
        "log in",
        "members-only",
        "private video",
        "requires authentication",
    ]) {
        ErrorCode::AuthRequired
    } else if has(&[
        "confirm you're not a bot",
        "confirm you’re not a bot",
        "failed to extract any player response",
    ]) {
        ErrorCode::BotCheck
    } else if has(&[
        "not available in your country",
        "geo restricted",
        "geo-restricted",
        "blocked it in your country",
    ]) {
        ErrorCode::GeoRestricted
    } else if has(&[
        "video unavailable",
        "removed by the uploader",
        "account associated with this video has been terminated",
        "has been terminated",
        "this video is unavailable",
    ]) {
        ErrorCode::Unavailable
    } else if has(&["premieres in", "is_upcoming", "live event will begin"]) {
        ErrorCode::NotYetLive
    } else if has(&[
        "requested format is not available",
        "no video formats found",
    ]) {
        ErrorCode::NoFormat
    } else if has(&["no space left on device"]) {
        ErrorCode::DiskFull
    } else if has(&["unsupported url"]) {
        ErrorCode::UnsupportedUrl
    } else if has(&[
        "http error 5",
        "timed out",
        "timeout",
        "connection reset",
        "temporary failure in name resolution",
        "network is unreachable",
        "connection refused",
    ]) {
        ErrorCode::Network
    } else if has(&["postprocessing", "ffmpeg exited"]) {
        ErrorCode::PostprocessingFailed
    } else {
        ErrorCode::Internal
    };

    WireError::new(code, cleaned)
}

/// DESIGN §9.6's message cleaning: strip a leading `ERROR: `, drop `\r` and ANSI escapes, trim to
/// 512 characters.
fn clean_error_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip a CSI/OSC sequence: everything up to the terminating letter or BEL.
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() || n == '\u{7}' {
                    break;
                }
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim();
    let trimmed = trimmed.strip_prefix("ERROR: ").unwrap_or(trimmed).trim();
    trimmed.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn staged(status: &str, collection: Collection, extra: &Value) -> Staged {
        let mut obj = json!({
            "url": "https://youtu.be/abc123",
            "id": "abc123",
            "title": "A video",
            "download_type": "video",
            "codec": "auto",
            "format": "mp4",
            "quality": "best",
            "status": status,
            "timestamp": 1_757_000_000_000_000_000_i64,
        });
        if let (Some(o), Some(e)) = (obj.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        Staged {
            record: LegacyRecord::from_json(&obj).expect("fixture"),
            collection,
            index: 0,
            sort_key: (0, 0, 0),
        }
    }

    fn opts() -> ItemOpts {
        ItemOpts {
            clear_completed_after_s: 0,
            fallback_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn the_status_table_maps_every_documented_row() {
        let cases = [
            ("pending", Collection::Pending, Status::Queued, false, 0),
            ("pending", Collection::Queue, Status::Queued, true, 0),
            ("preparing", Collection::Queue, Status::Queued, true, 1),
            ("downloading", Collection::Queue, Status::Queued, true, 1),
            ("finished", Collection::Completed, Status::Finished, true, 0),
            ("error", Collection::Completed, Status::Error, true, 0),
        ];
        for (legacy, coll, status, auto_start, attempt) in cases {
            let m = map_status(legacy, coll);
            assert_eq!(m.status, status, "{legacy} in {coll}");
            assert_eq!(m.auto_start, auto_start, "{legacy} in {coll}");
            assert_eq!(m.attempt, attempt, "{legacy} in {coll}");
            assert!(!m.unknown);
        }
        assert_eq!(
            map_status("preparing", Collection::Queue).msg.as_deref(),
            Some(RESTARTED_MSG)
        );
    }

    #[test]
    fn an_unknown_status_never_guesses_finished() {
        let m = map_status("cancelled", Collection::Completed);
        assert_eq!(m.status, Status::Error);
        assert!(m.unknown);
        assert_eq!(
            m.msg.as_deref(),
            Some("Imported with unknown legacy status: cancelled")
        );

        // In a live collection the same status is simply queued.
        for coll in [Collection::Queue, Collection::Pending] {
            let m = map_status("cancelled", coll);
            assert_eq!(m.status, Status::Queued);
            assert_eq!(m.auto_start, coll.auto_start());
            assert!(m.unknown);
        }
        // A `finished` record sitting in queue.json is queued, not finished.
        assert_eq!(
            map_status("finished", Collection::Queue).status,
            Status::Queued
        );
    }

    #[test]
    fn a_completed_row_keeps_its_output_and_arms_the_clear_timer() {
        let s = staged(
            "finished",
            Collection::Completed,
            &json!({"filename": "A video.mp4", "size": 4096,
                   "chapter_files": [{"filename": "A video - 01.mp4", "size": 512}]}),
        );
        let built = build(
            &s,
            7,
            ItemOpts {
                clear_completed_after_s: 60,
                fallback_ms: 0,
            },
        )
        .expect("must build");
        let i = &built.item;
        assert_eq!(i.ord, 7);
        assert_eq!(i.status, Status::Finished);
        assert_eq!(
            i.filename.as_ref().map(RelPath::as_str),
            Some("A video.mp4")
        );
        assert_eq!(i.size, Some(4096));
        assert_eq!(i.chapter_files.len(), 1);
        assert_eq!(&*i.chapter_files[0].filename, "A video - 01.mp4");
        assert_eq!(i.chapter_files[0].size, Some(512));
        assert_eq!(i.created_at, 1_757_000_000_000);
        assert_eq!(i.finished_at, Some(1_757_000_000_000));
        assert_eq!(i.clear_after, Some(1_757_000_060_000));
        assert_eq!(i.media_id.as_deref(), Some("abc123"));
        assert_eq!(i.provider.as_ref().map(ProviderId::as_str), Some("ytdlp"));
        assert_eq!(&*i.canonical_key, "ytdlp\u{1f}abc123");
        assert_eq!(i.source.kind, SourceKind::ApiV1);
        assert!(i.started_at.is_none());
        assert!(built.warnings.is_empty());
    }

    #[test]
    fn the_ulid_is_minted_from_the_legacy_timestamp() {
        let a = build(
            &staged("finished", Collection::Completed, &json!({})),
            0,
            opts(),
        )
        .expect("build");
        let b = build(
            &staged(
                "finished",
                Collection::Completed,
                &json!({"timestamp": 1_757_000_001_000_000_000_i64}),
            ),
            1,
            opts(),
        )
        .expect("build");
        assert!(
            a.item.id.to_string() < b.item.id.to_string(),
            "ULID order must follow the legacy timestamp"
        );
        assert_ne!(a.item.id, b.item.id);
    }

    #[test]
    fn a_record_with_no_timestamp_uses_the_import_time() {
        let mut s = staged("pending", Collection::Queue, &json!({}));
        s.record.timestamp_ms = None;
        let built = build(&s, 0, opts()).expect("build");
        assert_eq!(built.item.created_at, 1_700_000_000_000);
    }

    #[test]
    fn the_v1_selection_migration_reaches_the_request() {
        let s = staged("pending", Collection::Pending, &json!({}));
        // A v1 audio record: no `download_type`, `format` in AUDIO_FORMATS.
        let mut raw = json!({"url": "https://youtu.be/x", "format": "m4a", "quality": "192"});
        if let Some(o) = raw.as_object_mut() {
            o.remove("download_type");
        }
        let v1 = Staged {
            record: LegacyRecord::from_json(&raw).expect("fixture"),
            collection: s.collection,
            index: 1,
            sort_key: (0, 0, 1),
        };
        let built = build(&v1, 0, opts()).expect("build");
        let sel = &built.item.request.selection;
        assert_eq!(sel.download_type, aulos_core::DownloadType::Audio);
        assert_eq!(sel.format.as_str(), "m4a");
        assert_eq!(sel.quality.as_str(), "192");
        assert!(
            !built.item.auto_start,
            "pending.json means the user decides"
        );
        assert_eq!(built.item.request.auto_start, built.item.auto_start);
    }

    #[test]
    fn an_unusable_url_is_the_only_record_error() {
        let mut s = staged("pending", Collection::Queue, &json!({}));
        s.record.url = "not a url".into();
        let err = build(&s, 0, opts()).err().expect("must be rejected");
        assert!(err.contains("not usable"), "{err}");
    }

    #[test]
    fn an_uncontainable_folder_or_filename_is_dropped_not_fatal() {
        let s = staged(
            "finished",
            Collection::Completed,
            &json!({"folder": "../etc", "filename": "/abs/x.mp4"}),
        );
        let built = build(&s, 0, opts()).expect("the row must still import");
        assert!(built.item.request.folder.is_none());
        assert!(built.item.filename.is_none());
        assert_eq!(built.warnings.len(), 2);
        assert!(
            built
                .warnings
                .iter()
                .all(|w| w.code == WarningCode::FieldDropped)
        );
    }

    #[test]
    fn a_playlist_entry_is_compacted_and_a_plain_one_is_dropped() {
        let s = staged(
            "pending",
            Collection::Queue,
            &json!({"entry": {"playlist": "Mix", "playlist_index": 3, "channel_id": "UC1",
                             "n_entries": 12, "__last_playlist_index": 12,
                             "formats": [1, 2, 3], "thumbnail": "x"}}),
        );
        let built = build(&s, 0, opts()).expect("build");
        let entry = built
            .item
            .entry
            .as_ref()
            .expect("a playlist child keeps keys");
        let obj = entry.as_value().as_object().expect("object");
        assert_eq!(obj.len(), 5, "{obj:?}");
        assert_eq!(obj["playlist"], "Mix");
        assert_eq!(obj["n_entries"], 12);
        assert!(!obj.contains_key("formats"));

        let s = staged(
            "pending",
            Collection::Queue,
            &json!({"entry": {"formats": [1], "thumbnail": "x"}}),
        );
        assert!(
            build(&s, 0, opts()).expect("build").item.entry.is_none(),
            "a plain yt-dlp child keeps nothing"
        );
    }

    #[test]
    fn a_streamingcommunity_row_is_translated_and_attributed() {
        let s = staged(
            "pending",
            Collection::Queue,
            &json!({"id": "sc_9_77", "url": "https://sc.test/it/watch/9?e=77",
                   "entry": {"extractor": "streamingcommunity",
                             "_sc_base_url": "https://sc.test",
                             "_sc_needs_m3u8_extraction": true,
                             "season_number": 1, "episode_number": 2,
                             "episode": "Pilota", "series": "Serie", "ext": "mp4",
                             "plot": "Trama"}}),
        );
        let built = build(&s, 0, opts()).expect("build");
        assert_eq!(
            built.item.provider.as_ref().map(ProviderId::as_str),
            Some("streamingcommunity")
        );
        assert_eq!(
            &*built.item.canonical_key,
            "streamingcommunity\u{1f}sc_9_77"
        );
        let state = built.item.entry.as_ref().expect("state").as_value();
        assert_eq!(state["base_url"], "https://sc.test");
        assert_eq!(state["title_id"], 9);
        assert_eq!(state["legacy"]["plot"], "Trama");
        assert!(built.warnings.is_empty());
    }

    #[test]
    fn an_sc_row_with_unresolvable_ids_warns_but_imports() {
        let s = staged(
            "pending",
            Collection::Queue,
            &json!({"id": "weird", "url": "https://sc.test/it/titles/9-slug",
                   "entry": {"extractor": "StreamingCommunity"}}),
        );
        let built = build(&s, 0, opts()).expect("build");
        assert_eq!(built.warnings.len(), 1);
        assert_eq!(built.warnings[0].code, WarningCode::ScIdsUnresolved);
        assert_eq!(
            built.item.entry.as_ref().expect("state").as_value()["needs_m3u8_extraction"],
            true
        );
    }

    #[test]
    fn advancement_prefers_terminal_then_active_then_pending() {
        let done = staged("finished", Collection::Completed, &json!({}));
        let active = staged("downloading", Collection::Queue, &json!({}));
        let queued = staged("pending", Collection::Queue, &json!({}));
        let pending = staged("pending", Collection::Pending, &json!({}));
        assert!(done.advancement() > active.advancement());
        assert!(active.advancement() > queued.advancement());
        assert_eq!(queued.advancement(), pending.advancement());
        assert_eq!(done.imported_status(), Status::Finished);
    }

    #[test]
    fn legacy_error_text_is_cleaned_and_classified() {
        let cases = [
            ("ERROR: Video unavailable", ErrorCode::Unavailable),
            (
                "ERROR: [youtube] x: Sign in to confirm your age",
                ErrorCode::AuthRequired,
            ),
            (
                "Sign in to confirm you're not a bot",
                ErrorCode::AuthRequired,
            ),
            ("Failed to extract any player response", ErrorCode::BotCheck),
            ("HTTP Error 429: Too Many Requests", ErrorCode::Throttled),
            ("HTTP Error 503: Service Unavailable", ErrorCode::Network),
            ("Requested format is not available", ErrorCode::NoFormat),
            (
                "This live event will begin in 3 hours",
                ErrorCode::NotYetLive,
            ),
            ("No space left on device", ErrorCode::DiskFull),
            (
                "Unsupported URL: file:///etc/passwd",
                ErrorCode::UnsupportedUrl,
            ),
            ("something nobody has ever seen", ErrorCode::Internal),
        ];
        for (text, code) in cases {
            assert_eq!(classify_error(text).code, code, "{text}");
        }
        // Cleaning: the prefix, ANSI and \r all go, and the length is capped.
        let e = classify_error("ERROR: \u{1b}[31mVideo unavailable\u{1b}[0m\r\n");
        assert_eq!(&*e.message, "Video unavailable");
        let long = format!("ERROR: {}", "x".repeat(1_000));
        assert_eq!(classify_error(&long).message.chars().count(), 512);
    }

    #[test]
    fn a_pre_download_problem_survives_on_a_queued_row() {
        let s = staged(
            "pending",
            Collection::Queue,
            &json!({"error": "Premieres in 3 hours", "msg": "waiting"}),
        );
        let built = build(&s, 0, opts()).expect("build");
        assert_eq!(built.item.status, Status::Queued);
        let e = built.item.error.expect("the error must survive");
        assert_eq!(e.code, ErrorCode::NotYetLive);
        assert_eq!(built.item.msg.as_deref(), Some("waiting"));
    }

    #[test]
    fn a_title_falls_back_to_the_url() {
        let mut s = staged("pending", Collection::Queue, &json!({}));
        s.record.title = None;
        let built = build(&s, 0, opts()).expect("build");
        assert_eq!(&*built.item.title, "https://youtu.be/abc123");
    }
}
