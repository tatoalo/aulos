//! The command-line surface of the binary (DESIGN §3.1).
//!
//! `print-schema` and `repair-ids` are **not implemented in v1.0, see BRIEF** ("Scope trims for
//! v1.0"): the allocator boot check logs a `WARN` and continues rather than refusing to start, so
//! the documented recovery command has nothing to recover.

use std::path::PathBuf;

/// The parsed command line.
///
/// `cmd` is deliberately optional. `docker/entrypoint.sh` ends in
/// `exec … aulos-server "$@"`, so a bare invocation with no argument must run the server; a
/// required subcommand enum would make the container fail to start (DESIGN §18.1).
#[derive(Debug, clap::Parser)]
#[command(
    name = "aulos-server",
    version,
    about = "Aulos download server",
    long_about = None
)]
pub struct Cli {
    /// Absent => [`Cmd::Serve`].
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

/// One subcommand of `aulos-server` (DESIGN §3.1, trimmed by the BRIEF for v1.0).
#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum Cmd {
    /// Normal run. Also the default when no subcommand is given.
    Serve,
    /// Parse the environment and `YTDL_OPTIONS*`, print the effective config with secrets
    /// redacted, exit 0 on success and 1 on invalid config. Binds no port.
    CheckConfig,
    /// Run the legacy JSON importer standalone against an existing `STATE_DIR`.
    Import {
        /// Directory holding the legacy `queue.json` / `pending.json` / `completed.json` /
        /// `subscriptions.json` / `telegram_bot_config.json` files.
        #[arg(long, value_name = "DIR")]
        state_dir: PathBuf,
        /// Destination SQLite database.
        #[arg(long, value_name = "PATH")]
        db: PathBuf,
        /// Run the whole import against an in-memory database and print the report, writing
        /// nothing.
        #[arg(long)]
        dry_run: bool,
        /// Import even when the destination database already holds items.
        #[arg(long)]
        force: bool,
        /// Skip unparseable records instead of aborting the import.
        #[arg(long)]
        skip_corrupt: bool,
    },
    /// Probe `ffmpeg`, `ffprobe`, `N_m3u8DL-RE`, `deno`, `python3`, `yt-dlp` and `bgutil-pot`,
    /// print their versions, and exit non-zero when a *required* tool is missing.
    Doctor,
    /// The container `HEALTHCHECK` (DESIGN §18.1). Loads the config exactly as `serve` does, so
    /// `URL_PREFIX` normalisation applies, then requests `<prefix>healthz` on the loopback.
    Healthcheck,
}

impl Cli {
    /// The subcommand to run, defaulting to [`Cmd::Serve`].
    #[must_use]
    pub fn command(self) -> Cmd {
        self.cmd.unwrap_or(Cmd::Serve)
    }
}

impl Cmd {
    /// The stable name of this subcommand, used in log lines and in the skeleton's
    /// `not implemented` output.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::CheckConfig => "check-config",
            Self::Import { .. } => "import",
            Self::Doctor => "doctor",
            Self::Healthcheck => "healthcheck",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn bare_invocation_defaults_to_serve() {
        let cli = Cli::parse_from(["aulos-server"]);
        assert_eq!(cli.command(), Cmd::Serve);
    }

    #[test]
    fn explicit_serve_parses() {
        let cli = Cli::parse_from(["aulos-server", "serve"]);
        assert_eq!(cli.command(), Cmd::Serve);
    }

    #[test]
    fn every_trimmed_subcommand_parses() {
        for (argv, want) in [
            (vec!["aulos-server", "check-config"], "check-config"),
            (vec!["aulos-server", "doctor"], "doctor"),
            (vec!["aulos-server", "healthcheck"], "healthcheck"),
        ] {
            assert_eq!(Cli::parse_from(argv).command().name(), want);
        }
    }

    #[test]
    fn import_takes_the_design_flags() {
        let cli = Cli::parse_from([
            "aulos-server",
            "import",
            "--state-dir",
            "/downloads/.metube",
            "--db",
            "/config/aulos.db",
            "--dry-run",
            "--force",
            "--skip-corrupt",
        ]);
        match cli.command() {
            Cmd::Import {
                state_dir,
                db,
                dry_run,
                force,
                skip_corrupt,
            } => {
                assert_eq!(state_dir, PathBuf::from("/downloads/.metube"));
                assert_eq!(db, PathBuf::from("/config/aulos.db"));
                assert!(dry_run && force && skip_corrupt);
            }
            other => panic!("expected import, got {other:?}"),
        }
    }

    #[test]
    fn cut_subcommands_are_rejected() {
        // BRIEF scope trims: `print-schema` and `repair-ids` are CUT for v1.0.
        for cut in ["print-schema", "repair-ids"] {
            assert!(Cli::try_parse_from(["aulos-server", cut]).is_err());
        }
    }

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // Note the disambiguation: our `Cli::command(self) -> Cmd` (the PLAN signature) shadows
        // `clap::CommandFactory::command`.
        <Cli as clap::CommandFactory>::command().debug_assert();
    }
}
