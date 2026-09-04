//! `get_opts`: the yt-dlp option dict, a literal port of legacy `app/dl_formats.py`
//! (DESIGN §9.8, Appendix A.6).
//!
//! The caller's options are the *already layered* dict of DESIGN §17.2
//! (`YTDL_OPTIONS` → `YTDL_OPTIONS_FILE` → runtime overrides → presets → per-request overrides,
//! i.e. [`aulos_core::YtdlOptions::layer`]). This function adds the type-derived keys **on top**,
//! which is why a preset cannot break `writethumbnail`, `skip_download` or the postprocessor
//! chain.
//!
//! # Layering, exactly
//!
//! `postprocessors` is assembled as `[derived] + [caller] + [late]` and then **replaces** the
//! caller's key, which is the legacy statement
//! `opts["postprocessors"] = postprocessors + opts.get("postprocessors", []) + late`.
//!
//! The `[late]` list is where legacy appended
//! `{"key": "Exec", "exec_cmd": "python3 /app/app/audio_sync_fix.py %(filepath)q"}` for
//! `{video, mp4, best_remux}`. **It is omitted here** — the one deliberate behaviour change of
//! DESIGN §9.8 (Δ C9): the re-encode is the in-process `audio_sync` completion hook of §13.3, so
//! the shim never spawns a grandchild for it. The list is kept in the code as an explicit empty
//! stage rather than deleted, because it is the seam the hook replaced and the golden corpus
//! diff is expressed against it.
//!
//! # Preserved quirks
//!
//! - `preferredquality` is `0` (a **number**) for `quality == "best"` and the raw quality
//!   **string** otherwise — `"192"`, never `192`. yt-dlp accepts both; the corpus records the
//!   string, so the string is what we emit.
//! - The audio thumbnail trio (`FFmpegThumbnailsConvertor` + `FFmpegMetadata` +
//!   `EmbedThumbnail`) is skipped entirely when the caller already has a `writethumbnail` key,
//!   **whatever its value** — `writethumbnail: false` suppresses the trio, and so does
//!   `writethumbnail: true`.
//! - `quality` is **not** normalised. Legacy applied `or`/`strip`/`lower` to `download_type` and
//!   `format` only, and compared `quality` raw, so `"Best"` is not `"best"` here.
//! - The captions branch rewrites `txt` to `srt` for `subtitlesformat`: the `.srt → .txt`
//!   conversion happens in the shim afterwards (DESIGN §9.2 `policy.convert_srt_to_txt`).

use aulos_core::request::{SubtitleLang, SubtitleMode};
use aulos_core::selection::DownloadType;
use serde_json::{Map, Value, json};

use crate::formats::CAPTION_MODES;

/// The legacy `Exec` postprocessor this port deliberately does not emit (DESIGN §9.8, Δ C9).
///
/// Public so the golden-corpus test can assert that the *only* difference between the legacy
/// dict and ours is this exact entry, rather than trusting a hand-edited expectation.
#[must_use]
pub fn legacy_audio_sync_exec() -> Value {
    json!({
        "key": "Exec",
        "exec_cmd": "python3 /app/app/audio_sync_fix.py %(filepath)q",
    })
}

/// Legacy `_normalize_caption_mode`: strip, then fall back to `prefer_manual`.
#[must_use]
pub fn normalize_caption_mode(mode: &str) -> &str {
    let mode = mode.trim();
    if CAPTION_MODES.contains(&mode) {
        mode
    } else {
        "prefer_manual"
    }
}

/// Legacy `_normalize_subtitle_language`: strip, then fall back to `en`.
#[must_use]
pub fn normalize_subtitle_language(language: &str) -> &str {
    let language = language.trim();
    if language.is_empty() { "en" } else { language }
}

/// The extra yt-dlp options and postprocessors for a validated selection (DESIGN §9.8).
///
/// `user` is consumed: it plays the part of legacy's `copy.deepcopy(ytdl_opts)`, so the caller
/// hands over a clone and its own dict is untouched.
#[must_use]
pub fn get_opts(
    dt: DownloadType,
    format: &str,
    quality: &str,
    user: Map<String, Value>,
    subtitle_language: &SubtitleLang,
    subtitle_mode: SubtitleMode,
) -> Map<String, Value> {
    get_opts_raw(
        Some(dt.as_str()),
        Some(format),
        quality,
        user,
        subtitle_language.as_str(),
        subtitle_mode.as_str(),
    )
}

