//! `/config`: the byte-identical texts, the inline keyboards, and the **unchanged** `cfg:`
//! callback grammar (DESIGN §12.2).
//!
//! The keyboard's format list is [`aulos_core::catalog::FormatCatalog::bot_formats`] — the
//! documented nine-entry projection of the one shared `ytdlp` catalog, *not* its sixteen ids. Two
//! quirks of that projection are load-bearing for the grammar and are asserted here:
//!
//! - `any` offers an extra `audio` pseudo-quality, which
//!   [`aulos_core::normalize_download_selection`] maps to `(audio, m4a, best)`;
//! - the thumbnail button keeps the legacy id `thumbnail` while the catalog id is `jpg`.
//!
//! Caption formats are deliberately unreachable from the keyboard, because the legacy list had no
//! caption entry either — a caption default was only ever settable by hand-editing
//! `telegram_bot_config.json`, and an imported chat that says `captions` keeps working.

use aulos_core::ChatConfig;
use aulos_core::catalog::BotFormat;

/// One inline-keyboard button: the label the user sees and the callback payload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Button {
    /// The label.
    pub text: String,
    /// The `cfg:…` payload.
    pub data: String,
}

impl Button {
    /// A button with `text` and `data`.
    #[must_use]
    pub fn new(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: data.into(),
        }
    }
}

/// An inline keyboard: rows of buttons. Legacy put exactly one button per row, and so do we.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Keyboard(pub Vec<Vec<Button>>);

impl Keyboard {
    /// A keyboard of single-button rows.
    #[must_use]
    pub fn column(buttons: Vec<Button>) -> Self {
        Self(buttons.into_iter().map(|b| vec![b]).collect())
    }

    /// Every button, row by row.
    pub fn buttons(&self) -> impl Iterator<Item = &Button> {
        self.0.iter().flatten()
    }

    /// The labels, row by row — what a snapshot test compares.
    #[must_use]
    pub fn labels(&self) -> Vec<&str> {
        self.buttons().map(|b| b.text.as_str()).collect()
    }

    /// The callback payloads, row by row.
    #[must_use]
    pub fn payloads(&self) -> Vec<&str> {
        self.buttons().map(|b| b.data.as_str()).collect()
    }
}

/// `/start`, byte-identical (DESIGN §12.2).
pub const START_TEXT: &str = "Hi! Send one or more links and I will queue them for download.\n\
     Use /config to set default format/quality for this chat.";

/// The `cfg:menu:format` prompt.
pub const SELECT_FORMAT: &str = "Select format";
/// The `cfg:menu:quality` prompt.
pub const SELECT_QUALITY: &str = "Select quality";
/// The `cfg:menu:limit` prompt.
pub const SELECT_LIMIT: &str = "Select playlist limit";
/// The label of the button that returns to the main menu.
pub const BACK: &str = "Back";
/// The playlist-limit choices, in legacy order.
pub const LIMIT_CHOICES: [u32; 5] = [0, 1, 5, 10, 20];

/// The `/config` body (DESIGN §12.2, legacy `_format_config_text`).
#[must_use]
pub fn config_text(cfg: &ChatConfig) -> String {
    format!(
        "Current download config:\n- Format: {}\n- Quality: {}\n- Split by chapters: {}\n\
         - Playlist item limit: {}",
        cfg.format,
        cfg.quality,
        on_off(cfg.split_by_chapters),
        cfg.playlist_item_limit
    )
}

/// `on` / `off`, as legacy spelled it.
#[must_use]
pub const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// The main `/config` keyboard (legacy `_build_main_config_keyboard`).
#[must_use]
pub fn main_keyboard(cfg: &ChatConfig) -> Keyboard {
    Keyboard::column(vec![
        Button::new(format!("Format: {}", cfg.format), "cfg:menu:format"),
        Button::new(format!("Quality: {}", cfg.quality), "cfg:menu:quality"),
        Button::new(
            format!("Split Chapters: {}", on_off(cfg.split_by_chapters)),
            "cfg:toggle:split",
        ),
        Button::new(
            format!("Playlist Limit: {}", cfg.playlist_item_limit),
            "cfg:menu:limit",
        ),
    ])
}

