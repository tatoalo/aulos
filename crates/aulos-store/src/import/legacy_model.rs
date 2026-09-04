//! The legacy on-disk model: envelopes, the `schema_version 1 → 2` migration, the
//! `__metube_*` wrappers and the timestamp unit mess (DESIGN §7.6.1, legacy spec §5.2, §7.2).
//!
//! Everything here is a pure function of a [`serde_json::Value`]. Nothing touches the filesystem
//! and nothing touches the database, which is what makes the whole migration table testable as a
//! table.
//!
//! The parsing is deliberately hand-rolled rather than `#[derive(Deserialize)]` on a big struct:
//! a legacy file is a mixed-vintage array where **one** bad element must become a `record_skipped`
//! warning while its neighbours import (DESIGN §7.6.1), and a derived `Vec<Record>` fails the
//! whole file instead.

use base64::Engine as _;
use serde_json::{Map, Value};

/// `AUDIO_FORMATS` from `app/dl_formats.py`, verbatim.
pub(crate) const AUDIO_FORMATS: [&str; 5] = ["m4a", "mp3", "opus", "wav", "flac"];

/// The schema versions the importer accepts. `schema_version` outside this set is a *file* error
/// (DESIGN §7.6.1); pickle shelves are out of scope (BRIEF).
pub(crate) const ACCEPTED_SCHEMA_VERSIONS: [u32; 2] = [1, 2];

/// Which legacy collection a record came from. It decides `auto_start` and the status mapping
/// (DESIGN §7.6.3), and it breaks ties when two files hold the same `url`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum Collection {
    /// `completed.json` — terminal records.
    Completed,
    /// `pending.json` — records waiting for the user.
    Pending,
    /// `queue.json` — records the scheduler owned.
    Queue,
}

impl Collection {
    /// The file name in `STATE_DIR`.
    pub(crate) const fn file(self) -> &'static str {
        match self {
            Self::Completed => "completed.json",
            Self::Pending => "pending.json",
            Self::Queue => "queue.json",
        }
    }

    /// The `kind` the envelope must declare.
    pub(crate) const fn kind(self) -> &'static str {
        match self {
            Self::Completed => "persistent_queue:completed",
            Self::Pending => "persistent_queue:pending",
            Self::Queue => "persistent_queue:queue",
        }
    }

    /// The extensionless `shelve` path legacy would have migrated from.
    pub(crate) const fn shelf(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Pending => "pending",
            Self::Queue => "queue",
        }
    }

    /// Whether records from this collection start with `auto_start = true` (DESIGN §7.6.3).
    pub(crate) const fn auto_start(self) -> bool {
        matches!(self, Self::Queue)
    }

    /// Read order — and therefore `ord` tie-break order — is `completed → pending → queue`
    /// (DESIGN §7.6.2 step 1).
    pub(crate) const ALL: [Self; 3] = [Self::Completed, Self::Pending, Self::Queue];
}

impl std::fmt::Display for Collection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.file())
    }
}

/// What is wrong with a file, as a message. Every one of these is a *file* error.
#[derive(Debug)]
pub(crate) struct FileInvalid(pub Box<str>);

impl std::fmt::Display for FileInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated envelope: `{"schema_version":N,"kind":"…","items":[…]}` (legacy spec §5.2).
pub(crate) struct Envelope {
    /// `1` or `2`.
    pub schema_version: u32,
    /// The raw elements, still `Value`s so one bad element is a record error.
    pub items: Vec<Value>,
}

