//! The task graph (DESIGN §16.1 steps 10–16, §2.2, §2.2.1) and the graceful shutdown (§16.4).
//!
//! This is the only place in the workspace that decides **who receives what**. Every other crate
//! takes its collaborators as arguments precisely so that this file is the single answer to "what
//! is connected to what", and so that the two failure modes that are *silent* — an unwired
//! `HookFinalizer` and an unwired `PreTerminalHooks` — are visible in one screenful.
//!
//! # The order, and why the listener is last
//!
//! ```text
//!  1..9   bootstrap: config, tracing, dirs, store, importer, options, plugins, tool probes
//!  10     POT supervisor
//!  11     EventRouter::new → subscribe(aggregator, hooks, telegram) → EventSender to producers
//!         Store actor (already running), EventHub, Aggregator, QueueEngine
//!  12     boot recovery: re-queue in-flight items, recompute group counters
//!  13     HookDispatcher, SubscriptionScheduler, ConfigWatcher, health publisher
//!  14     Telegram actor, then EventRouter::spawn() — no subscriber after this point
//!  15     bind HOST:PORT (TLS when HTTPS=true), serve with graceful shutdown
//!  16     log "aulos-server <version> listening on …"
//! ```
//!
//! Steps 5–12 complete before the bind, so the first request already sees a consistent snapshot
//! rather than an empty queue that fills in over the next second. `SO_REUSEPORT` is set (parity
//! with legacy's `supports_reuse_port()`), which also makes a blue/green port swap possible.
//!
//! # The three cancellation tokens
//!
//! One token would conflate three different deadlines, so there are three, cancelled in order:
//!
//! | Token | Cancelled at | Reaches |
//! |---|---|---|
//! | `http` | the signal | axum's graceful shutdown, the subscription/Telegram pollers, the config watcher, the health publisher |
//! | `jobs` | after `AULOS_SHUTDOWN_GRACE_SECS` | every download's `CancellationToken`, so `killpg` reaches its process group (DESIGN §16.4 step 5) |
//! | `pot` | after the store is closed | the sidecar (step 9), so it outlives every job that might still need a POT token |

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aulos_api::{ApiState, ServerInfo};
use aulos_core::BootId;
use aulos_core::clock::{Clock, SystemClock};
use aulos_core::config::Config;
use aulos_core::event::{EventRouter, SubscriberSpec};
use aulos_core::health::HealthRegistry;
use aulos_core::subscription::SubscriptionsHandle;
use aulos_hooks::{AudioSyncHook, Hook, HookDispatcher, JellyfinHook, ManifestHook, NfoHook};
use aulos_provider::Provider;
use aulos_provider::Registry;
use aulos_provider::sink::{ProgressMsg, ProgressSinkFactory};
use aulos_queue::{Aggregator, Engine, EngineHookStore, EventHub};
use aulos_store::Store;
use aulos_subscriptions::{Checker, Manager, SubDeps};
use aulos_telegram::{TelegramActor, TelegramConfig, TgInitError};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::adapters::{DispatcherPreTerminal, EngineFinalizer, SwapOptions};
use crate::{bootstrap, config_watch, health, pot, signals};

/// The `ProgressMsg` channel budget (DESIGN §2.3).
pub const PROGRESS_CAPACITY: usize = 8_192;

/// The `DomainEvent` router inbox budget (DESIGN §2.3).
pub const EVENT_CAPACITY: usize = 4_096;

/// The `SubCmd` channel budget.
pub const SUB_CAPACITY: usize = 64;

/// How long the process waits for its tasks after the store is closed (DESIGN §16.4 step 10).
pub const TRACKER_CEILING: Duration = Duration::from_secs(10);

/// How long a WebSocket is given to notice the close frame before the listener task is dropped.
pub const WS_CLOSE_GRACE: Duration = Duration::from_secs(2);