/// The format keyboard: one button per [`BotFormat`], then `Back`.
#[must_use]
pub fn format_keyboard(formats: &[BotFormat]) -> Keyboard {
    let mut rows: Vec<Button> = formats
        .iter()
        .map(|f| Button::new(&*f.id, format!("cfg:set:format:{}", f.id)))
        .collect();
    rows.push(Button::new(BACK, "cfg:menu:main"));
    Keyboard::column(rows)
}

/// The quality keyboard for the chat's **current** format, then `Back`.
#[must_use]
pub fn quality_keyboard(formats: &[BotFormat], format_id: &str) -> Keyboard {
    let mut rows: Vec<Button> = qualities_for(formats, format_id)
        .iter()
        .map(|q| Button::new(&**q, format!("cfg:set:quality:{q}")))
        .collect();
    rows.push(Button::new(BACK, "cfg:menu:main"));
    Keyboard::column(rows)
}

/// The playlist-limit keyboard: `0 1 5 10 20`, then `Back`.
#[must_use]
pub fn limit_keyboard() -> Keyboard {
    let mut rows: Vec<Button> = LIMIT_CHOICES
        .iter()
        .map(|v| Button::new(v.to_string(), format!("cfg:set:limit:{v}")))
        .collect();
    rows.push(Button::new(BACK, "cfg:menu:main"));
    Keyboard::column(rows)
}

/// The qualities a button offers, or `["best"]` when the id is unknown.
///
/// The fallback is legacy's (`_get_format_qualities` returned `["best"]` on a miss), and it is what
/// keeps an imported `captions` chat config from producing an empty keyboard.
#[must_use]
pub fn qualities_for(formats: &[BotFormat], format_id: &str) -> Vec<Box<str>> {
    formats
        .iter()
        .find(|f| &*f.id == format_id)
        .map_or_else(|| vec![Box::from("best")], |f| f.qualities.clone())
}

/// What the actor should render after handling a `cfg:` callback.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Screen {
    /// The config text plus the main keyboard.
    Main,
    /// `Select format` plus the format keyboard.
    Format,
    /// `Select quality` plus the current format's quality keyboard.
    Quality,
    /// `Select playlist limit` plus the limit keyboard.
    Limit,
}

/// The result of one `cfg:` callback.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Applied {
    /// Which screen to show, or `None` when the payload was not understood (legacy fell through
    /// and rendered nothing at all).
    pub screen: Option<Screen>,
    /// Whether the chat config changed and must be persisted.
    pub changed: bool,
}

impl Applied {
    /// Nothing understood, nothing rendered.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            screen: None,
            changed: false,
        }
    }

    const fn show(screen: Screen) -> Self {
        Self {
            screen: Some(screen),
            changed: false,
        }
    }

    const fn changed(screen: Screen) -> Self {
        Self {
            screen: Some(screen),
            changed: true,
        }
    }
}

/// Applies one `cfg:` callback payload to `cfg` (DESIGN §12.2).
///
/// The grammar is unchanged from legacy: `cfg:menu:{main,format,quality,limit}`,
/// `cfg:set:{format,quality,limit}:{value}` and `cfg:toggle:split`. There is no
/// `cfg:menu:download_type` and no `cfg:set:download_type`, because in the bot's flat projection a
/// format id *is* a download type: `m4a` is how a user reaches audio, `thumbnail` is how they
/// reach a thumbnail, and `any` + the `audio` pseudo-quality is the legacy shortcut.
#[must_use]
pub fn apply_callback(data: &str, cfg: &mut ChatConfig, formats: &[BotFormat]) -> Applied {
    let parts: Vec<&str> = data.split(':').collect();
    // Legacy: `if len(parts) < 3: return` — after answering the callback query.
    if parts.len() < 3 || parts[0] != "cfg" {
        return Applied::ignored();
    }
    let (action, target) = (parts[1], parts[2]);

    match (action, target) {
        ("menu", "main") => Applied::show(Screen::Main),
        ("menu", "format") => Applied::show(Screen::Format),
        ("menu", "quality") => Applied::show(Screen::Quality),
        ("menu", "limit") => Applied::show(Screen::Limit),
        ("toggle", "split") => {
            cfg.split_by_chapters = !cfg.split_by_chapters;
            Applied::changed(Screen::Main)
        }
        ("set", _) if parts.len() >= 4 => {
            let value = parts[3];
            match target {
                "format" => {
                    cfg.format = value.into();
                    // Legacy: if the current quality is not in the new format's list, reset to the
                    // first one it offers.
                    let available = qualities_for(formats, value);
                    if !available.iter().any(|q| **q == *cfg.quality)
                        && let Some(first) = available.first()
                    {
                        cfg.quality = first.clone();
                    }
                }
                "quality" => {
                    // Legacy: set only if the value is in the *current* format's list, else ignore.
                    if qualities_for(formats, &cfg.format)
                        .iter()
                        .any(|q| **q == *value)
                    {
                        cfg.quality = value.into();
                    }
                }
                "limit" => {
                    // Legacy: `int(value)`, ignoring a `ValueError`.
                    if let Ok(n) = value.parse::<u32>() {
                        cfg.playlist_item_limit = n;
                    }
                }
                _ => return Applied::ignored(),
            }
            Applied::changed(Screen::Main)
        }
        // `cfg:set:format` with no value, and anything else: legacy answered the query and
        // rendered nothing.
        _ => Applied::ignored(),
    }
}

