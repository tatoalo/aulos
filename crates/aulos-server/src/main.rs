//! The `aulos-server` executable.
//!
//! It is deliberately thin: parse the command line, load the configuration, install tracing, and
//! hand over to the library. Everything testable lives in the library (see `lib.rs`), and the
//! `docker/entrypoint.sh` contract — `exec … aulos-server "$@"`, with a bare invocation meaning
//! `serve` — lives in [`aulos_server::cli`].

use std::process::ExitCode;

use aulos_core::config::RawEnv;
use aulos_server::cli::{Cli, Cmd};
use aulos_server::{bootstrap, check_config, doctor, healthcheck, import_cmd, wiring};
use clap::Parser as _;

fn main() -> ExitCode {
    let cmd = Cli::parse().command();

    // The four side-command paths own their own configuration loading, output and exit code. None
    // of them binds a port, opens the queue or spawns a task, so none of them needs the runtime
    // `serve` builds.
    let code = match cmd {
        Cmd::CheckConfig => check_config::run(),
        Cmd::Import {
            state_dir,
            db,
            dry_run,
            force,
            skip_corrupt,
        } => import_cmd::run(&import_cmd::Args {
            state_dir,
            db,
            dry_run,
            force,
            skip_corrupt,
        }),
        Cmd::Doctor => doctor::run(),
        Cmd::Healthcheck => healthcheck::run(),
        Cmd::Serve => serve(),
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// The `serve` path: DESIGN §16.1 steps 1–3, then [`wiring::run`].
///
/// Exit codes: `0` on a clean shutdown, `2` for invalid configuration (BRIEF §15: "invalid config
/// exits non-zero with a clear error"), `1` for a boot failure that is not the configuration's
/// fault — an unwritable download directory, a corrupt database, a missing `python3`.
fn serve() -> i32 {
    let env = RawEnv::from_process();
    let (cfg, warnings) = match bootstrap::load_config(&env) {
        Ok(loaded) => loaded,
        Err(report) => {
            // Before tracing is up, so this goes straight to stderr — which is also where a
            // `docker logs` reader looks first.
            eprint!("{report}");
            return bootstrap::EXIT_CONFIG;
        }
    };

    bootstrap::init_tracing(&cfg);
    aulos_server::signals::install_panic_hook();
    for w in &warnings {
        tracing::warn!("{w}");
    }
    bootstrap::log_effective_config(&env);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("aulos-worker")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("could not start the tokio runtime: {e}");
            return 1;
        }
    };
    match runtime.block_on(wiring::run(cfg)) {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("aulos-server failed to start: {e:#}");
            eprintln!("aulos-server failed to start: {e:#}");
            1
        }
    }
}