/// How the server is started, so the acceptance tests can drive the real wiring.
///
/// Everything here has a production default; a test overrides only what it must. The alternative —
/// a second, test-only wiring — is how a build ends up with a graph nobody has exercised.
pub struct RunOptions {
    /// The effective configuration.
    pub cfg: Arc<Config>,
    /// Wall-clock and monotonic time.
    pub clock: Arc<dyn Clock>,
    /// Extra providers, registered **before** `ytdlp` so they win the score tie-break.
    pub providers: Vec<Arc<dyn Provider>>,
    /// Extra completion hooks, appended to the built-ins.
    pub extra_hooks: Vec<Arc<dyn Hook>>,
    /// Cancelled to start the DESIGN §16.4 shutdown.
    pub shutdown: CancellationToken,
    /// Told the bound address once the listener is up (step 16).
    pub ready: Option<oneshot::Sender<SocketAddr>>,
    /// Install the `SIGTERM`/`SIGINT`/`SIGHUP`/`SIGQUIT` handlers.
    pub install_signals: bool,
    /// Skip the DESIGN §16.1 step 9 tool probes.
    ///
    /// The probes are **fatal** when `python3` + `yt-dlp` are missing, which is right for the
    /// image and wrong for a test that supplies its own provider and never touches yt-dlp.
    pub skip_tool_probes: bool,
}

impl RunOptions {
    /// The production defaults.
    #[must_use]
    pub fn new(cfg: Arc<Config>) -> Self {
        Self {
            cfg,
            clock: Arc::new(SystemClock),
            providers: Vec::new(),
            extra_hooks: Vec::new(),
            shutdown: CancellationToken::new(),
            ready: None,
            install_signals: true,
            skip_tool_probes: false,
        }
    }
}

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("port", &self.cfg.port)
            .field("providers", &self.providers.len())
            .field("extra_hooks", &self.extra_hooks.len())
            .field("install_signals", &self.install_signals)
            .finish_non_exhaustive()
    }
}

/// The PLAN WP-17 entry point: boot, serve, shut down cleanly.
///
/// # Errors
/// Anything in DESIGN §16.1 steps 4–12 — an uncreatable directory, an unopenable database, a fatal
/// legacy import, unloadable `YTDL_OPTIONS`, a missing `python3`/`yt-dlp`, a failed recovery, or a
/// port that cannot be bound.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    run_with(RunOptions::new(Arc::new(cfg))).await
}

