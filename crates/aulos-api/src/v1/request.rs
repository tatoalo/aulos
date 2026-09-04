//! `_migrate_legacy_request` and `parse_download_options`, ported check for check
//! (DESIGN §11.2, §11.2.1).
//!
//! Both functions are pure: they take a `serde_json` object and produce either a
//! [`DownloadRequest`] or the byte-identical legacy 400. That is what makes one unit test per row
//! of the migration table and one per leniency possible without an HTTP server.
//!
//! # Why the order is copied, not improved
//!
//! `parse_download_options` validated in a specific sequence and a shipped client renders whatever
//! reason comes back **first**. `{"format": "mkv", "quality": "999"}` has to answer about `format`,
//! not about `quality`, and `{"playlist_item_limit": "nope", "format": "mkv"}` has to answer about
//! `format` because `int()` was the *last* thing legacy did. WP-00's corpus leans on exactly that:
//! `playlist_item_limit: "nope"` is its probe for "everything before me validated", so nine of its
//! `POST add` cases are only meaningful if this order holds.
//!
//! The one deliberate addition is `auto_start`, checked **after** `playlist_item_limit` so it can
//! never displace a legacy reason (DESIGN §11.2 step 4).

use std::collections::BTreeSet;

use aulos_core::catalog::FormatCatalog;
use aulos_core::{
    Codec, Config, DownloadRequest, ErrorCode, FormatId, QualityId, RelDir, Selection,
    SubtitleLang, SubtitleMode,
};
use serde_json::{Map, Value};
use url::Url;

use super::legacy;
use crate::error::ApiError;
use crate::v2::parse_bool;

/// `_migrate_legacy_request` (`app/main.py:299-353`), in place.
///
/// A no-op when `download_type` is present — that is the legacy guard, and it is why
/// `{"download_type": "video", "format": "m4a"}` is a `format` 400 rather than a silent switch to
/// audio.
///
/// The table, for reference (DESIGN §11.2 step 2):
///
/// | legacy `format` | legacy `quality` | → `download_type` | `codec` | `format` | `quality` |
/// |---|---|---|---|---|---|
/// | `m4a\|mp3\|opus\|wav\|flac` | any | `audio` | `auto` | same | unchanged |
/// | `thumbnail` | any | `thumbnail` | `auto` | `jpg` | `best` |
/// | `captions` | any | `captions` | `auto` | `subtitle_format` or `srt` | `best` |
/// | other | `best_ios` | `video` | `video_codec` | `ios` | `best` |
/// | other | `audio` | `audio` | `auto` | `m4a` | `best` |
/// | other | else | `video` | `video_codec` | legacy `format` | legacy `quality` |
pub fn migrate_legacy_request(body: &mut Map<String, Value>) {
    if body.contains_key("download_type") {
        return;
    }

    let old_format = py_lower(body.get("format"), "any");
    let old_quality = py_lower(body.get("quality"), "best");
    let old_codec = py_lower(body.get("video_codec"), "auto");

    let set = |body: &mut Map<String, Value>, key: &str, value: &str| {
        body.insert(key.to_owned(), Value::String(value.to_owned()));
    };

    if legacy::AUDIO_FORMATS.contains(&old_format.as_str()) {
        set(body, "download_type", "audio");
        set(body, "codec", "auto");
        set(body, "format", &old_format);
    } else if old_format == "thumbnail" {
        set(body, "download_type", "thumbnail");
        set(body, "codec", "auto");
        set(body, "format", "jpg");
        set(body, "quality", "best");
    } else if old_format == "captions" {
        let subtitle_format = py_lower(body.get("subtitle_format"), "srt");
        set(body, "download_type", "captions");
        set(body, "codec", "auto");
        set(body, "format", &subtitle_format);
        set(body, "quality", "best");
    } else if old_quality == "best_ios" {
        set(body, "download_type", "video");
        set(body, "codec", &old_codec);
        set(body, "format", "ios");
        set(body, "quality", "best");
    } else if old_quality == "audio" {
        set(body, "download_type", "audio");
        set(body, "codec", "auto");
        set(body, "format", "m4a");
        set(body, "quality", "best");
    } else {
        set(body, "download_type", "video");
        set(body, "codec", &old_codec);
        set(body, "format", &old_format);
        set(body, "quality", &old_quality);
    }
}