#[cfg(test)]
// Most of these tests assert on the mutated `ChatConfig`, not on the returned `Applied`.
#[allow(clippy::expect_used, unused_must_use, clippy::let_underscore_must_use)]
mod tests {
    use aulos_core::catalog::ytdlp_catalog;
    use aulos_core::selection::DownloadType;

    use super::*;

    fn bot_formats() -> Vec<BotFormat> {
        ytdlp_catalog().bot_formats()
    }

    fn cfg() -> ChatConfig {
        ChatConfig::legacy_defaults(0, "")
    }

    #[test]
    fn the_start_text_is_byte_identical() {
        assert_eq!(
            START_TEXT,
            "Hi! Send one or more links and I will queue them for download.\n\
             Use /config to set default format/quality for this chat."
        );
    }

    #[test]
    fn the_config_text_is_byte_identical() {
        let mut c = cfg();
        c.playlist_item_limit = 5;
        assert_eq!(
            config_text(&c),
            "Current download config:\n\
             - Format: mp4\n\
             - Quality: best\n\
             - Split by chapters: off\n\
             - Playlist item limit: 5"
        );
        c.split_by_chapters = true;
        assert!(config_text(&c).contains("- Split by chapters: on"));
    }

    #[test]
    fn the_main_keyboard_is_the_legacy_four_buttons() {
        let k = main_keyboard(&cfg());
        assert_eq!(
            k.labels(),
            vec![
                "Format: mp4",
                "Quality: best",
                "Split Chapters: off",
                "Playlist Limit: 0"
            ]
        );
        assert_eq!(
            k.payloads(),
            vec![
                "cfg:menu:format",
                "cfg:menu:quality",
                "cfg:toggle:split",
                "cfg:menu:limit"
            ]
        );
        assert_eq!(k.0.len(), 4, "one button per row, as legacy");
    }

    /// DESIGN §12.2: `bot_formats()` drives the keyboard, and it is the legacy **nine** in legacy
    /// order — not the catalog's sixteen ids.
    #[test]
    fn the_format_keyboard_is_the_legacy_nine_plus_back() {
        let k = format_keyboard(&bot_formats());
        assert_eq!(
            k.labels(),
            vec![
                "any",
                "mp4",
                "ios",
                "m4a",
                "mp3",
                "opus",
                "wav",
                "flac",
                "thumbnail",
                "Back",
            ]
        );
        assert_eq!(
            k.payloads(),
            vec![
                "cfg:set:format:any",
                "cfg:set:format:mp4",
                "cfg:set:format:ios",
                "cfg:set:format:m4a",
                "cfg:set:format:mp3",
                "cfg:set:format:opus",
                "cfg:set:format:wav",
                "cfg:set:format:flac",
                "cfg:set:format:thumbnail",
                "cfg:menu:main",
            ]
        );
        assert!(
            !k.labels().contains(&"jpg"),
            "the button keeps the legacy id, not the catalog's `jpg`"
        );
        for caption in ["captions", "srt", "vtt", "ass"] {
            assert!(
                !k.labels().contains(&caption),
                "{caption} must be unreachable from the keyboard"
            );
        }
    }

