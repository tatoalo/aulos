//! The add request, its wire echo, and catalog-driven validation (DESIGN §4.3, §4.6.1, §6.6).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use url::Url;

use crate::catalog::{FormatCatalog, SUBTITLE_LANGUAGE_PATTERN};
use crate::error::{ErrorCode, REDACTED, WireError, is_secret_key};
use crate::paths::RelDir;
use crate::selection::{Codec, DownloadType, ProviderId, Selection};

/// Which subtitle track to prefer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode {
    /// Only automatic captions.
    AutoOnly,
    /// Only human-authored captions.
    ManualOnly,
    /// Human-authored if present, else automatic.
    #[default]
    PreferManual,
    /// Automatic if present, else human-authored.
    PreferAuto,
}

impl SubtitleMode {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoOnly => "auto_only",
            Self::ManualOnly => "manual_only",
            Self::PreferManual => "prefer_manual",
            Self::PreferAuto => "prefer_auto",
        }
    }

    /// Every value, in the order legacy's `VALID_SUBTITLE_MODES` sorts to.
    pub const ALL: [Self; 4] = [
        Self::AutoOnly,
        Self::ManualOnly,
        Self::PreferAuto,
        Self::PreferManual,
    ];

    /// Parses a wire string.
    #[must_use]
    pub fn from_str_exact(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

impl fmt::Display for SubtitleMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A subtitle language tag, matching [`SUBTITLE_LANGUAGE_PATTERN`].
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SubtitleLang(Arc<str>);

impl SubtitleLang {
    /// The legacy default, `"en"`.
    #[must_use]
    pub fn english() -> Self {
        Self("en".into())
    }

    /// Validates and wraps a language tag.
    ///
    /// # Errors
    /// [`RequestError::SubtitleLanguage`] when the value does not match
    /// `^[A-Za-z0-9][A-Za-z0-9-]{0,34}$`.
    pub fn parse(s: &str) -> Result<Self, RequestError> {
        if Self::is_valid(s) {
            Ok(Self(s.into()))
        } else {
            Err(RequestError::SubtitleLanguage(s.into()))
        }
    }

    /// Whether `s` matches `^[A-Za-z0-9][A-Za-z0-9-]{0,34}$`.
    #[must_use]
    pub fn is_valid(s: &str) -> bool {
        let bytes = s.as_bytes();
        (1..=35).contains(&bytes.len())
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    }

    /// The tag as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The tag as a shared string, for cheap cloning into a [`RequestView`].
    #[must_use]
    pub fn as_arc(&self) -> Arc<str> {
        Arc::clone(&self.0)
    }
}

impl Default for SubtitleLang {
    fn default() -> Self {
        Self::english()
    }
}

impl fmt::Display for SubtitleLang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for SubtitleLang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SubtitleLang({:?})", &*self.0)
    }
}

impl std::str::FromStr for SubtitleLang {
    type Err = RequestError;
    fn from_str(s: &str) -> Result<Self, RequestError> {
        Self::parse(s)
    }
}

impl<'de> Deserialize<'de> for SubtitleLang {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&raw)
            .map_err(|_| DeError::invalid_value(Unexpected::Str(&raw), &SUBTITLE_LANGUAGE_PATTERN))
    }
}

/// Everything one add request asks for (DESIGN §4.3).
///
/// Written once at insert and never mutated, which is what lets the delta diff of DESIGN §15.1
/// guarantee `selection`, `folder` and `request` never appear in a `delta`.
///
/// `PartialEq` but not `Eq`: `serde_json::Value` has no total equality (it can hold a float).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadRequest {
    /// The source page URL. Data, not a key.
    pub url: Url,
    /// What to download and in what shape.
    pub selection: Selection,
    /// A validated, containment-checked custom directory, or `None` for the base dir.
    pub folder: Option<RelDir>,
    /// Prepended to the output name. No `..`, no leading separator.
    pub custom_name_prefix: Box<str>,
    /// `0` = unlimited.
    pub playlist_item_limit: u32,
    /// Whether the item should be scheduled immediately.
    pub auto_start: bool,
    /// Write one file per chapter.
    pub split_by_chapters: bool,
    /// The chapter output template. Defaults to `OUTPUT_TEMPLATE_CHAPTER`.
    pub chapter_template: Box<str>,
    /// The subtitle language tag.
    pub subtitle_language: SubtitleLang,
    /// Which subtitle track to prefer.
    pub subtitle_mode: SubtitleMode,
    /// Named `YTDL_OPTIONS_PRESETS` entries, applied in this order.
    pub ytdl_options_presets: Vec<Box<str>>,
    /// Per-request yt-dlp option overrides. Gated by `ALLOW_YTDL_OPTIONS_OVERRIDES`.
    pub ytdl_options_overrides: Map<String, Value>,
    /// Forces a provider. `None` lets the registry decide.
    pub provider_hint: Option<ProviderId>,
}