/// `parse_download_options` (`app/main.py:547-664`), including the five §11.2.1 leniencies.
///
/// `body` is taken by value because legacy did `dict(post)` and then mutated it; the caller keeps
/// its own copy for the fields the shim reads afterwards (`check_interval_minutes`).
///
/// # Errors
/// A `400` whose `error.message` is the byte-identical legacy reason string. `code` is
/// `validation_failed` for the field checks, `unknown_preset` for an unconfigured preset,
/// `overrides_disabled` for the gate and `folder_invalid` for a bad `folder`, so a v2-aware client
/// reading a v1 response still gets the taxonomy.
pub fn parse_download_options(
    cfg: &Config,
    known_presets: &BTreeSet<Box<str>>,
    mut body: Map<String, Value>,
) -> Result<DownloadRequest, ApiError> {
    migrate_legacy_request(&mut body);

    // --- 1. the three required fields, on Python truthiness -----------------------------------
    let url_value = body.get("url").cloned().unwrap_or(Value::Null);
    let quality_value = body.get("quality").cloned().unwrap_or(Value::Null);
    let dt_value = body.get("download_type").cloned().unwrap_or(Value::Null);
    if !truthy(&url_value) || !truthy(&quality_value) || !truthy(&dt_value) {
        return Err(invalid("url", legacy::MISSING_REQUIRED));
    }
    let url = parse_url(py_str(&url_value).trim())?;

    // --- 2. custom_name_prefix, before anything else legacy checked ---------------------------
    let custom_name_prefix = match body.get("custom_name_prefix") {
        None | Some(Value::Null) => String::new(),
        Some(value) => py_str(value),
    };
    if has_path_escape(&custom_name_prefix) {
        return Err(invalid("custom_name_prefix", legacy::CUSTOM_NAME_PREFIX));
    }

    // --- 3. the two lenient parsers ------------------------------------------------------------
    let presets = parse_presets(&body)?;
    let overrides = parse_overrides(
        body.get("ytdl_options_overrides"),
        cfg.allow_ytdl_options_overrides,
    )?;

    // --- 4. chapter_template, subtitle_language, subtitle_mode, preset names -------------------
    let chapter_template = match body.get("chapter_template") {
        None | Some(Value::Null) => cfg.default_chapter_template().to_owned(),
        Some(value) => py_str(value),
    };
    if has_path_escape(&chapter_template) {
        return Err(invalid("chapter_template", legacy::CHAPTER_TEMPLATE));
    }

    let subtitle_language = match body.get("subtitle_language") {
        None | Some(Value::Null) => "en".to_owned(),
        Some(value) => py_str(value).trim().to_owned(),
    };
    let subtitle_language = SubtitleLang::parse(&subtitle_language)
        .map_err(|_| invalid("subtitle_language", legacy::SUBTITLE_LANGUAGE))?;

    let subtitle_mode_raw = match body.get("subtitle_mode") {
        None | Some(Value::Null) => "prefer_manual".to_owned(),
        Some(value) => py_str(value).trim().to_owned(),
    };
    let Some(subtitle_mode) = SubtitleMode::from_str_exact(&subtitle_mode_raw) else {
        return Err(invalid("subtitle_mode", legacy::subtitle_mode_message()));
    };

    for preset in &presets {
        if !known_presets.contains(preset.as_str()) {
            return Err(ApiError::new(
                ErrorCode::UnknownPreset,
                legacy::PRESETS,
                Some("ytdl_options_presets"),
            ));
        }
    }

    // --- 5. the hard-coded matrix, in legacy order ---------------------------------------------
    let download_type = py_str(&dt_value).trim().to_lowercase();
    let codec = match body.get("codec") {
        Some(value) if truthy(value) => py_str(value).trim().to_lowercase(),
        _ => "auto".to_owned(),
    };
    let format = match body.get("format") {
        Some(value) if truthy(value) => py_str(value).trim().to_lowercase(),
        _ => String::new(),
    };
    let quality = py_str(&quality_value).trim().to_lowercase();

    let accepted = legacy::validate_matrix(legacy::Tokens {
        download_type: &download_type,
        codec: &codec,
        format: &format,
        quality: &quality,
    })
    .map_err(|e| invalid(e.field, e.message))?;

    // --- 6. playlist_item_limit — legacy's LAST check ------------------------------------------
    let playlist_item_limit = match body.get("playlist_item_limit") {
        None | Some(Value::Null) => cfg.default_option_playlist_item_limit,
        Some(value) => py_int(value)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid("playlist_item_limit", legacy::PLAYLIST_ITEM_LIMIT))?,
    };

    // --- 7. auto_start — the shim's own addition, deliberately last ----------------------------
    let auto_start = match body.get("auto_start") {
        None | Some(Value::Null) => true,
        Some(value) => parse_bool("auto_start", value)
            .map_err(|_| invalid("auto_start", legacy::AUTO_START_NOT_BOOL))?,
    };

    let split_by_chapters = match body.get("split_by_chapters") {
        None | Some(Value::Null) => false,
        Some(value) => truthy(value),
    };

    let folder = parse_folder(body.get("folder"))?;

    let selection = Selection::new(
        accepted.download_type,
        accepted.codec,
        FormatId::parse(&accepted.format)
            .map_err(|e| ApiError::new(e.code(), e.to_string(), Some("format")))?,
        QualityId::parse(&accepted.quality)
            .map_err(|e| ApiError::new(e.code(), e.to_string(), Some("quality")))?,
    );

    Ok(DownloadRequest {
        url,
        selection,
        folder,
        custom_name_prefix: custom_name_prefix.into(),
        playlist_item_limit,
        auto_start,
        split_by_chapters,
        chapter_template: chapter_template.into(),
        subtitle_language,
        subtitle_mode,
        ytdl_options_presets: presets.iter().map(|p| Box::from(p.as_str())).collect(),
        ytdl_options_overrides: overrides,
        provider_hint: None,
    })
}

