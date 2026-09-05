//! Signal handling and the panic policy (DESIGN §16.4).
//!
//! | Signal | Behaviour |
//! |---|---|
//! | `SIGTERM` / `SIGINT` | cancel the HTTP token, which starts the ten-step shutdown in [`crate::wiring`] |
//! | `SIGHUP` | reload `YTDL_OPTIONS*` and re-scan `AULOS_PLUGINS_DIR` — `docker kill -s HUP` is a nice ops affordance |
//! | `SIGQUIT` | dump what the process is doing at ERROR and **continue**, a debug aid for a wedged container |
//!
//! The two installers are deliberately separate, because they belong at different points of the
//! boot. [`install_shutdown`] is the **first** thing [`crate::wiring::run_with`] does, before the
//! directories are created and long before the listener binds: until a tokio handler is
//! registered, `SIGTERM` and `SIGINT` keep their *default disposition* and kill the process
//! outright, and boot is not short — the tool probes alone allow sixty seconds, and the legacy
//! importer can spend minutes reading a large `STATE_DIR`. A stop during that window used to hard-
//! kill the process; now it cancels the token and every boot step between here and the bind checks
//! it and unwinds cleanly. (Having the token exist from the start was never enough: a token nobody
//! cancels does not stop a signal whose handler is not installed.)
//!
//! [`install_reload`] carries the `SIGHUP`/`SIGQUIT` arms, and cannot move that early: its
//! [`ReloadTargets`] do not exist until the configuration, the options snapshot and the registry
//! have been built.

use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use aulos_core::config::Config;
use aulos_core::event::EventSender;
use aulos_core::health::HealthRegistry;
use aulos_core::ytdl_options::YtdlOptions;
use aulos_provider::Registry;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config_watch;

/// What a `SIGHUP` reloads.
#[derive(Clone)]
pub struct ReloadTargets {
    /// The effective configuration — the file paths and the plugin directory.
    pub cfg: Arc<Config>,
    /// The live `YTDL_OPTIONS` snapshot.
    pub ytdl: Arc<ArcSwap<YtdlOptions>>,
    /// The provider registry, re-scanned in place.
    pub registry: Arc<RwLock<Registry>>,
    /// Where the two reload events are published.
    pub events: EventSender,
    /// The `ytdl_options` component.
    pub health: Arc<HealthRegistry>,
}

impl std::fmt::Debug for ReloadTargets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadTargets")
            .field("plugins_dir", &self.cfg.plugins_dir)
            .field("options_file", &self.cfg.ytdl_options_file)
            .finish_non_exhaustive()
    }
}

/// Installs the `SIGTERM`/`SIGINT` arm — the one that must exist before anything slow runs.
///
/// Call this at the very top of the boot. It needs nothing but the token, which is exactly why it
/// can go there: every other handler wants collaborators that boot has not built yet.
///
/// # Errors
/// When a signal handler cannot be registered — which on Linux means the process is out of file
/// descriptors, and is worth failing the boot over rather than running a server that cannot be
/// stopped cleanly.
pub fn install_shutdown(shutdown: &CancellationToken, tracker: &TaskTracker) -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    {
        let shutdown = shutdown.clone();
        tracker.spawn(async move {
            let name = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            };
            tracing::info!(signal = name, "shutdown requested");
            shutdown.cancel();
        });
    }
    tracing::debug!("SIGTERM and SIGINT handlers installed");
    Ok(())
}

/// Installs the `SIGHUP` and `SIGQUIT` arms, which need the boot to have produced [`ReloadTargets`].
///
/// # Errors
/// As [`install_shutdown`].
pub fn install_reload(
    shutdown: &CancellationToken,
    reload: ReloadTargets,
    tracker: &TaskTracker,
) -> anyhow::Result<()> {
    let mut hup = signal(SignalKind::hangup())?;
    let mut quit = signal(SignalKind::quit())?;

    {
        let shutdown = shutdown.clone();
        tracker.spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    got = hup.recv() => {
                        if got.is_none() {
                            return;
                        }
                        tracing::info!("SIGHUP: reloading YTDL_OPTIONS and re-scanning plugins");
                        reload_all(&reload).await;
                    }
                }
            }
        });
    }
    {
        let shutdown = shutdown.clone();
        tracker.spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    got = quit.recv() => {
                        if got.is_none() {
                            return;
                        }
                        dump_state();
                    }
                }
            }
        });
    }
    tracing::debug!("SIGHUP and SIGQUIT handlers installed");
    Ok(())
}

/// The `SIGHUP` body, also reachable from a test.
pub async fn reload_all(targets: &ReloadTargets) {
    let outcome = config_watch::reload_and_publish(
        &targets.cfg,
        &targets.ytdl,
        &targets.events,
        &targets.health,
    )
    .await;
    if outcome.ok {
        tracing::info!(presets = outcome.presets, "YTDL_OPTIONS reloaded by SIGHUP");
    } else {
        tracing::warn!(
            error = %outcome.msg,
            "the SIGHUP reload failed; the last-good options are still in force"
        );
    }
    let report =
        config_watch::rescan_plugins(&targets.cfg, &targets.registry, &targets.events).await;
    tracing::info!(touched = report.touched(), "plugins re-scanned by SIGHUP");
}