/// Validates an envelope, or explains why the file cannot be used.
///
/// A `kind` mismatch is a file error rather than a silent empty collection: legacy quarantined the
/// file and carried on, and the equivalent here is `AULOS_IMPORT_ON_ERROR=skip` — an explicit
/// operator decision instead of a silent one (DESIGN §7.6.1).
pub(crate) fn read_envelope(text: &str, expected_kind: &str) -> Result<Envelope, FileInvalid> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| FileInvalid(format!("not valid JSON: {e}").into_boxed_str()))?;
    let obj = root
        .as_object()
        .ok_or_else(|| FileInvalid("the payload is not a JSON object".into()))?;

    let schema_version = match obj.get("schema_version").and_then(Value::as_u64) {
        Some(v) if u32::try_from(v).is_ok_and(|v| ACCEPTED_SCHEMA_VERSIONS.contains(&v)) => {
            u32::try_from(v).unwrap_or(0)
        }
        other => {
            return Err(FileInvalid(
                format!(
                    "schema_version {} is not one of {ACCEPTED_SCHEMA_VERSIONS:?}",
                    other.map_or_else(|| "absent".to_owned(), |v| v.to_string())
                )
                .into_boxed_str(),
            ));
        }
    };

    match obj.get("kind").and_then(Value::as_str) {
        Some(k) if k == expected_kind => {}
        other => {
            return Err(FileInvalid(
                format!(
                    "kind is {:?}, expected {expected_kind:?}",
                    other.unwrap_or("absent")
                )
                .into_boxed_str(),
            ));
        }
    }

    let items = obj
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| FileInvalid("items is not an array".into()))?
        .clone();

    Ok(Envelope {
        schema_version,
        items,
    })
}

/// One produced auxiliary file as legacy persisted it: `{"filename": "...", "size": 123}`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct LegacyFile {
    /// Relative to the download directory.
    pub filename: Box<str>,
    /// Bytes on disk, when legacy knew it.
    pub size: Option<u64>,
}

/// A legacy `DownloadInfo` record, migrated to the v2 field set (DESIGN §7.6.1).
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct LegacyRecord {
    /// The legacy `id` — the yt-dlp video id, possibly `"<prefix>.<id>"`.
    pub media_id: Option<Box<str>>,
    /// The legacy `title`.
    pub title: Option<Box<str>>,
    /// The legacy `url`, still a string.
    pub url: Box<str>,
    /// `download_type` after migration.
    pub download_type: Box<str>,
    /// `codec` after migration.
    pub codec: Box<str>,
    /// `format` after migration.
    pub format: Box<str>,
    /// `quality` after migration.
    pub quality: Box<str>,
    /// `""` means the base directory.
    pub folder: Box<str>,
    /// Prepended to the output name.
    pub custom_name_prefix: Box<str>,
    /// `0` = unlimited.
    pub playlist_item_limit: u32,
    /// Write one file per chapter.
    pub split_by_chapters: bool,
    /// `""` means "use the configured default".
    pub chapter_template: Box<str>,
    /// Subtitle language tag.
    pub subtitle_language: Box<str>,
    /// Subtitle mode.
    pub subtitle_mode: Box<str>,
    /// Preset names; the singular `ytdl_options_preset` is migrated into this.
    pub ytdl_options_presets: Vec<Box<str>>,
    /// Per-request yt-dlp overrides.
    pub ytdl_options_overrides: Map<String, Value>,
    /// The legacy status, never empty — an absent one becomes `"pending"` (DESIGN §7.6.1).
    pub status: Box<str>,
    /// `time.time_ns()`, normalised to unix **ms**. `None` when absent or unusable.
    pub timestamp_ms: Option<i64>,
    /// The pre-download problem text.
    pub error: Option<Box<str>>,
    /// The last status message.
    pub msg: Option<Box<str>>,
    /// The produced file, relative to the download directory.
    pub filename: Option<Box<str>>,
    /// Bytes on disk.
    pub size: Option<u64>,
    /// Chapter splits.
    pub chapter_files: Vec<LegacyFile>,
    /// The compacted provider entry, with every `__metube_*` wrapper already decoded.
    pub entry: Option<Map<String, Value>>,
    /// Whether the `__setstate__` migration actually fired for this record.
    pub migrated: bool,
}

