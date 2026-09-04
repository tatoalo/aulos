//! `get_format`: the yt-dlp format selector, a literal port of legacy `app/dl_formats.py`
//! (DESIGN §9.8, Appendix A.6).
//!
//! Every table below is the legacy constant with the same name, and the decision order in
//! [`get_format_raw`] is the legacy statement order — including the three quirks the design
//! commits to keeping (Appendix B, K1):
//!
//! 1. `format.startswith("custom:")` is checked **first**, before the `download_type` dispatch,
//!    so `custom:` wins over every other rule and over an otherwise-illegal type/format pair.
//! 2. The `ios` selector chain returns before the codec filter is consulted, so `codec` is
//!    silently ignored for that format.
//! 3. `quality == "worst"` composes **no** `worst*` selector and no height filter, so it produces
//!    the same string as `best`. The catalog now says so out loud
//!    ([`aulos_core::catalog`]'s `worst` notice, Δ C21) but the selector is unchanged.
//!
//! The typed [`get_format`] is what the rest of the server calls; [`get_format_raw`] exists
//! because the legacy function took four free-form strings and applied `or`-defaults, `strip()`
//! and `lower()` to each — behaviour the v1 shim and the WP-00 golden corpus both exercise with
//! values no typed enum can express (`None`, `"  VIDEO "`, an unknown codec).

use aulos_core::selection::{Codec, DownloadType};

/// Legacy `AUDIO_FORMATS`, in source order.
pub const AUDIO_FORMATS: [&str; 5] = ["m4a", "mp3", "opus", "wav", "flac"];

/// The three video formats legacy's `get_format` admits, in source order.
pub const VIDEO_FORMATS: [&str; 3] = ["any", "mp4", "ios"];

/// Legacy `CAPTION_MODES`, in source order.
///
/// Lives here rather than in [`crate::opts`] because it is a `dl_formats` constant; the caption
/// branch of [`crate::opts::get_opts_raw`] is its only reader.
pub const CAPTION_MODES: [&str; 4] = ["auto_only", "manual_only", "prefer_manual", "prefer_auto"];

/// Legacy `CODEC_FILTER_MAP`, as a slice because four entries do not earn a hash map.
pub const CODEC_FILTER_MAP: [(&str, &str); 4] = [
    ("h264", "[vcodec~='^(h264|avc)']"),
    ("h265", "[vcodec~='^(h265|hevc)']"),
    ("av1", "[vcodec~='^av0?1']"),
    ("vp9", "[vcodec~='^vp0?9']"),
];

/// The fixed codec filter inside the `ios` selector chain. Not user-selectable.
const IOS_VCODEC: &str = "[vcodec~='^((he|a)vc|h26[45])']";

/// The prefix that makes a `format` a verbatim yt-dlp selector.
const CUSTOM_PREFIX: &str = "custom:";

/// The qualities that compose no `[height<=…]` filter.
///
/// `worst` is in this set, which is quirk 3 in the module docs: legacy never emitted a `worst*`
/// selector.
const UNFILTERED_QUALITIES: [&str; 3] = ["best", "best_remux", "worst"];

/// The `CODEC_FILTER_MAP` entry for `codec`, or `""` for `auto` and for anything unknown.
///
/// An unknown codec is deliberately **not** an error: legacy used
/// `CODEC_FILTER_MAP.get(codec, "")`, so `{video, vp8, any, 720}` produced an unfiltered selector
/// rather than a 400.
#[must_use]
pub fn codec_filter(codec: &str) -> &'static str {
    CODEC_FILTER_MAP
        .into_iter()
        .find_map(|(k, v)| (k == codec).then_some(v))
        .unwrap_or("")
}

/// A rejected `(download_type, format)` pair.
///
/// The `Display` text of each variant is byte-identical to the legacy `ValueError` message, which
/// is what the v1 shim echoes to a legacy client.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum FormatError {
    /// `download_type` was none of `video`, `audio`, `captions`, `thumbnail`.
    #[error("Unknown download_type {0}")]
    UnknownDownloadType(Box<str>),
    /// `download_type == "video"` with a `format` outside [`VIDEO_FORMATS`].
    #[error("Unknown video format {0}")]
    UnknownVideoFormat(Box<str>),
    /// `download_type == "audio"` with a `format` outside [`AUDIO_FORMATS`].
    #[error("Unknown audio format {0}")]
    UnknownAudioFormat(Box<str>),
}