    /// The two projection quirks the grammar depends on.
    #[test]
    fn any_offers_audio_and_mp4_offers_best_remux_while_ios_offers_only_best() {
        let formats = bot_formats();
        let any = qualities_for(&formats, "any");
        assert_eq!(
            any.iter().map(|q| &**q).collect::<Vec<_>>(),
            vec![
                "best", "2160", "1440", "1080", "720", "480", "360", "240", "worst", "audio",
            ],
            "`audio` is the legacy pseudo-quality, appended last"
        );
        let mp4 = qualities_for(&formats, "mp4");
        assert!(
            mp4.iter().any(|q| &**q == "best_remux"),
            "mp4 carries best_remux: {mp4:?}"
        );
        assert!(!any.iter().any(|q| &**q == "best_remux"));
        assert_eq!(
            qualities_for(&formats, "ios")
                .iter()
                .map(|q| &**q)
                .collect::<Vec<_>>(),
            vec!["best"],
            "the bot is the parity surface; the API is the honest one"
        );
        assert_eq!(
            qualities_for(&formats, "thumbnail")
                .iter()
                .map(|q| &**q)
                .collect::<Vec<_>>(),
            vec!["best"]
        );
        assert_eq!(
            qualities_for(&formats, "m4a")
                .iter()
                .map(|q| &**q)
                .collect::<Vec<_>>(),
            vec!["best", "192", "128"]
        );
    }

    #[test]
    fn an_unknown_format_falls_back_to_best_alone() {
        assert_eq!(
            qualities_for(&bot_formats(), "captions")
                .iter()
                .map(|q| &**q)
                .collect::<Vec<_>>(),
            vec!["best"],
            "legacy `_get_format_qualities` returned ['best'] on a miss"
        );
    }

    #[test]
    fn the_quality_and_limit_keyboards_end_with_back() {
        let k = quality_keyboard(&bot_formats(), "mp3");
        assert_eq!(k.labels(), vec!["best", "320", "192", "128", "Back"]);
        assert_eq!(
            k.payloads(),
            vec![
                "cfg:set:quality:best",
                "cfg:set:quality:320",
                "cfg:set:quality:192",
                "cfg:set:quality:128",
                "cfg:menu:main",
            ]
        );

        let l = limit_keyboard();
        assert_eq!(l.labels(), vec!["0", "1", "5", "10", "20", "Back"]);
        assert_eq!(
            l.payloads(),
            vec![
                "cfg:set:limit:0",
                "cfg:set:limit:1",
                "cfg:set:limit:5",
                "cfg:set:limit:10",
                "cfg:set:limit:20",
                "cfg:menu:main",
            ]
        );
    }

    #[test]
    fn the_menu_verbs_only_navigate() {
        let formats = bot_formats();
        for (data, screen) in [
            ("cfg:menu:main", Screen::Main),
            ("cfg:menu:format", Screen::Format),
            ("cfg:menu:quality", Screen::Quality),
            ("cfg:menu:limit", Screen::Limit),
        ] {
            let mut c = cfg();
            let applied = apply_callback(data, &mut c, &formats);
            assert_eq!(applied.screen, Some(screen), "{data}");
            assert!(!applied.changed, "{data} must not mutate");
            assert_eq!(c, cfg());
        }
    }

    /// DESIGN §12.2: changing the format resets a quality the new format does not offer.
    #[test]
    fn changing_the_format_resets_an_unavailable_quality() {
        let formats = bot_formats();
        let mut c = cfg();
        c.quality = "1080".into();

        let applied = apply_callback("cfg:set:format:mp3", &mut c, &formats);
        assert_eq!(applied.screen, Some(Screen::Main));
        assert!(applied.changed);
        assert_eq!(&*c.format, "mp3");
        assert_eq!(&*c.quality, "best", "1080 is not an mp3 quality");

        // A quality both formats offer survives the switch.
        c.quality = "192".into();
        apply_callback("cfg:set:format:m4a", &mut c, &formats);
        assert_eq!(&*c.format, "m4a");
        assert_eq!(&*c.quality, "192");
    }

