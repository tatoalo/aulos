//! `subscriptions.json` → the `subscriptions` and `subscription_seen` tables (DESIGN §7.6.4).

use std::collections::HashSet;

use aulos_core::{
    RelDir, SubId, SubscriptionRecord, SubtitleLang, SubtitleMode, UnixMs,
    normalize_download_selection,
};
use serde_json::{Map, Value};

use crate::import::legacy_model::unix_ms_of;
use crate::import::report::{Warning, WarningCode};
use crate::json::from_sql_string;

/// The `kind` `subscriptions.json` must declare (legacy spec §7.2).
pub(crate) const KIND: &str = "subscriptions";

/// The file name in `STATE_DIR`.
pub(crate) const FILE: &str = "subscriptions.json";

/// The extensionless `shelve` legacy would have migrated from.
pub(crate) const SHELF: &str = "subscriptions";

/// One imported subscription: the row, its seen ids, and the warnings it produced.
pub(crate) struct Built {
    /// The row.
    pub record: SubscriptionRecord,
    /// The media ids to write into `subscription_seen`, newest first, already capped.
    pub seen: Vec<Box<str>>,
    /// When those ids are recorded as first seen.
    pub seen_at: UnixMs,
    /// Non-fatal findings.
    pub warnings: Vec<Warning>,
}

/// What the caller must tell the subscription builder.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubOpts {
    /// `SUBSCRIPTION_MAX_SEEN_IDS`.
    pub max_seen_ids: u32,
    /// The import's own timestamp — the `next_due` base for a never-checked subscription and the
    /// `seen_at` fallback.
    pub now_ms: UnixMs,
}

/// Builds one subscription row from one legacy record.
///
/// # Errors
/// A message for the `record_skipped` warning when the record has no usable `url`, which is both
/// its uniqueness key and the only field with no sane default.
pub(crate) fn build(value: &Value, index: usize, opts: SubOpts) -> Result<Built, Box<str>> {
    let obj = value
        .as_object()
        .ok_or_else(|| Box::<str>::from("the record is not an object"))?;
    let mut warnings = Vec::new();
    let where_ = format!("{FILE}[{index}]");

    let url_raw = obj
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| Box::<str>::from("url is missing or not a non-empty string"))?;
    let url = from_sql_string(url_raw, "subscriptions.url")
        .map_err(|e| Box::<str>::from(format!("url {url_raw:?} is not usable: {e}")))?;

    // `id ← record.id verbatim` (a UUIDv4 string). An absent or unusable id would drop the
    // subscription, which is worse than losing the old handle, so one is minted and reported.
    let id = match obj.get("id").and_then(Value::as_str) {
        Some(raw) if SubId::is_valid(raw.trim()) => {
            SubId::parse(raw.trim()).unwrap_or_else(|_| SubId::new())
        }
        other => {
            let fresh = SubId::new();
            warnings.push(Warning::new(
                WarningCode::FieldDropped,
                format!(
                    "{where_} id {:?} is not a usable subscription id; minted {fresh}",
                    other.unwrap_or("absent")
                ),
            ));
            fresh
        }
    };

    let (format, quality) = (str_or(obj, "format", "any"), str_or(obj, "quality", "best"));
    let (dtype, codec) = (
        str_or(obj, "download_type", "video"),
        str_or(obj, "codec", "auto"),
    );
    let selection = normalize_download_selection(&format, &quality, &dtype, &codec);

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| Box::<str>::from(url_raw), Box::<str>::from);

    let mut record = SubscriptionRecord::new(id, name, url, selection);
    record.enabled = obj.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    // `max(1, int(...))` on write, exactly as legacy (legacy spec §7.1).
    record.check_interval_minutes = obj
        .get("check_interval_minutes")
        .and_then(as_u32)
        .unwrap_or(60)
        .max(1);
    record.folder = match &*str_or(obj, "folder", "") {
        "" => None,
        folder => match RelDir::parse(folder) {
            Ok(d) => Some(d),
            Err(e) => {
                warnings.push(Warning::new(
                    WarningCode::FieldDropped,
                    format!("{where_} folder {folder:?} is not usable ({e}); using the base dir"),
                ));
                None
            }
        },
    };
    record.custom_name_prefix = str_or(obj, "custom_name_prefix", "");
    record.auto_start = obj
        .get("auto_start")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    record.playlist_item_limit = obj.get("playlist_item_limit").and_then(as_u32).unwrap_or(0);
    record.split_by_chapters = obj
        .get("split_by_chapters")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // `""` already means "the configured default" for this field, so it is carried across as-is.
    record.chapter_template = str_or(obj, "chapter_template", "");
    record.subtitle_language = SubtitleLang::parse(&str_or(obj, "subtitle_language", "en"))
        .unwrap_or_else(|_| SubtitleLang::english());
    record.subtitle_mode = SubtitleMode::from_str_exact(&str_or(obj, "subtitle_mode", ""))
        .unwrap_or(SubtitleMode::PreferManual);
    record.ytdl_options_presets = presets(obj);
    record.ytdl_options_overrides = obj
        .get("ytdl_options_overrides")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Legacy stored epoch **seconds** as a float; every time in v2 is milliseconds.
    record.last_checked = obj.get("last_checked").and_then(unix_ms_of);
    record.next_due = Some(match record.last_checked {
        Some(last) => last.saturating_add(i64::from(record.check_interval_minutes) * 60_000),
        // Nothing checked yet: shortly after boot rather than at +60 s, spread so a dozen
        // subscriptions do not all fire in the same second (BRIEF §12, DESIGN §14.2).
        None => opts.now_ms.saturating_add(jitter_ms(record.id.as_str())),
    });
    // A fresh start for the backoff; the error text is preserved for display.
    record.consecutive_failures = 0;
    record.error = obj
        .get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Box::<str>::from);

    let seen_at = record.last_checked.unwrap_or(opts.now_ms);
    let seen = seen_ids(obj, opts.max_seen_ids);

    Ok(Built {
        record,
        seen,
        seen_at,
        warnings,
    })
}

