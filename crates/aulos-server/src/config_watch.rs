//! `YTDL_OPTIONS_FILE` / `YTDL_OPTIONS_PRESETS_FILE` hot reload and the plugin-directory re-scan
//! (DESIGN §17.2, §16.1 step 13).
//!
//! # Why the **parent directory** is watched
//!
//! Legacy used `watchfiles.awatch(<file>)` with a `samefile` filter, which has a real failure
//! mode: `vim`, `docker cp`, Ansible and `sed -i` all *replace* the file
//! (`write(tmp); rename(tmp, target)`), which invalidates an inode-level watch. The next edit is
//! then invisible forever, with nothing in the log to say so. Watching the parent directory
//! non-recursively and filtering by **file name** survives the replace, and it is also what makes
//! a *deleted* file heal when it comes back.
//!
//! # The reload contract
//!
//! On failure the **last-good** options stay in force (DESIGN §17.2 Δ) — legacy re-read
//! `YTDL_OPTIONS` from the environment first and *then* failed, silently discarding the whole
//! file's contribution, so a typo quietly changed download behaviour. Runtime overrides
//! (`cookiefile`) are re-applied after every successful reload, as legacy did.
//!
//! Every reload — from the watcher, from `SIGHUP`, or from `POST api/v2/ytdl-options/reload` —
//! publishes `DomainEvent::YtdlOptionsReloaded { ok, msg, update_time }`, which is the exact legacy
//! payload and becomes the WS `ytdl_options` frame.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::config::Config;
use aulos_core::event::{DomainEvent, EventSender};
use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_core::reload::ReloadReport;
use aulos_core::ytdl_options::YtdlOptions;
use aulos_provider::Registry;
use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The `healthz` component key for the options file (DESIGN §16.3).
pub const COMPONENT: &str = "ytdl_options";

/// The outcome of one reload, in the exact legacy payload shape.
#[derive(Clone, Debug, PartialEq)]
pub struct ReloadOutcome {
    /// Whether the reload succeeded.
    pub ok: bool,
    /// The legacy error string, empty on success.
    pub msg: Box<str>,
    /// The options file's mtime as fractional epoch seconds, or `None`.
    pub update_time: Option<f64>,
    /// How many presets the effective snapshot holds.
    pub presets: usize,
}

impl ReloadOutcome {
    /// `healthz.components.ytdl_options` (DESIGN §16.3).
    #[must_use]
    pub fn component(&self) -> ComponentHealth {
        let status = if self.ok {
            ComponentStatus::Ok
        } else {
            // Degraded, not down: the last-good options are still in force, so downloads work.
            ComponentStatus::Degraded
        };
        let mut c = ComponentHealth::new(status)
            .with("update_time", self.update_time)
            .with("presets", self.presets);
        if !self.ok {
            c = c.with("detail", self.msg.to_string());
        }
        c
    }

    /// The `DomainEvent` this outcome publishes.
    #[must_use]
    pub fn event(&self) -> DomainEvent {
        DomainEvent::YtdlOptionsReloaded {
            ok: self.ok,
            msg: self.msg.clone(),
            update_time: self.update_time,
        }
    }
}

/// Re-reads `YTDL_OPTIONS*` and swaps the snapshot in on success.
///
/// Synchronous on purpose: it is two small file reads, and every caller (the watcher task, the
/// `SIGHUP` handler, the boot sequence) already owns a task. Blocking a runtime worker for a
/// `/config/ytdl-options.json` read is cheaper than the `spawn_blocking` round trip.
#[must_use]
pub fn reload_options(cfg: &Config, ytdl: &ArcSwap<YtdlOptions>) -> ReloadOutcome {
    match YtdlOptions::load(
        &cfg.ytdl_options,
        cfg.ytdl_options_file.as_deref(),
        &cfg.ytdl_options_presets,
        cfg.ytdl_options_presets_file.as_deref(),
    ) {
        Ok(mut fresh) => {
            let previous = ytdl.load_full();
            fresh.inherit_overrides(&previous);
            let update_time = fresh.file_mtime;
            let presets = fresh.presets.len();
            ytdl.store(Arc::new(fresh));
            ReloadOutcome {
                ok: true,
                msg: "".into(),
                update_time,
                presets,
            }
        }
        Err(e) => {
            let kept = ytdl.load();
            tracing::warn!(error = %e, "the YTDL_OPTIONS reload failed; keeping the last-good set");
            ReloadOutcome {
                ok: false,
                msg: e.to_string().into_boxed_str(),
                update_time: kept.file_mtime,
                presets: kept.presets.len(),
            }
        }
    }
}

