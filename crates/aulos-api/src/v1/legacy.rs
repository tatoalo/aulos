//! Every legacy string and every legacy allowed-value set, in one place (DESIGN §11.7).
//!
//! The rule DESIGN §11.7 states is "every string below is produced in exactly one place in the
//! Rust tree, and a unit test asserts each one literally". This module is that place for the
//! strings the shim owns; the ones `aulos-core` already owns are **re-exported** here rather than
//! copied, so there is still exactly one definition:
//!
//! | Family | Owner |
//! |---|---|
//! | `custom_name_prefix`, `chapter_template`, `subtitle_language`, presets, `playlist_item_limit`, the `… must be one of […]` builders | [`aulos_core::request::legacy`] |
//! | `Missing URL`, `This URL is already subscribed`, `Could not resolve URL`, the single-video sentence | [`aulos_core::subscription::legacy`] |
//! | the four cookie messages | [`crate::v2::cookies`] |
//! | the body/JSON, interval, subscription-update, `ids`, `enabled`, `auto_start` and `robots.txt` strings | here |
//!
//! # The hard-coded matrix
//!
//! DESIGN §11.2 step 3 says the shim validates with the **legacy hard-coded matrix first**, then
//! with the catalog. That ordering is what keeps the 400 reason strings byte-identical: the
//! catalog's allowed sets are read from a provider and could drift (a plugin, a future format), so
//! validating against it first would eventually change a string a shipped client renders. The
//! seven `const` sets below are `app/main.py:255-261` transcribed, and
//! [`validate_matrix`] is `parse_download_options`' tail in the same order.

use aulos_core::request::legacy as core_legacy;
use aulos_core::{Codec, DownloadType};

/// Legacy: the body was not JSON at all (an empty body included — Python's `request.json()`
/// raises `JSONDecodeError` on `b""`).
pub const INVALID_JSON_BODY: &str = "Invalid JSON request body";

/// Legacy: the body parsed but is not an object.
pub const BODY_MUST_BE_OBJECT: &str = "JSON request body must be an object";

/// Legacy: `url`, `download_type` or `quality` was missing or falsy.
pub const MISSING_REQUIRED: &str = "missing 'url', 'download_type', or 'quality'";

/// Legacy: `ytdl_options_overrides` was a string that is not JSON.
pub const OVERRIDES_INVALID_JSON: &str = "ytdl_options_overrides must be valid JSON";

/// Legacy: `ytdl_options_overrides` parsed to something other than an object.
pub const OVERRIDES_NOT_OBJECT: &str = "ytdl_options_overrides must be a JSON object";

/// Legacy: overrides were sent while `ALLOW_YTDL_OPTIONS_OVERRIDES=false`.
pub const OVERRIDES_DISABLED: &str = "ytdl_options_overrides are disabled";

/// Legacy: `ytdl_options_presets` was neither a list nor a string.
pub const PRESETS_WRONG_TYPE: &str =
    "ytdl_options_presets must be a JSON array of strings (or legacy ytdl_options_preset string)";

/// Legacy: `check_interval_minutes` could not be `int()`ed.
pub const CHECK_INTERVAL_NOT_INT: &str = "check_interval_minutes must be an integer";

/// Legacy: `check_interval_minutes` was below 1.
pub const CHECK_INTERVAL_MIN: &str = "check_interval_minutes must be at least 1";

/// Legacy: `subscriptions/update` with no `id`.
pub const MISSING_SUBSCRIPTION_ID: &str = "missing subscription id";

/// Legacy: `subscriptions/update` carrying none of the three updatable keys.
pub const NO_VALID_FIELDS: &str = "no valid fields to update";

/// Legacy: `subscriptions/update` for an id that does not exist. Answered **200** with a
/// `status: "error"` body, exactly as legacy did.
pub const SUBSCRIPTION_NOT_FOUND: &str = "Subscription not found";

/// Legacy: `subscriptions/delete` with a missing, empty or non-list `ids`.
pub const MISSING_IDS_LIST: &str = "missing ids list";