/// `check_interval_minutes` for `POST <p>subscribe` (§11.2.1 row 5).
///
/// Absent or `null` uses `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL`; a numeric string is accepted;
/// anything else is `check_interval_minutes must be an integer`; below 1 is
/// `check_interval_minutes must be at least 1`.
///
/// # Errors
/// A `400` carrying one of those two byte-identical strings.
pub fn parse_check_interval(cfg: &Config, body: &Map<String, Value>) -> Result<u32, ApiError> {
    let raw = body.get("check_interval_minutes");
    let minutes = match raw {
        None | Some(Value::Null) => i64::from(cfg.subscription_default_check_interval),
        Some(value) => py_int(value).ok_or_else(|| {
            invalid_field("check_interval_minutes", legacy::CHECK_INTERVAL_NOT_INT)
        })?,
    };
    if minutes < 1 {
        return Err(invalid_field(
            "check_interval_minutes",
            legacy::CHECK_INTERVAL_MIN,
        ));
    }
    u32::try_from(minutes)
        .map_err(|_| invalid_field("check_interval_minutes", legacy::CHECK_INTERVAL_NOT_INT))
}

// ---------------------------------------------------------------------------
// the five leniencies
// ---------------------------------------------------------------------------

/// `_parse_ytdl_options_presets` — the singular alias and the bare string (§11.2.1 rows 1–2).
///
/// A list has its entries `str()`ed, trimmed and **blank-dropped**, which is why
/// `["", " ", "fast"]` is a one-preset request rather than a 400.
fn parse_presets(body: &Map<String, Value>) -> Result<Vec<String>, ApiError> {
    let raw = body
        .get("ytdl_options_presets")
        .filter(|v| !v.is_null())
        .or_else(|| body.get("ytdl_options_preset").filter(|v| !v.is_null()));
    match raw {
        None => Ok(Vec::new()),
        Some(Value::Array(list)) => Ok(list
            .iter()
            .map(|v| py_str(v).trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()),
        Some(Value::String(one)) => {
            let trimmed = one.trim();
            Ok(if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![trimmed.to_owned()]
            })
        }
        Some(_) => Err(invalid("ytdl_options_presets", legacy::PRESETS_WRONG_TYPE)),
    }
}

/// `_parse_ytdl_options_overrides` — the JSON-string form and the gate (§11.2.1 row 3).
///
/// The gate fires only for a **non-empty** object, exactly as legacy's `if value and not enabled`
/// did, so `{"ytdl_options_overrides": {}}` is accepted with overrides disabled.
fn parse_overrides(raw: Option<&Value>, enabled: bool) -> Result<Map<String, Value>, ApiError> {
    let value = match raw {
        None | Some(Value::Null) => return Ok(Map::new()),
        Some(Value::String(text)) if text.is_empty() => return Ok(Map::new()),
        Some(Value::String(text)) => serde_json::from_str::<Value>(text)
            .map_err(|_| invalid("ytdl_options_overrides", legacy::OVERRIDES_INVALID_JSON))?,
        Some(other) => other.clone(),
    };
    let Value::Object(map) = value else {
        return Err(invalid(
            "ytdl_options_overrides",
            legacy::OVERRIDES_NOT_OBJECT,
        ));
    };
    if !map.is_empty() && !enabled {
        return Err(ApiError::new(
            ErrorCode::OverridesDisabled,
            legacy::OVERRIDES_DISABLED,
            Some("ytdl_options_overrides"),
        ));
    }
    Ok(map)
}

/// `folder`, which legacy handed to the queue untouched.
fn parse_folder(raw: Option<&Value>) -> Result<Option<RelDir>, ApiError> {
    let text = match raw {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text.trim(),
        Some(_) => return Err(invalid("folder", legacy::FOLDER_NOT_STRING)),
    };
    if text.is_empty() {
        return Ok(None);
    }
    RelDir::parse(text)
        .map(Some)
        .map_err(|e| ApiError::new(ErrorCode::FolderInvalid, e.to_string(), Some("folder")))
}

