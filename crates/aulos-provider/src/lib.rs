//! The provider abstraction every downloader implements: the `Provider` trait, `MediaEntry`, the
//! `ProgressSink`, the scoring registry, process-group spawn/kill helpers, the `plugin.toml`
//! manifest model shared by `command` providers and community `[[hook]]`s, and (behind the `fake`
//! feature) a scripted no-network provider for the integration tests.
//!
//! No provider crate may depend on `aulos-store` or `aulos-queue` (DESIGN §3 rule A1, §6).
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the trait, its contexts, match scoring, the error taxonomy | [`provider`] |
//! | what a resolution produces | [`entry`] |
//! | what a download produces | [`outcome`] |
//! | the one channel a running job reports through | [`sink`] |
//! | selection, `Degraded`, the circuit breaker, plugin reload | [`registry`] |
//! | the `plugin.toml` schema, `command` providers, community `[[hook]]`s | [`command`] |
//! | spawning, process-group kill, the mandatory stderr drain, bounded lines | [`proc`] |
//! | the shared 1024-based size and `hms` parsers | [`humansize`] |
//! | the scripted test provider (feature `fake`) | [`fake`] |

pub mod command;
pub mod entry;
#[cfg(feature = "fake")]
pub mod fake;
pub mod humansize;
pub mod outcome;
pub mod proc;
pub mod provider;
pub mod registry;
pub mod sink;

// ---------------------------------------------------------------------------
// Flat re-exports. Every type is also reachable at its module path; these exist so a provider
// crate can write `use aulos_provider::{Provider, ProviderError, ProgressSink}` instead of four
// module paths, which is what all five downstream packages actually do.
// ---------------------------------------------------------------------------

pub use command::{
    CommandPluginLoader, CommandProvider, HookAction, HookFilter, HookSpec, HttpMethod,
    ManifestError, PluginManifest, ProgressParser, ProgressSpec, Template, TemplateCtx,
    TemplateError, Token, TokenScope, discover, load_manifest,
};
pub use entry::{EntryHints, EntryKind, LiveStatus, MediaEntry, outtmpl_info};
pub use humansize::{format_bytes, format_hms, parse_bytes, parse_hms, parse_rate};
pub use outcome::Outcome;
pub use proc::{Child, EnvPolicy, Lines, ProcError, Rlimits, SpawnSpec, StderrRing, strip_ansi};
pub use provider::{
    DegradedProvider, DownloadCtx, Match, MatchReason, OutTmpl, Provider, ProviderError,
    ProviderHealth, ResolveCtx, SCORE_FALLBACK, SCORE_HOST_REGEX, SCORE_HOST_SUFFIX,
    SCORE_PATH_REGEX, SCORE_SC,
};
pub use registry::{
    CommandLoadResult, CommandLoader, LoadedPlugin, ProviderState, Registry, Selected,
};
pub use sink::{ProgressMsg, ProgressSink, ProgressSinkFactory, Stage};

// `FileSlot`, `FileRef` and `ProviderId` are `aulos-core` types (they appear in `Item` and in
// `DomainEvent` payloads, DESIGN §3). They are re-exported here so a provider implementation has
// one `use` line rather than two, and so that the DESIGN §6.2 signature list reads as written.
pub use aulos_core::item::{FileRef, FileSlot};
pub use aulos_core::selection::ProviderId;
