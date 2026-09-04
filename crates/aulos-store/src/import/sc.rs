//! StreamingCommunity entry translation (DESIGN §7.6.3a).
//!
//! Legacy persisted the **whole** entry for any record whose `entry['extractor']` contained
//! `streamingcommunity` (legacy spec §5.2), and the download-time gate read
//! `entry['_sc_needs_m3u8_extraction']` / `entry['_sc_base_url']` (legacy spec §9.4). The v2
//! `state` object (DESIGN §10.3) renames both and splits out `title_id`/`episode_id`, which legacy
//! only ever had embedded in `id = sc_<title_id>[_<episode_id>]`.
//!
//! So this is a **mandatory, tested translation, not a passthrough**: without it an imported queued
//! SC item cannot be downloaded (the just-in-time extractor reads `state.base_url`) and its NFO
//! would be generated from legacy key names.
//!
//! The output is deliberately the serialisation of `aulos_provider_sc::ScState` rather than that
//! type itself: `aulos-store` sits upstream of every provider (DESIGN §3), so the shape is
//! reproduced here and pinned by an `insta` snapshot plus the cross-crate assertion of
//! `tests/import_sc.rs`.

use serde_json::{Map, Value};

/// `provider` for a translated row.
pub(crate) const PROVIDER: &str = "streamingcommunity";

/// The legacy keys this translation **consumes**: they become v2 fields and are not duplicated
/// into `state.legacy` (DESIGN §7.6.3a, last paragraph).
const CONSUMED: [&str; 9] = [
    "_sc_base_url",
    "_sc_needs_m3u8_extraction",
    "season_number",
    "episode_number",
    "episode",
    "series",
    "ext",
    "extractor",
    "extractor_key",
];

/// A translated entry, plus whether the ids had to be given up on.
pub(crate) struct Translated {
    /// The `state` object, in the DESIGN §10.3 shape.
    pub state: Value,
    /// `true` when neither the media id nor the watch URL yielded a `title_id`, which costs the
    /// NFO its `uniqueid` and nothing else — the just-in-time extractor re-derives the ids from
    /// the watch URL at download time (DESIGN §7.6.3a).
    pub ids_unresolved: bool,
}