/// The string-in form: the literal legacy signature, for the v1 shim and the golden corpus.
///
/// `download_type` and `format` get the `or`-default / `strip` / `lower` treatment; `quality`,
/// `subtitle_language` and `subtitle_mode` are passed to the same normalisers legacy used, which
/// for `quality` means none at all.
#[must_use]
pub fn get_opts_raw(
    download_type: Option<&str>,
    format: Option<&str>,
    quality: &str,
    user: Map<String, Value>,
    subtitle_language: &str,
    subtitle_mode: &str,
) -> Map<String, Value> {
    let download_type = norm(download_type, "video");
    let format = norm(format, "any");
    let mut opts = user;
    let mut derived: Vec<Value> = Vec::new();

    if download_type == "audio" {
        derived.push(json!({
            "key": "FFmpegExtractAudio",
            "preferredcodec": format,
            "preferredquality": if quality == "best" {
                Value::from(0)
            } else {
                Value::String(quality.to_owned())
            },
        }));

        if format != "wav" && !opts.contains_key("writethumbnail") {
            opts.insert("writethumbnail".to_owned(), Value::Bool(true));
            derived.push(thumbnails_convertor());
            derived.push(json!({ "key": "FFmpegMetadata" }));
            derived.push(json!({ "key": "EmbedThumbnail" }));
        }
    }

    if download_type == "thumbnail" {
        opts.insert("skip_download".to_owned(), Value::Bool(true));
        opts.insert("writethumbnail".to_owned(), Value::Bool(true));
        derived.push(thumbnails_convertor());
    }

    // The legacy `late_postprocessors` stage. Always empty: see the module docs (Δ C9).
    let late: Vec<Value> = Vec::new();

    if download_type == "video" && format == "mp4" && quality == "best_remux" {
        // Remove any caller `format` so it cannot override `formats::get_format`.
        opts.remove("format");
        opts.insert(
            "merge_output_format".to_owned(),
            Value::String("mp4".to_owned()),
        );
        derived.push(json!({
            "key": "FFmpegVideoConvertor",
            "preferedformat": "mp4",
        }));
    }

    if download_type == "captions" {
        captions(&mut opts, &format, subtitle_language, subtitle_mode);
    }

    let caller = caller_postprocessors(&mut opts);
    derived.extend(caller);
    derived.extend(late);
    opts.insert("postprocessors".to_owned(), Value::Array(derived));
    opts
}

/// The `FFmpegThumbnailsConvertor` entry, byte-identical in both branches that emit it.
fn thumbnails_convertor() -> Value {
    json!({ "key": "FFmpegThumbnailsConvertor", "format": "jpg", "when": "before_dl" })
}

/// Takes the caller's `postprocessors` out of `opts` and returns it as a list.
///
/// A non-array value is dropped with a warning. Legacy would have raised `TypeError` on
/// `list + str`, failing the whole add; a bad `YTDL_OPTIONS` key is not worth losing the download
/// over, and the warning names the key so the operator can fix it.
fn caller_postprocessors(opts: &mut Map<String, Value>) -> Vec<Value> {
    match opts.remove("postprocessors") {
        None => Vec::new(),
        Some(Value::Array(list)) => list,
        Some(other) => {
            tracing::warn!(
                value = %other,
                "YTDL_OPTIONS `postprocessors` is not a list; ignoring it"
            );
            Vec::new()
        }
    }
}

/// The `download_type == "captions"` branch: `skip_download`, `subtitlesformat` and the per-mode
/// `subtitleslangs` ordering.
///
/// The ordering is the whole point of the branch and is deliberately asymmetric:
/// `prefer_manual` asks for `[lang, lang-orig]` and every mode that wants automatic captions asks
/// for `[lang-orig, lang]`, because `-orig` is what YouTube tags an auto-generated track with and
/// yt-dlp takes the first match.
fn captions(
    opts: &mut Map<String, Value>,
    format: &str,
    subtitle_language: &str,
    subtitle_mode: &str,
) {
    let mode = normalize_caption_mode(subtitle_mode);
    let language = normalize_subtitle_language(subtitle_language);
    let orig = format!("{language}-orig");

    opts.insert("skip_download".to_owned(), Value::Bool(true));
    // `txt` is downloaded as `srt` and converted afterwards.
    let subtitle_format = if format == "txt" { "srt" } else { format };
    opts.insert(
        "subtitlesformat".to_owned(),
        Value::String(subtitle_format.to_owned()),
    );

    let (manual, automatic, langs): (bool, bool, Vec<&str>) = match mode {
        "manual_only" => (true, false, vec![language]),
        "auto_only" => (false, true, vec![&orig, language]),
        "prefer_auto" => (true, true, vec![&orig, language]),
        // `prefer_manual`, and anything `normalize_caption_mode` mapped onto it.
        _ => (true, true, vec![language, &orig]),
    };
    opts.insert("writesubtitles".to_owned(), Value::Bool(manual));
    opts.insert("writeautomaticsub".to_owned(), Value::Bool(automatic));
    opts.insert(
        "subtitleslangs".to_owned(),
        Value::Array(
            langs
                .into_iter()
                .map(|l| Value::String(l.to_owned()))
                .collect(),
        ),
    );
}

