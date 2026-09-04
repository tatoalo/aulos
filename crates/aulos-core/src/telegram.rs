//! Per-chat Telegram defaults (DESIGN §12.2, §12.3, §7.6.5).
//!
//! [`ChatConfig`] is an `aulos-core` type because `aulos-store` persists it in `telegram_chats`
//! and `aulos-store` depends only on `aulos-core` (DESIGN §3).

use serde::{Deserialize, Serialize};

use crate::selection::{Codec, DownloadType, FormatId, QualityId, Selection};

/// One chat's stored defaults.
///
/// The field set is legacy's `_get_chat_config` defaults verbatim (`app/telegram_bot.py:188-201`),
/// including the fact that it stores the **flat legacy `format`/`quality` pair** rather than a
/// normalised [`Selection`]: the `cfg:` callback grammar is keyed by that flat pair, and the pair
/// is normalised on read through [`normalize_download_selection`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatConfig {
    /// The flat legacy format id: one of the nine keyboard buttons, or a catalog id after import.
    pub format: Box<str>,
    /// The flat legacy quality id, including the `audio` and `best_ios` pseudo-qualities.
    pub quality: Box<str>,
    /// The stored download type. Ignored when `format` implies one.
    pub download_type: Box<str>,
    /// The stored codec.
    pub codec: Box<str>,
    /// Subtitle language tag.
    pub subtitle_language: Box<str>,
    /// Subtitle mode.
    pub subtitle_mode: Box<str>,
    /// `""` for the base dir.
    pub folder: Box<str>,
    /// Prepended to the output name.
    pub custom_name_prefix: Box<str>,
    /// `0` = unlimited. Defaults to `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT`.
    pub playlist_item_limit: u32,
    /// Whether queued jobs start immediately.
    pub auto_start: bool,
    /// Write one file per chapter.
    pub split_by_chapters: bool,
    /// The chapter output template. Defaults to `OUTPUT_TEMPLATE_CHAPTER`.
    pub chapter_template: Box<str>,
}

impl ChatConfig {
    /// The legacy defaults, with the two config-derived values passed in.
    ///
    /// Legacy read `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `OUTPUT_TEMPLATE_CHAPTER` from its
    /// `Config`; `aulos-core` does not reach into [`crate::config::Config`] here so the caller
    /// stays the single source of truth.
    #[must_use]
    pub fn legacy_defaults(playlist_item_limit: u32, chapter_template: &str) -> Self {
        Self {
            format: "mp4".into(),
            quality: "best".into(),
            download_type: "video".into(),
            codec: "auto".into(),
            subtitle_language: "en".into(),
            subtitle_mode: "prefer_manual".into(),
            folder: "".into(),
            custom_name_prefix: "".into(),
            playlist_item_limit,
            auto_start: true,
            split_by_chapters: false,
            chapter_template: chapter_template.into(),
        }
    }

    /// The normalised selection this config asks for.
    #[must_use]
    pub fn selection(&self) -> Selection {
        normalize_download_selection(
            &self.format,
            &self.quality,
            &self.download_type,
            &self.codec,
        )
    }
}

impl Default for ChatConfig {
    /// The legacy defaults with `playlist_item_limit = 0` and an empty `chapter_template`.
    fn default() -> Self {
        Self::legacy_defaults(0, "")
    }
}

/// Maps the flat legacy `format`/`quality` pair onto a real [`Selection`].
///
/// A line-by-line port of legacy `_normalize_download_selection`
/// (`app/telegram_bot.py:212-231`), used by the bot (DESIGN §12.3 step 4) **and** by the
/// `telegram_bot_config.json` importer (DESIGN §7.6.5), so a stored `{"format":"m4a"}` becomes
/// `download_type = audio` in exactly one place.
///
/// | Input | Result |
/// |---|---|
/// | `format ∈ {m4a, mp3, opus, wav, flac}` | `(audio, auto, format, quality)` |
/// | `format == thumbnail` | `(thumbnail, auto, jpg, best)` |
/// | `format == captions` | `(captions, auto, srt, best)` |
/// | `quality == audio` | `(audio, auto, m4a, best)` |
/// | `quality == best_ios` | `(video, auto, ios, best)` |
/// | otherwise | the stored `download_type`/`codec` with the given format and quality |
///
/// Every input is lower-cased and trimmed first, and an empty value falls back to the legacy
/// default (`mp4` / `best` / `video` / `auto`). An unparseable id also falls back, because this is
/// a *normalisation* port with no error channel — the resulting selection is then validated
/// against the catalog like any other.
#[must_use]
pub fn normalize_download_selection(
    format: &str,
    quality: &str,
    download_type: &str,
    codec: &str,
) -> Selection {
    let fmt = clean(format, "mp4");
    let qual = clean(quality, "best");

    if matches!(fmt.as_str(), "m4a" | "mp3" | "opus" | "wav" | "flac") {
        return build(DownloadType::Audio, Codec::Auto, &fmt, &qual);
    }
    if fmt == "thumbnail" {
        return build(DownloadType::Thumbnail, Codec::Auto, "jpg", "best");
    }
    if fmt == "captions" {
        return build(DownloadType::Captions, Codec::Auto, "srt", "best");
    }
    if qual == "audio" {
        return build(DownloadType::Audio, Codec::Auto, "m4a", "best");
    }
    if qual == "best_ios" {
        return build(DownloadType::Video, Codec::Auto, "ios", "best");
    }

    let dt =
        DownloadType::from_str_exact(&clean(download_type, "video")).unwrap_or(DownloadType::Video);
    let cd = Codec::from_str_exact(&clean(codec, "auto")).unwrap_or(Codec::Auto);
    build(dt, cd, &fmt, &qual)
}

