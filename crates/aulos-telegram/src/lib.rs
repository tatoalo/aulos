//! The teloxide bot: `/start` and `/config` with inline keyboards, allowed chat ids, per-chat
//! defaults persisted through the store, URL extraction behind the SSRF guard, the single live
//! progress message per chat edited at most every few seconds, discrete completion/failure
//! notifications, and the rate limiter that keeps all of it inside Telegram's budget.
//!
//! See DESIGN §12.