/// Legacy's `(value or default).strip().lower()`. See [`crate::formats`] for why the order
/// matters.
fn norm(value: Option<&str>, default: &'static str) -> String {
    let raw = value.unwrap_or("");
    let raw = if raw.is_empty() { default } else { raw };
    raw.trim().to_lowercase()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn pps(opts: &Map<String, Value>) -> Vec<String> {
        opts["postprocessors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["key"].as_str().unwrap_or("?").to_owned())
            .collect()
    }

    fn empty() -> Map<String, Value> {
        Map::new()
    }

    #[test]
    fn a_user_format_is_popped_for_best_remux() {
        let mut user = empty();
        user.insert("format".to_owned(), json!("bestvideo[height<=480]"));
        let out = get_opts_raw(
            Some("video"),
            Some("mp4"),
            "best_remux",
            user,
            "en",
            "prefer_manual",
        );
        assert!(!out.contains_key("format"));
        assert_eq!(out["merge_output_format"], json!("mp4"));
        assert_eq!(pps(&out), ["FFmpegVideoConvertor"]);
    }

    #[test]
    fn the_late_exec_postprocessor_is_not_emitted() {
        let out = get_opts_raw(
            Some("video"),
            Some("mp4"),
            "best_remux",
            empty(),
            "en",
            "prefer_manual",
        );
        let list = out["postprocessors"].as_array().unwrap();
        assert!(
            !list.contains(&legacy_audio_sync_exec()),
            "the audio_sync Exec step is the in-process hook now (DESIGN §9.8, Δ C9)"
        );
    }

    #[test]
    fn a_user_writethumbnail_suppresses_the_injected_trio() {
        for value in [json!(false), json!(true)] {
            let mut user = empty();
            user.insert("writethumbnail".to_owned(), value.clone());
            let out = get_opts_raw(Some("audio"), Some("m4a"), "best", user, "en", "x");
            assert_eq!(pps(&out), ["FFmpegExtractAudio"]);
            assert_eq!(out["writethumbnail"], value);
        }
    }

    #[test]
    fn a_user_postprocessor_list_sits_between_derived_and_late() {
        let mut user = empty();
        user.insert(
            "postprocessors".to_owned(),
            json!([{ "key": "SponsorBlock" }]),
        );
        let out = get_opts_raw(Some("audio"), Some("mp3"), "192", user, "en", "x");
        assert_eq!(
            pps(&out),
            [
                "FFmpegExtractAudio",
                "FFmpegThumbnailsConvertor",
                "FFmpegMetadata",
                "EmbedThumbnail",
                "SponsorBlock",
            ]
        );
    }

    #[test]
    fn a_non_list_postprocessors_value_is_dropped_not_fatal() {
        let mut user = empty();
        user.insert("postprocessors".to_owned(), json!("SponsorBlock"));
        let out = get_opts_raw(Some("video"), Some("any"), "best", user, "en", "x");
        assert_eq!(out["postprocessors"], json!([]));
    }

    #[test]
    fn preferredquality_is_zero_for_best_and_a_string_otherwise() {
        let best = get_opts_raw(Some("audio"), Some("mp3"), "best", empty(), "en", "x");
        assert_eq!(best["postprocessors"][0]["preferredquality"], json!(0));
        let k320 = get_opts_raw(Some("audio"), Some("mp3"), "320", empty(), "en", "x");
        assert_eq!(k320["postprocessors"][0]["preferredquality"], json!("320"));
    }

    #[test]
    fn wav_has_no_thumbnail_trio() {
        let out = get_opts_raw(Some("audio"), Some("wav"), "best", empty(), "en", "x");
        assert_eq!(pps(&out), ["FFmpegExtractAudio"]);
        assert!(!out.contains_key("writethumbnail"));
    }

    #[test]
    fn the_caption_normalisers_match_legacy() {
        assert_eq!(normalize_caption_mode("  auto_only "), "auto_only");
        assert_eq!(normalize_caption_mode("nonsense"), "prefer_manual");
        assert_eq!(normalize_caption_mode(""), "prefer_manual");
        assert_eq!(normalize_subtitle_language("  pt-BR  "), "pt-BR");
        assert_eq!(normalize_subtitle_language("   "), "en");
    }

    #[test]
    fn the_typed_and_raw_entry_points_agree() {
        let typed = get_opts(
            DownloadType::Captions,
            "txt",
            "best",
            empty(),
            &SubtitleLang::english(),
            SubtitleMode::PreferAuto,
        );
        let raw = get_opts_raw(
            Some("captions"),
            Some("txt"),
            "best",
            empty(),
            "en",
            "prefer_auto",
        );
        assert_eq!(typed, raw);
        assert_eq!(typed["subtitlesformat"], json!("srt"));
        assert_eq!(typed["subtitleslangs"], json!(["en-orig", "en"]));
    }
}