impl DownloadRequest {
    /// A request with every legacy default, for `url` and `selection`.
    ///
    /// `chapter_template` is left empty; the API layer substitutes `OUTPUT_TEMPLATE_CHAPTER` from
    /// the effective config, which `aulos-core` deliberately does not read here.
    #[must_use]
    pub fn new(url: Url, selection: Selection) -> Self {
        Self {
            url,
            selection,
            folder: None,
            custom_name_prefix: "".into(),
            playlist_item_limit: 0,
            auto_start: true,
            split_by_chapters: false,
            chapter_template: "".into(),
            subtitle_language: SubtitleLang::english(),
            subtitle_mode: SubtitleMode::PreferManual,
            ytdl_options_presets: Vec::new(),
            ytdl_options_overrides: Map::new(),
            provider_hint: None,
        }
    }

    /// The wire echo of the eight non-`selection`, non-`folder` request fields (DESIGN §4.6.1).
    ///
    /// Override **values** whose key looks like a secret are replaced with `«redacted»`; the key
    /// set is preserved so a client can still say "3 overrides" and show which ones.
    #[must_use]
    pub fn to_view(&self) -> RequestView {
        let mut overrides = Map::with_capacity(self.ytdl_options_overrides.len());
        for (k, v) in &self.ytdl_options_overrides {
            if is_secret_key(k) {
                overrides.insert(k.clone(), Value::String(REDACTED.to_owned()));
            } else {
                overrides.insert(k.clone(), v.clone());
            }
        }

        RequestView {
            custom_name_prefix: Arc::from(&*self.custom_name_prefix),
            playlist_item_limit: self.playlist_item_limit,
            auto_start: self.auto_start,
            split_by_chapters: self.split_by_chapters,
            chapter_template: Arc::from(&*self.chapter_template),
            subtitle_language: self.subtitle_language.as_arc(),
            subtitle_mode: self.subtitle_mode,
            ytdl_options_presets: self
                .ytdl_options_presets
                .iter()
                .map(|p| Arc::from(&**p))
                .collect(),
            ytdl_options_overrides: Arc::new(overrides),
        }
    }