// ---------------------------------------------------------------------------
// Python semantics, reproduced narrowly
// ---------------------------------------------------------------------------

/// Python truthiness for a JSON value: `null`, `false`, `0`, `""`, `[]` and `{}` are falsy.
///
/// This is what `if not url or not quality or not download_type` tested, and it is why
/// `{"url": ""}` and `{"quality": 0}` both answer `missing 'url', 'download_type', or 'quality'`
/// rather than a type error.
#[must_use]
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str()` for the scalars a request body can carry.
///
/// The interesting case is `True`/`False`/`None` with a capital letter: `str(True)` is `"True"`,
/// so `{"download_type": true}` answered
/// `download_type must be one of ['audio', 'captions', 'thumbnail', 'video']` in legacy and must
/// answer the same here.
#[must_use]
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}

/// [`py_str`], trimmed and lower-cased, with a default for a falsy value.
///
/// `str(post.get("format") or "any").strip().lower()` — the `or` is what makes `""`, `0` and
/// `null` all fall through to the default.
fn py_lower(value: Option<&Value>, default: &str) -> String {
    match value {
        Some(v) if truthy(v) => py_str(v).trim().to_lowercase(),
        _ => default.to_owned(),
    }
}

/// Python's `int()` for the values a request body can carry (§11.2.1 rows 4–5).
///
/// - an integer passes through;
/// - a float **truncates** (`int(3.7) == 3`), which is why a JSON `3.7` is accepted while the
///   string `"3.7"` is not;
/// - a string is stripped and parsed, so `"5"` and `" 5 "` both work and `"x"` does not;
/// - `True`/`False` are `1`/`0`;
/// - anything else (a list, an object, `null`) raises, and the caller turns that into the legacy
///   400.
#[must_use]
pub fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// Legacy's `'..' in v or v.startswith('/') or v.startswith('\\')` on a non-empty value.
fn has_path_escape(v: &str) -> bool {
    !v.is_empty() && (v.contains("..") || v.starts_with('/') || v.starts_with('\\'))
}

/// A URL with a usable scheme.
///
/// Legacy did no scheme check at all — it handed the string to yt-dlp, which answered
/// `Unsupported URL`. The shim needs a real [`Url`] to build a [`DownloadRequest`], so a string
/// that is not one is `unsupported_url` with the same sentence §11.7 pins for an unmappable
/// resource.
fn parse_url(raw: &str) -> Result<Url, ApiError> {
    let url = Url::parse(raw).map_err(|_| {
        ApiError::new(
            ErrorCode::UnsupportedUrl,
            format!("Unsupported resource \"{raw}\""),
            Some("url"),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::new(
            ErrorCode::UnsupportedUrl,
            format!("Unsupported resource \"{raw}\""),
            Some("url"),
        ));
    }
    Ok(url)
}

/// A `400 validation_failed` carrying a legacy reason string.
fn invalid(field: &str, message: impl AsRef<str>) -> ApiError {
    ApiError::new(ErrorCode::ValidationFailed, message, Some(field))
}

/// [`invalid`] with a `&'static str` field, for the subscribe helpers.
fn invalid_field(field: &'static str, message: &str) -> ApiError {
    invalid(field, message)
}

/// Snaps a legacy selection onto an **advisory** catalog's only offering (DESIGN §6.6).
///
/// Legacy had one hard-coded format matrix and no per-provider catalog, so it accepted every
/// matrix-legal `(download_type, codec, format, quality)` for *every* URL and let the downloader
/// do whatever it could. `streamingcommunity` serves exactly one HLS rendition and advertises one
/// **advisory** `mp4`/`best` entry — the word means "the server records your choice but does not
/// honour it" — so an unmodified legacy client asking for `1080`/`any`/`h264` on an SC link would
/// be rejected by the engine's catalog check for a request legacy accepted. That is a v1 parity
/// break, and this is the WP-15 request in `docs/INTEGRATION-NOTES.md`.
///
/// The rule is narrow on purpose: only a download type the catalog **does** declare, and only
/// when that type's default format is `advisory`. A download type the catalog does not declare at
/// all keeps its `400` — SC cannot produce audio-only, and saying so is better than silently
/// handing back a video file. `v2` is untouched: a v2 client reads
/// `GET api/v2/catalog?url=` first and gets an honest error if it ignores it.
pub fn snap_to_advisory_catalog(request: &mut DownloadRequest, catalog: &FormatCatalog) {
    let Some(spec) = catalog.spec_for(request.selection.download_type) else {
        return;
    };
    let Some(default) = spec.format(&spec.default_format) else {
        return;
    };
    if !default.flags.advisory {
        return;
    }
    if spec
        .format(request.selection.format.as_str())
        .is_some_and(|f| {
            f.qualities
                .iter()
                .any(|q| &*q.id == request.selection.quality.as_str())
        })
    {
        // Already something this catalog offers; leave it exactly as asked.
        return;
    }
    let Ok(format) = FormatId::parse(&default.id) else {
        return;
    };
    let Ok(quality) = QualityId::parse(&default.default_quality) else {
        return;
    };
    tracing::debug!(
        provider = %catalog.provider,
        from = %request.selection.format,
        to = %format,
        "v1: snapped a legacy selection onto the provider's advisory catalog"
    );
    // The codec control is hidden when `codecs` is empty, so `auto` is the only honest value.
    let codec = if default.codecs.is_empty() {
        Codec::Auto
    } else {
        request.selection.codec
    };
    request.selection = Selection::new(request.selection.download_type, codec, format, quality);
}

