//! Post-completion hooks: the debouncing dispatcher plus the `jellyfin` library refresh, `nfo`
//! generation for StreamingCommunity items, the `best_remux` audio-sync ffmpeg pass, and community
//! hooks declared as `[[hook]]` tables in a `plugin.toml`.
//!
//! Item state is reached only through `aulos_core::ports::HookStore`, never through `aulos-store`
//! or `aulos-queue` (DESIGN §3, §13).
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the trait, its context, the debounce and health value types | [`hook`] |
//! | the event loop, the two phases, ordering, timeouts, panic isolation | [`dispatcher`] |
//! | the debounced, targeted library refresh | [`jellyfin`] |
//! | the `quick-xml` port of `jellyfin_nfo_generator.py` | [`nfo`] |
//! | the `best_remux` re-encode and its `-progress` parsing | [`audio_sync`] |
//! | the ffmpeg/ffprobe spawn plumbing | [`ffprobe`] |
//! | executing a community `[[hook]]` from the WP-10 manifest | [`manifest_hook`] |
//!
//! # The two phases
//!
//! ```text
//! engine   Finished { id, outcome }
//! engine     SetStatus{ Postprocessing } ; publish DomainEvent::Finishing   # hooks inbox only
//! hooks    run every PreTerminal hook in ordering() order, sequentially
//! hooks      HooksFinished { id }                # ALWAYS, even on failure/panic/timeout
//! engine     finalise ; publish DomainEvent::Completed                      # the `completed` frame
//! hooks    run every PostTerminal hook
//! ```
//!
//! An item that never finalises is worse than a failed re-encode, which is why
//! [`dispatcher::HookFinalizer::hooks_finished`] is sent from every path.

pub mod audio_sync;
pub mod dispatcher;
pub mod error;
pub mod ffprobe;
pub mod hook;
pub mod jellyfin;
pub mod manifest_hook;
pub mod nfo;

pub use audio_sync::AudioSyncHook;
pub use dispatcher::{
    HookDispatcher, HookFinalizer, HookRunner, HookStat, HooksHealth, HooksHealthHandle,
    NoopFinalizer, RecordingFinalizer,
};
pub use error::HookError;
pub use ffprobe::MediaTools;
pub use hook::{BatchEntry, DEFAULT_HOOK_TIMEOUT, Debounce, Hook, HookCtx, HookHealth};
pub use jellyfin::JellyfinHook;
pub use manifest_hook::ManifestHook;
pub use nfo::NfoHook;

// Re-exported so a hook implementation and the wiring in `aulos-server` have one `use` line for
// the port vocabulary they share with the engine (DESIGN §7.1, §13).
pub use aulos_core::ports::{HookPhase, HookStore, PortError};