/// Legacy: `subscriptions/check` with a non-list `ids`.
pub const IDS_MUST_BE_LIST: &str = "ids must be a list";

/// Legacy `_coerce_bool`'s message. A **400** here where legacy leaked a 500 (Δ C25).
pub const ENABLED_NOT_BOOL: &str = "enabled must be a boolean";

/// The shim's own addition: legacy compared `auto_start is True` and silently routed anything else
/// to *pending*, so `"true"` from a Shortcut meant the opposite of what the user asked for
/// (DESIGN §11.2 step 4). The permissive parse is checked **last**, after every legacy 400, so no
/// legacy reason string is ever displaced by it.
pub const AUTO_START_NOT_BOOL: &str = "auto_start must be a boolean";

/// Legacy: `folder` was not a string. Legacy would have raised a `TypeError` (a 500); the shim
/// answers a 400, which is the honest status for a wrong-typed request field.
pub const FOLDER_NOT_STRING: &str = "folder must be a string";

/// The default `robots.txt`, byte-exact: three lines, each `\n`-terminated, no trailing blank line
/// (DESIGN §11.7).
pub const ROBOTS_TXT: &str = "User-agent: *\nDisallow: /download/\nDisallow: /audio_download/\n";

/// `POST <p>delete` with a missing `ids` or a `where` that is neither `queue` nor `done`.
///
/// Legacy raised a **reasonless** `HTTPBadRequest`, so aiohttp's own `Bad Request` was all a
/// client saw. The shim keeps the status and supplies the sentence legacy never wrote — there is
/// no byte-identical string to preserve here, and a client that shows `error.message` is strictly
/// better off.
pub const DELETE_BAD_REQUEST: &str =
    "ids must be a non-empty list and where must be \"queue\" or \"done\"";

/// `POST <p>start` with a missing or `null` `ids`.
///
/// Legacy crashed with a `TypeError` and answered `500 Server got itself in trouble`
/// (DESIGN §11.1). A 400 is the honest status.
pub const START_IDS_REQUIRED: &str = "ids is required and must be a list";

// ---------------------------------------------------------------------------
// re-exports: one definition, two callers
// ---------------------------------------------------------------------------

pub use crate::v2::cookies::{MANUAL_COOKIEFILE, NO_FILE, NOTHING_TO_DELETE, TOO_LARGE};
pub use aulos_core::request::legacy::{
    CHAPTER_TEMPLATE, CUSTOM_NAME_PREFIX, PLAYLIST_ITEM_LIMIT, PRESETS, SUBTITLE_LANGUAGE,
};
pub use aulos_core::subscription::legacy::{
    ALREADY_SUBSCRIBED, COULD_NOT_RESOLVE, MISSING_URL, VIDEO_ONLY,
};

/// Legacy: `Cookies uploaded (N bytes)` — `N` is the decoded byte count.
#[must_use]
pub fn cookies_uploaded(bytes: usize) -> String {
    format!("Cookies uploaded ({bytes} bytes)")
}

// ---------------------------------------------------------------------------
// the hard-coded matrix (app/main.py:255-261)
// ---------------------------------------------------------------------------

/// `VALID_VIDEO_FORMATS`.
pub const VIDEO_FORMATS: [&str; 3] = ["any", "mp4", "ios"];

/// `VALID_AUDIO_FORMATS`.
pub const AUDIO_FORMATS: [&str; 5] = ["m4a", "mp3", "opus", "wav", "flac"];

/// `VALID_SUBTITLE_FORMATS`.
pub const SUBTITLE_FORMATS: [&str; 7] = ["srt", "txt", "vtt", "ttml", "sbv", "scc", "dfxp"];

/// `VALID_THUMBNAIL_FORMATS`.
pub const THUMBNAIL_FORMATS: [&str; 1] = ["jpg"];

/// The nine video qualities every video format accepts. `mp4` additionally accepts `best_remux`.
pub const VIDEO_QUALITIES: [&str; 9] = [
    "best", "worst", "2160", "1440", "1080", "720", "480", "360", "240",
];

