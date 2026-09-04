//! Provider selection, the `Degraded` state and its circuit breaker (DESIGN §6.3, §6.4).
//!
//! Two rules do all the work. **Highest score wins, ties break by registration order** — that is
//! the whole of selection, and it is why `ytdlp` is registered last at `Weak(1)` and can never
//! steal a URL a real provider claimed. And **a provider that is broken stays visible**: it is
//! registered as [`ProviderState::Degraded`], it still matches its own URLs, and a job routed to
//! it fails immediately with `provider_degraded` rather than falling through to `ytdlp`, which
//! would download a login page and call it a success.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aulos_core::catalog::{FormatCatalog, MergedCatalog};
use aulos_core::clock::{Clock, SystemClock};
use aulos_core::id::UnixMs;
use aulos_core::reload::{ReloadFailure, ReloadReport};
use aulos_core::selection::ProviderId;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::provider::{Match, MatchReason, Provider};

/// The circuit breaker's failure threshold (DESIGN §6.4).
pub const BREAKER_FAILURES: u32 = 5;
/// The window failures are counted over, and the cool-down that follows (DESIGN §6.4).
pub const BREAKER_WINDOW_MS: i64 = 10 * 60 * 1000;

/// Whether a provider is usable (DESIGN §6.4).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProviderState {
    /// Working.
    Ready,
    /// Broken, but still registered and still matching its URLs.
    Degraded {
        /// Shown verbatim in `healthz` and in `GET api/v2/providers`.
        reason: Box<str>,
        /// When it entered this state, unix ms. Also the start of the cool-down.
        since: UnixMs,
        /// How many failures have been recorded in the current window.
        failures: u32,
    },
}

impl ProviderState {
    /// Whether a job may be routed to this provider.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The reason, when degraded.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::Degraded { reason, .. } => Some(reason),
        }
    }
}

/// The result of [`Registry::pick`] (DESIGN §6.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Selected {
    /// The winning provider.
    pub id: ProviderId,
    /// Its score, `255` for a forced pick.
    pub score: u8,
    /// Why it won.
    pub reason: MatchReason,
    /// The provider that would have been chosen next, and its score. This is what the one
    /// documented `Unsupported` fall-through retries through (DESIGN §6.4).
    pub runner_up: Option<(ProviderId, u8)>,
    /// The winner's state. `Degraded` here means the job must fail with `provider_degraded`
    /// **without** trying the runner-up.
    pub state: ProviderState,
}

/// A `command` plugin as a loader hands it over.
///
/// `degraded` carries the reason when the manifest loaded far enough to know which URLs the plugin
/// claims but not far enough to run it (DESIGN §6.4).
pub struct LoadedPlugin {
    /// The provider. For a degraded plugin this is usually a
    /// [`crate::provider::DegradedProvider`] built from the parsed `[match]` section.
    pub provider: Arc<dyn Provider>,
    /// `Some(reason)` to register it degraded.
    pub degraded: Option<Box<str>>,
    /// A hash of the manifest, so a reload can tell "unchanged" from "updated". `None` means
    /// "assume it changed".
    pub fingerprint: Option<u64>,
}

/// What one plugin-directory scan found.
#[derive(Default)]
pub struct CommandLoadResult {
    /// The plugins that produced a provider, degraded or not.
    pub plugins: Vec<LoadedPlugin>,
    /// The directories that produced nothing at all — not even a matcher.
    pub failed: Vec<ReloadFailure>,
}

/// Discovers `command:` providers in a plugin directory.
///
/// Implemented by `aulos_provider::command::discover` (WP-10) and injected with
/// [`Registry::set_command_loader`]. It is a seam rather than a direct call because the manifest
/// model is a large, separately-tested surface and the registry's selection logic has no business
/// knowing about TOML.
pub trait CommandLoader: Send + Sync {
    /// Scans `dir`. Never fails as a whole: an unreadable directory is an empty result and a
    /// broken manifest is a [`ReloadFailure`] or a degraded plugin.
    fn load(&self, dir: &Path) -> CommandLoadResult;
}

/// One registered provider.
struct Entry {
    provider: Arc<dyn Provider>,
    id: ProviderId,
    state: ProviderState,
    fingerprint: Option<u64>,
    /// Failure timestamps inside the current breaker window, oldest first.
    failures: Vec<UnixMs>,
}