impl FormatError {
    /// The wire error code. Always a client error: the four fields came from the request.
    #[must_use]
    pub const fn code(&self) -> aulos_core::error::ErrorCode {
        aulos_core::error::ErrorCode::ValidationFailed
    }
}

/// Legacy's `(value or default).strip().lower()`.
///
/// The `or` runs **before** `strip`, exactly as in Python, which is why `Some("   ")` normalises to
/// the empty string (and then fails validation) while `Some("")` and `None` both fall back to
/// `default`.
fn norm(value: Option<&str>, default: &'static str) -> String {
    let raw = value.unwrap_or("");
    let raw = if raw.is_empty() { default } else { raw };
    raw.trim().to_lowercase()
}

/// The yt-dlp format selector for a validated selection (DESIGN §9.8).
///
/// # Errors
/// [`FormatError::UnknownVideoFormat`] or [`FormatError::UnknownAudioFormat`] when `format` is not
/// in the legacy list for `dt`. [`FormatError::UnknownDownloadType`] is unreachable through this
/// entry point — [`DownloadType`] is closed — but the variant exists because
/// [`get_format_raw`] can produce it.
pub fn get_format(
    dt: DownloadType,
    codec: Codec,
    format: &str,
    quality: &str,
) -> Result<String, FormatError> {
    get_format_raw(
        Some(dt.as_str()),
        Some(codec.as_str()),
        Some(format),
        Some(quality),
    )
}

/// The string-in, string-out form: the literal legacy signature.
///
/// `None` and `""` are the same thing (Python falsiness), and each argument is `strip()`ped and
/// `lower()`ed after the default is applied.
///
/// # Errors
/// One of the three [`FormatError`] variants, whose `Display` reproduces the legacy `ValueError`
/// text byte for byte.
pub fn get_format_raw(
    download_type: Option<&str>,
    codec: Option<&str>,
    format: Option<&str>,
    quality: Option<&str>,
) -> Result<String, FormatError> {
    let download_type = norm(download_type, "video");
    let format = norm(format, "any");
    let codec = norm(codec, "auto");
    let quality = norm(quality, "best");

    // Quirk 1: `custom:` short-circuits everything, including the download_type dispatch.
    if let Some(rest) = format.strip_prefix(CUSTOM_PREFIX) {
        return Ok(rest.to_owned());
    }

    match download_type.as_str() {
        // Legacy has two consecutive `if`s here with the same body: both types download a
        // placeholder stream and keep only the side file, so both use the same selector.
        "thumbnail" | "captions" => Ok("bestaudio/best".to_owned()),
        "audio" => {
            if !AUDIO_FORMATS.contains(&format.as_str()) {
                return Err(FormatError::UnknownAudioFormat(format.into()));
            }
            Ok(format!("bestaudio[ext={format}]/bestaudio/best"))
        }
        "video" => video_selector(&format, &codec, &quality),
        _ => Err(FormatError::UnknownDownloadType(download_type.into())),
    }
}

