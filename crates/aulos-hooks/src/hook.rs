//! The [`Hook`] trait, its context and the two small value types every hook shares (DESIGN §13).
//!
//! # Deviations from the DESIGN §13 signature, and why
//!
//! | DESIGN | Here | Reason |
//! |---|---|---|
//! | `HookCtx.item: &Item` | [`HookCtx::item`]`: &ItemView` | The dispatcher's only event source is an [`aulos_core::event::EventInbox`], and `DomainEvent::Finishing` / `DomainEvent::Completed` carry `Arc<ItemView>` (DESIGN §8.1). An `Item` is not obtainable in this crate: [`aulos_core::ports::HookStore`] deliberately exposes no item read, and adding one would put the whole row behind the port the design narrowed to three methods. Every field the four built-ins and a community `[[hook]]` read — `status`, `provider`, `filename`, `size`, `folder`, `selection`, `error`, `title`, `url` — is on `ItemView`. |
//! | `applies(&self, item: &Item)` | [`Hook::applies`]`(&self, item, outcome)` | DESIGN §13 says a `PreTerminal` hook's `applies()` reads "the prospective outcome carried by `HookCtx.batch[0].status`", which a one-argument `applies(&item)` cannot see: on `Finishing` the row is still `postprocessing`. Passing the outcome explicitly makes the pre- and post-terminal cases one signature instead of two, and it is the same value the dispatcher puts in [`BatchEntry::status`]. |
//!
//! Both are recorded in `docs/INTEGRATION-NOTES.md` under WP-11.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::error::WireError;
use aulos_core::id::ItemId;
use aulos_core::item::{EntryBlob, ItemView};
use aulos_core::ports::{HookPhase, HookStore};
use aulos_core::status::TerminalStatus;
use aulos_provider::sink::ProgressSink;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::error::HookError;

/// The dispatcher's outer bound on one hook invocation when the hook does not name its own.
///
/// It exists so a wedged `PreTerminal` hook cannot park an item in `postprocessing` forever
/// (DESIGN §13): after this the dispatcher logs a WARN, counts a failure and finalises anyway.
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// One coalesced event inside a hook invocation (DESIGN §13).
///
/// `batch.len() == 1` for an undebounced hook; a debounced one gets one entry per event that
/// arrived in the window, in arrival order, and that is what `{count}`, `{titles_json}` and
/// `{filenames_json}` render from (DESIGN §13.4).
#[derive(Clone, PartialEq, Debug)]
pub struct BatchEntry {
    /// Which item.
    pub id: ItemId,
    /// Its title at the time of the event.
    pub title: Arc<str>,
    /// The produced file, relative to the item's download root.
    pub filename: Option<Arc<str>>,
    /// The terminal status the item has (`PostTerminal`) or is about to get (`PreTerminal`).
    pub status: TerminalStatus,
    /// The terminal error, when the outcome was a failure.
    pub error: Option<WireError>,
}

impl BatchEntry {
    /// The entry for one item view whose outcome is `status`.
    #[must_use]
    pub fn from_view(view: &ItemView, status: TerminalStatus) -> Self {
        Self {
            id: view.id,
            title: Arc::clone(&view.title),
            filename: view.filename.clone(),
            status,
            error: view.error.clone(),
        }
    }
}

/// A trailing debounce window with a hard cap (DESIGN §13.1, §13.4).
///
/// Each event arms or extends `window`, but the fire time is capped at `first_at + max_wait`, so a
/// 500-item playlist still refreshes every `max_wait` instead of only at the very end. A plain
/// trailing-edge debounce would make a long playlist invisible in Jellyfin for hours.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Debounce {
    /// The trailing window. `Duration::ZERO` means "fire per event".
    pub window: Duration,
    /// The cap measured from the first event of the batch.
    pub max_wait: Duration,
}

impl Debounce {
    /// No debouncing: one invocation per event.
    pub const NONE: Self = Self {
        window: Duration::ZERO,
        max_wait: Duration::ZERO,
    };