/// Reloads, publishes the event and updates the health component. Returns the outcome.
pub async fn reload_and_publish(
    cfg: &Config,
    ytdl: &ArcSwap<YtdlOptions>,
    events: &EventSender,
    health: &HealthRegistry,
) -> ReloadOutcome {
    let outcome = reload_options(cfg, ytdl);
    health.set(COMPONENT, outcome.component());
    events.publish(outcome.event()).await;
    outcome
}

/// Re-scans `AULOS_PLUGINS_DIR` and publishes `ProvidersReloaded` when anything changed.
///
/// The report is published only when it is non-empty, because a `SIGHUP` every minute from a cron
/// job must not put an identical `providers` frame on the wire each time (DESIGN §16.3: a warning
/// alone is not a change).
pub async fn rescan_plugins(
    cfg: &Config,
    registry: &RwLock<Registry>,
    events: &EventSender,
) -> ReloadReport {
    let report = {
        let mut guard = registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.reload_commands(&cfg.plugins_dir)
    };
    if report.is_empty() {
        tracing::debug!("the plugin re-scan found no change");
    } else {
        tracing::info!(
            added = report.added.len(),
            updated = report.updated.len(),
            removed = report.removed.len(),
            failed = report.failed.len(),
            "plugins re-scanned"
        );
        events
            .publish(DomainEvent::ProvidersReloaded(Arc::new(report.clone())))
            .await;
    }
    report
}

/// Whether a filesystem event concerns one of the watched file names (DESIGN §17.2 step 2).
///
/// `Create`, `Modify(Data|Any|Name)` and `Remove` are accepted; a rename is what an atomic replace
/// produces, and legacy's `{modified, added, deleted}` set maps onto the same three. Metadata-only
/// events (a `chmod`, an access-time bump) are ignored, because they cannot change the contents.
#[must_use]
pub fn accepts(event: &notify::Event, names: &HashSet<OsString>) -> bool {
    let kind_ok = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(
                notify::event::ModifyKind::Data(_)
                    | notify::event::ModifyKind::Any
                    | notify::event::ModifyKind::Name(_)
            )
            | EventKind::Any
    );
    kind_ok
        && event
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| names.contains(n)))
}

/// A `(mtime, len)` fingerprint of every target, for the poll fallback (DESIGN §17.2 step 8).
#[must_use]
pub fn fingerprint(targets: &[PathBuf]) -> Vec<Option<(std::time::SystemTime, u64)>> {
    targets
        .iter()
        .map(|p| {
            std::fs::metadata(p)
                .ok()
                .and_then(|m| m.modified().ok().map(|t| (t, m.len())))
        })
        .collect()
}

/// The watcher (DESIGN §17.2).
#[derive(Debug)]
pub struct ConfigWatcher {
    targets: Vec<PathBuf>,
    cfg: Arc<Config>,
    ytdl: Arc<ArcSwap<YtdlOptions>>,
    events: EventSender,
    health: Arc<HealthRegistry>,
    shutdown: CancellationToken,
    debounce: Duration,
    poll: Option<Duration>,
    force_poll: bool,
}

impl ConfigWatcher {
    /// The PLAN WP-17 entry point: watch `targets`, reload into `ytdl`, publish onto `events`.
    ///
    /// Deviations from the PLAN's signature, both additive: a [`HealthRegistry`] (DESIGN §17.2
    /// step 6 requires a *deleted* file to mark the component degraded, and the registry is the
    /// only place that can be said) and a [`CancellationToken`] (so the DESIGN §16.4 shutdown can
    /// stop the task rather than aborting it mid-reload).
    ///
    /// # Errors
    /// Only when **no** watch backend can be created at all *and* the poll fallback is disabled
    /// (`AULOS_CONFIG_POLL_SECS=0`) — the one case where the caller has to be told that edits will
    /// never be noticed.
    pub fn spawn(
        targets: Vec<PathBuf>,
        cfg: Arc<Config>,
        ytdl: Arc<ArcSwap<YtdlOptions>>,
        events: EventSender,
    ) -> anyhow::Result<JoinHandle<()>> {
        Self::new(targets, cfg, ytdl, events, Arc::new(HealthRegistry::new())).start()
    }

