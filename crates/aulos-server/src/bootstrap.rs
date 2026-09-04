//! Boot steps 1–9 of DESIGN §16.1: configuration, tracing, directories, the database, the
//! importer, `YTDL_OPTIONS`, the plugin scan and the tool probes.
//!
//! Every one of these completes **before** the listener binds (step 15), so the first request
//! already sees a consistent snapshot. Splitting them out of [`crate::wiring`] keeps the task
//! graph readable and lets `check-config` / `doctor` / `import` reuse the same code paths the
//! server takes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use arc_swap::ArcSwap;
use aulos_core::config::{self, Config, LogFormat, RawEnv};
use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_core::ytdl_options::YtdlOptions;
use aulos_provider::Registry;
use aulos_provider::command::provider::{CommandPluginLoader, PluginEnv};
use aulos_provider_sc::ScProvider;
use aulos_provider_ytdlp::YtdlpProvider;
use aulos_store::import::{self, ImportOpts, ImportReport, OnError};
use aulos_store::{Store, StoreOptions};

/// The exit code for invalid configuration (BRIEF §15, DESIGN §16.1 step 1).
pub const EXIT_CONFIG: i32 = 2;

/// The `healthz` component key for the legacy importer (DESIGN §16.3).
pub const IMPORTER_COMPONENT: &str = "importer";

/// The default `tracing` directives, i.e. legacy's `dampenThirdPartyLoggers()` (DESIGN §16.5).
///
/// These are *defaults*: they are prepended to the filter, so an explicit `RUST_LOG=hyper=trace`
/// still wins. Without them a `LOGLEVEL=DEBUG` run is unreadable — `h2` alone emits a line per
/// HTTP/2 frame.
pub const THIRD_PARTY_DAMPENING: &[&str] = &[
    "hyper=warn",
    "h2=warn",
    "rustls=warn",
    "reqwest=warn",
    "teloxide=warn",
    "notify=warn",
    "html5ever=warn",
    "tungstenite=warn",
    "selectors=warn",
    "sqlx=warn",
];

/// Everything the boot sequence produced, handed to [`crate::wiring`].
pub struct Booted {
    /// The effective configuration.
    pub cfg: Arc<Config>,
    /// The store, already migrated and (if this was a first start) imported.
    pub store: Store,
    /// The live `YTDL_OPTIONS` snapshot.
    pub ytdl: Arc<ArcSwap<YtdlOptions>>,
    /// The provider registry, with the built-ins and the command plugins registered.
    pub registry: Arc<RwLock<Registry>>,
    /// The community `[[hook]]` specs the plugin scan found.
    pub hook_specs: Vec<aulos_provider::command::HookSpec>,
    /// The `healthz` component map every probe writes into.
    pub health: Arc<HealthRegistry>,
    /// The yt-dlp version the shim reported, for `ServerInfo` (`null` when the shim did not run).
    pub yt_dlp: Option<Box<str>>,
}

/// Builds the effective configuration, reporting **every** error at once.
///
/// DESIGN §17.1 step 9: fixing a compose file one boot at a time is the experience this avoids, so
/// the two option files' errors join the same report as the environment's. `check-config` prints
/// the same list.
///
/// # Errors
/// The rendered report. The caller prints it and exits [`EXIT_CONFIG`].
pub fn load_config(env: &RawEnv) -> Result<(Config, Vec<String>), String> {
    use std::fmt::Write as _;

    let (cfg, warnings) = match config::load_with_warnings(env) {
        Ok((cfg, warns)) => (
            cfg,
            warns
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
        ),
        Err(errors) => {
            let mut out = format!("configuration is invalid ({} errors):\n", errors.len());
            for e in &errors {
                let _ = writeln!(out, "  {e}");
            }
            return Err(out);
        }
    };
    // Step 8 lives here rather than in `config::load`, which is deliberately IO-free.
    if let Err(e) = YtdlOptions::load(
        &cfg.ytdl_options,
        cfg.ytdl_options_file.as_deref(),
        &cfg.ytdl_options_presets,
        cfg.ytdl_options_presets_file.as_deref(),
    ) {
        return Err(format!("configuration is invalid (1 error):\n  {e}\n"));
    }
    Ok((cfg, warnings))
}