    /// A window with the DESIGN §13.4 default cap of `10 × window`.
    #[must_use]
    pub const fn new(window: Duration) -> Self {
        Self {
            window,
            max_wait: window.saturating_mul(10),
        }
    }

    /// A window with an explicit cap. A cap below the window is raised to it, since a cap that
    /// fires before the window would make the window unobservable.
    #[must_use]
    pub fn capped(window: Duration, max_wait: Duration) -> Self {
        Self {
            window,
            max_wait: max_wait.max(window),
        }
    }

    /// Whether this hook coalesces at all.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        !self.window.is_zero()
    }
}

/// A hook's own contribution to its `healthz` component (DESIGN §16.3).
///
/// The dispatcher owns the counters (`runs_total`, `failures_total`, `last_success_at`,
/// `last_error`, `pending`, `phase`); a hook adds only what the dispatcher cannot know — the
/// status floor when it is disabled or misconfigured, and any component-specific detail.
#[derive(Clone, PartialEq, Debug)]
pub struct HookHealth {
    /// The floor for this component's status. The dispatcher can only make it worse, never better.
    pub status: aulos_core::health::ComponentStatus,
    /// Component-specific detail fields, merged into the `healthz` entry.
    pub detail: Map<String, Value>,
}

impl Default for HookHealth {
    fn default() -> Self {
        Self::ok()
    }
}

impl HookHealth {
    /// Configured and working as far as the hook itself can tell.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            status: aulos_core::health::ComponentStatus::Ok,
            detail: Map::new(),
        }
    }

    /// Not configured, so not a failure (DESIGN §16.3).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            status: aulos_core::health::ComponentStatus::Disabled,
            detail: Map::new(),
        }
    }

    /// Configured but unusable, with `detail` naming the reason for the life of the process
    /// (DESIGN §13.1's precondition handling).
    #[must_use]
    pub fn degraded(reason: &str) -> Self {
        let mut detail = Map::new();
        detail.insert("detail".to_owned(), Value::String(reason.to_owned()));
        Self {
            status: aulos_core::health::ComponentStatus::Degraded,
            detail,
        }
    }

    /// Adds one detail field.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.detail.insert(key.to_owned(), value.into());
        self
    }
}

/// Everything a hook is given. Borrowed for the duration of one `run`, so a hook holds no state
/// of its own (DESIGN §13).
pub struct HookCtx<'a> {
    /// The item, exactly as the engine published it. `status` is terminal for a `PostTerminal`
    /// hook and still `postprocessing` for a `PreTerminal` one — the prospective outcome is
    /// `batch[0].status`.
    pub item: &'a ItemView,
    /// The item's provider entry blob, loaded through the port for the hooks that ask for it
    /// ([`Hook::wants_entry`]). `None` when the row has no blob or it was already dropped.
    pub entry: Option<&'a EntryBlob>,
    /// Absolute directory the item's primary file lives in.
    pub out_dir: &'a Path,
    /// Absolute path of the primary produced file, when there is one.
    pub file: Option<&'a Path>,
    /// A progress sink for **this** item, so `phase`/`phase_percent` from a long-running hook flow
    /// through the ordinary aggregator path (DESIGN §13.3). There is no second mechanism.
    pub sink: &'a ProgressSink,
    /// The two writes a hook is allowed to make, both engine-mediated (DESIGN §7.1, §13.3).
    pub store: &'a dyn HookStore,
    /// Effective config, for `JELLYFIN_*`, `AULOS_NFO_*` and the download roots.
    pub cfg: &'a Config,
    /// The coalesced batch this invocation represents. Never empty.
    pub batch: &'a [BatchEntry],
    /// Cancelled on shutdown. A hook must observe it or be killed at the grace deadline.
    pub cancel: &'a CancellationToken,
    /// Wall-clock time.
    pub clock: &'a dyn Clock,
}

