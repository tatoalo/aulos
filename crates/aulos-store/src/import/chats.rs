//! `telegram_bot_config.json` → the `telegram_chats` table (DESIGN §7.6.5).
//!
//! The file is a **bare** `{"<chat_id>": { …12 keys… }}` object: no `schema_version`, no `kind`,
//! no `items` — `TelegramBot._save_config` wrote `self._chat_config` directly
//! (`app/telegram_bot.py:69`). So it has no envelope to validate, and its `FileReport` carries a
//! `null` schema version.
//!
//! Every value goes through [`aulos_core::normalize_download_selection`], the same port the bot
//! uses (DESIGN §12.3 step 4), so a stored `{"format":"m4a"}` becomes `download_type = audio` in
//! exactly one place in the workspace.

use aulos_core::{ChatConfig, normalize_download_selection};
use serde_json::{Map, Value};

/// The file name in `STATE_DIR`.
pub(crate) const FILE: &str = "telegram_bot_config.json";

/// One imported chat.
pub(crate) struct Built {
    /// The Telegram chat id.
    pub chat_id: i64,
    /// Its stored defaults, with the selection normalised.
    pub config: ChatConfig,
}

/// Reads the bare object.
///
/// # Errors
/// A message for the `file_invalid` error when the payload is not a JSON object at all.
pub(crate) fn read_object(text: &str) -> Result<Map<String, Value>, Box<str>> {
    let root: Value =
        serde_json::from_str(text).map_err(|e| Box::<str>::from(format!("not valid JSON: {e}")))?;
    match root {
        Value::Object(o) => Ok(o),
        _ => Err("the payload is not a JSON object".into()),
    }
}

/// Builds one chat's config.
///
/// # Errors
/// A message for the `record_skipped` warning when the key is not a chat id or the value is not an
/// object.
pub(crate) fn build(key: &str, value: &Value) -> Result<Built, Box<str>> {
    let chat_id: i64 = key
        .trim()
        .parse()
        .map_err(|_| Box::<str>::from(format!("{key:?} is not a chat id")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| Box::<str>::from(format!("the config for {chat_id} is not an object")))?;

    // Unknown keys are dropped (with the DEBUG line DESIGN §7.6.5 asks for) rather than rejected:
    // the file is a Python dict dump and a future key must not fail a cutover.
    let defaults = ChatConfig::default();
    for k in obj.keys() {
        if !KNOWN_KEYS.contains(&k.as_str()) {
            tracing::debug!(chat_id, key = %k, "dropping unknown telegram chat config key");
        }
    }

    let format = str_or(obj, "format", &defaults.format);
    let quality = str_or(obj, "quality", &defaults.quality);
    let download_type = str_or(obj, "download_type", &defaults.download_type);
    let codec = str_or(obj, "codec", &defaults.codec);
    // The one substantive step: the flat legacy pair becomes a real selection, and the stored
    // config is rewritten to agree with it.
    let selection = normalize_download_selection(&format, &quality, &download_type, &codec);

    let config = ChatConfig {
        format: selection.format.as_str().into(),
        quality: selection.quality.as_str().into(),
        download_type: selection.download_type.as_str().into(),
        codec: selection.codec.as_str().into(),
        subtitle_language: str_or(obj, "subtitle_language", &defaults.subtitle_language),
        subtitle_mode: str_or(obj, "subtitle_mode", &defaults.subtitle_mode),
        folder: str_or(obj, "folder", &defaults.folder),
        custom_name_prefix: str_or(obj, "custom_name_prefix", &defaults.custom_name_prefix),
        playlist_item_limit: obj
            .get("playlist_item_limit")
            .and_then(as_u32)
            .unwrap_or(defaults.playlist_item_limit),
        auto_start: obj
            .get("auto_start")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.auto_start),
        split_by_chapters: obj
            .get("split_by_chapters")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.split_by_chapters),
        chapter_template: str_or(obj, "chapter_template", &defaults.chapter_template),
    };

    Ok(Built { chat_id, config })
}

/// The twelve keys legacy's `_get_chat_config` wrote (`app/telegram_bot.py:194`).
const KNOWN_KEYS: [&str; 12] = [
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
];

/// A trimmed string field with a default. An empty stored value keeps the default, matching the
/// `str(value or default).strip()` shape of the legacy normaliser.
fn str_or(obj: &Map<String, Value>, key: &str, default: &str) -> Box<str> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| default.into(), Box::<str>::from)
}

/// A lenient `u32`, accepting the float JSON encodes a Python int as.
fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
            .and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_legacy_chat_config_is_imported_verbatim_but_normalised() {
        let v = json!({
            "format": "m4a", "quality": "192", "download_type": "video", "codec": "h264",
            "subtitle_language": "it", "subtitle_mode": "auto_only", "folder": "Shows",
            "custom_name_prefix": "P", "playlist_item_limit": 5, "auto_start": false,
            "split_by_chapters": true, "chapter_template": "tpl"
        });
        let b = build("-1001234567890", &v).expect("must build");
        assert_eq!(b.chat_id, -1_001_234_567_890);
        let c = &b.config;
        // The audio format implies the audio type, and the stored pair is rewritten to agree.
        assert_eq!(&*c.download_type, "audio");
        assert_eq!(&*c.format, "m4a");
        assert_eq!(&*c.quality, "192");
        assert_eq!(&*c.codec, "auto");
        assert_eq!(c.selection().download_type, aulos_core::DownloadType::Audio);
        // Everything else survives.
        assert_eq!(&*c.subtitle_language, "it");
        assert_eq!(&*c.subtitle_mode, "auto_only");
        assert_eq!(&*c.folder, "Shows");
        assert_eq!(&*c.custom_name_prefix, "P");
        assert_eq!(c.playlist_item_limit, 5);
        assert!(!c.auto_start);
        assert!(c.split_by_chapters);
        assert_eq!(&*c.chapter_template, "tpl");
    }

    #[test]
    fn the_two_pseudo_qualities_are_normalised_too() {
        let b = build("1", &json!({"format": "any", "quality": "best_ios"})).expect("build");
        assert_eq!(&*b.config.download_type, "video");
        assert_eq!(&*b.config.format, "ios");
        assert_eq!(&*b.config.quality, "best");

        let b = build("1", &json!({"format": "any", "quality": "audio"})).expect("build");
        assert_eq!(&*b.config.download_type, "audio");
        assert_eq!(&*b.config.format, "m4a");
    }

    #[test]
    fn missing_and_unknown_keys_are_tolerated() {
        let b = build("42", &json!({"legacy_junk": 1})).expect("build");
        let d = ChatConfig::default();
        assert_eq!(b.config, d, "an empty config is the legacy defaults");
    }

    #[test]
    fn a_bad_key_or_value_is_a_record_error() {
        assert!(build("not-a-chat", &json!({})).is_err());
        assert!(build("1", &json!("nope")).is_err());
    }

    #[test]
    fn the_bare_object_has_no_envelope() {
        let o = read_object(r#"{"1": {"format": "mp4"}}"#).expect("must read");
        assert_eq!(o.len(), 1);
        assert!(read_object("[]").is_err());
        assert!(read_object("{").is_err());
    }
}