/// Installs the `tracing` subscriber (DESIGN §16.5).
///
/// `RUST_LOG` overrides everything — a documented escape hatch — and otherwise the base level is
/// `LOGLEVEL` with the third-party dampening prepended.
pub fn init_tracing(cfg: &Config) {
    use tracing_subscriber::EnvFilter;

    let filter = match std::env::var("RUST_LOG") {
        Ok(explicit) if !explicit.trim().is_empty() => explicit,
        _ => {
            let base = cfg.loglevel.to_lowercase();
            let mut directives = THIRD_PARTY_DAMPENING.join(",");
            // The bare level goes last so it applies to everything the dampening did not name;
            // `EnvFilter` resolves the most specific target, not the last directive.
            directives.push(',');
            directives.push_str(&base);
            directives
        }
    };
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());

    // `try_init` rather than `init`: a second installation (a test that boots two servers in one
    // process) must not abort.
    let result = match cfg.log_format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init(),
        LogFormat::Text => tracing_subscriber::fmt()
            .compact()
            .with_env_filter(filter)
            .with_ansi(ansi)
            .with_writer(std::io::stderr)
            .try_init(),
    };
    let _ = result;
}

/// Logs the effective configuration table with every secret redacted (DESIGN §16.1 step 3).
pub fn log_effective_config(env: &RawEnv) {
    match env.effective_redacted() {
        Ok(effective) => {
            for (key, value) in &effective {
                tracing::debug!(target: "aulos_server::config", "{key} = {value}");
            }
            tracing::info!(keys = effective.len(), "effective configuration loaded");
        }
        Err(errors) => {
            for e in &errors {
                tracing::warn!("{e}");
            }
        }
    }
}

/// Creates every directory the server writes into (DESIGN §16.1 step 4).
///
/// # Errors
/// The first directory that cannot be created. This is fatal: a server that cannot write into
/// `DOWNLOAD_DIR` fails every download, and failing at boot with the path in the message is a far
/// better experience than failing per item.
pub fn make_dirs(cfg: &Config) -> anyhow::Result<()> {
    let mut dirs: Vec<PathBuf> = vec![
        cfg.paths.download.clone(),
        cfg.paths.audio_download.clone(),
        cfg.paths.temp.clone(),
        cfg.paths.state.clone(),
    ];
    if let Some(parent) = cfg.db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        dirs.push(parent.to_path_buf());
    }
    dirs.sort();
    dirs.dedup();
    for dir in &dirs {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("could not create {}: {e}", dir.display()))?;
    }
    tracing::debug!(
        count = dirs.len(),
        "download, state and temp directories ready"
    );
    Ok(())
}

/// Opens the database and, when this is a first start, runs the legacy importer
/// (DESIGN §16.1 steps 5–6, §7.6).
///
/// The importer runs **only** when the database file did not exist: an existing database means the
/// import already happened (or was deliberately skipped), and re-running it would resurrect rows
/// the operator deleted. On a fatal import the database is deleted and the error is returned, so
/// the next boot retries from a clean slate rather than serving half a queue.
///
/// # Errors
/// A store that cannot be opened, or a fatal import.
pub async fn open_store(cfg: &Config, health: &HealthRegistry) -> anyhow::Result<Store> {
    let fresh = !cfg.db_path.exists();
    let store = Store::open(StoreOptions::from_config(cfg))
        .map_err(|e| anyhow::anyhow!("could not open {}: {e}", cfg.db_path.display()))?;
    for warning in store.id_warnings() {
        // BRIEF scope trims: `repair-ids` is CUT, so the boot consistency check WARNs and
        // continues rather than refusing to start.
        tracing::warn!(target: "aulos_store::alloc", "{warning}");
    }

    if fresh {
        tracing::info!(
            state_dir = %cfg.paths.state.display(),
            "a new database; looking for legacy state to import"
        );
        match import::import(&cfg.paths.state, &store, import_opts(cfg)).await {
            Ok(report) => {
                tracing::info!("\n{}", report.render_table());
                health.set(IMPORTER_COMPONENT, importer_component(Some(&report)));
            }
            Err(fatal) => {
                tracing::error!("\n{}", fatal.report.render_table());
                let _ = store.close().await;
                if fatal.should_delete_db()
                    && let Err(e) = import::delete_db_files(&cfg.db_path)
                {
                    tracing::warn!("could not remove {}: {e}", cfg.db_path.display());
                }
                anyhow::bail!("the legacy import failed: {fatal}");
            }
        }
    } else {
        let stored = import::stored_report(&store).await.ok().flatten();
        health.set(IMPORTER_COMPONENT, importer_component(stored.as_ref()));
    }
    Ok(store)
}