impl HookCtx<'_> {
    /// The prospective (or actual) terminal outcome of the representative item.
    ///
    /// This is the value a `PreTerminal` hook must branch on: on `Finishing` the row still reads
    /// `postprocessing` (DESIGN §13).
    #[must_use]
    pub fn outcome(&self) -> TerminalStatus {
        self.batch
            .first()
            .map_or(TerminalStatus::Finished, |b| b.status)
    }

    /// How many events this invocation represents — `{count}` (DESIGN §13.4).
    #[must_use]
    pub fn count(&self) -> u32 {
        u32::try_from(self.batch.len()).unwrap_or(u32::MAX)
    }
}

/// A post-completion hook (DESIGN §13).
///
/// A hook may take as long as it likes: hooks run **outside** the download slot, so a slow
/// Jellyfin scan or a 40-minute re-encode never blocks the next download. It may never change an
/// item's status — [`HookPhase`] is how a hook influences *when* the terminal status is written,
/// not what it is.
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// The stable id. It is also the `healthz` component key, so the built-ins are exactly
    /// `jellyfin`, `nfo` and `audio_sync` and a community hook is `hook:<dir>/<id>`.
    fn id(&self) -> Arc<str>;

    /// Lower runs first. Built-ins are 10 (`audio_sync`), 20 (`nfo`) and 90 (`jellyfin`);
    /// a community hook defaults to 50.
    fn ordering(&self) -> i16;

    /// When this hook runs relative to the terminal status write. Defaults to
    /// [`HookPhase::PostTerminal`], which is what every notifier wants.
    fn phase(&self) -> HookPhase {
        HookPhase::PostTerminal
    }

    /// The trailing debounce window, if any. Defaults to none.
    fn debounce(&self) -> Debounce {
        Debounce::NONE
    }

    /// The dispatcher's outer bound on one invocation, retries and backoff included.
    fn timeout(&self) -> Duration {
        DEFAULT_HOOK_TIMEOUT
    }

    /// Whether the dispatcher should load the item's entry blob through the port before running
    /// this hook. Only [`crate::nfo::NfoHook`] needs it, and it is not free.
    fn wants_entry(&self) -> bool {
        false
    }

    /// Whether this hook runs for `item`, whose terminal outcome is (or is about to be) `outcome`.
    ///
    /// Must be cheap and side-effect free: the dispatcher calls it once per event, before
    /// enqueueing anything.
    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool;

    /// The hook's own view of its health. See [`HookHealth`].
    fn health(&self) -> HookHealth {
        HookHealth::ok()
    }

    /// Runs the hook.
    ///
    /// # Errors
    /// Any [`HookError`]. A failure is logged, counted and surfaced in `healthz`; it never changes
    /// the item's status (DESIGN §13).
    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::health::ComponentStatus;

    #[test]
    fn a_debounce_caps_at_ten_windows_by_default() {
        let d = Debounce::new(Duration::from_secs(30));
        assert_eq!(d.window, Duration::from_secs(30));
        assert_eq!(d.max_wait, Duration::from_secs(300));
        assert!(d.is_armed());
        assert!(!Debounce::NONE.is_armed());
    }

    #[test]
    fn a_cap_below_the_window_is_raised_to_it() {
        let d = Debounce::capped(Duration::from_secs(30), Duration::from_secs(5));
        assert_eq!(d.max_wait, Duration::from_secs(30));
    }

    #[test]
    fn hook_health_constructors_carry_their_status() {
        assert_eq!(HookHealth::ok().status, ComponentStatus::Ok);
        assert_eq!(HookHealth::disabled().status, ComponentStatus::Disabled);
        let d = HookHealth::degraded("JELLYFIN_URL is required");
        assert_eq!(d.status, ComponentStatus::Degraded);
        assert_eq!(d.detail["detail"], "JELLYFIN_URL is required");
        assert_eq!(HookHealth::default(), HookHealth::ok());
        assert_eq!(
            HookHealth::ok().with("phase", "pre_terminal").detail.len(),
            1
        );
    }
}
