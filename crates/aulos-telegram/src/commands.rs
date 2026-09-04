//! What arrives from Telegram, and the message → jobs pipeline (DESIGN §12.3).
//!
//! [`Incoming`] is the actor's whole input vocabulary. The long-polling loop translates
//! `teloxide::types::Update` into it and the tests construct it directly, which is what lets every
//! command and callback text be asserted byte-for-byte without a token.

use aulos_core::paths::RelDir;
use aulos_core::request::{DownloadRequest, SubtitleLang, SubtitleMode};
use aulos_core::telegram::ChatConfig;

use crate::render::notify;
use crate::urls;

/// The two commands legacy registered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// `/start`.
    Start,
    /// `/config`.
    Config,
}

impl Command {
    /// Parses a message's text as a command.
    ///
    /// Accepts the `@BotName` suffix Telegram appends in groups, and nothing else — legacy used
    /// `CommandHandler`, which does the same.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let first = text.split_whitespace().next()?;
        let bare = first.split('@').next()?;
        match bare {
            "/start" => Some(Self::Start),
            "/config" => Some(Self::Config),
            _ => None,
        }
    }
}

/// One update, reduced to what the actor acts on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Incoming {
    /// `/start` or `/config`.
    Command {
        /// Which chat it came from.
        chat: i64,
        /// Which command.
        command: Command,
    },
    /// A `cfg:` inline-keyboard press.
    Callback {
        /// Which chat.
        chat: i64,
        /// The message the keyboard is attached to — the one the reply edits, as legacy's
        /// `query.edit_message_text` did.
        message: crate::transport::MessageId,
        /// The query id, which must be answered.
        query_id: String,
        /// The callback payload.
        data: String,
    },
    /// A plain text message, which may carry links.
    Text {
        /// Which chat.
        chat: i64,
        /// The message body.
        text: String,
    },
}

impl Incoming {
    /// Which chat this update is from — the value the allow-list is checked against.
    #[must_use]
    pub const fn chat(&self) -> i64 {
        match self {
            Self::Command { chat, .. } | Self::Callback { chat, .. } | Self::Text { chat, .. } => {
                *chat
            }
        }
    }
}

/// What one text message turns into (DESIGN §12.3).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct MessagePlan {
    /// The over-the-limit reply, when the message had more links than `TELEGRAM_MAX_URLS_PER_MESSAGE`.
    pub too_many: Option<String>,
    /// The `Ignored invalid links:` reply, when the guard refused any.
    pub ignored: Option<String>,
    /// One request per accepted URL, in message order. All of them go out in **one**
    /// `EngineCmd::Add`.
    pub requests: Vec<DownloadRequest>,
}

impl MessagePlan {
    /// Whether there is nothing at all to do — not even a reply.
    #[must_use]
    pub fn is_silent(&self) -> bool {
        self.too_many.is_none() && self.ignored.is_none() && self.requests.is_empty()
    }
}

/// Steps 1–4 of DESIGN §12.3: extract, cap, guard, then build one request per surviving URL.
///
/// A message with no links at all produces an empty plan and **no** reply, exactly as legacy: the
/// bot is used in chats where people also talk.
#[must_use]
pub fn plan_message(text: &str, cfg: &ChatConfig, max_urls: u32) -> MessagePlan {
    let mut plan = MessagePlan::default();
    let mut found = urls::extract(text);
    if found.is_empty() {
        return plan;
    }

    let cap = max_urls as usize;
    if cap > 0 && found.len() > cap {
        plan.too_many = Some(notify::too_many_urls(found.len(), max_urls));
        found.truncate(cap);
    }

    let mut rejected: Vec<(String, String)> = Vec::new();
    let selection = cfg.selection();
    for raw in found {
        match urls::validate(&raw) {
            Ok(url) => plan.requests.push(request_from(url, cfg, &selection)),
            Err(reason) => rejected.push((raw, reason.to_string())),
        }
    }
    if !rejected.is_empty() {
        plan.ignored = Some(notify::ignored(&rejected));
    }
    plan
}