    #[test]
    fn an_out_of_list_quality_is_ignored() {
        let formats = bot_formats();
        let mut c = cfg(); // mp4 / best
        apply_callback("cfg:set:quality:9999", &mut c, &formats);
        assert_eq!(&*c.quality, "best", "not in the mp4 list, so ignored");
        apply_callback("cfg:set:quality:1080", &mut c, &formats);
        assert_eq!(&*c.quality, "1080");
        // `audio` is not an mp4 quality; it belongs to `any`.
        apply_callback("cfg:set:quality:audio", &mut c, &formats);
        assert_eq!(&*c.quality, "1080");
    }

    /// `cfg:set:quality:audio` on `any` stores the legacy pair, and the normaliser turns it into
    /// `(audio, m4a, best)` (DESIGN §12.2, §12.3 step 4).
    #[test]
    fn the_audio_pseudo_quality_stores_the_legacy_pair_and_normalises() {
        let formats = bot_formats();
        let mut c = cfg();
        apply_callback("cfg:set:format:any", &mut c, &formats);
        assert_eq!(&*c.format, "any");
        apply_callback("cfg:set:quality:audio", &mut c, &formats);
        assert_eq!(
            (&*c.format, &*c.quality),
            ("any", "audio"),
            "the flat legacy pair is what is stored"
        );

        let selection = c.selection();
        assert_eq!(selection.download_type, DownloadType::Audio);
        assert_eq!(selection.format.as_str(), "m4a");
        assert_eq!(selection.quality.as_str(), "best");
    }

    /// The thumbnail button keeps the legacy id and resolves to `(thumbnail, jpg, best)`.
    #[test]
    fn the_thumbnail_button_resolves_to_the_catalog_id() {
        let formats = bot_formats();
        let mut c = cfg();
        apply_callback("cfg:set:format:thumbnail", &mut c, &formats);
        assert_eq!(&*c.format, "thumbnail");
        let selection = c.selection();
        assert_eq!(selection.download_type, DownloadType::Thumbnail);
        assert_eq!(selection.format.as_str(), "jpg");
        assert_eq!(selection.quality.as_str(), "best");
    }

    /// An imported chat whose stored format is `captions` keeps working, even though the keyboard
    /// cannot select it (DESIGN §12.2).
    #[test]
    fn an_imported_captions_chat_keeps_working() {
        let mut c = cfg();
        c.format = "captions".into();
        let selection = c.selection();
        assert_eq!(selection.download_type, DownloadType::Captions);
        assert_eq!(selection.format.as_str(), "srt");

        // And the config screen still renders, showing the stored id.
        assert!(config_text(&c).contains("- Format: captions"));
        // The quality keyboard degrades to `best` rather than being empty.
        assert_eq!(
            quality_keyboard(&bot_formats(), &c.format).labels(),
            vec!["best", "Back"]
        );
    }

    #[test]
    fn the_split_toggle_flips() {
        let formats = bot_formats();
        let mut c = cfg();
        assert!(!c.split_by_chapters);
        let applied = apply_callback("cfg:toggle:split", &mut c, &formats);
        assert!(applied.changed);
        assert_eq!(applied.screen, Some(Screen::Main));
        assert!(c.split_by_chapters);
        apply_callback("cfg:toggle:split", &mut c, &formats);
        assert!(!c.split_by_chapters);
    }

    #[test]
    fn a_bad_limit_is_ignored_and_a_good_one_is_stored() {
        let formats = bot_formats();
        let mut c = cfg();
        apply_callback("cfg:set:limit:notanumber", &mut c, &formats);
        assert_eq!(c.playlist_item_limit, 0);
        apply_callback("cfg:set:limit:20", &mut c, &formats);
        assert_eq!(c.playlist_item_limit, 20);
        apply_callback("cfg:set:limit:-3", &mut c, &formats);
        assert_eq!(c.playlist_item_limit, 20, "negatives are not accepted");
    }

    /// There is no `download_type` verb, and a malformed payload changes nothing.
    #[test]
    fn unknown_payloads_are_ignored() {
        let formats = bot_formats();
        for data in [
            "cfg:menu:download_type",
            "cfg:set:download_type:audio",
            "cfg:set:format",
            "cfg:set",
            "cfg",
            "other:menu:main",
            "",
        ] {
            let mut c = cfg();
            let applied = apply_callback(data, &mut c, &formats);
            assert_eq!(applied, Applied::ignored(), "{data}");
            assert_eq!(c, cfg(), "{data} must not mutate");
        }
    }
}