    /// A watcher with an explicit health registry.
    #[must_use]
    pub fn new(
        targets: Vec<PathBuf>,
        cfg: Arc<Config>,
        ytdl: Arc<ArcSwap<YtdlOptions>>,
        events: EventSender,
        health: Arc<HealthRegistry>,
    ) -> Self {
        let debounce = Duration::from_millis(cfg.config_debounce_ms.max(1));
        let poll = (cfg.config_poll_secs > 0).then(|| Duration::from_secs(cfg.config_poll_secs));
        Self {
            targets,
            cfg,
            ytdl,
            events,
            health,
            shutdown: CancellationToken::new(),
            debounce,
            poll,
            force_poll: false,
        }
    }

    /// The token the DESIGN §16.4 shutdown cancels.
    #[must_use]
    pub fn with_shutdown(mut self, token: CancellationToken) -> Self {
        self.shutdown = token;
        self
    }

    /// Overrides the debounce window. The tests use single-digit milliseconds.
    #[must_use]
    pub const fn with_debounce(mut self, d: Duration) -> Self {
        self.debounce = d;
        self
    }

    /// Overrides the poll period. `None` disables the fallback.
    #[must_use]
    pub const fn with_poll(mut self, poll: Option<Duration>) -> Self {
        self.poll = poll;
        self
    }

    /// Skips the native backend entirely, exercising the poll fallback (DESIGN §17.2 step 8).
    ///
    /// This is what "with inotify disabled, the poll fallback still reloads" means as a test: a
    /// container whose `fs.inotify` limits are exhausted takes exactly this path.
    #[must_use]
    pub const fn with_force_poll(mut self, force: bool) -> Self {
        self.force_poll = force;
        self
    }

    /// Spawns the loop.
    ///
    /// # Errors
    /// See [`ConfigWatcher::spawn`].
    pub fn start(self) -> anyhow::Result<JoinHandle<()>> {
        // Nothing to watch: `YTDL_OPTIONS` came from the environment alone. The task still exists,
        // so `SIGHUP` and the reload route behave identically either way, but it only waits for
        // shutdown.
        if self.targets.is_empty() {
            let shutdown = self.shutdown.clone();
            tracing::debug!("no YTDL_OPTIONS file is configured; nothing to watch");
            return Ok(tokio::spawn(async move { shutdown.cancelled().await }));
        }

        let names: HashSet<OsString> = self
            .targets
            .iter()
            .filter_map(|p| p.file_name().map(std::ffi::OsStr::to_os_string))
            .collect();
        let dirs: Vec<PathBuf> = {
            let mut dirs: Vec<PathBuf> = self
                .targets
                .iter()
                .filter_map(|p| parent_dir(p))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            dirs.sort();
            dirs
        };

        let (tx, rx) = mpsc::unbounded_channel::<notify::Event>();
        let watcher = if self.force_poll {
            None
        } else {
            match build_native(tx.clone(), &dirs) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "the native file watcher is unavailable; falling back to polling"
                    );
                    None
                }
            }
        };
        let polled = watcher.is_none();
        if polled && self.poll.is_none() {
            anyhow::bail!(
                "no file-watch backend is available and AULOS_CONFIG_POLL_SECS=0, so edits to \
                 {} would never be noticed",
                self.targets
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for dir in &dirs {
            tracing::info!(dir = %dir.display(), polled, "watching for YTDL_OPTIONS changes");
        }

        Ok(tokio::spawn(self.run(rx, names, watcher)))
    }

    /// The loop, so a test can drive it on the current task.
    async fn run(
        self,
        mut rx: mpsc::UnboundedReceiver<notify::Event>,
        names: HashSet<OsString>,
        watcher: Option<Box<dyn notify::Watcher + Send>>,
    ) {
        // The watcher is moved into the task so its lifetime is the task's: dropping it stops the
        // backend thread, which is what makes shutdown clean.
        let _watcher = watcher;
        let mut prints = fingerprint(&self.targets);
        let mut pending: Option<tokio::time::Instant> = None;

        loop {
            let poll_sleep = self.poll.unwrap_or(Duration::from_secs(3600));
            let debounce_at = pending;
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    tracing::debug!("the config watcher is stopping");
                    return;
                }
                event = rx.recv() => match event {
                    Some(event) => {
                        if accepts(&event, &names) {
                            tracing::debug!(?event.kind, "a watched config file changed");
                            pending = Some(tokio::time::Instant::now() + self.debounce);
                        }
                    }
                    // The backend went away; the poll fallback (if any) carries on.
                    None => {
                        if self.poll.is_none() {
                            tracing::warn!("the file watcher stopped and polling is disabled");
                            return;
                        }
                        tokio::time::sleep(poll_sleep).await;
                        if self.poll_changed(&mut prints) {
                            self.fire().await;
                        }
                    }
                },
                () = sleep_until_opt(debounce_at) => {
                    pending = None;
                    self.fire().await;
                    prints = fingerprint(&self.targets);
                }
                () = tokio::time::sleep(poll_sleep), if self.poll.is_some() => {
                    if self.poll_changed(&mut prints) {
                        tracing::debug!("the poll fallback saw a change");
                        // Go through the debounce, so a native event and a poll for the same edit
                        // still reload once.
                        pending = Some(tokio::time::Instant::now() + self.debounce);
                    }
                }
            }
        }
    }

    /// Whether the `(mtime, len)` fingerprint moved since the last check.
    fn poll_changed(&self, prints: &mut Vec<Option<(std::time::SystemTime, u64)>>) -> bool {
        let now = fingerprint(&self.targets);
        let changed = now != *prints;
        *prints = now;
        changed
    }

    async fn fire(&self) {
        let outcome = reload_and_publish(&self.cfg, &self.ytdl, &self.events, &self.health).await;
        if outcome.ok {
            tracing::info!(
                presets = outcome.presets,
                update_time = outcome.update_time,
                "YTDL_OPTIONS reloaded"
            );
        }
    }
}

