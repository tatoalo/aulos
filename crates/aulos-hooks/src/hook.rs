//! The [`Hook`] trait, its context and the two small value types every hook shares (DESIGN §13).
//!
//! # Deviations from the DESIGN §13 signature, and why
//!
//! | DESIGN | Here | Reason |
//! |---|---|---|
//! | `HookCtx.item: &Item` | [`HookCtx::item`]`: &ItemView` | The dispatcher's only event source is an [`aulos_core::event::EventInbox`], and `DomainEvent::Finishing` / `DomainEvent::Completed` carry `Arc<ItemView>` (DESIGN §8.1). An `Item` is not obtainable in this crate: [`aulos_core::ports::HookStore`] deliberately exposes no item read, and adding one would put the whole row behind the port the design narrowed to three methods. Every field the four built-ins and a community `[[hook]]` read — `status`, `provider`, `filename`, `size`, `folder`, `selection`, `error`, `title`, `url` — is on `ItemView`. |
//! | `applies(&self, item: &Item)` | [`Hook::applies`]`(&self, item, outcome)` | DESIGN §13 says a `PreTerminal` hook's `applies()` reads "the prospective outcome carried by `HookCtx.batch[0].status`", which a one-argument `applies(&item)` cannot see: on `Finishing` the row is still `postprocessing`. Passing the outcome explicitly makes the pre- and post-terminal cases one signature instead of two, and it is the same value the dispatcher puts in [`BatchEntry::status`]. |
//!
//! | `applies(..) -> bool` alone | [`Hook::applies`] plus [`Hook::skip_reason`] | A `false` says a hook did not run; it does not say why, and the dispatcher then has nothing to log or count. `runs_total: 0, failures_total: 0, status: "ok"` is what the built-in NFO hook reported for a whole production cutover while it was gated out by a provider check nobody could see. `skip_reason` is what the dispatcher gates on; `applies` stays as the cheap predicate, defined as `skip_reason(..).is_none()` by every built-in, and out-of-tree hooks that implement only `applies` still get counted (with a generic reason) through the default. |
//!
//! All three are recorded in `docs/INTEGRATION-NOTES.md` under WP-11.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::error::WireError;
use aulos_core::id::ItemId;
use aulos_core::item::{EntryBlob, ItemView};
use aulos_core::ports::{HookPhase, HookStore};
use aulos_core::selection::DownloadType;
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
    /// Which download root `filename` is relative to. Carried per entry because a debounced batch
    /// can mix video and audio items, and [`HookCtx::file`] only ever describes the representative
    /// one — the Jellyfin hook needs the path of **every** file the batch produced.
    pub download_type: DownloadType,
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
            download_type: view.selection.download_type,
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

/// Why a hook did **not** run for one event (DESIGN §13, §16.3).
///
/// A hook that never runs and never fails is otherwise indistinguishable from a healthy idle one:
/// `runs_total: 0, failures_total: 0, status: "ok"` is what the production report of the built-in
/// NFO hook looked like while it was silently gated out. Every gate a hook applies therefore names
/// itself, the dispatcher logs it at DEBUG and counts it, and `healthz` reports `skipped_total`
/// with the `last_skip_reason`.
///
/// The text is a short lowercase phrase completing "skipped because …", so it reads the same in a
/// log line and in a health payload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SkipReason(Cow<'static, str>);

impl SkipReason {
    /// A reason known at compile time.
    #[must_use]
    pub const fn new(reason: &'static str) -> Self {
        Self(Cow::Borrowed(reason))
    }

    /// A reason that has to name a runtime value (a provider id, an outcome).
    #[must_use]
    pub fn owned(reason: impl Into<String>) -> Self {
        Self(Cow::Owned(reason.into()))
    }

    /// The reason text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SkipReason> for String {
    fn from(r: SkipReason) -> Self {
        r.0.into_owned()
    }
}

/// The reason a hook that only implements [`Hook::applies`] gives, since it has none of its own.
pub const NOT_APPLICABLE: SkipReason = SkipReason::new("it does not apply to this item");

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
    ///
    /// A built-in implements this as `self.skip_reason(item, outcome).is_none()` and puts the
    /// gates in [`Hook::skip_reason`], so a skip can say *why*.
    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool;

    /// Why this hook does not run for `item`, or `None` when it does.
    ///
    /// This is the method the dispatcher gates on; [`Hook::applies`] stays as the cheap predicate
    /// a caller outside the dispatcher (a test, `doctor`) asks. The default derives the answer
    /// from `applies` with the generic [`NOT_APPLICABLE`] reason, so an out-of-tree hook keeps
    /// compiling and is still counted as skipped — it just cannot be specific about it.
    ///
    /// Must be cheap and side-effect free, for the same reason `applies` must be.
    fn skip_reason(&self, item: &ItemView, outcome: TerminalStatus) -> Option<SkipReason> {
        if self.applies(item, outcome) {
            None
        } else {
            Some(NOT_APPLICABLE)
        }
    }

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
    fn a_skip_reason_carries_its_text_either_way_it_was_built() {
        assert_eq!(SkipReason::new("it is disabled").as_str(), "it is disabled");
        assert_eq!(
            SkipReason::owned(format!("the outcome is {}", TerminalStatus::Error)).to_string(),
            "the outcome is error"
        );
        assert_eq!(
            String::from(NOT_APPLICABLE),
            "it does not apply to this item"
        );
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
