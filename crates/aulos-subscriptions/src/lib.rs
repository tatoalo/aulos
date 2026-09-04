//! Subscriptions: the record and its public projection, a per-subscription scheduler with jitter
//! and exponential backoff, bounded-concurrency checks that start shortly after boot, and the
//! check algorithm that turns newly-seen entries into queue items.
//!
//! See DESIGN §14.
//!
//! # Shape
//!
//! ```text
//!   aulos-api ──SubCmd──►┌──────────────────────────────────┐
//!   (SubscriptionsHandle)│ Manager: ONE task, owned state.  │──watch::Sender──► per-sub tasks
//!                        │ url index, pending_urls, in-     │◄──Internal────────┘  (JoinSet)
//!                        │ flight set, the JoinSet.         │
//!                        └───┬──────────────┬───────────────┘        │
//!                     WriteOp│              │DomainEvent             │ FeedChecker
//!                      Store ▼              ▼ EventSender            ▼
//!                                                            Registry + EngineCmd::Add
//! ```
//!
//! Three properties are worth stating, because each one is a legacy bug fixed:
//!
//! - **A dead feed backs off.** `last_checked` is written on **every** check, success or failure,
//!   and a failure multiplies the interval up to `AULOS_SUB_BACKOFF_MAX_SECS`. Legacy left
//!   `last_checked` untouched on failure, so a permanently broken feed was re-extracted every
//!   60 s forever.
//! - **The scheduler is an event producer only** (DESIGN §2.2.1). It holds an
//!   [`aulos_core::event::EventSender`] and registers no `EventInbox`, so nothing here can be
//!   starved by, or starve, the aggregator or the hook dispatcher.
//! - **`POST subscriptions/check` returns immediately.** It nudges the per-subscription timers and
//!   answers with a [`aulos_core::subscription::CheckJob`]; progress is observable as
//!   `checking: true/false` on the `subscription` frame.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the interval / backoff / jitter arithmetic | [`model`] |
//! | the v2 (16 keys) and v1 (13 keys) projections, and the two WS envelopes | [`public`] |
//! | `is_media_entry`, feed classification, tab-page recursion | [`detect`] |
//! | resolving a feed and queueing what is new | [`check`] |
//! | the per-subscription task, its timer and its permit | [`scheduler`] |
//! | the command loop that owns every mutation | [`manager`] |

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod check;
pub mod detect;
pub mod manager;
pub mod model;
pub mod public;
pub mod scheduler;

pub use check::{Checker, Feed, FeedChecker, OptionsSource, StaticOptions};
pub use detect::{Classified, classify, is_media_entry, media_id_of};
pub use manager::{Manager, SubDeps};
pub use model::{CheckFailure, CheckReport, FixedJitter, Jitter, RandJitter, Timing};
pub use public::{parse_enabled, to_v1_dict, v2_frame, v2_removed_frame};
pub use scheduler::{CheckMsg, SubTask, TaskParams};