/// `seen_ids`, deduped in legacy's `dict.fromkeys` order (newest first) and capped at
/// `SUBSCRIPTION_MAX_SEEN_IDS`.
fn seen_ids(obj: &Map<String, Value>, max: u32) -> Vec<Box<str>> {
    let Some(Value::Array(a)) = obj.get("seen_ids") else {
        return Vec::new();
    };
    let cap = if max == 0 { usize::MAX } else { max as usize };
    let mut out: Vec<Box<str>> = Vec::new();
    // The membership set is what keeps this linear: the cap is 50 000 by default, and a scan of
    // `out` per element made one busy subscription cost ~1.25e9 string comparisons on the
    // cutover's critical path (DESIGN §19.3). The `Vec` still fixes the order.
    let mut seen: HashSet<Box<str>> = HashSet::new();
    for v in a {
        let id = match v {
            Value::String(s) => s.trim().to_owned(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        if id.is_empty() {
            continue;
        }
        let id: Box<str> = id.into_boxed_str();
        if !seen.insert(id.clone()) {
            continue;
        }
        out.push(id);
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// `ytdl_options_presets`, with legacy's singular key migrated (legacy spec §7.2).
fn presets(obj: &Map<String, Value>) -> Vec<Box<str>> {
    let from = |v: &Value| -> Vec<Box<str>> {
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
    obj.get("ytdl_options_presets")
        .map(from)
        .or_else(|| obj.get("ytdl_options_preset").map(from))
        .unwrap_or_default()
}

/// A trimmed string field with a default.
fn str_or(obj: &Map<String, Value>, key: &str, default: &str) -> Box<str> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| default.into(), Box::<str>::from)
}

/// A lenient `u32`, accepting the float JSON encodes a Python int as.
///
/// A negative value saturates to `0` rather than being rejected, so `check_interval_minutes = -5`
/// lands on `1` through the `max(1, …)` floor exactly as legacy's `max(1, int(...))` did, instead
/// of silently becoming the 60-minute default.
fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| if f <= 0.0 { 0 } else { f as u64 }))
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .map(|f| if f <= 0.0 { 0 } else { f as u32 }),
        _ => None,
    }
}