/// One matrix rejection: the offending field and the byte-identical reason.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MatrixError {
    /// `download_type`, `codec`, `format` or `quality`.
    pub field: &'static str,
    /// The legacy reason string.
    pub message: String,
}

impl MatrixError {
    fn new(field: &'static str, message: String) -> Self {
        Self { field, message }
    }
}

/// The already-lower-cased, already-trimmed selection tokens the matrix validates.
#[derive(Clone, Copy, Debug)]
pub struct Tokens<'a> {
    /// `download_type`.
    pub download_type: &'a str,
    /// `codec`.
    pub codec: &'a str,
    /// `format`.
    pub format: &'a str,
    /// `quality`.
    pub quality: &'a str,
}

/// What the matrix accepted, with legacy's silent coercions already applied.
#[derive(Clone, Debug)]
pub struct Accepted {
    /// The parsed download type.
    pub download_type: DownloadType,
    /// The codec, forced to `auto` for every non-video type.
    pub codec: Codec,
    /// The format id, unchanged.
    pub format: String,
    /// The quality id, forced to `best` for `captions` and `thumbnail`.
    pub quality: String,
}

/// `parse_download_options`' validation tail, in legacy order (`app/main.py:606-651`).
///
/// The order is load-bearing: `download_type` before `codec` before the per-type `format` before
/// its `quality`, so the *first* reason a legacy client saw is the one the shim reports.
///
/// # Errors
/// One [`MatrixError`] carrying the byte-identical legacy reason.
pub fn validate_matrix(t: Tokens<'_>) -> Result<Accepted, MatrixError> {
    let Some(download_type) = DownloadType::from_str_exact(t.download_type) else {
        return Err(MatrixError::new(
            "download_type",
            core_legacy::download_type_legacy(),
        ));
    };
    let Some(mut codec) = Codec::from_str_exact(t.codec) else {
        return Err(MatrixError::new("codec", core_legacy::codec_legacy()));
    };

    let mut quality = t.quality.to_owned();
    match download_type {
        DownloadType::Video => {
            if !VIDEO_FORMATS.contains(&t.format) {
                return Err(MatrixError::new(
                    "format",
                    core_legacy::format_must_be_one_of(&VIDEO_FORMATS, "video"),
                ));
            }
            let mut allowed: Vec<&str> = VIDEO_QUALITIES.to_vec();
            if t.format == "mp4" {
                allowed.push("best_remux");
            }
            if !allowed.contains(&t.quality) {
                return Err(MatrixError::new(
                    "quality",
                    core_legacy::quality_must_be_one_of_for_type(&allowed, "video"),
                ));
            }
        }
        DownloadType::Audio => {
            if !AUDIO_FORMATS.contains(&t.format) {
                return Err(MatrixError::new(
                    "format",
                    core_legacy::format_must_be_one_of(&AUDIO_FORMATS, "audio"),
                ));
            }
            let mut allowed: Vec<&str> = vec!["best"];
            match t.format {
                "mp3" => allowed.extend(["320", "192", "128"]),
                "m4a" => allowed.extend(["192", "128"]),
                _ => {}
            }
            if !allowed.contains(&t.quality) {
                return Err(MatrixError::new(
                    "quality",
                    core_legacy::quality_must_be_one_of_for_format(&allowed, t.format),
                ));
            }
            codec = Codec::Auto;
        }
        DownloadType::Captions => {
            if !SUBTITLE_FORMATS.contains(&t.format) {
                return Err(MatrixError::new(
                    "format",
                    core_legacy::format_must_be_one_of(&SUBTITLE_FORMATS, "captions"),
                ));
            }
            quality = "best".to_owned();
            codec = Codec::Auto;
        }
        DownloadType::Thumbnail => {
            if !THUMBNAIL_FORMATS.contains(&t.format) {
                return Err(MatrixError::new(
                    "format",
                    core_legacy::format_must_be_one_of(&THUMBNAIL_FORMATS, "thumbnail"),
                ));
            }
            quality = "best".to_owned();
            codec = Codec::Auto;
        }
    }

    Ok(Accepted {
        download_type,
        codec,
        format: t.format.to_owned(),
        quality,
    })
}