/// The importer options a `serve` run uses.
#[must_use]
pub fn import_opts(cfg: &Config) -> ImportOpts {
    ImportOpts {
        dry_run: false,
        force: false,
        on_error: OnError::from(cfg.import_on_error),
        clear_completed_after_s: cfg.clear_completed_after,
        max_seen_ids: cfg.subscription_max_seen_ids,
    }
}

/// `healthz.components.importer` (DESIGN §16.3, §7.6.1).
///
/// "Degraded for the life of the process" is deliberate: a skipped `completed.json` means rows are
/// missing from history, and an operator who only ever looks at `healthz` should still find out.
#[must_use]
pub fn importer_component(report: Option<&ImportReport>) -> ComponentHealth {
    match report {
        None => ComponentHealth::new(ComponentStatus::Disabled)
            .with("detail", "no legacy state was imported"),
        Some(report) => {
            let status = if report.is_degraded() {
                ComponentStatus::Degraded
            } else {
                ComponentStatus::Ok
            };
            let skipped: Vec<String> = report
                .skipped_files()
                .into_iter()
                .map(str::to_owned)
                .collect();
            ComponentHealth::new(status)
                .with("imported_at", report.imported_at)
                .with("warnings", report.warnings.len())
                .with("errors", report.errors.len())
                .with("skipped_files", skipped)
        }
    }
}