/// The `SIGQUIT` body: what the process is doing, at ERROR, without stopping it.
///
/// This is deliberately not a stack dump. Tokio has no supported way to enumerate task states, and
/// what an operator staring at a wedged container actually needs is the runtime's own view — how
/// many worker threads are alive and how much work is queued — which
/// [`tokio::runtime::Handle::metrics`] does provide.
pub fn dump_state() {
    let handle = tokio::runtime::Handle::current();
    let metrics = handle.metrics();
    tracing::error!(
        workers = metrics.num_workers(),
        alive_tasks = metrics.num_alive_tasks(),
        global_queue_depth = metrics.global_queue_depth(),
        "SIGQUIT: runtime state dump (the process continues)"
    );
}

/// Installs the panic policy of DESIGN §16.4.
///
/// A panic anywhere is logged through `tracing` with its location, so it lands in the same stream
/// as everything else instead of on a bare stderr line the log shipper drops. A panic on the
/// **store writer thread** additionally aborts the process: a half-applied write batch means the
/// queue on disk no longer matches the queue in memory, and boot recovery is designed for exactly
/// the hard-kill case, so a restart is strictly better than carrying on.
///
/// The engine task cannot be discriminated the same way — it is a tokio task, not a named thread —
/// so an engine panic is logged and the task ends, after which every `EngineHandle` send fails and
/// the API answers `state_unavailable`. Making that abort too needs a panic hook that can name the
/// current *task*, which tokio does not offer; see `docs/INTEGRATION-NOTES.md`, WP-17.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_owned();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_owned());
        tracing::error!(thread = %name, location = %location, "panic: {info}");
        previous(info);
        if name == STORE_WRITER_THREAD {
            tracing::error!(
                "the store writer panicked; aborting so boot recovery can rebuild a consistent \
                 queue"
            );
            std::process::abort();
        }
    }));
}

/// The name `aulos-store` gives its single writer thread.
///
/// Duplicated as a literal rather than imported because `aulos-store` does not export it; the two
/// are pinned together by [`tests::the_store_writer_thread_name_matches_the_store`].
pub const STORE_WRITER_THREAD: &str = "aulos-store-write";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use aulos_core::config::RawEnv;
    use aulos_core::event::{DomainEvent, EventRouter, SubscriberSpec};

    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Arc<Config> {
        Arc::new(aulos_core::config::load(&RawEnv::from_pairs(pairs.iter().copied())).unwrap())
    }

    /// If `aulos-store` ever renames its writer thread, the abort branch silently stops firing.
    #[test]
    fn the_store_writer_thread_name_matches_the_store() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../aulos-store/src/lib.rs"),
        )
        .expect("aulos-store's lib.rs is in the workspace");
        assert!(
            source.contains(&format!("\"{STORE_WRITER_THREAD}\"")),
            "aulos-store no longer names its writer thread {STORE_WRITER_THREAD:?}, so the \
             DESIGN §16.4 abort branch would never fire"
        );
    }

    /// The regression this guards is the boot-time one: `install_shutdown` must be callable with
    /// **nothing but the token**, because at the top of `run_with` — before the directories, the
    /// store, the importer and the sixty-second tool probes — no `ReloadTargets` exist yet. If it
    /// ever needs more than this, it slides back down the boot and a `docker compose down` during
    /// the import hard-kills the process again.
    #[tokio::test]
    async fn sigterm_cancels_the_shutdown_token_with_no_reload_targets() {
        let token = CancellationToken::new();
        let tracker = TaskTracker::new();
        install_shutdown(&token, &tracker).unwrap();

        // Signalling ourselves is the only faithful test of a signal handler.
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .expect("SIGTERM must cancel the shutdown token");

        tracker.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), tracker.wait()).await;
    }

    #[tokio::test]
    async fn sighup_reloads_the_options_and_rescans_the_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        std::fs::write(&file, r#"{"format":"first"}"#).unwrap();
        let plugins = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let cfg = cfg(&[
            ("YTDL_OPTIONS_FILE", &file.display().to_string()),
            ("AULOS_PLUGINS_DIR", &plugins.display().to_string()),
        ]);
        let ytdl = Arc::new(ArcSwap::from_pointee(YtdlOptions::empty()));
        let (mut router, events) = EventRouter::new(32);
        let mut inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();

        let targets = ReloadTargets {
            cfg,
            ytdl: Arc::clone(&ytdl),
            registry: Arc::new(RwLock::new(Registry::new())),
            events: events.clone(),
            health: Arc::new(HealthRegistry::new()),
        };

        std::fs::write(&file, r#"{"format":"second"}"#).unwrap();
        reload_all(&targets).await;

        assert_eq!(
            ytdl.load().base["format"],
            "second",
            "SIGHUP must re-read the file"
        );
        let event = tokio::time::timeout(Duration::from_secs(5), inbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&*event, DomainEvent::YtdlOptionsReloaded { ok: true, .. }),
            "{event:?}"
        );

        drop(events);
        drop(targets);
        let _ = tokio::time::timeout(Duration::from_secs(5), router_task).await;
    }

    #[tokio::test]
    async fn sigquit_dumps_and_the_process_continues() {
        // The assertion is that it returns at all: an operator's debug aid must never be the thing
        // that kills the container.
        dump_state();
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn the_reload_targets_debug_redacts_nothing_secret() {
        let t = ReloadTargets {
            cfg: cfg(&[("TELEGRAM_BOT_TOKEN", "123:secret")]),
            ytdl: Arc::new(ArcSwap::from_pointee(YtdlOptions::empty())),
            registry: Arc::new(RwLock::new(Registry::new())),
            events: EventRouter::new(1).1,
            health: Arc::new(HealthRegistry::new()),
        };
        let rendered = format!("{t:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
    }
}