/// `subtitle_mode must be one of […]`, the legacy `sorted(set)` repr.
#[must_use]
pub fn subtitle_mode_message() -> String {
    core_legacy::subtitle_mode_legacy()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DESIGN §11.7, the **Add / validation** table, asserted literally rather than against the
    /// captured corpus, exactly as PLAN WP-15 requires.
    #[test]
    fn the_add_strings_are_byte_identical() {
        assert_eq!(INVALID_JSON_BODY, "Invalid JSON request body");
        assert_eq!(BODY_MUST_BE_OBJECT, "JSON request body must be an object");
        assert_eq!(
            MISSING_REQUIRED,
            "missing 'url', 'download_type', or 'quality'"
        );
        assert_eq!(
            PLAYLIST_ITEM_LIMIT,
            "playlist_item_limit must be an integer"
        );
        assert_eq!(
            OVERRIDES_INVALID_JSON,
            "ytdl_options_overrides must be valid JSON"
        );
        assert_eq!(
            OVERRIDES_NOT_OBJECT,
            "ytdl_options_overrides must be a JSON object"
        );
        assert_eq!(OVERRIDES_DISABLED, "ytdl_options_overrides are disabled");
        assert_eq!(
            PRESETS_WRONG_TYPE,
            "ytdl_options_presets must be a JSON array of strings (or legacy ytdl_options_preset string)"
        );
        assert_eq!(
            PRESETS,
            "ytdl_options_presets must only contain configured preset names"
        );
        assert_eq!(
            CUSTOM_NAME_PREFIX,
            "custom_name_prefix must not contain \"..\" or start with a path separator"
        );
        assert_eq!(
            CHAPTER_TEMPLATE,
            "chapter_template must not contain \"..\" or start with a path separator"
        );
        assert_eq!(
            SUBTITLE_LANGUAGE,
            "subtitle_language must match pattern [A-Za-z0-9-] and be at most 35 characters"
        );
    }

    /// DESIGN §11.7's **Subscriptions** table.
    #[test]
    fn the_subscription_strings_are_byte_identical() {
        assert_eq!(
            CHECK_INTERVAL_NOT_INT,
            "check_interval_minutes must be an integer"
        );
        assert_eq!(
            CHECK_INTERVAL_MIN,
            "check_interval_minutes must be at least 1"
        );
        assert_eq!(MISSING_SUBSCRIPTION_ID, "missing subscription id");
        assert_eq!(NO_VALID_FIELDS, "no valid fields to update");
        assert_eq!(SUBSCRIPTION_NOT_FOUND, "Subscription not found");
        assert_eq!(MISSING_IDS_LIST, "missing ids list");
        assert_eq!(IDS_MUST_BE_LIST, "ids must be a list");
        assert_eq!(MISSING_URL, "Missing URL");
        assert_eq!(ALREADY_SUBSCRIBED, "This URL is already subscribed");
        assert_eq!(COULD_NOT_RESOLVE, "Could not resolve URL");
        assert_eq!(
            VIDEO_ONLY,
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );
        assert_eq!(ENABLED_NOT_BOOL, "enabled must be a boolean");
    }

    /// DESIGN §11.7's **Cookies** table.
    #[test]
    fn the_cookie_strings_are_byte_identical() {
        assert_eq!(NO_FILE, "No cookies file provided");
        assert_eq!(TOO_LARGE, "Cookie file too large (max 1MB)");
        assert_eq!(NOTHING_TO_DELETE, "No uploaded cookies to delete");
        assert_eq!(cookies_uploaded(64), "Cookies uploaded (64 bytes)");
        assert_eq!(
            cookies_uploaded(1_000_000),
            "Cookies uploaded (1000000 bytes)"
        );
    }

    /// DESIGN §11.7's `robots.txt` block: three lines, no trailing blank line.
    #[test]
    fn robots_txt_is_the_three_line_body() {
        assert_eq!(
            ROBOTS_TXT,
            "User-agent: *\nDisallow: /download/\nDisallow: /audio_download/\n"
        );
        assert_eq!(ROBOTS_TXT.lines().count(), 3);
        assert!(ROBOTS_TXT.ends_with('\n'));
        assert!(!ROBOTS_TXT.ends_with("\n\n"));
    }

    /// The `sorted(set)` reprs legacy interpolated, which are why the matrix is hard-coded.
    #[test]
    fn the_matrix_messages_are_the_python_list_reprs() {
        let err = matrix_err("mkv", "video", "auto", "best");
        assert_eq!(err.field, "format");
        assert_eq!(
            err.message,
            "format must be one of ['any', 'ios', 'mp4'] for video"
        );

        let err = matrix_err("any", "video", "auto", "best_remux");
        assert_eq!(err.field, "quality");
        assert_eq!(
            err.message,
            "quality must be one of ['1080', '1440', '2160', '240', '360', '480', '720', 'best', 'worst'] for video"
        );

        let err = matrix_err("m4a", "audio", "auto", "320");
        assert_eq!(
            err.message,
            "quality must be one of ['128', '192', 'best'] for format m4a"
        );

        let err = matrix_err("wav", "audio", "auto", "192");
        assert_eq!(
            err.message,
            "quality must be one of ['best'] for format wav"
        );

        let err = matrix_err("mp3", "audio", "auto", "999");
        assert_eq!(
            err.message,
            "quality must be one of ['128', '192', '320', 'best'] for format mp3"
        );

        let err = matrix_err("nope", "captions", "auto", "best");
        assert_eq!(
            err.message,
            "format must be one of ['dfxp', 'sbv', 'scc', 'srt', 'ttml', 'txt', 'vtt'] for captions"
        );

        let err = matrix_err("png", "thumbnail", "auto", "best");
        assert_eq!(err.message, "format must be one of ['jpg'] for thumbnail");

        let err = matrix_err("mp4", "movie", "auto", "best");
        assert_eq!(err.field, "download_type");
        assert_eq!(
            err.message,
            "download_type must be one of ['audio', 'captions', 'thumbnail', 'video']"
        );

        let err = matrix_err("mp4", "video", "vp8", "best");
        assert_eq!(err.field, "codec");
        assert_eq!(
            err.message,
            "codec must be one of ['auto', 'av1', 'h264', 'h265', 'vp9']"
        );

        assert_eq!(
            subtitle_mode_message(),
            "subtitle_mode must be one of ['auto_only', 'manual_only', 'prefer_auto', 'prefer_manual']"
        );
    }

    #[test]
    fn best_remux_is_accepted_on_mp4_only() {
        assert!(ok("mp4", "video", "auto", "best_remux").quality == "best_remux");
        assert!(matrix("ios", "video", "auto", "best_remux").is_err());
    }

    #[test]
    fn the_silent_coercions_match_legacy() {
        // audio forces codec=auto but keeps the quality
        let a = ok("mp3", "audio", "h264", "320");
        assert_eq!(a.codec, Codec::Auto);
        assert_eq!(a.quality, "320");
        // captions and thumbnail force quality=best as well
        let c = ok("srt", "captions", "h265", "1080");
        assert_eq!(c.codec, Codec::Auto);
        assert_eq!(c.quality, "best");
        let t = ok("jpg", "thumbnail", "av1", "720");
        assert_eq!(t.codec, Codec::Auto);
        assert_eq!(t.quality, "best");
        // video keeps its codec
        assert_eq!(ok("mp4", "video", "h264", "1080").codec, Codec::H264);
    }

    fn matrix(
        format: &str,
        download_type: &str,
        codec: &str,
        quality: &str,
    ) -> Result<Accepted, MatrixError> {
        validate_matrix(Tokens {
            download_type,
            codec,
            format,
            quality,
        })
    }

    fn ok(format: &str, download_type: &str, codec: &str, quality: &str) -> Accepted {
        matrix(format, download_type, codec, quality).expect("a legal tuple")
    }

    fn matrix_err(format: &str, download_type: &str, codec: &str, quality: &str) -> MatrixError {
        matrix(format, download_type, codec, quality).expect_err("an illegal tuple")
    }
}