/// Loads `YTDL_OPTIONS*` and adopts `<STATE_DIR>/cookies.txt` when it exists
/// (DESIGN §16.1 step 7, §17.2).
///
/// # Errors
/// The legacy error string, verbatim. Fatal: an unparseable options file changes what every
/// download does, and legacy exited on it too.
pub fn load_ytdl_options(cfg: &Config) -> anyhow::Result<Arc<ArcSwap<YtdlOptions>>> {
    let mut options = YtdlOptions::load(
        &cfg.ytdl_options,
        cfg.ytdl_options_file.as_deref(),
        &cfg.ytdl_options_presets,
        cfg.ytdl_options_presets_file.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Legacy did this only inside its `__main__` block, so an imported cookie jar was invisible
    // to a subscription check (DESIGN §17.2).
    let cookies = cfg.paths.state.join(import::COOKIES_FILE);
    if cookies.is_file() {
        tracing::info!(path = %cookies.display(), "adopting the existing cookie jar");
        options.set_runtime_override(
            import::COOKIEFILE_KEY,
            serde_json::Value::String(cookies.display().to_string()),
        );
    }
    tracing::info!(
        presets = options.presets.len(),
        update_time = options.file_mtime,
        "YTDL_OPTIONS loaded"
    );
    Ok(Arc::new(ArcSwap::from_pointee(options)))
}

/// Builds the provider registry (DESIGN §16.1 step 8, §6.3).
///
/// Registration order is the tie-break for equal scores, so the specific providers come first and
/// `ytdlp` — the catch-all fallback, which answers `Match::Weak` to everything — comes last. A
/// `command` plugin that claims a host `streamingcommunity` also claims therefore loses, which is
/// the documented precedence.
#[must_use]
pub fn build_registry(
    cfg: &Arc<Config>,
    extra: Vec<Arc<dyn aulos_provider::Provider>>,
) -> (
    Arc<RwLock<Registry>>,
    Vec<aulos_provider::command::HookSpec>,
) {
    let mut registry = Registry::new();

    // First, so a test's provider wins a score tie against every built-in. Nothing in production
    // passes any; rebuilding the registry afterwards instead would lose each `command` plugin's
    // fingerprint and `Degraded` state, and make the next re-scan report every one as updated.
    for provider in extra {
        registry.register(provider);
    }

    match ScProvider::new(Arc::clone(cfg)) {
        Ok(sc) => registry.register(Arc::new(sc)),
        Err(e) => {
            // A degraded provider is visible in `healthz` and in `GET api/v2/providers`; an absent
            // one is not (DESIGN §6.4). The `Degraded` marker cannot be built without a provider
            // instance, so the honest fallback is a WARN plus no registration, and the URL then
            // falls through to `ytdlp` — which is also what a plain-`reqwest` build does.
            tracing::warn!(
                error = %e,
                "the StreamingCommunity provider could not be built; its URLs will fall through"
            );
        }
    }

    let loader = Arc::new(CommandPluginLoader::with_env(PluginEnv {
        state_dir: cfg.paths.state.clone(),
    }));
    if cfg.plugins_enabled {
        registry.set_command_loader(Arc::clone(&loader) as Arc<dyn aulos_provider::CommandLoader>);
        let report = registry.reload_commands(&cfg.plugins_dir);
        tracing::info!(
            dir = %cfg.plugins_dir.display(),
            added = report.added.len(),
            failed = report.failed.len(),
            "command plugins discovered"
        );
        for failure in &report.failed {
            tracing::warn!(plugin = %failure.name, "{}", failure.reason);
        }
        for warning in &report.warnings {
            tracing::warn!(target: "aulos_provider::manifest", "{warning}");
        }
    } else {
        tracing::info!("AULOS_PLUGINS_ENABLED=false; no plugin directory is scanned");
    }

    // Last, so it is the fallback the tie-break reaches (BRIEF §9).
    let (python, runner) = crate::tools::shim_paths();
    registry.register(Arc::new(YtdlpProvider::new(
        Arc::clone(cfg),
        python,
        runner,
    )));

    // The community `[[hook]]` tables. The loader caches them as `Arc<HookSpec>` for a later
    // reload, but `HookDispatcher::new` and `ManifestHook::new` both take an **owned** `HookSpec`
    // and `HookSpec` is not `Clone`, so the specs are read with a second, direct scan of the same
    // directory rather than unwrapped out of the loader's cache (which holds a reference and would
    // therefore refuse). Two walks of `/config/plugins` at boot is a rounding error, and DESIGN
    // §13.4 does not ask for live hook reload — see `docs/INTEGRATION-NOTES.md`, WP-17.
    let hooks: Vec<aulos_provider::command::HookSpec> = if cfg.hooks_enabled && cfg.plugins_enabled
    {
        let scan = aulos_provider::command::scan_with(
            &cfg.plugins_dir,
            &PluginEnv {
                state_dir: cfg.paths.state.clone(),
            },
        );
        scan.hooks
    } else {
        Vec::new()
    };
    if !hooks.is_empty() {
        tracing::info!(count = hooks.len(), "community hooks discovered");
    }
    (Arc::new(RwLock::new(registry)), hooks)
}

/// Runs the DESIGN §16.1 step 9 probes, publishes them, and returns the yt-dlp version.
///
/// # Errors
/// When the Python shim cannot run. `ytdlp` is the fallback provider for every URL, so a server
/// without it can download nothing at all — failing at boot with the reason is strictly better
/// than accepting adds that will all fail.
pub async fn probe_tools(
    cfg: &Config,
    health: &HealthRegistry,
) -> anyhow::Result<Option<Box<str>>> {
    let report = crate::tools::probe_everything(cfg, crate::tools::OPTIONAL_TOOLS).await;
    crate::tools::publish(&report, health);
    tracing::info!("external tools:\n{}", report.render());
    let missing = report.missing_optional();
    if !missing.is_empty() {
        tracing::warn!(
            missing = missing.join(", "),
            "optional tools are missing; the matching healthz components are degraded"
        );
    }
    if !report.required_ok() {
        anyhow::bail!(
            "python3 + yt-dlp are required (the ytdlp provider is the fallback for every URL): {}",
            match &report.shim {
                crate::tools::ShimProbe::Failed(why) => why.as_str(),
                crate::tools::ShimProbe::Ok(_) => "unknown",
            }
        );
    }
    Ok(report.shim.yt_dlp().map(Box::from))
}

/// Whether `path` is inside a directory that exists and is writable.
///
/// Used only by the tests, which need to know whether a temporary directory is usable before
/// asserting that [`make_dirs`] created it.
#[must_use]
pub fn is_dir(path: &Path) -> bool {
    path.is_dir()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> RawEnv {
        RawEnv::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn every_config_error_is_reported_at_once() {
        let err = load_config(&env(&[
            ("PORT", "eighty"),
            ("AULOS_WS_BATCH_MS", "nope"),
            ("AULOS_TYPO_HERE", "1"),
        ]))
        .expect_err("this environment is invalid");
        assert!(err.contains("PORT"), "{err}");
        assert!(err.contains("AULOS_WS_BATCH_MS"), "{err}");
        assert!(err.contains("AULOS_TYPO_HERE"), "{err}");
    }

    /// The BRIEF's e2e marker must not make the server exit 2.
    #[test]
    fn the_e2e_markers_are_accepted_but_a_typo_is_still_fatal() {
        let (_cfg, warnings) =
            load_config(&env(&[("AULOS_E2E", "1"), ("AULOS_E2E_ANYTHING", "yes")]))
                .expect("AULOS_E2E is an accepted key");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            load_config(&env(&[("AULOS_WS_BATCH_MSEC", "250")])).is_err(),
            "a near-miss must still be fatal"
        );
    }

    #[test]
    fn a_broken_options_file_joins_the_config_report() {
        let err = load_config(&env(&[("YTDL_OPTIONS_FILE", "/nonexistent/opts.json")]))
            .expect_err("the file does not exist");
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn the_dampening_directives_are_the_design_16_5_list() {
        for name in [
            "hyper",
            "h2",
            "rustls",
            "reqwest",
            "teloxide",
            "notify",
            "html5ever",
            "tungstenite",
        ] {
            assert!(
                THIRD_PARTY_DAMPENING.contains(&format!("{name}=warn").as_str()),
                "{name} must be dampened"
            );
        }
    }

    #[test]
    fn make_dirs_creates_all_four_roots_and_the_database_parent() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config::load(&env(&[
            (
                "DOWNLOAD_DIR",
                &root.path().join("dl").display().to_string(),
            ),
            (
                "AUDIO_DOWNLOAD_DIR",
                &root.path().join("audio").display().to_string(),
            ),
            ("TEMP_DIR", &root.path().join("tmp").display().to_string()),
            (
                "STATE_DIR",
                &root.path().join("state").display().to_string(),
            ),
            (
                "AULOS_DB_PATH",
                &root.path().join("db/aulos.db").display().to_string(),
            ),
        ]))
        .unwrap();
        make_dirs(&cfg).unwrap();
        for sub in ["dl", "audio", "tmp", "state", "db"] {
            assert!(is_dir(&root.path().join(sub)), "{sub} was not created");
        }
    }

    #[test]
    fn the_options_snapshot_adopts_an_existing_cookie_jar() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let jar = state.join(import::COOKIES_FILE);
        std::fs::write(&jar, "# Netscape HTTP Cookie File\n").unwrap();
        let cfg = config::load(&env(&[("STATE_DIR", &state.display().to_string())])).unwrap();

        let ytdl = load_ytdl_options(&cfg).unwrap();
        assert_eq!(
            ytdl.load().overrides[import::COOKIEFILE_KEY],
            serde_json::Value::String(jar.display().to_string()),
            "legacy adopted the jar only in __main__; we always do"
        );
    }

    #[test]
    fn no_cookie_jar_means_no_override() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config::load(&env(&[("STATE_DIR", &root.path().display().to_string())])).unwrap();
        let ytdl = load_ytdl_options(&cfg).unwrap();
        assert!(ytdl.load().overrides.is_empty());
    }

    #[test]
    fn the_registry_puts_ytdlp_last_so_it_is_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(
            config::load(&env(&[
                ("AULOS_PLUGINS_DIR", &dir.path().display().to_string()),
                ("STATE_DIR", &dir.path().display().to_string()),
            ]))
            .unwrap(),
        );
        let (registry, hooks) = build_registry(&cfg, Vec::new());
        let guard = registry.read().unwrap();
        let ids: Vec<String> = guard.ids().iter().map(ToString::to_string).collect();
        assert_eq!(
            ids.last().map(String::as_str),
            Some("ytdlp"),
            "the catch-all must be registered last: {ids:?}"
        );
        assert!(ids.contains(&"streamingcommunity".to_owned()), "{ids:?}");
        assert!(hooks.is_empty(), "an empty plugin dir declares no hooks");

        // The fallback really answers an arbitrary URL.
        let url = url::Url::parse("https://example.invalid/watch?v=1").unwrap();
        let picked = guard.pick(&url, None).expect("something must match");
        assert_eq!(picked.id.as_str(), "ytdlp");
    }

    #[test]
    fn disabling_plugins_installs_no_loader() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(
            config::load(&env(&[
                ("AULOS_PLUGINS_ENABLED", "false"),
                ("AULOS_PLUGINS_DIR", &dir.path().display().to_string()),
            ]))
            .unwrap(),
        );
        let (registry, hooks) = build_registry(&cfg, Vec::new());
        assert!(hooks.is_empty());
        // A rescan is a documented no-op with no loader installed.
        let report = registry.write().unwrap().reload_commands(&cfg.plugins_dir);
        assert!(report.is_empty(), "{report:?}");
    }

    #[test]
    fn the_importer_component_is_disabled_absent_degraded_or_ok() {
        assert_eq!(
            importer_component(None).status,
            ComponentStatus::Disabled,
            "no legacy state is not a failure"
        );
        let clean = ImportReport::new(PathBuf::from("/downloads/.metube"), 17);
        assert_eq!(importer_component(Some(&clean)).status, ComponentStatus::Ok);
        assert_eq!(importer_component(Some(&clean)).detail["imported_at"], 17);
    }

    #[tokio::test]
    async fn a_first_start_imports_and_a_second_does_not() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../aulos-store/tests/fixtures/state/v2");
        for entry in std::fs::read_dir(&fixture).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                std::fs::copy(entry.path(), state.join(entry.file_name())).unwrap();
            }
        }
        let cfg = config::load(&env(&[
            ("STATE_DIR", &state.display().to_string()),
            ("DOWNLOAD_DIR", &root.path().display().to_string()),
        ]))
        .unwrap();

        let health = HealthRegistry::new();
        let store = open_store(&cfg, &health).await.unwrap();
        let imported = import::stored_report(&store).await.unwrap();
        assert!(imported.is_some(), "the first start must import");
        assert!(
            state.join(import::MARKER_FILE).is_file(),
            "the marker must be written"
        );
        assert_eq!(
            health.snapshot().components[IMPORTER_COMPONENT].status,
            ComponentStatus::Ok
        );
        store.close().await.unwrap();

        // The second start finds a database and must not re-import — which is what stops it
        // resurrecting rows the operator deleted.
        std::fs::remove_file(state.join(import::MARKER_FILE)).unwrap();
        let health2 = HealthRegistry::new();
        let store2 = open_store(&cfg, &health2).await.unwrap();
        assert!(
            !state.join(import::MARKER_FILE).exists(),
            "an existing database must not trigger a second import"
        );
        assert_eq!(
            health2.snapshot().components[IMPORTER_COMPONENT].status,
            ComponentStatus::Ok,
            "the stored report is read back on a later boot"
        );
        store2.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_fatal_import_deletes_the_database_and_fails_the_boot() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../aulos-store/tests/fixtures/state/corrupt");
        for entry in std::fs::read_dir(&fixture).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                std::fs::copy(entry.path(), state.join(entry.file_name())).unwrap();
            }
        }
        let cfg = config::load(&env(&[
            ("STATE_DIR", &state.display().to_string()),
            ("DOWNLOAD_DIR", &root.path().display().to_string()),
        ]))
        .unwrap();

        let health = HealthRegistry::new();
        let err = open_store(&cfg, &health)
            .await
            .expect_err("a corrupt state dir is fatal under AULOS_IMPORT_ON_ERROR=fail");
        assert!(err.to_string().contains("import failed"), "{err}");
        assert!(
            !cfg.db_path.exists(),
            "the half-imported database must be gone so the next boot retries cleanly"
        );
    }
}