impl LegacyRecord {
    /// Reads and migrates one `info` object.
    ///
    /// # Errors
    /// A message for the `record_skipped` warning when the record cannot be used at all: it is not
    /// an object, or it has no usable `url` (the legacy primary key).
    pub(crate) fn from_json(info: &Value) -> Result<Self, Box<str>> {
        let obj = info
            .as_object()
            .ok_or_else(|| Box::<str>::from("info is not an object"))?;

        let url = obj
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| Box::<str>::from("url is missing or not a non-empty string"))?;

        // `__setstate__`: the migration fires only when `download_type` is absent, exactly as
        // legacy gated it (`app/ytdl.py:353`).
        let migrated = !obj.contains_key("download_type");
        let selection = migrate_selection(obj);

        Ok(Self {
            media_id: opt_str(obj, "id"),
            title: opt_str(obj, "title"),
            url: url.into(),
            download_type: selection.download_type,
            codec: selection.codec,
            format: selection.format,
            quality: selection.quality,
            folder: opt_str(obj, "folder").unwrap_or_else(|| "".into()),
            custom_name_prefix: opt_str(obj, "custom_name_prefix").unwrap_or_else(|| "".into()),
            playlist_item_limit: u32_of(obj.get("playlist_item_limit")).unwrap_or(0),
            split_by_chapters: obj
                .get("split_by_chapters")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            chapter_template: opt_str(obj, "chapter_template").unwrap_or_else(|| "".into()),
            subtitle_language: opt_str(obj, "subtitle_language").unwrap_or_else(|| "en".into()),
            subtitle_mode: opt_str(obj, "subtitle_mode").unwrap_or_else(|| "prefer_manual".into()),
            ytdl_options_presets: presets(obj),
            ytdl_options_overrides: obj
                .get("ytdl_options_overrides")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            // DESIGN §7.6.1: a missing `status` is `pending`, matching
            // `_download_info_from_record`.
            status: opt_str(obj, "status").unwrap_or_else(|| "pending".into()),
            timestamp_ms: obj.get("timestamp").and_then(unix_ms_of),
            error: opt_str(obj, "error"),
            msg: opt_str(obj, "msg"),
            filename: opt_str(obj, "filename"),
            size: u64_of(obj.get("size")),
            chapter_files: files(obj.get("chapter_files")),
            entry: obj
                .get("entry")
                .map(decode_wrappers)
                .as_ref()
                .and_then(Value::as_object)
                .cloned(),
            migrated,
        })
    }

    /// The extractor name the legacy entry declared, lower-cased. Empty when there is no entry.
    pub(crate) fn extractor(&self) -> String {
        self.entry
            .as_ref()
            .and_then(|e| e.get("extractor"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// Whether this is a StreamingCommunity record (legacy spec §5.2: a substring test on
    /// `entry['extractor']`, which is what `_compact_persisted_entry` used).
    pub(crate) fn is_streamingcommunity(&self) -> bool {
        self.extractor().contains("streamingcommunity")
    }
}

/// The four selection fields after migration.
struct MigratedSelection {
    download_type: Box<str>,
    codec: Box<str>,
    format: Box<str>,
    quality: Box<str>,
}

/// `DownloadInfo.__setstate__`'s selection half, as a table (DESIGN §7.6.1).
///
/// | Legacy field state | Migration |
/// |---|---|
/// | `format ∈ AUDIO_FORMATS` | `download_type=audio`, `codec=auto`, `format` unchanged |
/// | `format == "thumbnail"` | `download_type=thumbnail`, `format=jpg`, `quality=best` |
/// | `format == "captions"` | `download_type=captions`, `format = subtitle_format or "srt"`, `quality=best` |
/// | `quality == "best_ios"` | `download_type=video`, `format=ios`, `quality=best` |
/// | `quality == "audio"` | `download_type=audio`, `format=m4a`, `quality=best` |
///
/// The two `quality` rows are only reachable from the *video* branch, exactly as in legacy, so a
/// v1 audio record with `quality = "192"` keeps its quality.
///
/// The one deliberate difference from `__setstate__`: legacy left `quality` untouched for the
/// `thumbnail` and `captions` rows, while DESIGN pins it to `best`. Neither branch reads `quality`
/// (`get_opts` ignores it for both types), so this only makes the imported row self-consistent.
fn migrate_selection(obj: &Map<String, Value>) -> MigratedSelection {
    let format = opt_str(obj, "format").unwrap_or_else(|| "any".into());
    let quality = opt_str(obj, "quality").unwrap_or_else(|| "best".into());

    if let Some(dt) = opt_str(obj, "download_type") {
        // Already v2. `if not getattr(self, "codec", None): self.codec = "auto"`.
        let codec = opt_str(obj, "codec")
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "auto".into());
        return MigratedSelection {
            download_type: dt,
            codec,
            format,
            quality,
        };
    }

    let video_codec = opt_str(obj, "video_codec")
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "auto".into());
    let subtitle_format = opt_str(obj, "subtitle_format").unwrap_or_else(|| "srt".into());

    if AUDIO_FORMATS.contains(&&*format) {
        return MigratedSelection {
            download_type: "audio".into(),
            codec: "auto".into(),
            format,
            quality,
        };
    }
    if &*format == "thumbnail" {
        return MigratedSelection {
            download_type: "thumbnail".into(),
            codec: "auto".into(),
            format: "jpg".into(),
            quality: "best".into(),
        };
    }
    if &*format == "captions" {
        return MigratedSelection {
            download_type: "captions".into(),
            codec: "auto".into(),
            format: subtitle_format,
            quality: "best".into(),
        };
    }
    if &*quality == "best_ios" {
        return MigratedSelection {
            download_type: "video".into(),
            codec: video_codec,
            format: "ios".into(),
            quality: "best".into(),
        };
    }
    if &*quality == "audio" {
        return MigratedSelection {
            download_type: "audio".into(),
            codec: "auto".into(),
            format: "m4a".into(),
            quality: "best".into(),
        };
    }
    MigratedSelection {
        download_type: "video".into(),
        codec: video_codec,
        format,
        quality,
    }
}