#[cfg(test)]
// The helpers below take `Value` by value because every call site builds one with `json!` on the
// spot; threading references through fifty assertions would only make them harder to read.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::needless_pass_by_value
)]
mod tests {
    use super::*;
    use aulos_core::Codec;
    use aulos_core::config::{RawEnv, load};
    use serde_json::json;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        load(&RawEnv::from_pairs(pairs.to_vec())).unwrap()
    }

    fn presets() -> BTreeSet<Box<str>> {
        ["archive", "fast"].into_iter().map(Box::from).collect()
    }

    fn object(value: &Value) -> Map<String, Value> {
        value.as_object().expect("an object").clone()
    }

    fn migrate(value: Value) -> Map<String, Value> {
        let mut body = object(&value);
        migrate_legacy_request(&mut body);
        body
    }

    fn parse(value: Value) -> Result<DownloadRequest, ApiError> {
        parse_download_options(&cfg(&[]), &presets(), object(&value))
    }

    fn parse_with(pairs: &[(&str, &str)], value: Value) -> Result<DownloadRequest, ApiError> {
        parse_download_options(&cfg(pairs), &presets(), object(&value))
    }

    fn reason(value: Value) -> String {
        parse(value).expect_err("a 400").message.to_string()
    }

    fn base(extra: Value) -> Value {
        let mut body = object(&json!({
            "url": "https://www.youtube.com/watch?v=x",
            "download_type": "video",
            "codec": "auto",
            "format": "mp4",
            "quality": "best",
        }));
        for (k, v) in object(&extra) {
            body.insert(k, v);
        }
        Value::Object(body)
    }

    // -----------------------------------------------------------------------
    // the migration table: one case per row
    // -----------------------------------------------------------------------

    #[test]
    fn row_1_an_audio_format_keeps_its_quality() {
        let body = migrate(json!({ "format": "mp3", "quality": "320" }));
        assert_eq!(body["download_type"], "audio");
        assert_eq!(body["codec"], "auto");
        assert_eq!(body["format"], "mp3");
        assert_eq!(body["quality"], "320", "unchanged, per the table");
    }

    #[test]
    fn row_2_thumbnail_forces_jpg_and_best() {
        let body = migrate(json!({ "format": "thumbnail", "quality": "1080" }));
        assert_eq!(body["download_type"], "thumbnail");
        assert_eq!(body["format"], "jpg");
        assert_eq!(body["quality"], "best");
    }

    #[test]
    fn row_3_captions_uses_subtitle_format_or_srt() {
        let body = migrate(json!({ "format": "captions" }));
        assert_eq!(body["download_type"], "captions");
        assert_eq!(body["format"], "srt");
        assert_eq!(body["quality"], "best");

        let body = migrate(json!({ "format": "captions", "subtitle_format": "VTT" }));
        assert_eq!(body["format"], "vtt", "lower-cased, as legacy did");
    }

    #[test]
    fn row_4_best_ios_becomes_the_ios_format() {
        let body =
            migrate(json!({ "format": "any", "quality": "best_ios", "video_codec": "h264" }));
        assert_eq!(body["download_type"], "video");
        assert_eq!(body["codec"], "h264");
        assert_eq!(body["format"], "ios");
        assert_eq!(body["quality"], "best");
    }

    #[test]
    fn row_5_the_audio_pseudo_quality_becomes_m4a() {
        let body = migrate(json!({ "format": "any", "quality": "audio" }));
        assert_eq!(body["download_type"], "audio");
        assert_eq!(body["codec"], "auto");
        assert_eq!(body["format"], "m4a");
        assert_eq!(body["quality"], "best");
    }

    #[test]
    fn row_6_passes_the_format_and_quality_through() {
        let body = migrate(json!({ "format": "any", "quality": "1080", "video_codec": "vp9" }));
        assert_eq!(body["download_type"], "video");
        assert_eq!(body["codec"], "vp9");
        assert_eq!(body["format"], "any");
        assert_eq!(body["quality"], "1080");
    }

    #[test]
    fn migration_is_skipped_when_download_type_is_present() {
        let body = migrate(json!({ "download_type": "video", "format": "m4a" }));
        assert_eq!(body["format"], "m4a", "not rewritten");
        assert!(body.get("codec").is_none(), "nothing was filled in");
        // and the consequence a corpus case pins: it is a `format` 400 for video.
        assert_eq!(
            reason(json!({
                "url": "https://a.test/x", "download_type": "video",
                "format": "m4a", "quality": "best"
            })),
            "format must be one of ['any', 'ios', 'mp4'] for video"
        );
    }

    #[test]
    fn migration_defaults_an_empty_body_to_video_any_best() {
        let body = migrate(json!({}));
        assert_eq!(body["download_type"], "video");
        assert_eq!(body["format"], "any");
        assert_eq!(body["quality"], "best");
        // which is why `{}` fails on the missing url, not on the missing quality.
        assert_eq!(reason(json!({})), legacy::MISSING_REQUIRED);
    }

    // -----------------------------------------------------------------------
    // the required-field gate
    // -----------------------------------------------------------------------

    #[test]
    fn a_falsy_url_quality_or_download_type_is_the_missing_message() {
        for body in [
            json!({}),
            json!({ "url": "" }),
            // `download_type` present, so migration does not fill `quality` in — which is why
            // the corpus's `add_null_quality` sends it.
            json!({ "download_type": "video", "format": "any", "quality": null,
                    "url": "https://a.test/x" }),
            json!({ "download_type": "video", "format": "any", "quality": "",
                    "url": "https://a.test/x" }),
            json!({ "download_type": "video", "format": "any", "url": "https://a.test/x" }),
            json!({ "url": "https://a.test/x", "download_type": "", "quality": "best" }),
        ] {
            assert_eq!(reason(body.clone()), legacy::MISSING_REQUIRED, "{body}");
        }
    }

    #[test]
    fn migration_fills_a_falsy_quality_before_the_gate_sees_it() {
        // Without an explicit `download_type`, `_migrate_legacy_request` writes
        // `quality = str(post.get("quality") or "best")`, so a `null` quality never reaches the
        // required-field check. This is legacy behaviour, not a leniency of ours.
        let parsed = parse(json!({ "url": "https://a.test/x", "quality": null })).expect("ok");
        assert_eq!(parsed.selection.quality.as_str(), "best");
        assert_eq!(parsed.selection.format.as_str(), "any");
    }

    #[test]
    fn a_url_is_trimmed_like_legacy() {
        let parsed = parse(base(json!({ "url": "  https://a.test/x  " }))).expect("ok");
        assert_eq!(parsed.url.as_str(), "https://a.test/x");
    }

    // -----------------------------------------------------------------------
    // the five §11.2.1 leniencies, one case each
    // -----------------------------------------------------------------------

    #[test]
    fn the_singular_preset_key_is_an_alias() {
        let parsed = parse(base(json!({ "ytdl_options_preset": "fast" }))).expect("ok");
        assert_eq!(&*parsed.ytdl_options_presets[0], "fast");
    }

    #[test]
    fn a_bare_preset_string_is_wrapped() {
        let parsed = parse(base(json!({ "ytdl_options_presets": "archive" }))).expect("ok");
        assert_eq!(parsed.ytdl_options_presets.len(), 1);
        assert_eq!(&*parsed.ytdl_options_presets[0], "archive");
    }

    #[test]
    fn a_preset_list_drops_blanks_and_rejects_unknown_names() {
        let parsed =
            parse(base(json!({ "ytdl_options_presets": ["", "  ", "fast"] }))).expect("ok");
        assert_eq!(parsed.ytdl_options_presets.len(), 1);
        assert_eq!(
            reason(base(json!({ "ytdl_options_presets": ["nope"] }))),
            legacy::PRESETS
        );
        let err = parse(base(json!({ "ytdl_options_presets": ["nope"] }))).expect_err("400");
        assert_eq!(err.code, ErrorCode::UnknownPreset);
        assert_eq!(
            reason(base(json!({ "ytdl_options_presets": 7 }))),
            legacy::PRESETS_WRONG_TYPE
        );
    }

    #[test]
    fn overrides_accept_a_json_string_and_reject_a_broken_one() {
        let parsed = parse_with(
            &[("ALLOW_YTDL_OPTIONS_OVERRIDES", "true")],
            base(json!({ "ytdl_options_overrides": "{\"a\":1}" })),
        )
        .expect("ok");
        assert_eq!(parsed.ytdl_options_overrides["a"], 1);

        assert_eq!(
            reason(base(json!({ "ytdl_options_overrides": "{" }))),
            legacy::OVERRIDES_INVALID_JSON
        );
        assert_eq!(
            reason(base(json!({ "ytdl_options_overrides": "[1]" }))),
            legacy::OVERRIDES_NOT_OBJECT
        );
        assert_eq!(
            reason(base(json!({ "ytdl_options_overrides": 7 }))),
            legacy::OVERRIDES_NOT_OBJECT
        );
        // the gate, and the gate's one exception
        assert_eq!(
            reason(base(json!({ "ytdl_options_overrides": { "a": 1 } }))),
            legacy::OVERRIDES_DISABLED
        );
        assert_eq!(
            reason(base(json!({ "ytdl_options_overrides": "{\"a\":1}" }))),
            legacy::OVERRIDES_DISABLED,
            "a JSON string is gated too"
        );
        assert!(
            parse(base(json!({ "ytdl_options_overrides": {} }))).is_ok(),
            "an empty object was never gated"
        );
    }

    #[test]
    fn playlist_item_limit_accepts_a_numeric_string() {
        assert_eq!(
            parse(base(json!({ "playlist_item_limit": "5" })))
                .expect("ok")
                .playlist_item_limit,
            5
        );
        assert_eq!(
            parse(base(json!({ "playlist_item_limit": " 5 " })))
                .expect("ok")
                .playlist_item_limit,
            5
        );
        for bad in [json!("x"), json!("3.7"), json!([1]), json!({}), json!(-1)] {
            assert_eq!(
                reason(base(json!({ "playlist_item_limit": bad }))),
                legacy::PLAYLIST_ITEM_LIMIT,
                "{bad}"
            );
        }
        assert_eq!(
            parse(base(json!({ "playlist_item_limit": 3.7 })))
                .expect("int() truncates a float")
                .playlist_item_limit,
            3
        );
    }

    #[test]
    fn check_interval_minutes_accepts_a_numeric_string() {
        let c = cfg(&[]);
        assert_eq!(
            parse_check_interval(&c, &object(&json!({ "check_interval_minutes": "30" }))).unwrap(),
            30
        );
        assert_eq!(
            parse_check_interval(&c, &object(&json!({}))).unwrap(),
            c.subscription_default_check_interval
        );
        assert_eq!(
            parse_check_interval(&c, &object(&json!({ "check_interval_minutes": null }))).unwrap(),
            c.subscription_default_check_interval
        );
        let err = parse_check_interval(&c, &object(&json!({ "check_interval_minutes": "x" })))
            .expect_err("400");
        assert_eq!(&*err.message, legacy::CHECK_INTERVAL_NOT_INT);
        for zero in [json!(0), json!("0"), json!(-5)] {
            let err = parse_check_interval(&c, &object(&json!({ "check_interval_minutes": zero })))
                .expect_err("400");
            assert_eq!(&*err.message, legacy::CHECK_INTERVAL_MIN);
        }
    }

    // -----------------------------------------------------------------------
    // auto_start
    // -----------------------------------------------------------------------

    #[test]
    fn auto_start_accepts_the_whole_token_set_and_rejects_garbage() {
        for (raw, expected) in [
            (json!(true), true),
            (json!(false), false),
            (json!("true"), true),
            (json!("false"), false),
            (json!("1"), true),
            (json!("0"), false),
            (json!("on"), true),
            (json!("off"), false),
            (json!("TRUE"), true),
        ] {
            let parsed = parse(base(json!({ "auto_start": raw }))).expect("a legal token");
            assert_eq!(parsed.auto_start, expected, "{raw}");
        }
        assert!(
            parse(base(json!({}))).expect("ok").auto_start,
            "the default is true"
        );
        let err = parse(base(json!({ "auto_start": "maybe" }))).expect_err("400");
        assert_eq!(&*err.message, legacy::AUTO_START_NOT_BOOL);
        assert_eq!(err.field.as_deref(), Some("auto_start"));
    }

    #[test]
    fn auto_start_never_displaces_a_legacy_reason() {
        // The WP-00 probe: garbage in both fields must answer about `playlist_item_limit`,
        // because that is the reason a legacy client would have seen.
        assert_eq!(
            reason(base(
                json!({ "auto_start": "maybe", "playlist_item_limit": "nope" })
            )),
            legacy::PLAYLIST_ITEM_LIMIT
        );
    }

    // -----------------------------------------------------------------------
    // the rest of the order
    // -----------------------------------------------------------------------

    #[test]
    fn the_path_and_subtitle_checks_carry_their_strings() {
        for bad in ["../x", "/x", "\\x"] {
            assert_eq!(
                reason(base(json!({ "custom_name_prefix": bad }))),
                legacy::CUSTOM_NAME_PREFIX,
                "{bad}"
            );
            assert_eq!(
                reason(base(json!({ "chapter_template": bad }))),
                legacy::CHAPTER_TEMPLATE,
                "{bad}"
            );
        }
        for bad in ["", "-en", "en_US", &"a".repeat(36)] {
            assert_eq!(
                reason(base(json!({ "subtitle_language": bad }))),
                legacy::SUBTITLE_LANGUAGE,
                "{bad}"
            );
        }
        assert_eq!(
            reason(base(json!({ "subtitle_mode": "nope" }))),
            legacy::subtitle_mode_message()
        );
    }

    #[test]
    fn custom_name_prefix_is_checked_before_the_selection() {
        // Legacy's order: the prefix check ran before the matrix, so this answers about the
        // prefix even though the format is also wrong.
        assert_eq!(
            reason(json!({
                "url": "https://a.test/x", "download_type": "video",
                "format": "mkv", "quality": "best", "custom_name_prefix": "../x"
            })),
            legacy::CUSTOM_NAME_PREFIX
        );
    }

    #[test]
    fn the_matrix_coercions_reach_the_request() {
        let parsed = parse(json!({
            "url": "https://a.test/x", "download_type": "captions",
            "codec": "h264", "format": "srt", "quality": "1080"
        }))
        .expect("legacy coerced this silently");
        assert_eq!(parsed.selection.codec, Codec::Auto);
        assert_eq!(parsed.selection.quality.as_str(), "best");
    }

    #[test]
    fn a_non_string_scalar_is_str_ed_the_python_way() {
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(false)), "False");
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(7)), "7");
        assert_eq!(
            reason(base(json!({ "download_type": true }))),
            "download_type must be one of ['audio', 'captions', 'thumbnail', 'video']"
        );
    }

    #[test]
    fn a_folder_is_parsed_and_a_traversal_is_rejected() {
        assert!(
            parse(base(json!({ "folder": "" })))
                .expect("ok")
                .folder
                .is_none()
        );
        assert_eq!(
            parse(base(json!({ "folder": "Talks" })))
                .expect("ok")
                .folder
                .expect("some")
                .as_str(),
            "Talks"
        );
        let err = parse(base(json!({ "folder": "../etc" }))).expect_err("400");
        assert_eq!(err.code, ErrorCode::FolderInvalid);
        let err = parse(base(json!({ "folder": 7 }))).expect_err("400");
        assert_eq!(&*err.message, legacy::FOLDER_NOT_STRING);
    }

    #[test]
    fn an_unusable_scheme_is_unsupported_url() {
        let err = parse(base(json!({ "url": "magnet:?xt=1" }))).expect_err("400");
        assert_eq!(err.code, ErrorCode::UnsupportedUrl);
        assert_eq!(&*err.message, "Unsupported resource \"magnet:?xt=1\"");
        let err = parse(base(json!({ "url": "not a url" }))).expect_err("400");
        assert_eq!(err.code, ErrorCode::UnsupportedUrl);
    }

    #[test]
    fn defaults_come_from_the_configuration() {
        let c = cfg(&[
            ("DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT", "7"),
            ("OUTPUT_TEMPLATE_CHAPTER", "chapters/%(title)s.%(ext)s"),
        ]);
        let parsed = parse_download_options(&c, &presets(), object(&base(json!({})))).expect("ok");
        assert_eq!(parsed.playlist_item_limit, 7);
        assert_eq!(&*parsed.chapter_template, "chapters/%(title)s.%(ext)s");
        assert_eq!(parsed.subtitle_language.as_str(), "en");
        assert_eq!(parsed.subtitle_mode, SubtitleMode::PreferManual);
    }

    #[test]
    fn python_truthiness_and_int_match_the_interpreter() {
        assert!(!truthy(&json!(null)) && !truthy(&json!(0)) && !truthy(&json!("")));
        assert!(!truthy(&json!([])) && !truthy(&json!({})) && !truthy(&json!(false)));
        assert!(truthy(&json!("0")) && truthy(&json!(1)) && truthy(&json!([0])));
        assert_eq!(py_int(&json!(5)), Some(5));
        assert_eq!(py_int(&json!("  5 ")), Some(5));
        assert_eq!(py_int(&json!(3.7)), Some(3));
        assert_eq!(py_int(&json!(true)), Some(1));
        assert_eq!(py_int(&json!("3.7")), None);
        assert_eq!(py_int(&json!(null)), None);
    }
}