    /// Validates this request against a provider catalog, collecting **every** failure.
    ///
    /// The messages are byte-identical to the legacy ones (`app/main.py parse_download_options`)
    /// but the *allowed sets* are read out of `catalog`, so a plugin gets validation for free.
    /// Call [`Selection::coerce_legacy`] first if you want legacy's silent coercions.
    ///
    /// # Errors
    /// One [`WireError`] per failing field, each with `field` set, in legacy check order.
    pub fn validate(
        &self,
        catalog: &FormatCatalog,
        known_presets: &BTreeSet<Box<str>>,
    ) -> Result<(), Vec<WireError>> {
        let mut errs: Vec<WireError> = Vec::new();

        if has_path_escape(&self.custom_name_prefix) {
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "custom_name_prefix",
                legacy::CUSTOM_NAME_PREFIX,
            ));
        }
        if has_path_escape(&self.chapter_template) {
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "chapter_template",
                legacy::CHAPTER_TEMPLATE,
            ));
        }
        if !SubtitleLang::is_valid(self.subtitle_language.as_str()) {
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "subtitle_language",
                legacy::SUBTITLE_LANGUAGE,
            ));
        }
        for preset in &self.ytdl_options_presets {
            if !known_presets.contains(preset) {
                errs.push(WireError::field(
                    ErrorCode::UnknownPreset,
                    "ytdl_options_presets",
                    legacy::PRESETS,
                ));
                break;
            }
        }

        let sel = &self.selection;
        let Some(dt) = catalog.spec_for(sel.download_type) else {
            let allowed: Vec<&str> = catalog.download_types.iter().map(|d| &*d.id).collect();
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "download_type",
                legacy::download_type_must_be_one_of(&allowed),
            ));
            return Err(errs);
        };

        let Some(fmt) = dt.format(sel.format.as_str()) else {
            let allowed: Vec<&str> = dt.formats.iter().map(|f| &*f.id).collect();
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "format",
                legacy::format_must_be_one_of(&allowed, &dt.id),
            ));
            return Err(errs);
        };

        if fmt.quality(sel.quality.as_str()).is_none() {
            let allowed: Vec<&str> = fmt.qualities.iter().map(|q| &*q.id).collect();
            // Legacy phrased the video message "for video" and the audio one "for format m4a".
            let msg = if sel.download_type == DownloadType::Video {
                legacy::quality_must_be_one_of_for_type(&allowed, &dt.id)
            } else {
                legacy::quality_must_be_one_of_for_format(&allowed, &fmt.id)
            };
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "quality",
                msg,
            ));
        }

        // An empty `codecs` list means the control does not apply, so only `auto` is acceptable.
        // For the `ytdlp` catalog every video format lists all five, so this only bites a
        // provider that genuinely has no codec choice (StreamingCommunity, plugins).
        let codec_ok = if fmt.codecs.is_empty() {
            sel.codec == Codec::Auto
        } else {
            fmt.codecs.iter().any(|c| &*c.id == sel.codec.as_str())
        };
        if !codec_ok {
            let allowed: Vec<&str> = if fmt.codecs.is_empty() {
                vec!["auto"]
            } else {
                fmt.codecs.iter().map(|c| &*c.id).collect()
            };
            errs.push(WireError::field(
                ErrorCode::ValidationFailed,
                "codec",
                legacy::codec_must_be_one_of(&allowed),
            ));
        }

        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

/// Legacy's `'..' in v or v.startswith('/') or v.startswith('\\')` on a non-empty value.
fn has_path_escape(v: &str) -> bool {
    !v.is_empty() && (v.contains("..") || v.starts_with('/') || v.starts_with('\\'))
}

/// `Item.request` on the wire — the rest of the request echo (DESIGN §4.6.1, PROTOCOL §2.3).
///
/// v1 projected every request field back to the client, so echoing only `selection` would make v2
/// a strictly *narrower* payload than v1 and make "download this again with the same options"
/// impossible in a v2-only client. All nine keys are always present.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct RequestView {
    /// `""` when unset.
    pub custom_name_prefix: Arc<str>,
    /// `0` = unlimited.
    pub playlist_item_limit: u32,
    /// The value that was **requested**; `ItemView.auto_start` is the current one.
    pub auto_start: bool,
    /// Write one file per chapter.
    pub split_by_chapters: bool,
    /// The effective template, never `null`.
    pub chapter_template: Arc<str>,
    /// The subtitle language tag.
    pub subtitle_language: Arc<str>,
    /// Which subtitle track was preferred.
    pub subtitle_mode: SubtitleMode,
    /// Possibly empty, never `null`.
    pub ytdl_options_presets: Arc<[Arc<str>]>,
    /// The exact key set; secret-looking values are `"«redacted»"`.
    pub ytdl_options_overrides: Arc<Map<String, Value>>,
}

impl Selection {
    /// The coercions legacy applied silently before validating (`app/main.py:626-639`).
    ///
    /// - every non-video download type forces `codec = auto`
    /// - `captions` and `thumbnail` force `quality = best`
    ///
    /// The v1 shim and the Telegram/importer paths apply this so a legacy client keeps getting
    /// legacy behaviour; a v2 client that sends a nonsense combination gets an honest 400 instead.
    pub fn coerce_legacy(&mut self) {
        if self.download_type != DownloadType::Video {
            self.codec = Codec::Auto;
        }
        if matches!(
            self.download_type,
            DownloadType::Captions | DownloadType::Thumbnail
        ) && self.quality.as_str() != "best"
        {
            self.quality = crate::selection::QualityId::parse("best")
                .unwrap_or_else(|_| unreachable!("\"best\" is a valid quality id"));
        }
    }
}

