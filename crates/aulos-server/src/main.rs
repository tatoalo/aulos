//! The `aulos-server` binary: configuration loading, task wiring, the `bgutil-pot` sidecar
//! supervisor and signal handling (DESIGN §16).
//!
//! `import` and `check-config` are implemented (WP-05); `serve`, `doctor` and `healthcheck` are
//! still the WP-01 skeleton. `serve` binds no socket: it announces itself and then parks on the
//! shutdown signals of DESIGN §16.4, because the container's `HEALTHCHECK` needs a process that
//! stays up. The wiring lands in later work packages — the shape that matters here is that a bare
//! `aulos-server` with no argument runs the `serve` path, because `docker/entrypoint.sh` ends in
//! `exec … aulos-server "$@"`.

pub mod check_config;
pub mod cli;
pub mod import_cmd;

use std::process::ExitCode;

use clap::Parser as _;

use crate::cli::{Cli, Cmd};

fn main() -> anyhow::Result<ExitCode> {
    init_tracing();
    let cmd = Cli::parse().command();

    // The two implemented subcommands own their own output and their own exit code.
    match cmd {
        Cmd::CheckConfig => return Ok(exit(check_config::run())),
        Cmd::Import {
            state_dir,
            db,
            dry_run,
            force,
            skip_corrupt,
        } => {
            return Ok(exit(import_cmd::run(&import_cmd::Args {
                state_dir,
                db,
                dry_run,
                force,
                skip_corrupt,
            })));
        }
        _ => {}
    }

    match cmd {
        // `serve` announces itself from inside the runtime, once its signal handlers exist.
        Cmd::Serve => serve()?,
        other => {
            println!("aulos-server {}: not implemented", other.name());
            tracing::debug!(subcommand = other.name(), "nothing to do yet");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Narrows a subcommand's `i32` exit code onto the process's.
fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// The skeleton `serve` path: announce, then wait for `SIGTERM`/`SIGINT` and exit 0.
///
/// Nothing is bound and no task is spawned; what this preserves is the two properties the image
/// depends on — the process stays alive so the `HEALTHCHECK` has something to probe, and it shuts
/// down cleanly on the signal `tini` forwards.
fn serve() -> anyhow::Result<()> {
    use std::io::Write as _;
    use tokio::signal::unix::{SignalKind, signal};

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // The handlers are installed **before** the announce line, not after. A supervisor (or the
        // WP-01 CLI test) that sees the line and immediately sends `SIGTERM` would otherwise race
        // the installation and win, and the default action for an uninstalled `SIGTERM` kills the
        // process instead of shutting it down cleanly.
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;

        println!("aulos-server serve: not implemented");
        let _ = std::io::stdout().flush();
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            "serve is not implemented yet (WP-01 skeleton); binding nothing, waiting for shutdown"
        );

        let signal = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        };
        tracing::info!(signal, "shutting down");
        Ok::<(), anyhow::Error>(())
    })
}

/// Install the `tracing` subscriber.
///
/// `AULOS_LOG` wins over `RUST_LOG`; the full logging and tracing configuration of DESIGN §16.5
/// lands with the wiring.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("AULOS_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_owned());
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("info"));

    // `try_init` rather than `init`: a duplicate installation must not abort the process.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}