/// [`run`] with the seams a test needs.
///
/// # Errors
/// See [`run`].
#[allow(clippy::too_many_lines)] // the boot order IS the function; splitting it would hide it
pub async fn run_with(opts: RunOptions) -> anyhow::Result<()> {
    let RunOptions {
        cfg,
        clock,
        providers: extra_providers,
        extra_hooks,
        shutdown: http_token,
        ready,
        install_signals,
        skip_tool_probes,
    } = opts;

    let tracker = TaskTracker::new();
    let jobs_token = CancellationToken::new();
    let pot_token = CancellationToken::new();

    // --- 4. the directories ---------------------------------------------------------------
    bootstrap::make_dirs(&cfg)?;

    let health = Arc::new(HealthRegistry::new());

    // --- 5..6. the database, and the legacy importer on a first start ---------------------
    let store = bootstrap::open_store(&cfg, &health).await?;

    // --- 7. YTDL_OPTIONS and the cookie jar ----------------------------------------------
    let ytdl = bootstrap::load_ytdl_options(&cfg)?;

    // --- 8. the provider registry ---------------------------------------------------------
    let (registry, hook_specs) = bootstrap::build_registry(&cfg);
    if !extra_providers.is_empty() {
        // Registered at the front so they win a score tie against `ytdlp` (DESIGN §6.3).
        let mut guard = registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing: Vec<Arc<dyn Provider>> =
            guard.iter().map(|(_, p, _)| Arc::clone(p)).collect();
        *guard = Registry::new();
        for p in extra_providers.into_iter().chain(existing) {
            guard.register(p);
        }
    }

    // --- 9. the tool probes ----------------------------------------------------------------
    let yt_dlp = if skip_tool_probes {
        None
    } else {
        bootstrap::probe_tools(&cfg, &health).await?
    };

    // --- 10. the POT sidecar ---------------------------------------------------------------
    let (event_router, events) = EventRouter::new(EVENT_CAPACITY);
    let mut event_router = event_router;
    let (pot_supervisor, pot_task) =
        pot::PotSupervisor::builder(pot::PotSettings::from_config(&cfg))
            .with_enabled(cfg.pot_enabled)
            .with_shutdown(pot_token.clone())
            .with_clock(Arc::clone(&clock))
            .with_events(events.clone())
            .spawn_with(Arc::clone(&health));
    tracker.spawn(async move {
        let _ = pot_task.await;
    });

    // --- 11. the event router's subscribers, the hub, the aggregator and the engine --------
    //
    // The DESIGN §2.2.1 table, verbatim. The `hooks` filter is `Finishing | Completed`, and the
    // aggregator's deliberately omits `Finishing`: a `Finishing` event describes a status
    // transition that has **not been written yet**, so framing it would put a state on the wire
    // that the database does not hold.
    let aggregator_inbox = event_router.subscribe(SubscriberSpec::aggregator());
    let hooks_inbox = event_router.subscribe(SubscriberSpec::hooks());
    let telegram_inbox = event_router.subscribe(SubscriberSpec::telegram());

    let hub = EventHub::new(store.seq_allocator(), BootId::new(), &cfg);
    health.set_identity(hub.boot_id());

    let (progress_tx, progress_rx) = mpsc::channel::<ProgressMsg>(PROGRESS_CAPACITY);
    let sink = ProgressSinkFactory::new(progress_tx.clone());

    let (engine, engine_handle) = Engine::new(
        store.clone(),
        Arc::clone(&registry),
        Arc::clone(&cfg),
        Arc::clone(&ytdl),
        Arc::clone(&clock),
        events.clone(),
        progress_tx,
    );

    // The completion hooks, built once and shared: the dispatcher runs them, and the *same* list
    // answers the engine's pre-terminal question through `DispatcherPreTerminal`. Two lists could
    // disagree about whether `audio_sync` applies, and the engine would then wait for a phase that
    // never runs (or skip one that should have).
    let hooks = build_hooks(&cfg, hook_specs, extra_hooks);
    let pre_terminal = Arc::new(DispatcherPreTerminal::new(&hooks));
    tracing::info!(
        hooks = ?hooks.iter().map(|h| h.id().to_string()).collect::<Vec<_>>(),
        pre_terminal = ?pre_terminal.ids().iter().map(std::string::ToString::to_string).collect::<Vec<_>>(),
        "completion hooks registered"
    );

    let mut engine = engine
        .with_pre_terminal(pre_terminal)
        .with_shutdown(jobs_token.clone());

    // --- 12. boot recovery, before anything can serve a snapshot --------------------------
    let recovery = engine.recover().await?;
    tracing::info!(
        requeued_resolving = recovery.requeued_resolving,
        requeued_running = recovery.requeued_running,
        scheduled = recovery.scheduled,
        parked = recovery.parked,
        terminal_window = recovery.terminal,
        terminal_total = recovery.terminal_total,
        groups = recovery.groups,
        orphan_temp = recovery.orphan_temp.len(),
        policy = recovery.policy,
        "boot recovery complete"
    );

    // `with_done_total` is what stops a restart reporting `done_total` as the *window* length,
    // which every client would read as "my history was truncated to 500 rows".
    let (aggregator, state) = Aggregator::new(hub.clone(), Arc::clone(&cfg), Arc::clone(&clock));
    let aggregator_task = aggregator.with_done_total(recovery.terminal_total).spawn(
        progress_rx,
        aggregator_inbox,
        engine_handle.clone(),
    );
    tracker.spawn(async move {
        let _ = aggregator_task.await;
    });

    let engine_task = engine.spawn();
    tracker.spawn(async move {
        let _ = engine_task.await;
    });

    // --- 13. the hook dispatcher, the subscription manager, the watchers, the health pass --
    let dispatcher = HookDispatcher::with_hooks(Arc::clone(&cfg), hooks, Arc::clone(&clock))
        .with_finalizer(Arc::new(EngineFinalizer::new(engine_handle.clone())))
        .with_cancel(jobs_token.clone());
    // Taken before `spawn` consumes the dispatcher, as `HookDispatcher::health_handle`'s docs say.
    let hooks_health = dispatcher.health_handle();
    let hook_store: Arc<dyn aulos_core::ports::HookStore> =
        Arc::new(EngineHookStore::new(engine_handle.clone(), store.clone()));
    let dispatcher_task = dispatcher.spawn(hooks_inbox, sink.clone(), hook_store);
    tracker.spawn(async move {
        let _ = dispatcher_task.await;
    });

    let (subs_handle, sub_rx) = SubscriptionsHandle::channel(SUB_CAPACITY);
    let checker = Arc::new(Checker::new(
        store.clone(),
        Arc::clone(&registry),
        Arc::clone(&cfg),
        Arc::new(SwapOptions::new(Arc::clone(&ytdl))),
        engine_handle.clone(),
        Arc::clone(&clock),
    ));
    let mut manager = Manager::new(
        SubDeps::new(
            store.clone(),
            Arc::clone(&cfg),
            Arc::clone(&clock),
            events.clone(),
            checker,
        ),
        sub_rx,
    );
    if let Err(e) = manager.load().await {
        // A subscription table that cannot be read is not worth refusing to serve downloads over.
        tracing::error!(error = %e, "the subscriptions could not be loaded");
    }
    let manager_task = manager.spawn();
    tracker.spawn(async move {
        let _ = manager_task.await;
    });

    let watcher_task = config_watch::ConfigWatcher::new(
        config_watch::targets_of(&cfg),
        Arc::clone(&cfg),
        Arc::clone(&ytdl),
        events.clone(),
        Arc::clone(&health),
    )
    .with_shutdown(http_token.clone())
    .start()?;
    tracker.spawn(async move {
        let _ = watcher_task.await;
    });
    // The boot value of the component, so `healthz` reports the presets count from the first
    // request rather than from the first edit.
    health.set(
        config_watch::COMPONENT,
        config_watch::ReloadOutcome {
            ok: true,
            msg: "".into(),
            update_time: ytdl.load().file_mtime,
            presets: ytdl.load().presets.len(),
        }
        .component(),
    );

    // --- 14. the Telegram actor, then the router ------------------------------------------
    spawn_telegram(
        &cfg,
        &store,
        &engine_handle,
        &clock,
        &health,
        telegram_inbox,
        http_token.clone(),
        &tracker,
    )
    .await;

    // No subscriber may be registered after this point — and cannot be, because `spawn` consumes
    // the router (DESIGN §2.2.1).
    let router_task = event_router.spawn();
    tracker.spawn(async move {
        let _ = router_task.await;
    });

    // The recovered working set reaches the published snapshot through the router and the
    // aggregator, both of which have only just been spawned — so "steps 5–12 complete before the
    // bind" is not yet true from a *reader's* point of view. Waiting here is what makes it true:
    // without it the very first `GET api/v2/state` (or the first WebSocket `snapshot`) answers an
    // empty queue for up to one `AULOS_WS_BATCH_MS` tick, which a client reads as "everything I
    // had is gone".
    await_first_publish(&state, &recovery, &cfg).await;

    let health_probes = health::Probes {
        store: store.clone(),
        state: state.clone(),
        registry: Arc::clone(&registry),
        cfg: Arc::clone(&cfg),
        sink: sink.clone(),
        hooks: Some(hooks_health),
        subs: subs_handle.clone(),
    };
    // One pass before the bind, so the very first `healthz` is complete.
    health::publish_once(&health_probes, &health).await;
    {
        let health = Arc::clone(&health);
        let events = events.clone();
        let token = http_token.clone();
        tracker.spawn(health::run(health_probes, health, events, token));
    }

    // --- 15. the listener --------------------------------------------------------------------
    let mut api_state = ApiState::new(
        engine_handle.clone(),
        state.clone(),
        hub.clone(),
        store.clone(),
        Arc::clone(&registry),
        Arc::clone(&cfg),
        Arc::clone(&ytdl),
        Arc::clone(&health),
        subs_handle.clone(),
    )
    .with_clock(Arc::clone(&clock));
    if let Some(version) = &yt_dlp {
        // Without this `capabilities.yt_dlp`, `GET <p>version` and `healthz.yt_dlp` all stay
        // `null` (the WP-14 request in `docs/INTEGRATION-NOTES.md`).
        api_state = api_state
            .with_info(ServerInfo::new(&cfg, clock.as_ref()).with_yt_dlp(version.to_string()));
    }
    let app = aulos_api::router(api_state);

    let listener = bind(&cfg).await?;
    let addr = listener.local_addr()?;
    tracing::info!(
        version = %cfg.version,
        %addr,
        prefix = %cfg.url_prefix,
        v1_shim = cfg.v1_enabled,
        "aulos-server listening"
    );
    // --- 16. the announce line, on stdout so a supervisor can read it ---------------------
    println!(
        "aulos-server {} listening on {}{} (v1 shim: {})",
        cfg.version,
        addr,
        cfg.url_prefix,
        if cfg.v1_enabled { "on" } else { "off" }
    );
    if let Some(tx) = ready {
        let _ = tx.send(addr);
    }

    if install_signals {
        signals::install(
            &http_token,
            signals::ReloadTargets {
                cfg: Arc::clone(&cfg),
                ytdl: Arc::clone(&ytdl),
                registry: Arc::clone(&registry),
                events: events.clone(),
                health: Arc::clone(&health),
            },
            &tracker,
        )?;
    }

    let served = tokio::spawn(serve(listener, app, Arc::clone(&cfg), http_token.clone()));

    http_token.cancelled().await;
    tracing::info!("shutting down");

    // --- DESIGN §16.4, steps 1..10 -----------------------------------------------------------
    //
    // 1..3 already happened: `http_token` is what axum's graceful shutdown, the Telegram poller,
    // the config watcher and the health publisher all wait on.
    //
    // 4. let in-flight downloads finish, up to the grace.
    let grace = Duration::from_secs(cfg.shutdown_grace_secs);
    if grace > Duration::ZERO {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let counts = state.load().counts;
            let active = counts.downloading + counts.postprocessing + counts.resolving;
            if active == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    active,
                    grace_s = cfg.shutdown_grace_secs,
                    "the shutdown grace expired with downloads still running; killing them"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // The ids to hand back to the next boot, captured **before** the kill: once the jobs are
    // cancelled the engine writes them `canceled`, and a `canceled` row is terminal.
    let interrupted: Vec<aulos_core::ItemId> = state
        .load()
        .items
        .iter()
        .filter(|i| i.status.is_running() || i.status == aulos_core::Status::Resolving)
        .map(|i| i.id)
        .collect();

    // 5. `killpg SIGTERM`, then `SIGKILL`, for every job — each job task owns that ladder and
    //    observes this token.
    jobs_token.cancel();

    // 6. Mark the still-active rows `queued` so the next boot resumes them.
    //
    //    `aulos-queue` has no shutdown command, and the engine's own reaction to a cancelled job
    //    is to write `canceled` — which is right for a user's cancel and wrong for a shutdown,
    //    because a `canceled` row is terminal and would never be picked up again. So the rows are
    //    rewritten through the store's own `WriteOp`, after the engine has settled: the store has
    //    exactly one writer, so "last write wins" is deterministic, and the loser is the engine's
    //    `canceled` — which is the outcome DESIGN §16.4 step 6 asks for. See
    //    `docs/INTEGRATION-NOTES.md`, WP-17: an additive `EngineCmd::Shutdown` would let the
    //    engine write this itself.
    if !interrupted.is_empty() {
        settle(&state).await;
        let now = clock.now_ms();
        let ops: Vec<aulos_store::WriteOp> = interrupted
            .iter()
            .map(|id| aulos_store::WriteOp::SetStatus {
                id: *id,
                status: aulos_core::Status::Queued,
                msg: aulos_core::FieldUpdate::Set(SHUTDOWN_MSG.into()),
                error: aulos_core::FieldUpdate::Keep,
                auto_start: Some(true),
                at: now,
            })
            .collect();
        tracing::info!(
            count = ops.len(),
            "handing interrupted downloads back to the next boot"
        );
        if let Err(e) = store.write(ops, aulos_store::Durability::Sync).await {
            tracing::warn!(error = %e, "the interrupted rows could not be re-queued");
        }
    }

    // 7. The final aggregator flush happens when its two inputs close, which is what dropping
    //    these does.
    let _ = tokio::time::timeout(WS_CLOSE_GRACE, served).await;
    drop(engine_handle);
    drop(subs_handle);
    drop(events);
    drop(sink);
    drop(state);

    // 8. drain the store actor, checkpoint the WAL, `PRAGMA optimize`, close.
    tracker.close();
    let _ = tokio::time::timeout(TRACKER_CEILING, tracker.wait()).await;
    if let Err(e) = store.close().await {
        tracing::warn!(error = %e, "the store did not close cleanly");
    }

    // 9. the POT child.
    pot_token.cancel();
    let state = pot_supervisor.state();
    tracing::info!(pot = ?state.status, "the sidecar supervisor was told to stop");

    // 10. exit 0.
    tracing::info!("aulos-server stopped");
    Ok(())
}

/// The `msg` an interrupted row carries into the next boot (DESIGN §16.4 step 6).
pub const SHUTDOWN_MSG: &str = "Interrupted by shutdown";

/// How long the bind waits for the recovered set to reach the published snapshot.
///
/// Generous, because the alternative is worse: a snapshot that is briefly empty reads to a client
/// as "the server lost my queue". On the timeout the bind proceeds anyway with a WARN — refusing
/// to serve at all would be a strictly worse failure than one stale first read.
const FIRST_PUBLISH_CEILING: Duration = Duration::from_secs(10);

/// How long the shutdown waits for the engine to settle after the jobs are killed.
const SETTLE_CEILING: Duration = Duration::from_secs(5);

/// Waits until the aggregator has published the rows boot recovery loaded.
async fn await_first_publish(
    state: &aulos_queue::StateView,
    recovery: &aulos_queue::RecoveryReport,
    cfg: &Config,
) {
    let expected = usize::try_from(recovery.requeued_resolving)
        .unwrap_or(0)
        .saturating_add(usize::try_from(recovery.requeued_running).unwrap_or(0))
        .saturating_add(usize::try_from(recovery.scheduled).unwrap_or(0))
        .saturating_add(usize::try_from(recovery.parked).unwrap_or(0))
        .saturating_add(usize::try_from(recovery.terminal).unwrap_or(0));
    if expected == 0 {
        return;
    }
    let deadline = tokio::time::Instant::now() + FIRST_PUBLISH_CEILING;
    let step = Duration::from_millis(cfg.ws_batch_ms.clamp(1, 50));
    loop {
        if state.load().len() >= expected {
            tracing::debug!(expected, "the recovered queue is published");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                expected,
                published = state.load().len(),
                "binding before the recovered queue was fully published"
            );
            return;
        }
        tokio::time::sleep(step).await;
    }
}

