//! The Bot API seam: the three calls the actor makes, behind a trait.
//!
//! DESIGN §12.1 names `teloxide::Bot` as the field type. It is a trait here for one reason: the
//! WP-16 acceptance list requires every command text, every callback text and the whole rate-limit
//! behaviour to be asserted **against a mocked transport, never a real token** — and a `Bot` can
//! only be exercised by talking to `api.telegram.org`. [`TeloxideTransport`] is the one
//! implementation that ships; [`MockTransport`] is what the tests run on.
//!
//! Keeping the surface to three calls is deliberate: `sendMessage`, `editMessageText` and
//! `answerCallbackQuery` are the whole of what DESIGN §12 needs, and each maps 1:1 to a Bot API
//! method, so the adapter has no logic to get wrong.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::config_ui::Keyboard;

/// A Telegram message id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MessageId(pub i32);

/// What a Bot API call can fail with, reduced to the four cases DESIGN §12.4 reacts to.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum TgError {
    /// `429`, with the server's `retry_after`. Must be respected or throttling escalates.
    #[error("retry after {0:?}")]
    RetryAfter(Duration),
    /// `400 Bad Request: message is not modified`.
    ///
    /// The limiter's `last_rendered` check exists to make this unreachable; it is still a variant
    /// because Telegram is the authority on whether a message changed, not us.
    #[error("message is not modified")]
    NotModified,
    /// Any other API-level rejection. Not retried.
    #[error("{0}")]
    Api(Box<str>),
    /// A transport-level failure. Retried with backoff, then the edit is dropped.
    #[error("{0}")]
    Network(Box<str>),
}

impl TgError {
    /// Whether another attempt could plausibly succeed.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Network(_))
    }
}

/// The three Bot API calls the actor makes.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// `sendMessage`.
    ///
    /// # Errors
    /// Any [`TgError`].
    async fn send_message(
        &self,
        chat: i64,
        text: &str,
        keyboard: Option<&Keyboard>,
    ) -> Result<MessageId, TgError>;

    /// `editMessageText`.
    ///
    /// # Errors
    /// Any [`TgError`], in particular [`TgError::RetryAfter`] and [`TgError::NotModified`].
    async fn edit_message_text(
        &self,
        chat: i64,
        message: MessageId,
        text: &str,
        keyboard: Option<&Keyboard>,
    ) -> Result<(), TgError>;

    /// `answerCallbackQuery`.
    ///
    /// # Errors
    /// Any [`TgError`].
    async fn answer_callback_query(&self, query_id: &str) -> Result<(), TgError>;
}

// ---------------------------------------------------------------------------
// The mock.
// ---------------------------------------------------------------------------

/// One recorded Bot API call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Call {
    /// `sendMessage`.
    Send {
        /// Which chat.
        chat: i64,
        /// The text as sent.
        text: String,
        /// The keyboard, when one was attached.
        keyboard: Option<Keyboard>,
    },
    /// `editMessageText`.
    Edit {
        /// Which chat.
        chat: i64,
        /// Which message.
        message: MessageId,
        /// The text as sent.
        text: String,
        /// The keyboard, when one was attached.
        keyboard: Option<Keyboard>,
    },
    /// `answerCallbackQuery`.
    Answer {
        /// The query id.
        query_id: String,
    },
}

impl Call {
    /// The text of a `Send` or `Edit`, or `""`.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Send { text, .. } | Self::Edit { text, .. } => text,
            Self::Answer { .. } => "",
        }
    }

    /// The keyboard of a `Send` or `Edit`.
    #[must_use]
    pub const fn keyboard(&self) -> Option<&Keyboard> {
        match self {
            Self::Send { keyboard, .. } | Self::Edit { keyboard, .. } => keyboard.as_ref(),
            Self::Answer { .. } => None,
        }
    }

    /// The chat of a `Send` or `Edit`.
    #[must_use]
    pub const fn chat(&self) -> Option<i64> {
        match self {
            Self::Send { chat, .. } | Self::Edit { chat, .. } => Some(*chat),
            Self::Answer { .. } => None,
        }
    }
}

/// A recording [`Transport`] with a scriptable failure queue. Never touches the network.
#[derive(Debug, Default)]
pub struct MockTransport {
    calls: Mutex<Vec<Call>>,
    /// Errors to return, oldest first, for `send`/`edit` only.
    failures: Mutex<Vec<TgError>>,
    next_message_id: Mutex<i32>,
}