/// The provider registry: registration order, scores, states and catalogs (DESIGN §6.3).
pub struct Registry {
    providers: Vec<Entry>,
    by_id: HashMap<ProviderId, usize>,
    clock: Arc<dyn Clock>,
    loader: Option<Arc<dyn CommandLoader>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field(
                "providers",
                &self
                    .providers
                    .iter()
                    .map(|e| (e.id.as_str().to_owned(), e.state.clone()))
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry on the system clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// An empty registry on a caller-supplied clock, so the circuit breaker's ten-minute window
    /// can be tested without waiting ten minutes.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            providers: Vec::new(),
            by_id: HashMap::new(),
            clock,
            loader: None,
        }
    }

    /// Installs the `command` plugin loader used by [`Self::reload_commands`].
    pub fn set_command_loader(&mut self, loader: Arc<dyn CommandLoader>) {
        self.loader = Some(loader);
    }

    /// Registers a working provider. Registration order is the tie-break order.
    ///
    /// Re-registering an id replaces the provider in place, keeping its position and its state.
    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.insert(provider, ProviderState::Ready, None);
    }

    /// Registers a provider that could not be constructed (DESIGN §6.4).
    ///
    /// `provider` must still `matches()` the URLs the real one would — see
    /// [`crate::provider::DegradedProvider`].
    pub fn register_degraded(&mut self, provider: Arc<dyn Provider>, reason: impl Into<Box<str>>) {
        let state = ProviderState::Degraded {
            reason: reason.into(),
            since: self.clock.now_ms(),
            failures: 0,
        };
        self.insert(provider, state, None);
    }

    fn insert(
        &mut self,
        provider: Arc<dyn Provider>,
        state: ProviderState,
        fingerprint: Option<u64>,
    ) {
        let id = provider.id();
        if let Some(&i) = self.by_id.get(&id) {
            self.providers[i].provider = provider;
            self.providers[i].state = state;
            self.providers[i].fingerprint = fingerprint;
            return;
        }
        self.by_id.insert(id.clone(), self.providers.len());
        self.providers.push(Entry {
            provider,
            id,
            state,
            fingerprint,
            failures: Vec::new(),
        });
    }

    /// How many providers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Every provider, in registration order, with its id and state.
    ///
    /// This is what `GET api/v2/providers` and `healthz` iterate.
    pub fn iter(&self) -> impl Iterator<Item = (&ProviderId, &Arc<dyn Provider>, &ProviderState)> {
        self.providers
            .iter()
            .map(|e| (&e.id, &e.provider, &e.state))
    }

    /// Every registered id, in registration order.
    #[must_use]
    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers.iter().map(|e| e.id.clone()).collect()
    }

    /// A provider by id.
    #[must_use]
    pub fn by_id(&self, id: &ProviderId) -> Option<&Arc<dyn Provider>> {
        self.by_id.get(id).map(|&i| &self.providers[i].provider)
    }

    /// A provider's state.
    #[must_use]
    pub fn state_of(&self, id: &ProviderId) -> Option<&ProviderState> {
        self.by_id.get(id).map(|&i| &self.providers[i].state)
    }

    /// Picks the provider for `url`, or `None` when nothing matched.
    ///
    /// `hint` is the request's `provider_hint`: a registered hint produces [`Match::Forced`],
    /// which beats every score. An unregistered hint is ignored and normal scoring applies, so a
    /// stale client cannot make an add un-addable.
    ///
    /// # A note on the return type
    /// DESIGN §6.3 writes this as `-> Selected`. It is `Option<Selected>` here because "no
    /// provider matched" is a real outcome the engine must handle — it is exactly the
    /// `unsupported_url` case of DESIGN §5 (`magnet:`, `file:`, a scheme `ytdlp` declines) — and
    /// a synthesised `Selected` would be a lie the engine could route a job to. See
    /// `docs/INTEGRATION-NOTES.md`, WP-03.
    #[must_use]
    pub fn pick(&self, url: &Url, hint: Option<&ProviderId>) -> Option<Selected> {
        let forced = hint.and_then(|h| self.by_id.get(h)).copied();

        // (index, Match), best first. Ties break by registration order, which `max_by_key` does
        // **not** guarantee, so the fold is explicit.
        let mut best: Option<(usize, Match)> = None;
        let mut second: Option<(usize, Match)> = None;
        for (i, entry) in self.providers.iter().enumerate() {
            let m = if forced == Some(i) {
                Match::Forced
            } else {
                entry.provider.matches(url)
            };
            if !m.is_match() {
                continue;
            }
            match best {
                Some((_, bm)) if bm >= m => {
                    if second.is_none_or(|(_, sm)| m > sm) {
                        second = Some((i, m));
                    }
                }
                Some(prev) => {
                    second = Some(prev);
                    best = Some((i, m));
                }
                None => best = Some((i, m)),
            }
        }

        let (i, m) = best?;
        let entry = &self.providers[i];
        Some(Selected {
            id: entry.id.clone(),
            score: m.score(),
            reason: m.reason().unwrap_or(MatchReason::Fallback),
            runner_up: second.map(|(j, sm)| (self.providers[j].id.clone(), sm.score())),
            state: entry.state.clone(),
        })
    }

    /// The catalog of the provider [`Self::pick`] would choose, for `GET api/v2/catalog?url=`.
    ///
    /// `None` when nothing matched, for the same reason [`Self::pick`] is optional.
    #[must_use]
    pub fn catalog_for(&self, url: &Url) -> Option<(ProviderId, Arc<FormatCatalog>, MatchReason)> {
        let selected = self.pick(url, None)?;
        let catalog = self.by_id(&selected.id)?.catalog();
        Some((selected.id, catalog, selected.reason))
    }

    /// The union of every registered provider's catalog, in registration order (DESIGN §6.6).
    #[must_use]
    pub fn merged_catalog(&self) -> Arc<MergedCatalog> {
        let catalogs: Vec<Arc<FormatCatalog>> = self
            .providers
            .iter()
            .map(|e| e.provider.catalog())
            .collect();
        Arc::new(MergedCatalog::merge(&catalogs))
    }

    /// Rescans the plugin directory and swaps the `command:` providers (DESIGN §6.5).
    ///
    /// A running job is untouched: it holds its own `Arc<dyn Provider>` for its lifetime and only
    /// new jobs see the new one. With no loader installed (or plugins disabled) this is an empty
    /// report and a debug line.
    pub fn reload_commands(&mut self, dir: &Path) -> ReloadReport {
        let Some(loader) = self.loader.clone() else {
            tracing::debug!(dir = %dir.display(), "no command loader installed; skipping reload");
            return ReloadReport::empty();
        };

        let result = loader.load(dir);
        let before: Vec<(ProviderId, Option<u64>)> = self
            .providers
            .iter()
            .filter(|e| is_command_id(&e.id))
            .map(|e| (e.id.clone(), e.fingerprint))
            .collect();

        let mut report = ReloadReport {
            failed: result.failed,
            ..ReloadReport::empty()
        };

        let mut seen = Vec::with_capacity(result.plugins.len());
        for plugin in result.plugins {
            let id = plugin.provider.id();
            seen.push(id.clone());
            let previous = before.iter().find(|(bid, _)| *bid == id);
            match previous {
                None => report.added.push(id.clone()),
                Some((_, fp)) => {
                    if fp.is_none() || plugin.fingerprint.is_none() || *fp != plugin.fingerprint {
                        report.updated.push(id.clone());
                    }
                }
            }
            let state = match plugin.degraded {
                None => ProviderState::Ready,
                Some(reason) => ProviderState::Degraded {
                    reason,
                    since: self.clock.now_ms(),
                    failures: 0,
                },
            };
            self.insert_command(plugin.provider, state, plugin.fingerprint);
        }

        for (id, _) in before {
            if !seen.contains(&id) {
                self.remove(&id);
                report.removed.push(id);
            }
        }

        if !report.is_empty() {
            tracing::info!(
                added = report.added.len(),
                updated = report.updated.len(),
                removed = report.removed.len(),
                failed = report.failed.len(),
                "plugins reloaded"
            );
        }
        report
    }

    /// Inserts a `command:` provider, keeping the plugins grouped ahead of the built-ins so that
    /// registration order — the documented tie-break — does not depend on the order reloads
    /// happened to run in.
    fn insert_command(
        &mut self,
        provider: Arc<dyn Provider>,
        state: ProviderState,
        fingerprint: Option<u64>,
    ) {
        let id = provider.id();
        if self.by_id.contains_key(&id) {
            self.insert(provider, state, fingerprint);
            return;
        }
        let at = self
            .providers
            .iter()
            .rposition(|e| is_command_id(&e.id))
            .map_or(0, |i| i + 1);
        self.providers.insert(
            at,
            Entry {
                provider,
                id,
                state,
                fingerprint,
                failures: Vec::new(),
            },
        );
        self.reindex();
    }

    /// Removes a provider. Only used by [`Self::reload_commands`] for a plugin directory that
    /// disappeared.
    fn remove(&mut self, id: &ProviderId) {
        if let Some(&i) = self.by_id.get(id) {
            self.providers.remove(i);
            self.reindex();
        }
    }

    fn reindex(&mut self) {
        self.by_id = self
            .providers
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
    }

    /// Forces a provider into [`ProviderState::Degraded`], e.g. because a `probe()` came back
    /// [`crate::provider::ProviderHealth::Down`].
    pub fn set_degraded(&mut self, id: &ProviderId, reason: impl Into<Box<str>>) {
        let now = self.clock.now_ms();
        if let Some(&i) = self.by_id.get(id) {
            let failures = match &self.providers[i].state {
                ProviderState::Ready => 0,
                ProviderState::Degraded { failures, .. } => *failures,
            };
            self.providers[i].state = ProviderState::Degraded {
                reason: reason.into(),
                since: now,
                failures,
            };
        }
    }

    /// Clears a provider's degraded state and its failure history.
    pub fn set_ready(&mut self, id: &ProviderId) {
        if let Some(&i) = self.by_id.get(id) {
            self.providers[i].state = ProviderState::Ready;
            self.providers[i].failures.clear();
        }
    }

    /// Records a successful job, which retires the breaker's failure history.
    pub fn record_success(&mut self, id: &ProviderId) {
        if let Some(&i) = self.by_id.get(id) {
            self.providers[i].failures.clear();
            if !self.providers[i].state.is_ready() {
                tracing::info!(provider = %id, "provider recovered");
                self.providers[i].state = ProviderState::Ready;
            }
        }
    }

    /// Records a runtime failure and arms the circuit breaker (DESIGN §6.4).
    ///
    /// Five failures inside ten minutes put the provider in [`ProviderState::Degraded`] for a
    /// ten-minute cool-down; [`Self::allow_probation`] then releases exactly one probationary
    /// attempt. Returns `true` when this call tripped the breaker.
    pub fn record_failure(&mut self, id: &ProviderId, reason: impl Into<Box<str>>) -> bool {
        let now = self.clock.now_ms();
        let Some(&i) = self.by_id.get(id) else {
            return false;
        };
        let entry = &mut self.providers[i];
        entry.failures.retain(|t| now - *t < BREAKER_WINDOW_MS);
        entry.failures.push(now);
        let failures = u32::try_from(entry.failures.len()).unwrap_or(u32::MAX);
        if failures < BREAKER_FAILURES || !entry.state.is_ready() {
            return false;
        }
        let reason = reason.into();
        tracing::warn!(
            provider = %id,
            failures,
            window_ms = BREAKER_WINDOW_MS,
            reason = %reason,
            "circuit breaker tripped"
        );
        entry.state = ProviderState::Degraded {
            reason,
            since: now,
            failures,
        };
        true
    }

    /// Whether a degraded provider's cool-down has expired, releasing one probationary attempt.
    ///
    /// Calling this **consumes** the probation: the failure history is cleared and the provider
    /// goes back to [`ProviderState::Ready`], so a sixth failure has to accumulate five more
    /// before the breaker trips again. A provider that was degraded at construction time (a
    /// broken manifest, a missing binary) never gets probation — `failures == 0` means nothing
    /// will fix itself by waiting.
    pub fn allow_probation(&mut self, id: &ProviderId) -> bool {
        let now = self.clock.now_ms();
        let Some(&i) = self.by_id.get(id) else {
            return false;
        };
        let ProviderState::Degraded {
            since, failures, ..
        } = self.providers[i].state
        else {
            return false;
        };
        if failures == 0 || now - since < BREAKER_WINDOW_MS {
            return false;
        }
        tracing::info!(provider = %id, "cool-down expired, allowing one probationary attempt");
        self.providers[i].state = ProviderState::Ready;
        self.providers[i].failures.clear();
        true
    }
}