/// Waits (bounded) until no item is running or resolving any more.
async fn settle(state: &aulos_queue::StateView) {
    let deadline = tokio::time::Instant::now() + SETTLE_CEILING;
    while tokio::time::Instant::now() < deadline {
        let counts = state.load().counts;
        if counts.downloading + counts.postprocessing + counts.preparing + counts.resolving == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The stock hook list plus one [`ManifestHook`] per community `[[hook]]` plus any injected hooks.
///
/// `HookDispatcher::new` builds the same list; it is spelled out here because the **same** `Vec`
/// has to reach `DispatcherPreTerminal`, and `HookDispatcher` exposes only the ids.
fn build_hooks(
    cfg: &Arc<Config>,
    specs: Vec<aulos_provider::command::HookSpec>,
    extra: Vec<Arc<dyn Hook>>,
) -> Vec<Arc<dyn Hook>> {
    let mut hooks: Vec<Arc<dyn Hook>> = vec![
        Arc::new(AudioSyncHook::new()),
        Arc::new(NfoHook::from_config(cfg)),
        Arc::new(JellyfinHook::new(cfg)),
    ];
    for spec in specs {
        hooks.push(Arc::new(ManifestHook::new(spec, &cfg.plugins_dir)));
    }
    hooks.extend(extra);
    hooks
}

/// Builds and spawns the Telegram actor, or logs one line and carries on (DESIGN §12.1).
///
/// A missing token must never stop the server: the bot is an optional integration, and a container
/// that refuses to start because `TELEGRAM_BOT_TOKEN` is empty is strictly worse than one that
/// serves HTTP and says so in the log.
#[allow(clippy::too_many_arguments)] // one argument per collaborator, all of them required
async fn spawn_telegram(
    cfg: &Arc<Config>,
    store: &Store,
    engine: &aulos_queue::EngineHandle,
    clock: &Arc<dyn Clock>,
    health: &Arc<HealthRegistry>,
    inbox: aulos_core::event::EventInbox,
    shutdown: CancellationToken,
    tracker: &TaskTracker,
) {
    let tg_cfg = Arc::new(TelegramConfig::from_config(cfg));
    let catalog = Arc::new(aulos_core::catalog::ytdlp_catalog());
    let transport = if tg_cfg.enabled && !tg_cfg.token.trim().is_empty() {
        Some(Arc::new(aulos_telegram::TeloxideTransport::new(
            teloxide_bot(&tg_cfg.token),
        )))
    } else {
        None
    };

    // `TelegramActor::new` builds its own transport, which would leave the long-polling loop with
    // no `teloxide::Bot` to poll (`transport::poll_updates` takes one). The gating `new` performs
    // is reproduced above — enabled, non-empty token — and `with_transport` then checks the
    // allow-list.
    let actor = match transport {
        None if !tg_cfg.enabled => {
            tracing::info!("the Telegram bot is disabled");
            health.set("telegram", health::telegram_component(None));
            return;
        }
        None => {
            tracing::error!("TELEGRAM_BOT_ENABLED=true but TELEGRAM_BOT_TOKEN is empty");
            health.set("telegram", health::telegram_component(None));
            return;
        }
        Some(transport) => {
            let bot = transport.bot().clone();
            match TelegramActor::with_transport(
                Arc::clone(&tg_cfg),
                store.clone(),
                engine.clone(),
                catalog,
                Arc::clone(clock),
                transport as Arc<dyn aulos_telegram::Transport>,
            ) {
                Ok(actor) => (actor, bot),
                Err(TgInitError::Disabled) => {
                    tracing::info!("the Telegram bot is disabled");
                    health.set("telegram", health::telegram_component(None));
                    return;
                }
                Err(e) => {
                    tracing::error!("the Telegram bot will not start: {e}");
                    health.set("telegram", health::telegram_component(None));
                    return;
                }
            }
        }
    };
    let (mut actor, bot) = actor;

    if let Err(e) = actor.load().await {
        tracing::warn!(error = %e, "the Telegram chat defaults could not be loaded");
    }
    health.set(
        "telegram",
        health::telegram_component(Some(&actor.health())),
    );

    // Both taken **before** `spawn` consumes the actor (the WP-16 note in
    // `docs/INTEGRATION-NOTES.md`).
    let incoming = actor.incoming();
    let actor_task = actor.spawn(inbox);
    tracker.spawn(async move {
        let _ = actor_task.await;
    });
    tracker.spawn(aulos_telegram::poll_updates(bot, incoming, shutdown));
}

/// A `teloxide::Bot` for a token.
fn teloxide_bot(token: &str) -> teloxide::Bot {
    teloxide::Bot::new(token)
}

/// Binds `HOST:PORT` with `SO_REUSEPORT` where the platform supports it.
///
/// Parity with legacy's `supports_reuse_port()`, and it is what makes a blue/green port swap
/// possible on the VPS: the new container can bind before the old one lets go.
///
/// # Errors
/// A malformed `HOST`, or a port that is already in use.
pub async fn bind(cfg: &Config) -> anyhow::Result<tokio::net::TcpListener> {
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("HOST/PORT is not an address: {e}"))?;
    let socket = if addr.is_ipv4() {
        tokio::net::TcpSocket::new_v4()
    } else {
        tokio::net::TcpSocket::new_v6()
    }?;
    socket.set_reuseaddr(true)?;
    if let Err(e) = socket.set_reuseport(true) {
        tracing::debug!("SO_REUSEPORT is unavailable on this platform: {e}");
    }
    socket
        .bind(addr)
        .map_err(|e| anyhow::anyhow!("could not bind {addr}: {e}"))?;
    Ok(socket.listen(1_024)?)
}

/// Serves `app` on `listener` until `shutdown` is cancelled.
///
/// # The one deviation from DESIGN §16.4 step 2
///
/// A WebSocket session is closed with `1001 "server shutting down"` when its frame bus closes,
/// which happens when the last `EventHub` is dropped — and one of them lives in the router's
/// state, i.e. inside this future. So the sequence is: signal axum's graceful shutdown, give the
/// sockets [`WS_CLOSE_GRACE`] to notice, then drop the future. A client that has not read its
/// close frame by then sees a TCP close and reconnects, which is the same observable behaviour one
/// hop later. An additive `EventHub::close()` in `aulos-queue` would let step 2 happen exactly as
/// written — see `docs/INTEGRATION-NOTES.md`, WP-17.
async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    cfg: Arc<Config>,
    shutdown: CancellationToken,
) {
    if cfg.https {
        serve_tls(listener, app, &cfg, shutdown).await;
        return;
    }
    let signal = async move { shutdown.cancelled().await };
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(signal)
        .await
    {
        tracing::error!(error = %e, "the HTTP listener stopped");
    }
}

