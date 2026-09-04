//! Domain vocabulary shared by every other crate: item identity and ordering, the closed status
//! enum, the download request and selection types, the `ItemView` wire shape, the event router,
//! the format/quality catalog, configuration loading, health and reload reports, and the error
//! taxonomy.
//!
//! `aulos-core` depends on nothing else in the workspace, and **every** type that appears in a
//! [`DomainEvent`] payload or in a cross-crate port lives here — not for tidiness, but because
//! `DomainEvent` is declared here and the dependency graph is strictly
//! downward (DESIGN §3). Putting `SubscriptionView` in `aulos-subscriptions`, `HealthView` in
//! `aulos-server` or `ReloadReport` in `aulos-provider` would produce three dependency cycles that
//! `tests/arch.rs` rejects.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | ids, ordering, the durable allocator trait | [`id`] |
//! | the closed status vocabulary and its transition table | [`status`] |
//! | the persisted row and the one wire shape | [`item`] |
//! | what the user asked for | [`selection`], [`request`] |
//! | the format / quality catalog | [`catalog`] |
//! | attribution | [`source`] |
//! | the error taxonomy and the secret guard | [`error`] |
//! | events and the one-to-N router | [`event`] |
//! | subscriptions: record, view, command handle | [`subscription`] |
//! | Telegram per-chat defaults | [`telegram`] |
//! | `healthz` state | [`health`] |
//! | the plugin reload report | [`reload`] |
//! | cross-crate seams: `HookStore`, `HookPhase`, `FieldUpdate` | [`ports`] |
//! | configuration | [`config`], [`ytdl_options`] |
//! | paths, containment, the URL prefix | [`paths`], [`prefix`] |
//! | time and progress | [`clock`], [`progress`] |
//!
//! # BRIEF scope trims applied here
//!
//! `ConnId` and the engine watch registry are CUT, so no connection newtype is declared; the
//! `print-schema` subcommand is CUT, so [`item::ItemView::FIELDS`] is the authoritative key list
//! that the aggregator's diff and the serde tests are asserted against instead.

pub mod catalog;
pub mod clock;
pub mod config;
pub mod error;
pub mod event;
pub mod health;
pub mod id;
pub mod item;
pub mod paths;
pub mod ports;
pub mod prefix;
pub mod progress;
pub mod reload;
pub mod request;
pub mod selection;
pub mod source;
pub mod status;
pub mod subscription;
pub mod telegram;
pub mod ytdl_options;

// ---------------------------------------------------------------------------
// Flat re-exports.
//
// Every type below is also reachable at its module path; these exist so a downstream crate can
// write `use aulos_core::{ItemId, ItemView, Status}` instead of three module paths, which is what
// the seven parallel packages actually do.
// ---------------------------------------------------------------------------

pub use catalog::{
    BotFormat, Choice, CodecSpec, DownloadTypeSpec, FlatFormat, FlatQuality, FormatCatalog,
    FormatFlags, FormatSpec, MergedCatalog, NamingPolicy, OptionKind, OptionSpec, QualitySpec,
    SUBTITLE_LANGUAGE_PATTERN, YTDLP_CATALOG, ytdlp_catalog,
};
pub use clock::{Clock, FakeClock, SystemClock};
pub use config::{
    Config, ConfigError, ConfigWarning, CorsOrigins, DbSynchronous, DedupeMode, ImportOnError,
    JellyfinRefreshMode, LogFormat, RawEnv, RestartPolicy, ScHttpMode, TelegramBoard, Theme, load,
    load_with_warnings,
};
pub use error::{ErrorCode, REDACTED, Redact, SECRET_KEY_PATTERN, WireError, is_secret_key};
pub use event::{
    AddReason, DomainEvent, DropPolicy, EventFilter, EventInbox, EventKind, EventRouter,
    EventSender, Level, Notice, RemoveReason, SubscriberSpec, TryPublishError, notice_code,
};
pub use health::{ComponentHealth, ComponentStatus, HealthRegistry, HealthView};
pub use id::{BootId, GroupId, HiLoAllocator, IdError, ItemId, Ord0, Seq, SubId, UnixMs};
pub use item::{EntryBlob, FileRef, FileSlot, Item, ItemView, Kind, ViewExtras};
pub use paths::{PathError, Paths, RelDir, RelPath, contain, sanitize_path_component};
pub use ports::{FieldUpdate, HookPhase, HookStore, PortError};
pub use prefix::{Prefix, PrefixFixup};
pub use progress::{ACTIVE_CEILING, Normalizer, PhaseTag, ProgressCell, RawProgress};
pub use reload::{ReloadFailure, ReloadReport};
pub use request::{DownloadRequest, RequestError, RequestView, SubtitleLang, SubtitleMode};
pub use selection::{
    Codec, DownloadType, FormatId, ProviderId, QualityId, Selection, SelectionError, SelectionView,
};
pub use source::{SourceKind, SourceRef};
pub use status::{NotTerminal, Status, StatusEdge, TerminalStatus, can_transition};
pub use subscription::{
    CheckJob, SubChanges, SubCmd, SubError, SubsHealth, SubscriptionRecord, SubscriptionView,
    SubscriptionsHandle,
};
pub use telegram::{ChatConfig, normalize_download_selection};
pub use ytdl_options::{YtdlOptions, YtdlOptionsError};