/// Sleeps until `at`, or forever when there is nothing pending.
async fn sleep_until_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The target's parent directory, canonicalised when possible.
///
/// Canonicalisation is best-effort: the file may not exist yet (`YTDL_OPTIONS_FILE` pointing at a
/// `/config` volume an operator has not populated), and a watch on the uncanonicalised parent
/// still works — it just does not collapse two paths that reach the same directory through a
/// symlink.
fn parent_dir(target: &Path) -> Option<PathBuf> {
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty())?;
    Some(
        parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf()),
    )
}

/// The platform's own watcher, watching each directory non-recursively.
fn build_native(
    tx: mpsc::UnboundedSender<notify::Event>,
    dirs: &[PathBuf],
) -> notify::Result<Box<dyn notify::Watcher + Send>> {
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                let _ = tx.send(event);
            }
            Err(e) => tracing::warn!(error = %e, "the file watcher reported an error"),
        })?;
    for dir in dirs {
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
    }
    Ok(Box::new(watcher))
}

/// The files a given configuration wants watched, in a stable order.
#[must_use]
pub fn targets_of(cfg: &Config) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = &cfg.ytdl_options_file {
        out.push(p.clone());
    }
    // DESIGN §17.2 step 9: legacy did not watch the presets file despite its README claiming so.
    if let Some(p) = &cfg.ytdl_options_presets_file {
        out.push(p.clone());
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::config::RawEnv;
    use aulos_core::event::{EventRouter, SubscriberSpec};

    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Arc<Config> {
        Arc::new(aulos_core::config::load(&RawEnv::from_pairs(pairs.iter().copied())).unwrap())
    }

    fn names(list: &[&str]) -> HashSet<OsString> {
        list.iter().map(|n| OsString::from(*n)).collect()
    }

    fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::new(),
        }
    }

    #[test]
    fn the_filter_accepts_a_write_and_rename_and_a_delete_but_not_a_chmod() {
        use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
        let want = names(&["opts.json"]);

        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Any,
        ] {
            assert!(
                accepts(&event(kind, &["/config/opts.json"]), &want),
                "{kind:?} must be accepted"
            );
        }
        assert!(
            !accepts(
                &event(
                    EventKind::Modify(ModifyKind::Metadata(
                        notify::event::MetadataKind::Permissions
                    )),
                    &["/config/opts.json"]
                ),
                &want
            ),
            "a chmod cannot change the contents"
        );
        assert!(
            !accepts(
                &event(EventKind::Create(CreateKind::File), &["/config/other.json"]),
                &want
            ),
            "a sibling file must not trigger a reload"
        );
    }

    #[test]
    fn the_targets_are_the_options_file_then_the_presets_file() {
        let dir = tempfile::tempdir().unwrap();
        let opts = dir.path().join("o.json");
        let presets = dir.path().join("p.json");
        std::fs::write(&opts, "{}").unwrap();
        std::fs::write(&presets, "{}").unwrap();
        let cfg = config(&[
            ("YTDL_OPTIONS_FILE", &opts.display().to_string()),
            ("YTDL_OPTIONS_PRESETS_FILE", &presets.display().to_string()),
        ]);
        assert_eq!(targets_of(&cfg), vec![opts, presets]);
        assert!(targets_of(&config(&[])).is_empty());
    }

    #[test]
    fn a_failed_reload_keeps_the_last_good_options_and_degrades_the_component() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        std::fs::write(&file, r#"{"format":"bestaudio"}"#).unwrap();
        let cfg = config(&[("YTDL_OPTIONS_FILE", &file.display().to_string())]);
        let ytdl = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));

        let first = reload_options(&cfg, &ytdl);
        assert!(first.ok, "{first:?}");
        assert_eq!(ytdl.load().base["format"], "bestaudio");
        assert_eq!(first.component().status, ComponentStatus::Ok);

        // A deleted file fails with the legacy string, keeps the options, and degrades.
        std::fs::remove_file(&file).unwrap();
        let second = reload_options(&cfg, &ytdl);
        assert!(!second.ok);
        assert!(second.msg.contains("not found"), "{second:?}");
        assert_eq!(
            ytdl.load().base["format"],
            "bestaudio",
            "the last-good options must survive"
        );
        assert_eq!(second.component().status, ComponentStatus::Degraded);

        // Re-creating it heals, without a restart.
        std::fs::write(&file, r#"{"format":"bestvideo"}"#).unwrap();
        let third = reload_options(&cfg, &ytdl);
        assert!(third.ok);
        assert_eq!(ytdl.load().base["format"], "bestvideo");
    }

    #[test]
    fn a_reload_re_applies_the_runtime_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        std::fs::write(&file, "{}").unwrap();
        let cfg = config(&[("YTDL_OPTIONS_FILE", &file.display().to_string())]);
        let mut initial = YtdlOptions::empty();
        initial.set_runtime_override("cookiefile", "/state/cookies.txt".into());
        let ytdl = Arc::new(ArcSwap::from_pointee(initial));

        assert!(reload_options(&cfg, &ytdl).ok);
        assert_eq!(
            ytdl.load().overrides["cookiefile"],
            "/state/cookies.txt",
            "the cookie override must survive a reload, as in legacy"
        );
    }

    /// Five rapid edits must produce exactly one reload (DESIGN §17.2 step 3).
    #[tokio::test]
    async fn rapid_edits_coalesce_into_one_reload() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        std::fs::write(&file, "{}").unwrap();
        let cfg = config(&[("YTDL_OPTIONS_FILE", &file.display().to_string())]);
        let ytdl = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));
        let (mut router, sender) = EventRouter::new(64);
        let mut inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();

        let shutdown = CancellationToken::new();
        let task = ConfigWatcher::new(
            targets_of(&cfg),
            Arc::clone(&cfg),
            Arc::clone(&ytdl),
            sender.clone(),
            Arc::new(HealthRegistry::new()),
        )
        .with_shutdown(shutdown.clone())
        .with_debounce(Duration::from_millis(120))
        .with_poll(None)
        .start()
        .unwrap();

        // Five `sed -i`-style replaces in a tight loop.
        for i in 0..5 {
            let tmp = dir.path().join(format!("opts.json.tmp{i}"));
            std::fs::write(&tmp, format!(r#"{{"retries":{i}}}"#)).unwrap();
            std::fs::rename(&tmp, &file).unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let first = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
            .await
            .expect("one reload event must arrive")
            .expect("the router must forward it");
        assert!(
            matches!(&*first, DomainEvent::YtdlOptionsReloaded { ok: true, .. }),
            "{first:?}"
        );
        assert_eq!(ytdl.load().base["retries"], 4, "the last edit must win");

        // Nothing else within a debounce window and a half.
        let extra = tokio::time::timeout(Duration::from_millis(400), inbox.recv()).await;
        assert!(
            extra.is_err(),
            "the five edits coalesced into one: {extra:?}"
        );

        shutdown.cancel();
        drop(sender);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), router_task).await;
    }

    /// A `vim`-style write-and-rename is the case an inode watch misses entirely.
    #[tokio::test]
    async fn a_write_and_rename_triggers_exactly_one_reload() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        std::fs::write(&file, "{}").unwrap();
        let cfg = config(&[("YTDL_OPTIONS_FILE", &file.display().to_string())]);
        let ytdl = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));
        let (mut router, sender) = EventRouter::new(64);
        let mut inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();

        let shutdown = CancellationToken::new();
        let task = ConfigWatcher::new(
            targets_of(&cfg),
            Arc::clone(&cfg),
            Arc::clone(&ytdl),
            sender.clone(),
            Arc::new(HealthRegistry::new()),
        )
        .with_shutdown(shutdown.clone())
        .with_debounce(Duration::from_millis(80))
        .with_poll(None)
        .start()
        .unwrap();

        let tmp = dir.path().join(".opts.json.swp");
        std::fs::write(&tmp, r#"{"format":"worst"}"#).unwrap();
        std::fs::rename(&tmp, &file).unwrap();

        let event = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
            .await
            .expect("the rename must be noticed")
            .unwrap();
        assert!(matches!(
            &*event,
            DomainEvent::YtdlOptionsReloaded { ok: true, .. }
        ));
        assert_eq!(ytdl.load().base["format"], "worst");

        shutdown.cancel();
        drop(sender);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), router_task).await;
    }

    /// With no native backend at all — a container whose `fs.inotify` limits are exhausted — the
    /// poll fallback still reloads (DESIGN §17.2 step 8).
    #[tokio::test]
    async fn the_poll_fallback_reloads_without_a_native_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        std::fs::write(&file, "{}").unwrap();
        let cfg = config(&[("YTDL_OPTIONS_FILE", &file.display().to_string())]);
        let ytdl = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));
        let (mut router, sender) = EventRouter::new(64);
        let mut inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();

        let shutdown = CancellationToken::new();
        let task = ConfigWatcher::new(
            targets_of(&cfg),
            Arc::clone(&cfg),
            Arc::clone(&ytdl),
            sender.clone(),
            Arc::new(HealthRegistry::new()),
        )
        .with_shutdown(shutdown.clone())
        .with_debounce(Duration::from_millis(20))
        .with_poll(Some(Duration::from_millis(30)))
        .with_force_poll(true)
        .start()
        .unwrap();

        // The fingerprint is `(mtime, len)`, so change the length as well as the contents: a
        // filesystem with second-granularity mtimes would otherwise hide the edit.
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::fs::write(&file, r#"{"format":"bestaudio","retries":10}"#).unwrap();

        let event = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
            .await
            .expect("the poll fallback must notice the edit")
            .unwrap();
        assert!(matches!(
            &*event,
            DomainEvent::YtdlOptionsReloaded { ok: true, .. }
        ));
        assert_eq!(ytdl.load().base["format"], "bestaudio");

        shutdown.cancel();
        drop(sender);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), router_task).await;
    }

    #[tokio::test]
    async fn no_configured_file_still_spawns_a_task_that_stops_on_shutdown() {
        let cfg = config(&[]);
        let ytdl = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));
        let (router, sender) = EventRouter::new(8);
        let router_task = router.spawn();
        let shutdown = CancellationToken::new();
        let task = ConfigWatcher::new(
            targets_of(&cfg),
            cfg,
            ytdl,
            sender.clone(),
            Arc::new(HealthRegistry::new()),
        )
        .with_shutdown(shutdown.clone())
        .start()
        .unwrap();
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the idle watcher must stop")
            .unwrap();
        drop(sender);
        let _ = tokio::time::timeout(Duration::from_secs(5), router_task).await;
    }

    #[tokio::test]
    async fn a_rescan_of_an_empty_plugin_dir_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(&[("AULOS_PLUGINS_DIR", &dir.path().display().to_string())]);
        let (mut router, sender) = EventRouter::new(8);
        let mut inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();
        let registry = RwLock::new(Registry::new());

        let report = rescan_plugins(&cfg, &registry, &sender).await;
        assert!(report.is_empty(), "{report:?}");
        drop(sender);
        let _ = tokio::time::timeout(Duration::from_secs(5), router_task).await;
        assert!(inbox.recv().await.is_none(), "no frame for a no-op rescan");
    }
}