/// The `HTTPS=true` path (BRIEF: kept, and not on the e2e path).
async fn serve_tls(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    cfg: &Config,
    shutdown: CancellationToken,
) {
    let (Some(cert), Some(key)) = (cfg.certfile.as_ref(), cfg.keyfile.as_ref()) else {
        tracing::error!("HTTPS=true requires CERTFILE and KEYFILE");
        return;
    };
    // `axum-server` is built without a crypto provider, so one has to be installed once per
    // process. A second install is not an error worth reporting: it means another component got
    // there first, and either provider serves.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                error = %e,
                cert = %cert.display(),
                key = %key.display(),
                "the TLS certificate could not be loaded"
            );
            return;
        }
    };
    let std_listener = match listener.into_std() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "the listener could not be converted for TLS");
            return;
        }
    };
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            handle.graceful_shutdown(Some(WS_CLOSE_GRACE));
        });
    }
    if let Err(e) = axum_server::from_tcp_rustls(std_listener, config)
        .handle(handle)
        .serve(app.into_make_service())
        .await
    {
        tracing::error!(error = %e, "the HTTPS listener stopped");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::config::RawEnv;

    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Arc<Config> {
        Arc::new(aulos_core::config::load(&RawEnv::from_pairs(pairs.iter().copied())).unwrap())
    }

    #[tokio::test]
    async fn the_listener_sets_reuseport_and_reports_its_address() {
        let c = cfg(&[("HOST", "127.0.0.1"), ("PORT", "0")]);
        let listener = bind(&c).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0, "port 0 must resolve to a real port");

        // SO_REUSEPORT really is set: a second bind to the same address succeeds, which is what
        // makes a blue/green port swap possible.
        let second = tokio::net::TcpSocket::new_v4().unwrap();
        second.set_reuseaddr(true).unwrap();
        let reuseport = second.set_reuseport(true).is_ok();
        if reuseport {
            assert!(
                second.bind(addr).is_ok(),
                "SO_REUSEPORT was accepted but a second bind failed"
            );
        }
    }

    #[tokio::test]
    async fn an_occupied_port_fails_the_bind_with_the_address_in_the_message() {
        let first = bind(&cfg(&[("HOST", "127.0.0.1"), ("PORT", "0")]))
            .await
            .unwrap();
        let port = first.local_addr().unwrap().port().to_string();
        // Without SO_REUSEPORT support the second bind is the interesting case; with it, the
        // second bind succeeds and there is nothing to assert, so this only checks the error
        // *shape* when the platform refuses.
        let second = bind(&cfg(&[("HOST", "127.0.0.1"), ("PORT", &port)])).await;
        if let Err(e) = second {
            assert!(e.to_string().contains("127.0.0.1"), "{e}");
        }
    }

    #[test]
    fn the_hook_list_is_the_three_built_ins_in_dispatcher_order() {
        let c = cfg(&[]);
        let hooks = build_hooks(&c, Vec::new(), Vec::new());
        let ids: Vec<String> = hooks.iter().map(|h| h.id().to_string()).collect();
        assert_eq!(ids, ["audio_sync", "nfo", "jellyfin"]);
        // And exactly one of them is pre-terminal, which is the seam that must not be missed.
        assert_eq!(
            DispatcherPreTerminal::new(&hooks)
                .ids()
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            ["audio_sync"]
        );
    }

    #[test]
    fn the_channel_budgets_are_the_design_2_3_numbers() {
        assert_eq!(PROGRESS_CAPACITY, 8_192);
        assert_eq!(EVENT_CAPACITY, 4_096);
    }

    /// The DESIGN §2.2.1 table, asserted as a table.
    #[test]
    fn the_three_subscribers_are_registered_with_the_documented_specs() {
        use aulos_core::event::{DropPolicy, EventKind};

        let agg = SubscriberSpec::aggregator();
        assert_eq!(agg.name, "aggregator");
        assert_eq!(agg.capacity, 1024);
        assert!(matches!(agg.policy, DropPolicy::Block));
        assert!(
            !agg.filter.allows(EventKind::Finishing),
            "the aggregator must NOT see Finishing: it would frame a status that is not written"
        );

        let hooks = SubscriberSpec::hooks();
        assert_eq!(hooks.name, "hooks");
        assert_eq!(hooks.capacity, 256);
        assert!(matches!(hooks.policy, DropPolicy::DropNewest));
        assert!(hooks.filter.allows(EventKind::Finishing));
        assert!(hooks.filter.allows(EventKind::Completed));
        assert!(!hooks.filter.allows(EventKind::Added));

        let telegram = SubscriberSpec::telegram();
        assert_eq!(telegram.name, "telegram");
        assert_eq!(telegram.capacity, 512);
        assert!(matches!(telegram.policy, DropPolicy::DropNewest));
        assert!(!telegram.filter.allows(EventKind::Finishing));
    }
}