/// The request one URL is queued with, built from the chat's stored defaults.
fn request_from(
    url: url::Url,
    cfg: &ChatConfig,
    selection: &aulos_core::selection::Selection,
) -> DownloadRequest {
    DownloadRequest {
        url,
        selection: selection.clone(),
        // An unusable stored folder falls back to the base directory rather than failing the add:
        // the value came from a JSON file a human may have edited (DESIGN §7.6.5).
        folder: (!cfg.folder.trim().is_empty())
            .then(|| RelDir::parse(&cfg.folder).ok())
            .flatten(),
        custom_name_prefix: cfg.custom_name_prefix.clone(),
        playlist_item_limit: cfg.playlist_item_limit,
        auto_start: cfg.auto_start,
        split_by_chapters: cfg.split_by_chapters,
        chapter_template: cfg.chapter_template.clone(),
        subtitle_language: SubtitleLang::parse(&cfg.subtitle_language)
            .unwrap_or_else(|_| SubtitleLang::english()),
        subtitle_mode: SubtitleMode::from_str_exact(&cfg.subtitle_mode)
            .unwrap_or(SubtitleMode::PreferManual),
        ytdl_options_presets: Vec::new(),
        ytdl_options_overrides: serde_json::Map::new(),
        provider_hint: None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use aulos_core::selection::DownloadType;

    use super::*;

    fn cfg() -> ChatConfig {
        ChatConfig::legacy_defaults(0, "%(title)s - %(section_number)02d.%(ext)s")
    }

    #[test]
    fn the_two_commands_parse_with_and_without_a_bot_suffix() {
        assert_eq!(Command::parse("/start"), Some(Command::Start));
        assert_eq!(Command::parse("  /config  "), Some(Command::Config));
        assert_eq!(Command::parse("/start@AulosBot"), Some(Command::Start));
        assert_eq!(
            Command::parse("/config@AulosBot extra"),
            Some(Command::Config)
        );
        assert_eq!(Command::parse("/help"), None);
        assert_eq!(Command::parse("hello"), None);
        assert_eq!(Command::parse(""), None);
        assert_eq!(Command::parse("start"), None, "the slash is required");
    }

    #[test]
    fn an_incoming_update_names_its_chat() {
        assert_eq!(
            Incoming::Command {
                chat: -100,
                command: Command::Start
            }
            .chat(),
            -100
        );
        assert_eq!(
            Incoming::Text {
                chat: 7,
                text: String::new()
            }
            .chat(),
            7
        );
        assert_eq!(
            Incoming::Callback {
                chat: 9,
                message: crate::transport::MessageId(1),
                query_id: "q".to_owned(),
                data: "cfg:menu:main".to_owned()
            }
            .chat(),
            9
        );
    }

    #[test]
    fn a_message_with_no_links_produces_no_reply_at_all() {
        let plan = plan_message("just chatting", &cfg(), 10);
        assert!(plan.is_silent());
        assert!(plan_message("", &cfg(), 10).is_silent());
    }

    /// DESIGN §12.3 step 5: one message with three URLs produces **one** batch of three requests.
    #[test]
    fn three_urls_become_three_requests_in_message_order() {
        let plan = plan_message(
            "https://a.test/1 and https://b.test/2 plus https://c.test/3",
            &cfg(),
            10,
        );
        assert_eq!(plan.requests.len(), 3);
        assert_eq!(
            plan.requests
                .iter()
                .map(|r| r.url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://a.test/1", "https://b.test/2", "https://c.test/3"]
        );
        assert!(plan.too_many.is_none());
        assert!(plan.ignored.is_none());
    }

    /// DESIGN §12.3 step 2: over the cap produces the exact message and truncates.
    #[test]
    fn over_the_max_urls_limit_the_message_is_exact_and_the_list_is_truncated() {
        let text = (1..=14)
            .map(|i| format!("https://a.test/{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let plan = plan_message(&text, &cfg(), 10);
        assert_eq!(
            plan.too_many.as_deref(),
            Some("Too many links in one message (14). Maximum allowed: 10.")
        );
        assert_eq!(plan.requests.len(), 10);
        assert_eq!(plan.requests[9].url.as_str(), "https://a.test/10");
    }

    #[test]
    fn exactly_the_limit_is_not_over_it() {
        let text = (1..=10)
            .map(|i| format!("https://a.test/{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let plan = plan_message(&text, &cfg(), 10);
        assert!(plan.too_many.is_none());
        assert_eq!(plan.requests.len(), 10);
    }

    #[test]
    fn a_zero_limit_means_unlimited() {
        let text = (1..=30)
            .map(|i| format!("https://a.test/{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let plan = plan_message(&text, &cfg(), 0);
        assert!(plan.too_many.is_none());
        assert_eq!(plan.requests.len(), 30);
    }

    /// DESIGN §12.3 step 3: rejected URLs are reported with their reason, and the good ones still
    /// go through.
    #[test]
    fn rejected_urls_are_reported_and_the_rest_survive() {
        let plan = plan_message(
            "https://good.test/1 http://127.0.0.1/x https://good.test/2 http://0.0.0.0/y",
            &cfg(),
            10,
        );
        assert_eq!(plan.requests.len(), 2);
        assert_eq!(
            plan.ignored.as_deref(),
            Some(
                "Ignored invalid links:\n\
                 - http://127.0.0.1/x (private/local IP targets are not allowed)\n\
                 - http://0.0.0.0/y (private/local IP targets are not allowed)"
            )
        );
    }

    #[test]
    fn a_message_of_only_bad_links_replies_but_queues_nothing() {
        let plan = plan_message("http://localhost/x", &cfg(), 10);
        assert!(plan.requests.is_empty());
        assert!(plan.ignored.is_some());
        assert!(!plan.is_silent());
    }

    /// The stored chat config is what every request carries.
    #[test]
    fn the_request_carries_the_chats_stored_defaults() {
        let mut c = cfg();
        c.custom_name_prefix = "TG - ".into();
        c.playlist_item_limit = 5;
        c.split_by_chapters = true;
        c.auto_start = false;
        c.subtitle_language = "it".into();
        c.subtitle_mode = "manual_only".into();
        c.folder = "Shows".into();

        let plan = plan_message("https://a.test/1", &c, 10);
        let r = &plan.requests[0];
        assert_eq!(&*r.custom_name_prefix, "TG - ");
        assert_eq!(r.playlist_item_limit, 5);
        assert!(r.split_by_chapters);
        assert!(!r.auto_start);
        assert_eq!(r.subtitle_language.as_str(), "it");
        assert_eq!(r.subtitle_mode, SubtitleMode::ManualOnly);
        assert_eq!(
            r.folder.as_ref().map(aulos_core::RelDir::as_str),
            Some("Shows")
        );
        assert_eq!(
            &*r.chapter_template,
            "%(title)s - %(section_number)02d.%(ext)s"
        );
    }

    /// An imported config's flat pair is normalised on read, so `any` + `audio` becomes real audio.
    #[test]
    fn the_selection_comes_from_the_normaliser() {
        let mut c = cfg();
        c.format = "any".into();
        c.quality = "audio".into();
        let plan = plan_message("https://a.test/1", &c, 10);
        let s = &plan.requests[0].selection;
        assert_eq!(s.download_type, DownloadType::Audio);
        assert_eq!(s.format.as_str(), "m4a");
        assert_eq!(s.quality.as_str(), "best");
    }

    #[test]
    fn a_hand_edited_folder_or_language_falls_back_rather_than_failing() {
        let mut c = cfg();
        c.folder = "../escape".into();
        c.subtitle_language = "not a tag!".into();
        c.subtitle_mode = "nonsense".into();
        let plan = plan_message("https://a.test/1", &c, 10);
        let r = &plan.requests[0];
        assert_eq!(r.folder, None, "an escaping folder becomes the base dir");
        assert_eq!(r.subtitle_language.as_str(), "en");
        assert_eq!(r.subtitle_mode, SubtitleMode::PreferManual);
    }

    #[test]
    fn the_bot_never_sends_presets_or_overrides() {
        let plan = plan_message("https://a.test/1", &cfg(), 10);
        assert!(plan.requests[0].ytdl_options_presets.is_empty());
        assert!(plan.requests[0].ytdl_options_overrides.is_empty());
        assert_eq!(plan.requests[0].provider_hint, None);
    }
}