/// A deterministic 0..30 000 ms spread, derived from the subscription id.
///
/// DESIGN §7.6.4 asks for `jitter(0..30 s)`; `aulos-store`'s DESIGN §3 row does not budget for
/// `rand`, and a hash of the id is *better* here than a random draw: re-running the importer twice
/// produces the same schedule, so the rehearsal's report and the real run's agree.
fn jitter_ms(id: &str) -> i64 {
    // FNV-1a, 64-bit.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    i64::try_from(hash % 30_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aulos_core::DownloadType;
    use serde_json::json;

    fn opts() -> SubOpts {
        SubOpts {
            max_seen_ids: 50_000,
            now_ms: 1_757_000_000_000,
        }
    }

    fn legacy() -> Value {
        json!({
            "id": "9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f",
            "name": "Veritasium",
            "url": " https://www.youtube.com/@veritasium ",
            "enabled": true,
            "check_interval_minutes": 120,
            "download_type": "video",
            "codec": "auto",
            "format": "any",
            "quality": "best",
            "folder": "",
            "custom_name_prefix": "",
            "auto_start": true,
            "playlist_item_limit": 0,
            "split_by_chapters": false,
            "chapter_template": "",
            "subtitle_language": "en",
            "subtitle_mode": "prefer_manual",
            "ytdl_options_preset": "archive",
            "ytdl_options_overrides": {"retries": 3},
            "last_checked": 1_757_000_100.5_f64,
            "seen_ids": ["a", "b", "a", "", "c"],
            "error": null
        })
    }

    #[test]
    fn the_documented_mapping_holds() {
        let b = build(&legacy(), 0, opts()).expect("must build");
        let r = &b.record;
        assert_eq!(r.id.as_str(), "9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f");
        assert_eq!(&*r.name, "Veritasium");
        assert_eq!(r.url.as_str(), "https://www.youtube.com/@veritasium");
        assert!(r.enabled);
        assert_eq!(r.check_interval_minutes, 120);
        assert_eq!(r.selection.download_type, DownloadType::Video);
        assert_eq!(r.selection.format.as_str(), "any");
        assert!(r.folder.is_none(), "\"\" becomes the base dir");
        assert_eq!(r.ytdl_options_presets, vec![Box::<str>::from("archive")]);
        assert_eq!(r.ytdl_options_overrides["retries"], 3);
        // epoch seconds → ms, rounded.
        assert_eq!(r.last_checked, Some(1_757_000_100_500));
        assert_eq!(r.next_due, Some(1_757_000_100_500 + 120 * 60_000));
        assert_eq!(r.consecutive_failures, 0);
        assert!(r.error.is_none());
        // seen ids: deduped, order preserved, empties dropped.
        assert_eq!(
            b.seen,
            vec![
                Box::<str>::from("a"),
                Box::<str>::from("b"),
                Box::<str>::from("c")
            ]
        );
        assert_eq!(b.seen_at, 1_757_000_100_500);
        assert!(b.warnings.is_empty());
    }

    /// A full `SUBSCRIPTION_MAX_SEEN_IDS` list must dedupe in linear time.
    ///
    /// The importer runs before the listener binds during the cutover (DESIGN §19.3), so a scan of
    /// the accumulator per element — 1.25e9 string comparisons at the default cap — is a hang the
    /// operator watches. The bound is deliberately loose: the linear version is milliseconds, the
    /// quadratic one is minutes in a debug build.
    #[test]
    fn a_full_seen_id_list_dedupes_without_a_quadratic_scan() {
        let mut ids: Vec<Value> = (0..50_000)
            .map(|n| Value::String(format!("video-{n:06}")))
            .collect();
        // Duplicates take the other branch; they must not extend the accumulator.
        ids.extend((0..5_000).map(|n| Value::String(format!("video-{n:06}"))));
        let mut v = legacy();
        v.as_object_mut()
            .expect("object")
            .insert("seen_ids".to_owned(), Value::Array(ids));

        let started = std::time::Instant::now();
        let b = build(&v, 0, opts()).expect("must build");
        let elapsed = started.elapsed();

        assert_eq!(b.seen.len(), 50_000, "each id kept exactly once");
        assert_eq!(&*b.seen[0], "video-000000", "in first-seen order");
        assert_eq!(&*b.seen[49_999], "video-049999");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "deduping 55 000 seen ids took {elapsed:?}; the scan is quadratic again"
        );
    }

    #[test]
    fn a_never_checked_subscription_is_due_shortly_after_boot() {
        let mut v = legacy();
        if let Some(o) = v.as_object_mut() {
            o.remove("last_checked");
        }
        let b = build(&v, 0, opts()).expect("build");
        assert!(b.record.last_checked.is_none());
        let due = b.record.next_due.expect("next_due must be set");
        assert!(
            (opts().now_ms..=opts().now_ms + 30_000).contains(&due),
            "{due} must be within the 30 s jitter window"
        );
        // Deterministic, so a rehearsal and the real run agree.
        assert_eq!(
            build(&v, 0, opts()).expect("build").record.next_due,
            Some(due)
        );
        assert_eq!(b.seen_at, opts().now_ms);
    }

    #[test]
    fn the_interval_is_floored_at_one_minute() {
        for raw in [json!(0), json!(-5), json!(0.4_f64)] {
            let mut v = legacy();
            if let Some(o) = v.as_object_mut() {
                o.insert("check_interval_minutes".to_owned(), raw.clone());
            }
            assert_eq!(
                build(&v, 0, opts())
                    .expect("build")
                    .record
                    .check_interval_minutes,
                1,
                "{raw}"
            );
        }
    }

    #[test]
    fn seen_ids_are_capped_newest_first() {
        let mut v = legacy();
        if let Some(o) = v.as_object_mut() {
            o.insert("seen_ids".to_owned(), json!(["n1", "n2", "n3", "n4", "n5"]));
        }
        let b = build(
            &v,
            0,
            SubOpts {
                max_seen_ids: 2,
                ..opts()
            },
        )
        .expect("build");
        assert_eq!(b.seen, vec![Box::<str>::from("n1"), Box::<str>::from("n2")]);
    }

    #[test]
    fn an_error_text_is_preserved_but_the_backoff_is_reset() {
        let mut v = legacy();
        if let Some(o) = v.as_object_mut() {
            o.insert("error".to_owned(), json!("  Could not resolve URL  "));
        }
        let b = build(&v, 0, opts()).expect("build");
        assert_eq!(b.record.error.as_deref(), Some("Could not resolve URL"));
        assert_eq!(b.record.consecutive_failures, 0);
    }

    #[test]
    fn an_unusable_id_is_replaced_and_reported() {
        let mut v = legacy();
        if let Some(o) = v.as_object_mut() {
            o.insert("id".to_owned(), json!("has spaces"));
        }
        let b = build(&v, 0, opts()).expect("build");
        assert_eq!(b.warnings.len(), 1);
        assert_eq!(b.warnings[0].code, WarningCode::FieldDropped);
        assert!(SubId::is_valid(b.record.id.as_str()));
    }

    #[test]
    fn a_record_without_a_url_is_a_record_error() {
        for bad in [
            json!({}),
            json!({"url": ""}),
            json!({"url": "nope"}),
            json!(7),
        ] {
            assert!(build(&bad, 0, opts()).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn a_missing_name_falls_back_to_the_url() {
        let mut v = legacy();
        if let Some(o) = v.as_object_mut() {
            o.remove("name");
        }
        let b = build(&v, 0, opts()).expect("build");
        assert_eq!(&*b.record.name, "https://www.youtube.com/@veritasium");
    }
}
