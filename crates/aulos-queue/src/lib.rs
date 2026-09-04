//! The queue engine: the command/event loop, global and per-provider slots, the bounded resolution
//! pool, playlist expansion into groups, dedupe, cooperative cancellation, boot recovery, the
//! clear/delete paths and the watchdogs — plus the realtime side: the delta aggregator, the event
//! hub with its monotonic `seq` and replay ring, and the lock-free published snapshot.
//!
//! See DESIGN §8 and §15.
//!
//! # Shape
//!
//! ```text
//!  aulos-api ──EngineCmd──►┌──────────────────────────────────┐──spawn──► resolve tasks
//!  aulos-telegram          │ Engine: ONE task, owned state,   │──spawn──► download tasks
//!  aulos-subscriptions     │ no Mutex. Every mutation is a    │──spawn──► per-job watchdogs
//!  aulos-hooks (port)      │ message.                         │
//!                          └───┬──────────────┬───────────────┘
//!                       WriteOp│              │DomainEvent
//!                        Store ▼              ▼ EventRouter ──► aggregator / hooks / telegram
//! ```
//!
//! Three properties are worth stating up front, because each one is a design rule the code is
//! shaped around:
//!
//! - **The engine holds no `Mutex`.** Every piece of queue state — the four ready deques, the slot
//!   semaphores, the group accumulators, the dedupe index, the item cache — is owned by the single
//!   engine task and mutated only while handling one [`EngineCmd`]. Races become message
//!   ordering, which a test can reproduce deterministically.
//! - **Progress never enters the engine** (DESIGN §2.2). Only the ≤ 6 lossless
//!   [`aulos_provider::sink::ProgressMsg::Stage`] / `::File` messages per job do, forwarded by the
//!   aggregator, because those are persisted. 500 children × 20 frames/s cannot starve
//!   `POST /downloads`.
//! - **An item's id never changes.** A playlist resolve promotes the row the client already holds
//!   in place, keeping both `id` and `ord` (DESIGN §8.6), so a group row morphs instead of
//!   blinking.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the command vocabulary, the handle, the result types | [`cmd`] |
//! | the engine itself: state, loop, add path, writes, views | [`engine`], [`add`] |
//! | resolution, playlist expansion, the runner-up fall-through | [`resolve`] |
//! | running a download, stages, terminal transitions, retries | [`run`] |
//! | cancel, pause, delete, `cancel-resolve` | [`cancel`] |
//! | group accumulators and their roll-ups | [`groups`] |
//! | slots: global, per-provider, resolution | [`slots`] |
//! | the derived priority classes | [`priority`] |
//! | the dedupe key | [`dedupe`] |
//! | entry compaction and rebuilding | [`entry`] |
//! | boot recovery | [`recovery`] |
//! | `CLEAR_COMPLETED_AFTER` | [`clear`] |
//! | the stall / hard-timeout watchdogs | [`watchdog`] |
//! | the `HookStore` port implementation | [`hookstore`] |
//! | the delta aggregator, the generated diff, the flush order | [`aggregator`] |
//! | frames, the replay ring and the resume merge | [`ring`] |
//! | `seq`, serialise-once, broadcast, `resume()` | [`hub`] |
//! | the lock-free published snapshot | [`publish`] |
//!
//! # BRIEF scope trims applied here
//!
//! The WS `watch`/`unwatch` frames, `ConnId` and the engine watch registry are **CUT**, so
//! [`EngineCmd`] has no `Watch`, `Unwatch` or `ConnClosed` variant and there is no
//! connection→groups map anywhere in the process. The snapshot carries every non-terminal item,
//! children included (DESIGN §8.6's `children_inline` is always `true`).
//!
//! The client → server `ack` frame is **CUT** too, so nothing here takes a client cursor: the
//! replay ring's `floor` is advanced solely by its frame and byte bounds, which is what makes
//! "one client's cursor can never shorten another client's `?since=` window" (DESIGN §15.3,
//! PROTOCOL §5.11) structural rather than a discipline.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod add;
pub mod aggregator;
pub mod cancel;
pub mod clear;
pub mod cmd;
pub mod dedupe;
pub mod engine;
pub mod entry;
pub mod groups;
pub mod hookstore;
pub mod hub;
pub mod priority;
pub mod publish;
pub mod recovery;
pub mod resolve;
pub mod ring;
pub mod run;
pub mod slots;
pub mod watchdog;

pub use aggregator::{Aggregator, DIFF_FIELDS, diff, is_urgent, protocol_block, text_changed};
pub use cmd::{
    AckActions, Action, ActionsResult, AddError, AddOutcome, CancelScope, Duplicate, EngineCmd,
    EngineError, EngineHandle, HookWrite, ResolveReport, SkipReason, Skipped,
};
pub use dedupe::{DedupeKey, canonical_key};
pub use engine::{Engine, NoPreTerminalHooks, PreTerminalHooks};
pub use entry::{compact_entry, rebuild_entry};
pub use groups::GroupAcc;
pub use hookstore::EngineHookStore;
pub use hub::{BROADCAST_CAPACITY, EventHub, Resume};
pub use priority::Priority;
pub use publish::{Published, StateView, StatusCounts, Truncated};
pub use recovery::RecoveryReport;
pub use ring::{
    DeltaBatch, DeltaItem, Fold, FrameBody, FrameKind, MergeCounts, REASON_ORDER, Ring, RingEntry,
    WireFrame,
};
pub use slots::Slots;
pub use watchdog::{Heartbeats, JobBeat};