/// Translates one legacy SC entry into the v2 `state` object.
///
/// `media_id` is the legacy `id` field (`sc_<title>[_<ep>]`) and `url` the watch URL; both are
/// used, in that order, to recover `title_id`/`episode_id`.
pub(crate) fn translate(
    entry: Option<&Map<String, Value>>,
    media_id: Option<&str>,
    url: &str,
) -> Translated {
    let empty = Map::new();
    let entry = entry.unwrap_or(&empty);

    let base_url = entry
        .get("_sc_base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| origin_of(url), std::borrow::ToOwned::to_owned);

    let needs_m3u8_extraction = entry
        .get("_sc_needs_m3u8_extraction")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let (title_id, episode_id) = media_id
        .and_then(parse_media_id)
        .or_else(|| parse_watch_url(url))
        .unwrap_or((None, None));
    let ids_unresolved = title_id.is_none();

    let mut state = Map::new();
    state.insert("base_url".to_owned(), Value::String(base_url));
    state.insert("title_id".to_owned(), opt_u64(title_id));
    state.insert("episode_id".to_owned(), opt_u64(episode_id));
    state.insert(
        "needs_m3u8_extraction".to_owned(),
        // A row whose ids are unresolved keeps `true`, so the extractor re-derives them.
        Value::Bool(needs_m3u8_extraction || ids_unresolved),
    );
    state.insert("season_number".to_owned(), copy_u32(entry, "season_number"));
    state.insert(
        "episode_number".to_owned(),
        copy_u32(entry, "episode_number"),
    );
    state.insert(
        "episode".to_owned(),
        Value::String(copy_str(entry, "episode").unwrap_or_default()),
    );
    state.insert(
        "series".to_owned(),
        copy_str(entry, "series").map_or(Value::Null, Value::String),
    );
    state.insert(
        "ext".to_owned(),
        Value::String(copy_str(entry, "ext").unwrap_or_else(|| "mp4".to_owned())),
    );
    state.insert(
        "extractor".to_owned(),
        Value::String(copy_str(entry, "extractor").unwrap_or_else(|| PROVIDER.to_owned())),
    );
    state.insert(
        "extractor_key".to_owned(),
        Value::String(
            copy_str(entry, "extractor_key").unwrap_or_else(|| "StreamingCommunity".to_owned()),
        ),
    );

    // Everything else, verbatim — this is what keeps a pre-cutover NFO byte-identical
    // (`plot`, `upload_date`, `uploader`, `channel`, `tags`, `duration`, `original_url`, …).
    let legacy: Map<String, Value> = entry
        .iter()
        .filter(|(k, _)| !CONSUMED.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !legacy.is_empty() {
        state.insert("legacy".to_owned(), Value::Object(legacy));
    }

    Translated {
        state: Value::Object(state),
        ids_unresolved,
    }
}

/// `^sc_(\d+)(?:_(\d+))?$`, hand-rolled (`aulos-store` does not budget for `regex`).
fn parse_media_id(id: &str) -> Option<(Option<u64>, Option<u64>)> {
    let rest = id.trim().strip_prefix("sc_")?;
    let (title, episode) = match rest.split_once('_') {
        Some((t, e)) => (t, Some(e)),
        None => (rest, None),
    };
    let title_id = digits(title)?;
    let episode_id = match episode {
        Some(e) => Some(digits(e)?),
        None => None,
    };
    Some((Some(title_id), episode_id))
}

/// `/watch/(\d+)(?:[?&]e=(\d+))?`, hand-rolled.
fn parse_watch_url(url: &str) -> Option<(Option<u64>, Option<u64>)> {
    let after = url.split_once("/watch/")?.1;
    let (id_part, query) = match after.find(['?', '/', '#']) {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    };
    let title_id = digits(id_part)?;
    let episode_id = query
        .trim_start_matches(['?', '/', '#'])
        .split(['&', '?'])
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == "e")
        .and_then(|(_, v)| digits(v));
    Some((Some(title_id), episode_id))
}

/// A run of ASCII digits, in full, as a `u64`.
fn digits(s: &str) -> Option<u64> {
    let s = s.trim();
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// `{scheme}://{host}` of a URL — the documented `base_url` fallback.
fn origin_of(url: &str) -> String {
    let raw = url.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return String::new();
    };
    let authority = match rest.find(['/', '?', '#']) {
        Some(i) => &rest[..i],
        None => rest,
    };
    if authority.is_empty() {
        return String::new();
    }
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

fn opt_u64(v: Option<u64>) -> Value {
    v.map_or(Value::Null, Value::from)
}

/// A numeric legacy field, tolerating the float JSON encodes a Python int as.
fn copy_u32(entry: &Map<String, Value>, key: &str) -> Value {
    match entry.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .and_then(|n| u32::try_from(n).ok())
            .map_or(Value::Null, Value::from),
        Some(Value::String(s)) => s.trim().parse::<u32>().map_or(Value::Null, Value::from),
        _ => Value::Null,
    }
}

/// A string legacy field, `None` when absent, non-string or empty.
fn copy_str(entry: &Map<String, Value>, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::needless_pass_by_value)] // `json!` produces an owned Value at every call
    fn entry(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn an_episode_translates_into_the_v2_state_shape() {
        let e = entry(json!({
            "id": "sc_9_77", "title": "Serie S01E02 - Pilota",
            "url": "https://sc.test/it/watch/9?e=77",
            "webpage_url": "https://sc.test/it/watch/9?e=77",
            "ext": "mp4", "_type": "video",
            "extractor": "streamingcommunity", "extractor_key": "StreamingCommunity",
            "season_number": 1, "episode_number": 2, "episode": "Pilota", "series": "Serie",
            "_sc_needs_m3u8_extraction": true, "_sc_base_url": "https://sc.test",
            "plot": "Trama", "upload_date": "20260101",
        }));
        let t = translate(Some(&e), Some("sc_9_77"), "https://sc.test/it/watch/9?e=77");
        assert!(!t.ids_unresolved);
        let s = &t.state;
        assert_eq!(s["base_url"], "https://sc.test");
        assert_eq!(s["title_id"], 9);
        assert_eq!(s["episode_id"], 77);
        assert_eq!(s["needs_m3u8_extraction"], true);
        assert_eq!(s["season_number"], 1);
        assert_eq!(s["episode_number"], 2);
        assert_eq!(s["episode"], "Pilota");
        assert_eq!(s["series"], "Serie");
        assert_eq!(s["ext"], "mp4");
        assert_eq!(s["extractor"], "streamingcommunity");
        assert_eq!(s["extractor_key"], "StreamingCommunity");
        // The NFO metadata survives …
        assert_eq!(s["legacy"]["plot"], "Trama");
        assert_eq!(s["legacy"]["upload_date"], "20260101");
        // … and the two `_sc_*` keys are consumed, not duplicated.
        assert!(s["legacy"].get("_sc_base_url").is_none());
        assert!(s["legacy"].get("_sc_needs_m3u8_extraction").is_none());
        // As are the keys that became real v2 fields.
        for consumed in CONSUMED {
            assert!(
                s["legacy"].get(consumed).is_none(),
                "{consumed} must be consumed"
            );
        }
    }

    #[test]
    fn a_movie_translates_with_null_series_fields() {
        let e = entry(json!({
            "id": "sc_5", "title": "Un film", "ext": "mp4",
            "extractor": "streamingcommunity", "extractor_key": "StreamingCommunity",
            "season_number": null, "episode_number": null, "episode": "", "series": null,
            "_sc_needs_m3u8_extraction": true, "_sc_base_url": "https://sc.test",
        }));
        let t = translate(Some(&e), Some("sc_5"), "https://sc.test/it/watch/5");
        assert_eq!(t.state["title_id"], 5);
        assert_eq!(t.state["episode_id"], Value::Null);
        assert_eq!(t.state["season_number"], Value::Null);
        assert_eq!(t.state["episode_number"], Value::Null);
        assert_eq!(t.state["series"], Value::Null);
        assert_eq!(t.state["episode"], "");
    }

    #[test]
    fn the_ids_fall_back_to_the_watch_url() {
        let e =
            entry(json!({"_sc_base_url": "https://sc.test", "extractor": "StreamingCommunity"}));
        let t = translate(
            Some(&e),
            Some("garbage"),
            "https://sc.test/it/watch/12?e=34",
        );
        assert!(!t.ids_unresolved);
        assert_eq!(t.state["title_id"], 12);
        assert_eq!(t.state["episode_id"], 34);

        // No media id at all.
        let t = translate(Some(&e), None, "https://sc.test/it/watch/12");
        assert_eq!(t.state["title_id"], 12);
        assert_eq!(t.state["episode_id"], Value::Null);
    }

    #[test]
    fn unresolvable_ids_are_reported_and_keep_the_extraction_flag_on() {
        let e = entry(json!({"_sc_base_url": "https://sc.test",
                             "_sc_needs_m3u8_extraction": false}));
        let t = translate(Some(&e), Some("nope"), "https://sc.test/it/titles/9-slug");
        assert!(t.ids_unresolved);
        assert_eq!(t.state["title_id"], Value::Null);
        assert_eq!(
            t.state["needs_m3u8_extraction"], true,
            "the extractor must re-derive the ids at download time"
        );
    }

    #[test]
    fn the_base_url_falls_back_to_the_records_origin() {
        let e = entry(json!({"extractor": "streamingcommunity"}));
        let t = translate(
            Some(&e),
            Some("sc_9"),
            "https://StreamingCommunity.test/it/watch/9?e=1",
        );
        assert_eq!(t.state["base_url"], "https://streamingcommunity.test");

        // An unusable URL leaves an empty base_url rather than inventing a host; the item is still
        // imported and still reports its problem at download time.
        let t = translate(Some(&e), Some("sc_9"), "not a url");
        assert_eq!(t.state["base_url"], "");
    }

    #[test]
    fn an_absent_entry_still_produces_a_usable_state() {
        let t = translate(None, Some("sc_9_77"), "https://sc.test/it/watch/9?e=77");
        assert_eq!(t.state["base_url"], "https://sc.test");
        assert_eq!(t.state["title_id"], 9);
        assert_eq!(t.state["episode_id"], 77);
        assert_eq!(t.state["ext"], "mp4");
        assert_eq!(t.state["extractor"], "streamingcommunity");
        assert_eq!(t.state["extractor_key"], "StreamingCommunity");
        assert!(
            t.state.get("legacy").is_none(),
            "an empty legacy map is elided"
        );
    }

    #[test]
    fn media_ids_and_watch_urls_parse_exactly() {
        assert_eq!(parse_media_id("sc_9"), Some((Some(9), None)));
        assert_eq!(parse_media_id("sc_9_77"), Some((Some(9), Some(77))));
        assert_eq!(parse_media_id(" sc_9_77 "), Some((Some(9), Some(77))));
        for bad in ["sc_", "sc_a", "sc_9_", "sc_9_x", "9_77", "prefix.sc_9", ""] {
            assert_eq!(parse_media_id(bad), None, "{bad}");
        }
        assert_eq!(
            parse_watch_url("https://sc.test/it/watch/9?e=77"),
            Some((Some(9), Some(77)))
        );
        assert_eq!(
            parse_watch_url("https://sc.test/it/watch/9"),
            Some((Some(9), None))
        );
        assert_eq!(
            parse_watch_url("https://sc.test/it/watch/9?lang=it&e=77"),
            Some((Some(9), Some(77)))
        );
        for bad in [
            "https://sc.test/it/titles/9-slug",
            "https://sc.test/it/watch/",
            "https://sc.test/it/watch/abc",
        ] {
            assert_eq!(parse_watch_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn numeric_legacy_fields_tolerate_floats_and_strings() {
        let e = entry(json!({"season_number": 1.0, "episode_number": "2"}));
        let t = translate(Some(&e), Some("sc_1_2"), "https://sc.test/it/watch/1?e=2");
        assert_eq!(t.state["season_number"], 1);
        assert_eq!(t.state["episode_number"], 2);
    }
}