/// Whether an id belongs to a `command` plugin.
#[must_use]
pub fn is_command_id(id: &ProviderId) -> bool {
    id.as_str().starts_with("command:")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aulos_core::catalog::NamingPolicy;
    use aulos_core::clock::FakeClock;

    use super::*;
    use crate::provider::{
        DegradedProvider, DownloadCtx, ProviderError, ProviderHealth, ResolveCtx, SCORE_FALLBACK,
        SCORE_PATH_REGEX, SCORE_SC,
    };
    use crate::sink::ProgressSink;
    use crate::{MediaEntry, Outcome};

    /// A provider that answers a fixed [`Match`] for every URL whose host contains `host_needle`.
    struct Stub {
        id: ProviderId,
        answer: Match,
        host_needle: &'static str,
        catalog: Arc<FormatCatalog>,
        resolves: Arc<AtomicUsize>,
    }

    impl Stub {
        fn new(id: &str, answer: Match, host_needle: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id: ProviderId::parse(id).unwrap(),
                answer,
                host_needle,
                catalog: Arc::new(FormatCatalog {
                    provider: ProviderId::parse(id).unwrap(),
                    version: 1,
                    naming: NamingPolicy::Template,
                    download_types: Vec::new(),
                }),
                resolves: Arc::new(AtomicUsize::new(0)),
            })
        }
    }

    #[async_trait::async_trait]
    impl Provider for Stub {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }
        fn matches(&self, url: &Url) -> Match {
            if self.host_needle.is_empty()
                || url.host_str().is_some_and(|h| h.contains(self.host_needle))
            {
                self.answer
            } else {
                Match::No
            }
        }
        fn catalog(&self) -> Arc<FormatCatalog> {
            Arc::clone(&self.catalog)
        }
        async fn resolve(
            &self,
            _url: &Url,
            _ctx: ResolveCtx<'_>,
        ) -> Result<Vec<MediaEntry>, ProviderError> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
        async fn download(
            &self,
            _ctx: DownloadCtx<'_>,
            _sink: ProgressSink,
        ) -> Result<Outcome, ProviderError> {
            Ok(Outcome::default())
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// The production registration order of DESIGN §6.3: plugins, then SC, then the fallback.
    fn realistic() -> Registry {
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        r.register(Stub::new(
            "command:bandcamp",
            Match::Strong(SCORE_PATH_REGEX),
            "bandcamp.com",
        ));
        r.register(Stub::new(
            "streamingcommunity",
            Match::Strong(SCORE_SC),
            "streamingcommunity",
        ));
        r.register(Stub::new("ytdlp", Match::Weak(SCORE_FALLBACK), ""));
        r
    }

    #[test]
    fn the_highest_score_wins_and_the_runner_up_is_reported() {
        let r = realistic();
        let s = r.pick(&url("https://bandcamp.com/album/1"), None).unwrap();
        assert_eq!(s.id, "command:bandcamp");
        assert_eq!(s.score, 250);
        assert_eq!(s.reason, MatchReason::PathRegex);
        assert_eq!(
            s.runner_up.as_ref().map(|(id, _)| id.as_str()),
            Some("ytdlp")
        );
        assert_eq!(s.runner_up.as_ref().map(|(_, sc)| *sc), Some(1));
        assert!(s.state.is_ready());

        let s = r
            .pick(&url("https://streamingcommunity.test/watch/1"), None)
            .unwrap();
        assert_eq!(s.id, "streamingcommunity");
        assert_eq!(s.score, 200);
        assert_eq!(s.reason, MatchReason::HostContains);

        let s = r.pick(&url("https://youtube.com/watch?v=1"), None).unwrap();
        assert_eq!(s.id, "ytdlp");
        assert_eq!(s.reason, MatchReason::Fallback);
        assert_eq!(s.runner_up, None);
    }

    #[test]
    fn a_plugin_at_250_beats_sc_at_200_beats_ytdlp_at_1() {
        // One URL every provider claims, so only the scores decide.
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        r.register(Stub::new("command:p", Match::Strong(250), ""));
        r.register(Stub::new("streamingcommunity", Match::Strong(200), ""));
        r.register(Stub::new("ytdlp", Match::Weak(1), ""));
        let s = r.pick(&url("https://any.test/x"), None).unwrap();
        assert_eq!(s.id, "command:p");
        assert_eq!(s.runner_up.unwrap().0, "streamingcommunity");
    }

    #[test]
    fn ties_break_by_registration_order() {
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        r.register(Stub::new("command:first", Match::Strong(100), ""));
        r.register(Stub::new("command:second", Match::Strong(100), ""));
        let s = r.pick(&url("https://any.test/x"), None).unwrap();
        assert_eq!(s.id, "command:first");
        assert_eq!(s.runner_up.unwrap().0, "command:second");
    }

    #[test]
    fn a_forced_hint_beats_every_score() {
        let r = realistic();
        let hint = ProviderId::parse("ytdlp").unwrap();
        let s = r
            .pick(&url("https://bandcamp.com/album/1"), Some(&hint))
            .unwrap();
        assert_eq!(s.id, "ytdlp");
        assert_eq!(s.score, 255);
        assert_eq!(s.reason, MatchReason::Forced);
        assert_eq!(s.runner_up.unwrap().0, "command:bandcamp");

        // An unregistered hint is ignored rather than fatal.
        let stale = ProviderId::parse("command:gone").unwrap();
        let s = r
            .pick(&url("https://bandcamp.com/album/1"), Some(&stale))
            .unwrap();
        assert_eq!(s.id, "command:bandcamp");
    }

    #[test]
    fn a_veto_and_an_empty_registry_both_mean_no_match() {
        // `exclude_path_regex` is expressed as `Match::No` by the provider (DESIGN §6.5.1).
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        r.register(Stub::new("command:p", Match::No, ""));
        assert!(r.pick(&url("https://any.test/x"), None).is_none());
        assert!(r.catalog_for(&url("https://any.test/x")).is_none());
        assert!(
            Registry::new()
                .pick(&url("https://any.test/x"), None)
                .is_none()
        );
        assert!(Registry::new().is_empty());
    }

    #[test]
    fn catalog_for_returns_the_selected_providers_catalog() {
        let r = realistic();
        let (id, catalog, reason) = r.catalog_for(&url("https://bandcamp.com/album/1")).unwrap();
        assert_eq!(id, "command:bandcamp");
        assert_eq!(catalog.provider, "command:bandcamp");
        assert_eq!(reason, MatchReason::PathRegex);
        let merged = r.merged_catalog();
        assert_eq!(merged.providers.len(), 3);
        assert_eq!(merged.version, 3, "the sum of the contributing versions");
    }

    #[test]
    fn a_degraded_provider_still_matches_and_still_wins() {
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        let sc = Stub::new("streamingcommunity", Match::Strong(SCORE_SC), "sc.test");
        r.register_degraded(sc, "SC_HTTP=impersonate but the feature is off");
        r.register(Stub::new("ytdlp", Match::Weak(SCORE_FALLBACK), ""));

        let s = r.pick(&url("https://sc.test/watch/1"), None).unwrap();
        assert_eq!(s.id, "streamingcommunity", "must NOT fall through to ytdlp");
        assert!(!s.state.is_ready());
        assert_eq!(
            s.state.reason(),
            Some("SC_HTTP=impersonate but the feature is off")
        );
        // The runner-up is still reported, but the engine must not use it for a Degraded winner.
        assert_eq!(s.runner_up.unwrap().0, "ytdlp");
    }

    #[tokio::test]
    async fn a_degraded_stand_in_fails_every_operation() {
        let catalog = Arc::new(FormatCatalog {
            provider: ProviderId::parse("command:broken").unwrap(),
            version: 1,
            naming: NamingPolicy::Template,
            download_types: Vec::new(),
        });
        let p = DegradedProvider::new(
            ProviderId::parse("command:broken").unwrap(),
            "download.command[0] not executable",
            Arc::clone(&catalog),
            Box::new(|_| Match::Strong(100)),
        );
        assert_eq!(p.matches(&url("https://any.test/x")), Match::Strong(100));
        assert_eq!(p.reason(), "download.command[0] not executable");
        match p.probe().await {
            ProviderHealth::Down(r) => assert_eq!(&*r, "download.command[0] not executable"),
            other => panic!("expected Down, got {other:?}"),
        }
        let unmatched = DegradedProvider::unmatched(
            ProviderId::parse("command:broken").unwrap(),
            "manifest unparseable",
            catalog,
        );
        assert_eq!(unmatched.matches(&url("https://any.test/x")), Match::No);
    }

    #[test]
    fn five_failures_in_ten_minutes_trip_the_breaker() {
        let clock = Arc::new(FakeClock::default());
        let mut r = Registry::with_clock(clock.clone());
        r.register(Stub::new("ytdlp", Match::Weak(1), ""));
        let id = ProviderId::parse("ytdlp").unwrap();

        for i in 1..BREAKER_FAILURES {
            assert!(!r.record_failure(&id, "boom"), "failure {i} must not trip");
            assert!(r.state_of(&id).unwrap().is_ready());
            clock.advance(std::time::Duration::from_secs(60));
        }
        assert!(r.record_failure(&id, "boom"), "the fifth trips it");
        match r.state_of(&id).unwrap() {
            ProviderState::Degraded {
                failures, reason, ..
            } => {
                assert_eq!(*failures, 5);
                assert_eq!(&**reason, "boom");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }

        // Still inside the cool-down: no probation.
        clock.advance(std::time::Duration::from_secs(60));
        assert!(!r.allow_probation(&id));
        // The eleventh minute after the trip releases exactly one attempt.
        clock.advance(std::time::Duration::from_secs(10 * 60));
        assert!(r.allow_probation(&id));
        assert!(r.state_of(&id).unwrap().is_ready());
        assert!(!r.allow_probation(&id), "probation is consumed");
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let clock = Arc::new(FakeClock::default());
        let mut r = Registry::with_clock(clock.clone());
        r.register(Stub::new("ytdlp", Match::Weak(1), ""));
        let id = ProviderId::parse("ytdlp").unwrap();
        for _ in 0..20 {
            assert!(!r.record_failure(&id, "boom"));
            clock.advance(std::time::Duration::from_secs(11 * 60));
        }
        assert!(r.state_of(&id).unwrap().is_ready());
    }

    #[test]
    fn a_success_retires_the_failure_history() {
        let clock = Arc::new(FakeClock::default());
        let mut r = Registry::with_clock(clock);
        r.register(Stub::new("ytdlp", Match::Weak(1), ""));
        let id = ProviderId::parse("ytdlp").unwrap();
        for _ in 0..4 {
            r.record_failure(&id, "boom");
        }
        r.record_success(&id);
        for _ in 0..4 {
            assert!(!r.record_failure(&id, "boom"));
        }
        assert!(r.state_of(&id).unwrap().is_ready());
    }

    #[test]
    fn a_construction_failure_never_gets_probation() {
        let clock = Arc::new(FakeClock::default());
        let mut r = Registry::with_clock(clock.clone());
        r.register_degraded(
            Stub::new("command:broken", Match::Strong(100), ""),
            "manifest unparseable",
        );
        let id = ProviderId::parse("command:broken").unwrap();
        clock.advance(std::time::Duration::from_secs(3600));
        assert!(
            !r.allow_probation(&id),
            "waiting does not fix a broken manifest"
        );
        // A reload does.
        r.set_ready(&id);
        assert!(r.state_of(&id).unwrap().is_ready());
        r.set_degraded(&id, "again");
        assert_eq!(r.state_of(&id).unwrap().reason(), Some("again"));
    }

    #[test]
    fn registering_the_same_id_twice_replaces_it_in_place() {
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        r.register(Stub::new("command:a", Match::Strong(100), ""));
        r.register(Stub::new("ytdlp", Match::Weak(1), ""));
        r.register(Stub::new("command:a", Match::Strong(250), ""));
        assert_eq!(r.len(), 2);
        assert_eq!(
            r.ids().iter().map(ProviderId::as_str).collect::<Vec<_>>(),
            ["command:a", "ytdlp"]
        );
        assert_eq!(r.pick(&url("https://any.test/x"), None).unwrap().score, 250);
    }

    #[test]
    fn state_serialises_as_a_tagged_object() {
        assert_eq!(
            serde_json::to_value(ProviderState::Ready).unwrap(),
            serde_json::json!({ "state": "ready" })
        );
        let v = serde_json::to_value(ProviderState::Degraded {
            reason: "boom".into(),
            since: 17,
            failures: 5,
        })
        .unwrap();
        assert_eq!(v["state"], "degraded");
        assert_eq!(v["reason"], "boom");
        assert!(is_command_id(&ProviderId::parse("command:x").unwrap()));
        assert!(!is_command_id(&ProviderId::parse("ytdlp").unwrap()));
    }

    /// `(provider id, manifest fingerprint, degraded reason)`.
    type PluginRow = (&'static str, Option<u64>, Option<&'static str>);

    /// A loader standing in for WP-10's `discover`.
    struct FakeLoader(std::sync::Mutex<Vec<PluginRow>>);

    impl CommandLoader for FakeLoader {
        fn load(&self, _dir: &Path) -> CommandLoadResult {
            let plugins = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|(id, fp, degraded)| LoadedPlugin {
                    provider: Stub::new(id, Match::Strong(100), ""),
                    degraded: degraded.map(Into::into),
                    fingerprint: *fp,
                })
                .collect();
            CommandLoadResult {
                plugins,
                failed: vec![ReloadFailure {
                    name: "broken".into(),
                    reason: "download.command[0] not executable".into(),
                }],
            }
        }
    }

    #[test]
    fn reload_commands_reports_added_updated_and_removed() {
        let mut r = Registry::with_clock(Arc::new(FakeClock::default()));
        r.register(Stub::new("ytdlp", Match::Weak(1), ""));
        let dir = Path::new("/nonexistent/plugins");

        // No loader installed: an empty report, not a panic.
        assert!(r.reload_commands(dir).is_empty());

        let loader = Arc::new(FakeLoader(std::sync::Mutex::new(vec![
            ("command:a", Some(1), None),
            ("command:b", Some(2), Some("no binary")),
        ])));
        r.set_command_loader(loader.clone());

        let report = r.reload_commands(dir);
        assert_eq!(
            report
                .added
                .iter()
                .map(ProviderId::as_str)
                .collect::<Vec<_>>(),
            ["command:a", "command:b"]
        );
        assert!(report.updated.is_empty());
        assert_eq!(report.failed.len(), 1);
        // Plugins are grouped ahead of the built-ins, whatever order reloads ran in.
        assert_eq!(
            r.ids().iter().map(ProviderId::as_str).collect::<Vec<_>>(),
            ["command:a", "command:b", "ytdlp"]
        );
        assert!(
            !r.state_of(&ProviderId::parse("command:b").unwrap())
                .unwrap()
                .is_ready()
        );

        // An unchanged fingerprint is not an update; a changed one is; a gone directory is a
        // removal.
        *loader.0.lock().unwrap() = vec![("command:a", Some(1), None), ("command:c", None, None)];
        let report = r.reload_commands(dir);
        assert_eq!(
            report
                .added
                .iter()
                .map(ProviderId::as_str)
                .collect::<Vec<_>>(),
            ["command:c"]
        );
        assert!(report.updated.is_empty(), "{:?}", report.updated);
        assert_eq!(
            report
                .removed
                .iter()
                .map(ProviderId::as_str)
                .collect::<Vec<_>>(),
            ["command:b"]
        );
        assert_eq!(
            r.ids().iter().map(ProviderId::as_str).collect::<Vec<_>>(),
            ["command:a", "command:c", "ytdlp"]
        );

        *loader.0.lock().unwrap() = vec![("command:a", Some(9), None), ("command:c", None, None)];
        let report = r.reload_commands(dir);
        assert_eq!(
            report
                .updated
                .iter()
                .map(ProviderId::as_str)
                .collect::<Vec<_>>(),
            ["command:a", "command:c"],
            "a changed fingerprint and an unknown one both count as updated"
        );
    }
}
