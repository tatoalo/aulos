//! The `aulos-server` binary: configuration loading, task wiring, the `bgutil-pot` sidecar
//! supervisor and signal handling (DESIGN §16).
//!
//! This is the WP-01 skeleton. Every subcommand except `serve` prints `not implemented` and exits
//! 0. `serve` binds no socket: it announces itself and then parks on the shutdown signals of
//! DESIGN §16.4, because the container's `HEALTHCHECK` needs a process that stays up. The wiring
//! lands in later work packages — the shape that matters here is that a bare `aulos-server` with
//! no argument runs the `serve` path, because `docker/entrypoint.sh` ends in
//! `exec … aulos-server "$@"`.

pub mod cli;

use clap::Parser as _;

use crate::cli::{Cli, Cmd};

fn main() -> anyhow::Result<()> {
    init_tracing();
    let cmd = Cli::parse().command();

    println!("aulos-server {}: not implemented", cmd.name());

    match cmd {
        Cmd::Serve => serve()?,
        other => tracing::debug!(subcommand = other.name(), "nothing to do yet"),
    }
    Ok(())
}

/// The skeleton `serve` path: announce, then wait for `SIGTERM`/`SIGINT` and exit 0.
///
/// Nothing is bound and no task is spawned; what this preserves is the two properties the image
/// depends on — the process stays alive so the `HEALTHCHECK` has something to probe, and it shuts
/// down cleanly on the signal `tini` forwards.
fn serve() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            "serve is not implemented yet (WP-01 skeleton); binding nothing, waiting for shutdown"
        );
        let signal = shutdown_signal().await?;
        tracing::info!(signal, "shutting down");
        Ok::<(), anyhow::Error>(())
    })
}

/// Resolve when the process is asked to stop, returning the name of the signal that did it.
async fn shutdown_signal() -> anyhow::Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
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