/// The legacy validation strings, byte-identical (DESIGN §11.7).
///
/// They are here rather than inline in `aulos-api` because the v1 shim (WP-15) must emit them for
/// the *typed* fields too, where a bad value fails at deserialisation and never reaches
/// [`DownloadRequest::validate`].
pub mod legacy {
    use crate::request::SubtitleMode;
    use crate::selection::{Codec, DownloadType};

    /// `custom_name_prefix must not contain ".." or start with a path separator`
    pub const CUSTOM_NAME_PREFIX: &str =
        r#"custom_name_prefix must not contain ".." or start with a path separator"#;
    /// `chapter_template must not contain ".." or start with a path separator`
    pub const CHAPTER_TEMPLATE: &str =
        r#"chapter_template must not contain ".." or start with a path separator"#;
    /// `subtitle_language must match pattern [A-Za-z0-9-] and be at most 35 characters`
    pub const SUBTITLE_LANGUAGE: &str =
        "subtitle_language must match pattern [A-Za-z0-9-] and be at most 35 characters";
    /// `ytdl_options_presets must only contain configured preset names`
    pub const PRESETS: &str = "ytdl_options_presets must only contain configured preset names";
    /// `playlist_item_limit must be an integer`
    pub const PLAYLIST_ITEM_LIMIT: &str = "playlist_item_limit must be an integer";

    /// Renders a `sorted(set)` the way Python's `repr` does: `['a', 'b']`.
    #[must_use]
    pub fn py_list(items: &[&str]) -> String {
        let mut sorted: Vec<&str> = items.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let body = sorted
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{body}]")
    }

    /// `download_type must be one of [...]`
    #[must_use]
    pub fn download_type_must_be_one_of(allowed: &[&str]) -> String {
        format!("download_type must be one of {}", py_list(allowed))
    }

    /// `download_type must be one of ['audio', 'captions', 'thumbnail', 'video']` — the legacy
    /// `VALID_DOWNLOAD_TYPES` message, for the v1 shim's pre-check.
    #[must_use]
    pub fn download_type_legacy() -> String {
        let all: Vec<&str> = DownloadType::ALL.iter().map(|d| d.as_str()).collect();
        download_type_must_be_one_of(&all)
    }

    /// `codec must be one of [...]`
    #[must_use]
    pub fn codec_must_be_one_of(allowed: &[&str]) -> String {
        format!("codec must be one of {}", py_list(allowed))
    }

    /// `codec must be one of ['auto', 'av1', 'h264', 'h265', 'vp9']` — the legacy message.
    #[must_use]
    pub fn codec_legacy() -> String {
        let all: Vec<&str> = Codec::ALL.iter().map(|c| c.as_str()).collect();
        codec_must_be_one_of(&all)
    }

    /// `subtitle_mode must be one of ['auto_only', 'manual_only', 'prefer_auto', 'prefer_manual']`
    #[must_use]
    pub fn subtitle_mode_legacy() -> String {
        let all: Vec<&str> = SubtitleMode::ALL.iter().map(|m| m.as_str()).collect();
        format!("subtitle_mode must be one of {}", py_list(&all))
    }

    /// `format must be one of [...] for <download_type>`
    #[must_use]
    pub fn format_must_be_one_of(allowed: &[&str], download_type: &str) -> String {
        format!(
            "format must be one of {} for {download_type}",
            py_list(allowed)
        )
    }

    /// `quality must be one of [...] for <download_type>` — legacy's phrasing for `video`.
    #[must_use]
    pub fn quality_must_be_one_of_for_type(allowed: &[&str], download_type: &str) -> String {
        format!(
            "quality must be one of {} for {download_type}",
            py_list(allowed)
        )
    }

    /// `quality must be one of [...] for format <format>` — legacy's phrasing for `audio`.
    #[must_use]
    pub fn quality_must_be_one_of_for_format(allowed: &[&str], format: &str) -> String {
        format!(
            "quality must be one of {} for format {format}",
            py_list(allowed)
        )
    }
}