/// `ytdl_options_presets`, with the legacy singular `ytdl_options_preset` migrated
/// (`__setstate__`, `app/ytdl.py:400`).
fn presets(obj: &Map<String, Value>) -> Vec<Box<str>> {
    let collect = |v: &Value| -> Vec<Box<str>> {
        match v {
            Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(Box::<str>::from)
                .collect(),
            Value::String(s) if !s.trim().is_empty() => vec![s.trim().into()],
            _ => Vec::new(),
        }
    };
    if let Some(v) = obj.get("ytdl_options_presets") {
        return collect(v);
    }
    obj.get("ytdl_options_preset")
        .map(collect)
        .unwrap_or_default()
}

/// `chapter_files` / `subtitle_files` as legacy persisted them.
fn files(v: Option<&Value>) -> Vec<LegacyFile> {
    let Some(Value::Array(a)) = v else {
        return Vec::new();
    };
    a.iter()
        .filter_map(|e| {
            let o = e.as_object()?;
            let filename = o.get("filename").and_then(Value::as_str)?;
            if filename.is_empty() {
                return None;
            }
            Some(LegacyFile {
                filename: filename.into(),
                size: u64_of(o.get("size")),
            })
        })
        .collect()
}

/// A non-empty trimmed string field, or `None`.
fn opt_str(obj: &Map<String, Value>, key: &str) -> Option<Box<str>> {
    let raw = obj.get(key)?;
    let s = match raw {
        Value::String(s) => s.trim(),
        // Legacy stored `id` as whatever yt-dlp produced; a numeric id is a string here.
        Value::Number(n) => return Some(n.to_string().into_boxed_str()),
        _ => return None,
    };
    (!s.is_empty()).then(|| s.into())
}