/// The `download_type == "video"` branch, split out only to keep [`get_format_raw`] readable.
fn video_selector(format: &str, codec: &str, quality: &str) -> Result<String, FormatError> {
    if !VIDEO_FORMATS.contains(&format) {
        return Err(FormatError::UnknownVideoFormat(format.into()));
    }
    let (vfmt, afmt) = if matches!(format, "mp4" | "ios") {
        ("[ext=mp4]", "[ext=m4a]")
    } else {
        ("", "")
    };
    let vres = if UNFILTERED_QUALITIES.contains(&quality) {
        String::new()
    } else {
        format!("[height<={quality}]")
    };
    let vcombo = format!("{vres}{vfmt}");

    // Quirk 2: the `ios` chain returns before `codec_filter` is consulted.
    if format == "ios" {
        return Ok(format!(
            "bestvideo{IOS_VCODEC}{vres}+bestaudio[acodec=aac]\
             /bestvideo{IOS_VCODEC}{vres}+bestaudio{afmt}\
             /bestvideo{vcombo}+bestaudio{afmt}\
             /best{vcombo}"
        ));
    }

    // `best_remux` also returns early, so it too ignores the codec filter. The remux itself is
    // arranged by `opts::get_opts` (`merge_output_format` + `FFmpegVideoConvertor`).
    if format == "mp4" && quality == "best_remux" {
        return Ok("bestvideo+bestaudio/best".to_owned());
    }

    let codec_filter = codec_filter(codec);
    if codec_filter.is_empty() {
        Ok(format!("bestvideo{vcombo}+bestaudio{afmt}/best{vcombo}"))
    } else {
        Ok(format!(
            "bestvideo{codec_filter}{vcombo}+bestaudio{afmt}\
             /bestvideo{vcombo}+bestaudio{afmt}\
             /best{vcombo}"
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn custom_wins_over_every_other_rule() {
        // Over the audio branch, which would otherwise reject `bestaudio` as an unknown format.
        assert_eq!(
            get_format_raw(Some("audio"), None, Some("custom:bestaudio"), None).unwrap(),
            "bestaudio"
        );
        // Over an unknown download_type, which would otherwise be a ValueError.
        assert_eq!(
            get_format_raw(Some("podcast"), None, Some("custom:worst"), None).unwrap(),
            "worst"
        );
        // And the remainder is verbatim, including nothing at all.
        assert_eq!(
            get_format_raw(Some("video"), None, Some("custom:"), None).unwrap(),
            ""
        );
    }

    #[test]
    fn the_defaults_are_applied_before_stripping() {
        // `None` and `""` are both falsy, so both become the defaults.
        assert_eq!(
            get_format_raw(None, None, None, None).unwrap(),
            "bestvideo+bestaudio/best"
        );
        assert_eq!(
            get_format_raw(Some(""), Some(""), Some(""), Some("")).unwrap(),
            "bestvideo+bestaudio/best"
        );
        // Whitespace is truthy in Python, so it survives the `or` and then strips to "".
        let err = get_format_raw(Some("   "), None, None, None).unwrap_err();
        assert_eq!(err.to_string(), "Unknown download_type ");
    }

    #[test]
    fn case_and_whitespace_are_normalised() {
        assert_eq!(
            get_format_raw(
                Some("  VIDEO "),
                Some(" H264 "),
                Some(" MP4 "),
                Some(" 1080 ")
            )
            .unwrap(),
            get_format(DownloadType::Video, Codec::H264, "mp4", "1080").unwrap()
        );
    }

    #[test]
    fn an_unknown_codec_is_not_an_error() {
        assert_eq!(codec_filter("vp8"), "");
        assert_eq!(codec_filter("auto"), "");
        assert_eq!(
            get_format_raw(Some("video"), Some("vp8"), Some("any"), Some("720")).unwrap(),
            "bestvideo[height<=720]+bestaudio/best[height<=720]"
        );
    }

    #[test]
    fn worst_emits_no_worst_selector() {
        let worst = get_format(DownloadType::Video, Codec::Auto, "any", "worst").unwrap();
        let best = get_format(DownloadType::Video, Codec::Auto, "any", "best").unwrap();
        assert_eq!(worst, best);
        assert!(!worst.contains("worst"));
    }

    #[test]
    fn the_error_texts_are_the_legacy_valueerror_texts() {
        assert_eq!(
            get_format_raw(Some("podcast"), None, None, None)
                .unwrap_err()
                .to_string(),
            "Unknown download_type podcast"
        );
        assert_eq!(
            get_format_raw(Some("video"), None, Some("mkv"), None)
                .unwrap_err()
                .to_string(),
            "Unknown video format mkv"
        );
        assert_eq!(
            get_format_raw(Some("audio"), None, Some("aac"), None)
                .unwrap_err()
                .to_string(),
            "Unknown audio format aac"
        );
    }

    #[test]
    fn ios_and_best_remux_ignore_the_codec_filter() {
        let with = get_format(DownloadType::Video, Codec::Av1, "ios", "1080").unwrap();
        let without = get_format(DownloadType::Video, Codec::Auto, "ios", "1080").unwrap();
        assert_eq!(with, without);
        assert!(with.contains(IOS_VCODEC));

        assert_eq!(
            get_format(DownloadType::Video, Codec::H265, "mp4", "best_remux").unwrap(),
            "bestvideo+bestaudio/best"
        );
    }
}