/// Failures constructing a request field.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// `subtitle_language` did not match the documented pattern.
    #[error("{}", crate::request::legacy::SUBTITLE_LANGUAGE)]
    SubtitleLanguage(Box<str>),
}

impl RequestError {
    /// The wire error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        ErrorCode::ValidationFailed
    }

    /// Never retryable.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::catalog::ytdlp_catalog;
    use crate::selection::{FormatId, QualityId};

    fn req(dt: DownloadType, codec: Codec, format: &str, quality: &str) -> DownloadRequest {
        DownloadRequest::new(
            Url::parse("https://example.com/v").unwrap(),
            Selection::new(
                dt,
                codec,
                FormatId::parse(format).unwrap(),
                QualityId::parse(quality).unwrap(),
            ),
        )
    }

    fn presets() -> BTreeSet<Box<str>> {
        BTreeSet::new()
    }

    #[test]
    fn subtitle_lang_matches_the_documented_pattern() {
        for ok in ["en", "en-US", "9", "a", &"a".repeat(35)] {
            assert!(SubtitleLang::is_valid(ok), "{ok} must be accepted");
        }
        for bad in ["", "-en", "en_US", "en US", &"a".repeat(36)] {
            assert!(!SubtitleLang::is_valid(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn every_legal_tuple_validates() {
        let c = ytdlp_catalog();
        let p = presets();
        let mut checked = 0;
        for dt in &c.download_types {
            let dtv = dt.download_type().unwrap();
            for f in &dt.formats {
                for q in &f.qualities {
                    let codecs: Vec<Codec> = if f.codecs.is_empty() {
                        vec![Codec::Auto]
                    } else {
                        Codec::ALL.to_vec()
                    };
                    for codec in codecs {
                        let r = req(dtv, codec, &f.id, &q.id);
                        assert!(
                            r.validate(&c, &p).is_ok(),
                            "({dtv}, {codec}, {}, {}) must validate",
                            f.id,
                            q.id
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 60, "the matrix should be broad, got {checked}");
    }

    #[test]
    fn rejected_cases_carry_the_exact_legacy_strings() {
        let c = ytdlp_catalog();
        let p = presets();

        // video / bad format
        let e = req(DownloadType::Video, Codec::Auto, "mkv", "best")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "format must be one of ['any', 'ios', 'mp4'] for video"
        );
        assert_eq!(e[0].field.as_deref(), Some("format"));

        // video / bad quality, non-mp4 (no best_remux)
        let e = req(DownloadType::Video, Codec::Auto, "any", "best_remux")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "quality must be one of ['1080', '1440', '2160', '240', '360', '480', '720', 'best', 'worst'] for video"
        );

        // video / mp4 does allow best_remux
        assert!(
            req(DownloadType::Video, Codec::Auto, "mp4", "best_remux")
                .validate(&c, &p)
                .is_ok()
        );

        // video / bad quality on mp4 lists best_remux
        let e = req(DownloadType::Video, Codec::Auto, "mp4", "144")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "quality must be one of ['1080', '1440', '2160', '240', '360', '480', '720', 'best', 'best_remux', 'worst'] for video"
        );

        // audio / bad format
        let e = req(DownloadType::Audio, Codec::Auto, "aac", "best")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "format must be one of ['flac', 'm4a', 'mp3', 'opus', 'wav'] for audio"
        );

        // audio / bad quality — legacy phrasing is "for format <format>"
        let e = req(DownloadType::Audio, Codec::Auto, "m4a", "320")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "quality must be one of ['128', '192', 'best'] for format m4a"
        );
        let e = req(DownloadType::Audio, Codec::Auto, "opus", "320")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "quality must be one of ['best'] for format opus"
        );

        // captions / bad format
        let e = req(DownloadType::Captions, Codec::Auto, "ass", "best")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "format must be one of ['dfxp', 'sbv', 'scc', 'srt', 'ttml', 'txt', 'vtt'] for captions"
        );

        // thumbnail / bad format
        let e = req(DownloadType::Thumbnail, Codec::Auto, "png", "best")
            .validate(&c, &p)
            .unwrap_err();
        assert_eq!(
            &*e[0].message,
            "format must be one of ['jpg'] for thumbnail"
        );
    }

    #[test]
    fn legacy_messages_for_typed_fields_match_python_repr() {
        assert_eq!(
            legacy::download_type_legacy(),
            "download_type must be one of ['audio', 'captions', 'thumbnail', 'video']"
        );
        assert_eq!(
            legacy::codec_legacy(),
            "codec must be one of ['auto', 'av1', 'h264', 'h265', 'vp9']"
        );
        assert_eq!(
            legacy::subtitle_mode_legacy(),
            "subtitle_mode must be one of ['auto_only', 'manual_only', 'prefer_auto', 'prefer_manual']"
        );
        assert_eq!(legacy::py_list(&["b", "a", "a"]), "['a', 'b']");
        assert_eq!(legacy::py_list(&[]), "[]");
    }

    #[test]
    fn path_escapes_and_presets_are_rejected() {
        let c = ytdlp_catalog();
        let p = presets();
        let mut r = req(DownloadType::Video, Codec::Auto, "mp4", "best");
        r.custom_name_prefix = "../x".into();
        r.chapter_template = "/abs".into();
        r.ytdl_options_presets = vec!["nope".into()];
        let errs = r.validate(&c, &p).unwrap_err();
        let msgs: Vec<&str> = errs.iter().map(|e| &*e.message).collect();
        assert_eq!(
            msgs,
            [
                legacy::CUSTOM_NAME_PREFIX,
                legacy::CHAPTER_TEMPLATE,
                legacy::PRESETS
            ]
        );
        assert_eq!(errs[2].code, ErrorCode::UnknownPreset);
    }

    #[test]
    fn a_configured_preset_is_accepted() {
        let c = ytdlp_catalog();
        let mut known = BTreeSet::new();
        known.insert(Box::from("sponsorblock"));
        let mut r = req(DownloadType::Video, Codec::Auto, "mp4", "best");
        r.ytdl_options_presets = vec!["sponsorblock".into()];
        assert!(r.validate(&c, &known).is_ok());
    }

    #[test]
    fn coerce_legacy_reproduces_the_silent_coercions() {
        let mut s = Selection::new(
            DownloadType::Captions,
            Codec::H265,
            FormatId::parse("srt").unwrap(),
            QualityId::parse("1080").unwrap(),
        );
        s.coerce_legacy();
        assert_eq!(s.codec, Codec::Auto);
        assert_eq!(s.quality.as_str(), "best");

        let mut v = Selection::new(
            DownloadType::Video,
            Codec::H265,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("1080").unwrap(),
        );
        v.coerce_legacy();
        assert_eq!(v.codec, Codec::H265, "video keeps its codec");
        assert_eq!(v.quality.as_str(), "1080");
    }

    #[test]
    fn request_view_has_exactly_nine_keys_and_redacts_secret_values() {
        let mut r = req(DownloadType::Video, Codec::Auto, "mp4", "1080");
        r.ytdl_options_overrides
            .insert("cookiefile".to_owned(), Value::String("/secret".to_owned()));
        r.ytdl_options_overrides
            .insert("format_sort".to_owned(), Value::String("res".to_owned()));
        let v = serde_json::to_value(r.to_view()).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 9);
        for k in [
            "custom_name_prefix",
            "playlist_item_limit",
            "auto_start",
            "split_by_chapters",
            "chapter_template",
            "subtitle_language",
            "subtitle_mode",
            "ytdl_options_presets",
            "ytdl_options_overrides",
        ] {
            assert!(obj.contains_key(k), "{k} missing");
        }
        let ov = obj["ytdl_options_overrides"].as_object().unwrap();
        assert_eq!(ov["cookiefile"], Value::String(REDACTED.to_owned()));
        assert_eq!(ov["format_sort"], Value::String("res".to_owned()));
        assert_eq!(ov.len(), 2, "the key set is preserved");
    }
}