impl MockTransport {
    /// An empty recorder that always succeeds.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
            next_message_id: Mutex::new(1_000),
        })
    }

    /// Queues errors for the next `send`/`edit` calls, in order.
    pub fn fail_next(&self, errors: Vec<TgError>) {
        *self.lock_failures() = errors;
    }

    /// Every call made so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.lock_calls().clone()
    }

    /// How many calls have been made.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock_calls().len()
    }

    /// Every text sent or edited, in order.
    #[must_use]
    pub fn texts(&self) -> Vec<String> {
        self.lock_calls()
            .iter()
            .filter(|c| !matches!(c, Call::Answer { .. }))
            .map(|c| c.text().to_owned())
            .collect()
    }

    /// The `edit` calls only.
    #[must_use]
    pub fn edits(&self) -> Vec<Call> {
        self.lock_calls()
            .iter()
            .filter(|c| matches!(c, Call::Edit { .. }))
            .cloned()
            .collect()
    }

    /// Forgets everything recorded so far.
    pub fn clear(&self) {
        self.lock_calls().clear();
    }

    fn lock_calls(&self) -> std::sync::MutexGuard<'_, Vec<Call>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_failures(&self) -> std::sync::MutexGuard<'_, Vec<TgError>> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take_failure(&self) -> Option<TgError> {
        let mut queue = self.lock_failures();
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    fn mint(&self) -> MessageId {
        let mut next = self
            .next_message_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *next += 1;
        MessageId(*next)
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send_message(
        &self,
        chat: i64,
        text: &str,
        keyboard: Option<&Keyboard>,
    ) -> Result<MessageId, TgError> {
        if let Some(e) = self.take_failure() {
            return Err(e);
        }
        self.lock_calls().push(Call::Send {
            chat,
            text: text.to_owned(),
            keyboard: keyboard.cloned(),
        });
        Ok(self.mint())
    }

    async fn edit_message_text(
        &self,
        chat: i64,
        message: MessageId,
        text: &str,
        keyboard: Option<&Keyboard>,
    ) -> Result<(), TgError> {
        if let Some(e) = self.take_failure() {
            return Err(e);
        }
        self.lock_calls().push(Call::Edit {
            chat,
            message,
            text: text.to_owned(),
            keyboard: keyboard.cloned(),
        });
        Ok(())
    }

    async fn answer_callback_query(&self, query_id: &str) -> Result<(), TgError> {
        self.lock_calls().push(Call::Answer {
            query_id: query_id.to_owned(),
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The real thing.
// ---------------------------------------------------------------------------

/// The shipping [`Transport`]: `teloxide::Bot`.
#[derive(Clone)]
pub struct TeloxideTransport {
    bot: teloxide::Bot,
}

impl std::fmt::Debug for TeloxideTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TeloxideTransport")
    }
}

impl TeloxideTransport {
    /// Wraps a bot.
    #[must_use]
    pub const fn new(bot: teloxide::Bot) -> Self {
        Self { bot }
    }

    /// The bot, for the long-polling loop.
    pub const fn bot(&self) -> &teloxide::Bot {
        &self.bot
    }
}

/// Maps a `teloxide` failure onto the four cases the actor reacts to.
#[must_use]
pub fn map_error(e: &teloxide::RequestError) -> TgError {
    use teloxide::RequestError;
    match e {
        RequestError::RetryAfter(secs) => TgError::RetryAfter(secs.duration()),
        RequestError::Network(inner) => TgError::Network(inner.to_string().into_boxed_str()),
        RequestError::Api(api) => {
            let text = api.to_string();
            // Telegram spells this one `Bad Request: message is not modified: …`.
            if text.contains("message is not modified") {
                TgError::NotModified
            } else {
                TgError::Api(text.into_boxed_str())
            }
        }
        other => TgError::Api(other.to_string().into_boxed_str()),
    }
}

#[async_trait]
impl Transport for TeloxideTransport {
    async fn send_message(
        &self,
        chat: i64,
        text: &str,
        keyboard: Option<&Keyboard>,
    ) -> Result<MessageId, TgError> {
        use teloxide::payloads::SendMessageSetters;
        use teloxide::prelude::Requester;

        let request = self.bot.send_message(teloxide::types::ChatId(chat), text);
        let message = match keyboard {
            Some(k) => request.reply_markup(to_markup(k)).await,
            None => request.await,
        }
        .map_err(|e| map_error(&e))?;
        Ok(MessageId(message.id.0))
    }

    async fn edit_message_text(
        &self,
        chat: i64,
        message: MessageId,
        text: &str,
        keyboard: Option<&Keyboard>,
    ) -> Result<(), TgError> {
        use teloxide::payloads::EditMessageTextSetters;
        use teloxide::prelude::Requester;

        let request = self.bot.edit_message_text(
            teloxide::types::ChatId(chat),
            teloxide::types::MessageId(message.0),
            text,
        );
        match keyboard {
            Some(k) => request.reply_markup(to_markup(k)).await,
            None => request.await,
        }
        .map_err(|e| map_error(&e))?;
        Ok(())
    }

    async fn answer_callback_query(&self, query_id: &str) -> Result<(), TgError> {
        use teloxide::prelude::Requester;

        self.bot
            .answer_callback_query(teloxide::types::CallbackQueryId(query_id.to_owned()))
            .await
            .map_err(|e| map_error(&e))?;
        Ok(())
    }
}

/// Converts a [`Keyboard`] into teloxide's markup.
#[must_use]
pub fn to_markup(keyboard: &Keyboard) -> teloxide::types::InlineKeyboardMarkup {
    use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

    InlineKeyboardMarkup::new(keyboard.0.iter().map(|row| {
        row.iter()
            .map(|b| InlineKeyboardButton::callback(b.text.clone(), b.data.clone()))
            .collect::<Vec<_>>()
    }))
}

// ---------------------------------------------------------------------------
// Long polling.
// ---------------------------------------------------------------------------

/// How long `getUpdates` holds the connection open, in seconds.
///
/// **This must stay comfortably below the HTTP client's own request timeout.**
/// `teloxide::Bot::new` builds its `reqwest::Client` from
/// `teloxide_core::net::default_reqwest_settings`, which sets a 17 s request timeout, so a longer
/// server-side
/// long poll can never complete on a quiet bot: the client aborts the request first and the loop
/// sees a `Network` error, logs a WARN and then sleeps [`POLL_BACKOFF`] — one bogus warning every
/// 20 s in production, plus up to 3 s of added latency for a message that arrives just after the
/// abort. 10 s is teloxide's own polling default and leaves 7 s of headroom.
pub const POLL_TIMEOUT_SECS: u32 = 10;

/// How long the loop waits after a failed `getUpdates` before trying again.
pub const POLL_BACKOFF: Duration = Duration::from_secs(3);

/// An error rendered together with every `source()` under it.
///
/// `reqwest`'s `Display` is the useless half of the story — `error sending request for url (…)` —
/// while the cause that names what actually failed (DNS, a TLS handshake, an elapsed client-side
/// timeout) sits one or two `source()` hops down. Without the chain, a genuine network outage
/// reads exactly like the client-timeout bug [`POLL_TIMEOUT_SECS`] documents, which is how that
/// one survived in production.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(source) = cause {
        out.push_str(": ");
        out.push_str(&source.to_string());
        cause = source.source();
    }
    out
}

/// Translates one `teloxide` update into the actor's vocabulary, or `None` for anything the bot
/// does not act on.
///
/// A separate function so it can be tested against decoded JSON without a network: the mapping is
/// the only part of the polling loop with any logic in it.
#[must_use]
pub fn to_incoming(update: &teloxide::types::Update) -> Option<crate::commands::Incoming> {
    use teloxide::types::UpdateKind;

    match &update.kind {
        UpdateKind::Message(message) => {
            let chat = message.chat.id.0;
            let text = message.text()?;
            Some(match crate::commands::Command::parse(text) {
                Some(command) => crate::commands::Incoming::Command { chat, command },
                None => crate::commands::Incoming::Text {
                    chat,
                    text: text.to_owned(),
                },
            })
        }
        UpdateKind::CallbackQuery(query) => {
            // Only `cfg:` payloads are ours; anything else is answered by nobody, exactly as the
            // legacy `CallbackQueryHandler(pattern=r"^cfg:")` filter did.
            let data = query.data.as_deref()?;
            if !data.starts_with("cfg:") {
                return None;
            }
            let message = query.message.as_ref()?;
            let chat = message.regular_message()?.chat.id.0;
            Some(crate::commands::Incoming::Callback {
                chat,
                message: MessageId(message.id().0),
                query_id: query.id.0.clone(),
                data: data.to_owned(),
            })
        }
        _ => None,
    }
}

/// The long-polling loop (DESIGN §12.1).
///
/// `drop_pending_updates` is legacy parity and matters after a restart: without it the bot replays
/// every link sent while it was down, and re-queues downloads the user has already got. It is
/// implemented the way the Bot API documents for pollers — one throwaway `getUpdates` with
/// `offset = -1` to learn the newest update id — because that is the only mechanism `getUpdates`
/// offers.
///
/// Returns when `shutdown` is cancelled or the actor's channel closes.
pub async fn poll_updates(
    bot: teloxide::Bot,
    updates: tokio::sync::mpsc::Sender<crate::commands::Incoming>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use teloxide::payloads::GetUpdatesSetters;
    use teloxide::prelude::Requester;
    use teloxide::types::AllowedUpdate;

    let allowed = [AllowedUpdate::Message, AllowedUpdate::CallbackQuery];

    // Drop whatever piled up while the bot was down.
    let mut offset: i32 = match bot
        .get_updates()
        .offset(-1)
        .limit(1)
        .timeout(0)
        .allowed_updates(allowed)
        .await
    {
        Ok(batch) => batch
            .last()
            .and_then(|u| i32::try_from(u.id.0).ok())
            .map_or(0, |id| id.saturating_add(1)),
        Err(e) => {
            tracing::warn!(
                "could not drop pending Telegram updates: {}",
                error_chain(&e)
            );
            0
        }
    };
    tracing::info!("Telegram long polling started at offset {offset}");

    loop {
        let request = bot
            .get_updates()
            .offset(offset)
            .timeout(POLL_TIMEOUT_SECS)
            .allowed_updates(allowed);
        let batch = tokio::select! {
            () = shutdown.cancelled() => break,
            result = request => match result {
                Ok(batch) => batch,
                Err(e) => {
                    tracing::warn!("getUpdates failed: {}", error_chain(&e));
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(POLL_BACKOFF) => {}
                    }
                    continue;
                }
            },
        };

        for update in &batch {
            if let Ok(id) = i32::try_from(update.id.0) {
                offset = offset.max(id.saturating_add(1));
            }
            if let Some(incoming) = to_incoming(update)
                && updates.send(incoming).await.is_err()
            {
                tracing::info!("the Telegram actor is gone; stopping long polling");
                return;
            }
        }
    }
    tracing::info!("Telegram long polling stopped");
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config_ui::{Button, Keyboard};

    #[tokio::test]
    async fn the_mock_records_every_call_in_order() {
        let t = MockTransport::new();
        let k = Keyboard::column(vec![Button::new("Back", "cfg:menu:main")]);
        let id = t.send_message(7, "hello", Some(&k)).await.expect("sent");
        t.edit_message_text(7, id, "hello again", None)
            .await
            .expect("edited");
        t.answer_callback_query("q1").await.expect("answered");

        assert_eq!(t.count(), 3);
        assert_eq!(t.texts(), vec!["hello", "hello again"]);
        assert_eq!(t.calls()[0].chat(), Some(7));
        assert_eq!(t.calls()[0].keyboard(), Some(&k));
        assert_eq!(t.edits().len(), 1);
        assert!(matches!(t.calls()[2], Call::Answer { .. }));
        t.clear();
        assert_eq!(t.count(), 0);
    }

    #[tokio::test]
    async fn the_mock_replays_a_scripted_failure_queue() {
        let t = MockTransport::new();
        t.fail_next(vec![
            TgError::RetryAfter(Duration::from_secs(7)),
            TgError::NotModified,
        ]);
        assert_eq!(
            t.send_message(1, "a", None).await.expect_err("scripted"),
            TgError::RetryAfter(Duration::from_secs(7))
        );
        assert_eq!(
            t.send_message(1, "a", None).await.expect_err("scripted"),
            TgError::NotModified
        );
        t.send_message(1, "a", None).await.expect("queue drained");
        assert_eq!(t.count(), 1, "the failures recorded nothing");
    }

    #[test]
    fn only_a_network_failure_is_worth_retrying() {
        assert!(TgError::Network("timeout".into()).retryable());
        assert!(!TgError::RetryAfter(Duration::from_secs(1)).retryable());
        assert!(!TgError::NotModified.retryable());
        assert!(!TgError::Api("Forbidden".into()).retryable());
    }

    #[test]
    fn a_keyboard_maps_onto_teloxide_markup_row_for_row() {
        let k = Keyboard::column(vec![
            Button::new("Format: mp4", "cfg:menu:format"),
            Button::new("Back", "cfg:menu:main"),
        ]);
        let markup = to_markup(&k);
        assert_eq!(markup.inline_keyboard.len(), 2);
        assert_eq!(markup.inline_keyboard[0].len(), 1);
        assert_eq!(markup.inline_keyboard[0][0].text, "Format: mp4");
    }

    /// A decoded Bot API `Update`, so the one piece of logic in the polling loop is covered
    /// without a network. The JSON is the shape Telegram documents.
    fn parse(raw: &str) -> teloxide::types::Update {
        serde_json::from_str(raw).expect("a decodable Update")
    }

    const FROM: &str = r#"{"id":1,"is_bot":false,"first_name":"A"}"#;
    const CHAT: &str = r#"{"id":4242,"type":"private","first_name":"A"}"#;

    #[test]
    fn a_text_message_becomes_a_command_or_a_text_update() {
        let start = parse(&format!(
            r#"{{"update_id":10,"message":{{"message_id":1,"date":1700000000,
                "chat":{CHAT},"from":{FROM},"text":"/start"}}}}"#
        ));
        assert_eq!(
            to_incoming(&start),
            Some(crate::commands::Incoming::Command {
                chat: 4242,
                command: crate::commands::Command::Start
            })
        );

        let link = parse(&format!(
            r#"{{"update_id":11,"message":{{"message_id":2,"date":1700000000,
                "chat":{CHAT},"from":{FROM},"text":"https://a.test/1"}}}}"#
        ));
        assert_eq!(
            to_incoming(&link),
            Some(crate::commands::Incoming::Text {
                chat: 4242,
                text: "https://a.test/1".to_owned()
            })
        );
    }

    #[test]
    fn a_cfg_callback_carries_its_query_id_and_its_message() {
        let update = parse(&format!(
            r#"{{"update_id":12,"callback_query":{{"id":"q9","from":{FROM},
                "chat_instance":"ci","data":"cfg:menu:format",
                "message":{{"message_id":77,"date":1700000000,"chat":{CHAT},"text":"x"}}}}}}"#
        ));
        assert_eq!(
            to_incoming(&update),
            Some(crate::commands::Incoming::Callback {
                chat: 4242,
                message: MessageId(77),
                query_id: "q9".to_owned(),
                data: "cfg:menu:format".to_owned(),
            })
        );
    }

    /// The legacy handler was registered with `pattern=r"^cfg:"`, so anything else is not ours.
    #[test]
    fn a_non_cfg_callback_and_a_non_text_message_are_ignored() {
        let other = parse(&format!(
            r#"{{"update_id":13,"callback_query":{{"id":"q9","from":{FROM},
                "chat_instance":"ci","data":"other:thing",
                "message":{{"message_id":77,"date":1700000000,"chat":{CHAT},"text":"x"}}}}}}"#
        ));
        assert_eq!(to_incoming(&other), None);

        let sticker = parse(&format!(
            r#"{{"update_id":14,"message":{{"message_id":3,"date":1700000000,
                "chat":{CHAT},"from":{FROM},
                "sticker":{{"file_id":"f","file_unique_id":"u","width":1,"height":1,
                  "is_animated":false,"is_video":false,"type":"regular"}}}}}}"#
        ));
        assert_eq!(to_incoming(&sticker), None);
    }

    /// The polling WARN has to name the *cause*, not `reqwest`'s opaque top line: the 17 s
    /// client-timeout regression that [`POLL_TIMEOUT_SECS`] documents looked exactly like a real
    /// outage until the chain was printed.
    #[test]
    fn a_logged_error_carries_every_cause_under_it() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1
                    .as_deref()
                    .map(|l| l as &(dyn std::error::Error + 'static))
            }
        }

        let deep = Layer(
            "error sending request for url (https://api.telegram.org/GetUpdates)",
            Some(Box::new(Layer(
                "operation timed out",
                Some(Box::new(Layer("connection closed", None))),
            ))),
        );
        assert_eq!(
            error_chain(&deep),
            "error sending request for url (https://api.telegram.org/GetUpdates): \
             operation timed out: connection closed"
        );
        assert_eq!(error_chain(&Layer("alone", None)), "alone");
    }

    #[test]
    fn the_error_mapping_recognises_the_cases_the_actor_reacts_to() {
        use teloxide::types::Seconds;
        assert_eq!(
            map_error(&teloxide::RequestError::RetryAfter(Seconds::from_seconds(
                7
            ))),
            TgError::RetryAfter(Duration::from_secs(7))
        );
        let not_modified = teloxide::RequestError::Api(teloxide::ApiError::Unknown(
            "Bad Request: message is not modified".to_owned(),
        ));
        assert_eq!(map_error(&not_modified), TgError::NotModified);
        let other = teloxide::RequestError::Api(teloxide::ApiError::BotBlocked);
        assert!(matches!(map_error(&other), TgError::Api(_)));
    }
}