/// A lenient `u32`, accepting the float JSON encodes a Python int as.
fn u32_of(v: Option<&Value>) -> Option<u32> {
    u64_of(v).and_then(|n| u32::try_from(n).ok())
}

/// A lenient `u64`, accepting a float and a numeric string.
fn u64_of(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Number(n) => n.as_u64().or_else(|| {
            let f = n.as_f64()?;
            (f.is_finite() && f >= 0.0).then_some(f as u64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Normalises a legacy timestamp to unix **milliseconds**.
///
/// The legacy files mix units: `DownloadInfo.timestamp` is `time.time_ns()` (nanoseconds) while
/// `SubscriptionInfo.last_checked` is `time.time()` (float seconds). Rather than trusting a field
/// name, the magnitude decides — every real value is post-2001, so the ranges cannot overlap.
pub(crate) fn unix_ms_of(v: &Value) -> Option<i64> {
    let n = match v {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    // 1e9 s ≈ 2001-09-09; the thresholds are that instant in each unit.
    let ms = if n >= 1e17 {
        n / 1e6 // nanoseconds
    } else if n >= 1e14 {
        n / 1e3 // microseconds
    } else if n >= 1e11 {
        n // milliseconds
    } else {
        n * 1e3 // seconds
    };
    (ms.is_finite() && ms >= 0.0 && ms <= i64::MAX as f64).then_some(ms as i64)
}

/// Decodes the `{"__metube_bytes__": "<b64>"}` / `{"__metube_datetime__": "<iso>"}` wrappers
/// `AtomicJsonStore` writes (legacy spec §5.2), recursively.
///
/// `bytes` become a string: an entry blob is JSON and JSON has no byte type. UTF-8 payloads decode
/// to their text (which is what these actually were — a cookie header, a thumbnail id); anything
/// else keeps its base64 text, because losing the value entirely would be worse than keeping it in
/// the only encoding JSON can hold.
pub(crate) fn decode_wrappers(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            if o.len() == 1 {
                if let Some(Value::String(b64)) = o.get("__metube_bytes__") {
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(b64.as_bytes())
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok());
                    return Value::String(decoded.unwrap_or_else(|| b64.clone()));
                }
                if let Some(Value::String(iso)) = o.get("__metube_datetime__") {
                    return Value::String(iso.clone());
                }
            }
            Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), decode_wrappers(v)))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(decode_wrappers).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)] // `json!` produces an owned Value at every call
    fn record(v: Value) -> LegacyRecord {
        LegacyRecord::from_json(&v).expect("the fixture record must parse")
    }

    #[test]
    fn an_envelope_is_validated_on_version_kind_and_items() {
        let good = r#"{"schema_version":2,"kind":"persistent_queue:queue","items":[]}"#;
        let env = read_envelope(good, "persistent_queue:queue").unwrap();
        assert_eq!(env.schema_version, 2);
        assert!(env.items.is_empty());

        for (text, needle) in [
            ("{", "not valid JSON"),
            ("[]", "not a JSON object"),
            (
                r#"{"schema_version":3,"kind":"persistent_queue:queue","items":[]}"#,
                "schema_version 3",
            ),
            (r#"{"kind":"persistent_queue:queue","items":[]}"#, "absent"),
            (
                r#"{"schema_version":2,"kind":"subscriptions","items":[]}"#,
                "expected",
            ),
            (
                r#"{"schema_version":2,"kind":"persistent_queue:queue","items":{}}"#,
                "items is not an array",
            ),
        ] {
            let err = read_envelope(text, "persistent_queue:queue")
                .err()
                .unwrap_or_else(|| panic!("{text} must be rejected"));
            assert!(err.to_string().contains(needle), "{err} lacks {needle}");
        }
    }

    #[test]
    fn a_v2_record_passes_through_untouched() {
        let r = record(json!({
            "id": "abc123", "title": "A video", "url": "https://youtu.be/abc123",
            "download_type": "video", "codec": "h264", "format": "mp4", "quality": "1080",
            "status": "finished", "timestamp": 1_757_000_000_000_000_000_i64,
            "filename": "A video.mp4", "size": 1024,
            "chapter_files": [{"filename": "A video - 01.mp4", "size": 512}],
        }));
        assert!(!r.migrated);
        assert_eq!(&*r.download_type, "video");
        assert_eq!(&*r.codec, "h264");
        assert_eq!(&*r.format, "mp4");
        assert_eq!(&*r.quality, "1080");
        assert_eq!(r.media_id.as_deref(), Some("abc123"));
        assert_eq!(r.timestamp_ms, Some(1_757_000_000_000));
        assert_eq!(r.size, Some(1024));
        assert_eq!(r.chapter_files.len(), 1);
        assert_eq!(r.chapter_files[0].size, Some(512));
    }

    #[test]
    fn every_row_of_the_setstate_migration_table_fires() {
        // format ∈ AUDIO_FORMATS ⇒ audio, codec auto, format kept, quality kept.
        for f in AUDIO_FORMATS {
            let r = record(json!({"url": "u://x", "format": f, "quality": "192",
                                  "video_codec": "h265"}));
            assert!(r.migrated);
            assert_eq!(&*r.download_type, "audio", "{f}");
            assert_eq!(&*r.codec, "auto", "{f}");
            assert_eq!(&*r.format, f);
            assert_eq!(&*r.quality, "192", "an audio quality survives");
        }

        // thumbnail ⇒ jpg.
        let r = record(json!({"url": "u://x", "format": "thumbnail", "quality": "1080"}));
        assert_eq!(
            (&*r.download_type, &*r.format, &*r.quality),
            ("thumbnail", "jpg", "best")
        );

        // captions ⇒ subtitle_format, defaulting to srt.
        let r = record(json!({"url": "u://x", "format": "captions", "subtitle_format": "vtt"}));
        assert_eq!(
            (&*r.download_type, &*r.format, &*r.quality),
            ("captions", "vtt", "best")
        );
        let r = record(json!({"url": "u://x", "format": "captions"}));
        assert_eq!(&*r.format, "srt", "the documented default");

        // quality best_ios ⇒ the ios format.
        let r = record(json!({"url": "u://x", "format": "any", "quality": "best_ios"}));
        assert_eq!(
            (&*r.download_type, &*r.format, &*r.quality),
            ("video", "ios", "best")
        );

        // quality audio ⇒ m4a audio.
        let r = record(json!({"url": "u://x", "format": "any", "quality": "audio"}));
        assert_eq!(
            (&*r.download_type, &*r.codec, &*r.format, &*r.quality),
            ("audio", "auto", "m4a", "best")
        );

        // the plain video branch keeps video_codec.
        let r = record(json!({"url": "u://x", "format": "mp4", "quality": "720",
                              "video_codec": "av1"}));
        assert_eq!(
            (&*r.download_type, &*r.codec, &*r.quality),
            ("video", "av1", "720")
        );

        // ytdl_options_preset: str → list.
        let r = record(json!({"url": "u://x", "ytdl_options_preset": " archive "}));
        assert_eq!(r.ytdl_options_presets, vec![Box::<str>::from("archive")]);
        let r = record(json!({"url": "u://x", "ytdl_options_preset": ["a", "", " b "]}));
        assert_eq!(
            r.ytdl_options_presets,
            vec![Box::<str>::from("a"), Box::<str>::from("b")]
        );

        // missing status ⇒ pending; every other missing post-v1 field ⇒ its default.
        let r = record(json!({"url": "u://x"}));
        assert_eq!(&*r.status, "pending");
        assert_eq!(&*r.folder, "");
        assert_eq!(&*r.custom_name_prefix, "");
        assert_eq!(r.playlist_item_limit, 0);
        assert!(!r.split_by_chapters);
        assert_eq!(&*r.chapter_template, "");
        assert_eq!(&*r.subtitle_language, "en");
        assert_eq!(&*r.subtitle_mode, "prefer_manual");
        assert!(r.ytdl_options_presets.is_empty());
        assert!(r.ytdl_options_overrides.is_empty());
        assert!(r.entry.is_none());
        assert_eq!(&*r.codec, "auto");
    }

    #[test]
    fn a_v2_record_with_an_empty_codec_still_gets_auto() {
        let r = record(json!({"url": "u://x", "download_type": "video", "codec": ""}));
        assert_eq!(&*r.codec, "auto");
    }

    #[test]
    fn a_record_without_a_usable_url_is_a_record_error() {
        for bad in [
            json!({"title": "no url"}),
            json!({"url": ""}),
            json!({"url": "   "}),
            json!({"url": 42}),
            json!("not an object"),
        ] {
            assert!(
                LegacyRecord::from_json(&bad).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn timestamps_are_normalised_by_magnitude() {
        assert_eq!(
            unix_ms_of(&json!(1_757_000_000_000_000_000_i64)),
            Some(1_757_000_000_000)
        );
        assert_eq!(
            unix_ms_of(&json!(1_757_000_000_000_000_i64)),
            Some(1_757_000_000_000)
        );
        assert_eq!(
            unix_ms_of(&json!(1_757_000_000_000_i64)),
            Some(1_757_000_000_000)
        );
        assert_eq!(
            unix_ms_of(&json!(1_757_000_000.5_f64)),
            Some(1_757_000_000_500)
        );
        assert_eq!(unix_ms_of(&json!("1757000000")), Some(1_757_000_000_000));
        for bad in [json!(0), json!(-1), json!(null), json!("soon"), json!({})] {
            assert_eq!(unix_ms_of(&bad), None, "{bad} must not produce a timestamp");
        }
    }

    #[test]
    fn metube_wrappers_are_decoded_recursively() {
        let v = json!({
            "cookie": {"__metube_bytes__": "aGVsbG8="},
            "when": {"__metube_datetime__": "2026-09-04T10:00:00+00:00"},
            "nested": [{"__metube_bytes__": "///"}, {"keep": 1}],
            "plain": "x",
        });
        let out = decode_wrappers(&v);
        assert_eq!(out["cookie"], "hello");
        assert_eq!(out["when"], "2026-09-04T10:00:00+00:00");
        assert_eq!(
            out["nested"][0], "///",
            "undecodable bytes keep their base64 text"
        );
        assert_eq!(out["nested"][1]["keep"], 1);
        assert_eq!(out["plain"], "x");
    }

    #[test]
    fn the_sc_test_is_a_case_insensitive_substring_of_the_entry_extractor() {
        let r = record(json!({"url": "u://x",
                              "entry": {"extractor": "StreamingCommunity"}}));
        assert!(r.is_streamingcommunity());
        let r = record(json!({"url": "u://x", "entry": {"extractor": "youtube"}}));
        assert!(!r.is_streamingcommunity());
        let r = record(json!({"url": "u://x"}));
        assert!(!r.is_streamingcommunity());
    }

    #[test]
    fn collections_carry_their_legacy_names() {
        assert_eq!(
            Collection::ALL.map(Collection::file),
            ["completed.json", "pending.json", "queue.json"]
        );
        assert_eq!(
            Collection::ALL.map(Collection::kind),
            [
                "persistent_queue:completed",
                "persistent_queue:pending",
                "persistent_queue:queue"
            ]
        );
        assert_eq!(
            Collection::ALL.map(Collection::shelf),
            ["completed", "pending", "queue"]
        );
        assert_eq!(
            Collection::ALL.map(Collection::auto_start),
            [false, false, true]
        );
    }
}
