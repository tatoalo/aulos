//! The teloxide bot: `/start` and `/config` with inline keyboards, allowed chat ids, per-chat
//! defaults persisted through the store, URL extraction behind the SSRF guard, the single live
//! progress message per chat edited at most every few seconds, discrete completion/failure
//! notifications, and the rate limiter that keeps all of it inside Telegram's budget.
//!
//! See DESIGN §12.
//!
//! # Shape
//!
//! ```text
//!   long polling ──Incoming──►┌─────────────────────────────────┐──Transport──► Bot API
//!   EventRouter ──EventInbox─►│ TelegramActor: chats, boards,   │──EngineCmd──► aulos-queue
//!   1 Hz tick ───────────────►│ watches, limiter. No Mutex.     │──WriteOp────► aulos-store
//!                             └─────────────────────────────────┘
//! ```
//!
//! Four properties are worth stating up front, because each one is a legacy bug fixed or a trap
//! avoided:
//!
//! - **Attribution is a field, not a `contextvar`.** Every job the bot creates carries
//!   `source = { kind: "telegram", ref: "<chat_id>" }`, which the engine copies onto playlist
//!   children. Legacy learned the chat id from a `contextvars.ContextVar` and keyed its watch table
//!   by URL, so the same URL twice merged two jobs into one watch and every non-bot job was
//!   invisible.
//! - **The keyboard's format list is a projection, not the catalog.**
//!   [`aulos_core::catalog::FormatCatalog::bot_formats`] is the documented nine-entry legacy list;
//!   the catalog has sixteen format ids. Conflating them breaks the `cfg:` grammar
//!   ([`config_ui`] asserts the nine byte-for-byte).
//! - **An unchanged board issues no API call.** Telegram answers an unmodified edit with a 400, and
//!   that 400 counts against the chat's rate budget. The rendered string is compared against
//!   `last_rendered` before the call is even attempted.
//! - **The bot is a leaf.** It sends `EngineCmd`s and `WriteOp`s and never reads queue state
//!   directly, so nothing here can stall the engine.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the actor, its state, the tick, startup gating | [`bot`] |
//! | the update vocabulary and the message → jobs pipeline | [`commands`] |
//! | the `/config` texts, keyboards and the `cfg:` grammar | [`config_ui`] |
//! | URL extraction and the hardened SSRF guard | [`urls`] |
//! | who is told about which job, and the two watchdogs | [`watch`] |
//! | the board layout and the notification texts | [`render`] |
//! | the per-chat and global rate budget | [`limiter`] |
//! | the three Bot API calls, behind a trait, plus long polling | [`transport`] |

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bot;
pub mod commands;
pub mod config_ui;
pub mod limiter;
pub mod render;
pub mod transport;
pub mod urls;
pub mod watch;

pub use bot::{TelegramActor, TelegramConfig, TelegramHealth, TelegramHealthHandle, TgInitError};
pub use commands::{Command, Incoming, MessagePlan, plan_message};
pub use config_ui::{Applied, Button, Keyboard, Screen, apply_callback, config_text};
pub use limiter::{Denied, Limiter};
pub use render::{JobLine, Mark, notify, render_board};
pub use transport::{
    Call, MessageId, MockTransport, TeloxideTransport, TgError, Transport, poll_updates,
    to_incoming,
};
pub use urls::{Reject, extract, validate};
pub use watch::{Notifier, Warning, WatchRegistry, Watched};