/// `str(value or default).strip().lower()`.
fn clean(value: &str, default: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default.to_ascii_lowercase()
    } else {
        trimmed.to_ascii_lowercase()
    }
}

/// Builds a selection, falling back to the legacy defaults for an unparseable id.
fn build(dt: DownloadType, codec: Codec, format: &str, quality: &str) -> Selection {
    let format = FormatId::parse(format).unwrap_or_else(|_| {
        FormatId::parse("mp4").unwrap_or_else(|_| unreachable!("mp4 is valid"))
    });
    let quality = QualityId::parse(quality).unwrap_or_else(|_| {
        QualityId::parse("best").unwrap_or_else(|_| unreachable!("best is valid"))
    });
    Selection::new(dt, codec, format, quality)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn norm(format: &str, quality: &str) -> (DownloadType, Codec, String, String) {
        let s = normalize_download_selection(format, quality, "video", "auto");
        (
            s.download_type,
            s.codec,
            s.format.as_str().to_owned(),
            s.quality.as_str().to_owned(),
        )
    }

    #[test]
    fn audio_formats_imply_the_audio_download_type() {
        for f in ["m4a", "mp3", "opus", "wav", "flac"] {
            let (dt, codec, fmt, q) = norm(f, "192");
            assert_eq!(dt, DownloadType::Audio, "{f}");
            assert_eq!(codec, Codec::Auto);
            assert_eq!(fmt, f);
            assert_eq!(q, "192", "the quality passes through");
        }
    }

    #[test]
    fn thumbnail_and_captions_map_to_their_catalog_ids() {
        assert_eq!(
            norm("thumbnail", "1080"),
            (
                DownloadType::Thumbnail,
                Codec::Auto,
                "jpg".to_owned(),
                "best".to_owned()
            )
        );
        assert_eq!(
            norm("captions", "1080"),
            (
                DownloadType::Captions,
                Codec::Auto,
                "srt".to_owned(),
                "best".to_owned()
            )
        );
    }

    #[test]
    fn the_two_pseudo_qualities_are_honoured() {
        assert_eq!(
            norm("any", "audio"),
            (
                DownloadType::Audio,
                Codec::Auto,
                "m4a".to_owned(),
                "best".to_owned()
            )
        );
        assert_eq!(
            norm("any", "best_ios"),
            (
                DownloadType::Video,
                Codec::Auto,
                "ios".to_owned(),
                "best".to_owned()
            )
        );
    }

    #[test]
    fn anything_else_passes_through_with_the_stored_type_and_codec() {
        let s = normalize_download_selection("MP4 ", " 1080", "video", "h265");
        assert_eq!(s.download_type, DownloadType::Video);
        assert_eq!(s.codec, Codec::H265);
        assert_eq!(s.format.as_str(), "mp4", "trimmed and lower-cased");
        assert_eq!(s.quality.as_str(), "1080");
    }

    #[test]
    fn empty_values_fall_back_to_the_legacy_defaults() {
        let s = normalize_download_selection("", "", "", "");
        assert_eq!(s.download_type, DownloadType::Video);
        assert_eq!(s.codec, Codec::Auto);
        assert_eq!(s.format.as_str(), "mp4");
        assert_eq!(s.quality.as_str(), "best");
    }

    #[test]
    fn an_unknown_download_type_or_codec_falls_back_rather_than_failing() {
        let s = normalize_download_selection("mp4", "best", "movie", "h266");
        assert_eq!(s.download_type, DownloadType::Video);
        assert_eq!(s.codec, Codec::Auto);
    }

    #[test]
    fn the_legacy_defaults_are_the_documented_twelve_keys() {
        let c = ChatConfig::legacy_defaults(
            0,
            "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
        );
        let v = serde_json::to_value(&c).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 12, "legacy _get_chat_config wrote twelve keys");
        for k in [
            "format",
            "quality",
            "download_type",
            "codec",
            "subtitle_language",
            "subtitle_mode",
            "folder",
            "custom_name_prefix",
            "playlist_item_limit",
            "auto_start",
            "split_by_chapters",
            "chapter_template",
        ] {
            assert!(obj.contains_key(k), "{k} missing");
        }
        assert_eq!(obj["format"], "mp4");
        assert_eq!(obj["quality"], "best");
        assert_eq!(c.selection().download_type, DownloadType::Video);
    }

    #[test]
    fn an_imported_captions_config_keeps_working() {
        // DESIGN §12.2: the keyboard cannot reach captions, but an imported chat config can.
        let c = ChatConfig {
            format: "captions".into(),
            ..ChatConfig::default()
        };
        let s = c.selection();
        assert_eq!(s.download_type, DownloadType::Captions);
        assert_eq!(s.format.as_str(), "srt");
        assert_eq!(s.quality.as_str(), "best");
    }

    #[test]
    fn unknown_keys_are_dropped_on_deserialisation() {
        // DESIGN §7.6.5: unknown keys are dropped with a DEBUG log, never rejected.
        let raw = r#"{"format":"m4a","quality":"192","download_type":"audio","codec":"auto",
                      "subtitle_language":"en","subtitle_mode":"prefer_manual","folder":"",
                      "custom_name_prefix":"","playlist_item_limit":0,"auto_start":true,
                      "split_by_chapters":false,"chapter_template":"","legacy_junk":1}"#;
        let c: ChatConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(c.selection().download_type, DownloadType::Audio);
    }
}
