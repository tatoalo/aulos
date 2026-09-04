//! `aulos-server import` (DESIGN §3.1, §7.6, §19.2).
//!
//! The standalone entry point to the importer: the mandatory rehearsal step of the cutover runbook
//! is `import --dry-run` against the **live** `STATE_DIR`, so this command has to be safe to point
//! at production state. Two things make it so:
//!
//! - the importer itself never writes into `STATE_DIR` except the `.aulos-imported` marker, and
//!   never at all under `--dry-run` (T2);
//! - `--dry-run` gets a **throwaway database** in the OS temporary directory, which is deleted
//!   before the command returns, so the `--db` path is not even created.
//!
//! DESIGN §7.6.6 says the dry run goes against `:memory:`. It cannot: the store's pragma set
//! includes `journal_mode = WAL`, which an in-memory database refuses, and the read pool opens its
//! own connections — two `:memory:` connections are two *different* databases. A temporary file is
//! the same rehearsal with the same guarantees, and it exercises the real `STRICT` and `UNIQUE`
//! checks the in-memory form would too.

use std::path::{Path, PathBuf};

use aulos_core::config::{self, Config, RawEnv};
use aulos_store::import::{self, ImportOpts, ImportReport, OnError};
use aulos_store::{Store, StoreOptions};

/// The exit code for a completed import.
pub const EXIT_OK: i32 = 0;

/// The exit code for a rolled-back import, and for invalid configuration.
pub const EXIT_FAILED: i32 = 1;

/// What the CLI parsed.
#[derive(Clone, Debug)]
pub struct Args {
    /// `--state-dir`.
    pub state_dir: PathBuf,
    /// `--db`.
    pub db: PathBuf,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--force`.
    pub force: bool,
    /// `--skip-corrupt`, which sets `AULOS_IMPORT_ON_ERROR=skip` for this run.
    pub skip_corrupt: bool,
}

/// Runs the import and returns the process exit code.
///
/// The report is printed on both paths — that is the whole point of the command — and the failure
/// path also deletes the destination database, per DESIGN §7.6.6.
pub fn run(args: &Args) -> i32 {
    let env = RawEnv::from_process();
    let cfg = match config::load(&env) {
        Ok(cfg) => cfg,
        Err(errs) => {
            eprintln!("configuration is invalid ({} errors):", errs.len());
            for e in &errs {
                eprintln!("  {e}");
            }
            return EXIT_FAILED;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start a runtime: {e}");
            return EXIT_FAILED;
        }
    };
    runtime.block_on(execute(args, &cfg))
}

/// The async body, so the tests can call it inside their own runtime.
pub async fn execute(args: &Args, cfg: &Config) -> i32 {
    let opts = ImportOpts {
        dry_run: args.dry_run,
        force: args.force,
        on_error: if args.skip_corrupt {
            OnError::Skip
        } else {
            OnError::from(cfg.import_on_error)
        },
        clear_completed_after_s: cfg.clear_completed_after,
        max_seen_ids: cfg.subscription_max_seen_ids,
    };

    // A dry run must not create the real database, so it gets a scratch one it also removes.
    let scratch = args.dry_run.then(scratch_dir);
    let db_path = match &scratch {
        Some(Ok(dir)) => dir.join("dry-run.db"),
        Some(Err(e)) => {
            eprintln!("could not create a scratch directory for the dry run: {e}");
            return EXIT_FAILED;
        }
        None => args.db.clone(),
    };

    let mut store_opts = StoreOptions::from_config(cfg);
    store_opts.path = db_path.clone();
    let store = match Store::open(store_opts) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("could not open {}: {e}", db_path.display());
            cleanup(&scratch, &db_path);
            return EXIT_FAILED;
        }
    };

    let outcome = import::import(&args.state_dir, &store, opts).await;
    let _ = store.close().await;

    let code = match outcome {
        Ok(report) => {
            print!("{}", render(&report, args.dry_run));
            EXIT_OK
        }
        Err(fatal) => {
            print!("{}", render(&fatal.report, args.dry_run));
            eprintln!("import failed: {fatal}");
            if fatal.should_delete_db() && !args.dry_run {
                match import::delete_db_files(&args.db) {
                    Ok(()) => eprintln!("removed {}", args.db.display()),
                    Err(e) => eprintln!("could not remove {}: {e}", args.db.display()),
                }
            }
            EXIT_FAILED
        }
    };

    cleanup(&scratch, &db_path);
    code
}

/// The printed report: the deterministic table, then the timestamp line a diff can ignore.
fn render(report: &ImportReport, dry_run: bool) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("DRY RUN — nothing was written\n");
    }
    out.push_str(&report.render_table());
    out.push_str(&format!("  imported_at: {}\n", report.imported_at));
    out
}

/// A fresh directory under the OS temporary directory.
fn scratch_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("aulos-import-dry-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Removes the dry run's scratch database.
fn cleanup(scratch: &Option<std::io::Result<PathBuf>>, db_path: &Path) {
    if let Some(Ok(dir)) = scratch {
        let _ = import::delete_db_files(db_path);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        #[allow(clippy::expect_used)] // an empty environment is the documented default set
        config::load(&RawEnv::default()).expect("the defaults must load")
    }

    fn args(state_dir: &Path, db: &Path) -> Args {
        Args {
            state_dir: state_dir.to_path_buf(),
            db: db.to_path_buf(),
            dry_run: false,
            force: false,
            skip_corrupt: false,
        }
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../aulos-store/tests/fixtures/state")
            .join(name)
    }

    /// The fixtures are read-only inputs; copying keeps the checked-in corpus pristine and lets
    /// the marker file land somewhere harmless.
    fn copy_fixture(name: &str, to: &Path) {
        std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("mkdir {}: {e}", to.display()));
        for entry in std::fs::read_dir(fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}")) {
            let entry = entry.unwrap_or_else(|e| panic!("{name}: {e}"));
            if entry.path().is_file() {
                let _ = std::fs::copy(entry.path(), to.join(entry.file_name()));
            }
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aulos-import-cmd-{name}-{}", ulid()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        dir
    }

    fn ulid() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    #[tokio::test]
    async fn a_dry_run_creates_no_database_and_no_marker() {
        let root = tmp("dry");
        let state = root.join("state");
        copy_fixture("v2", &state);
        let db = root.join("aulos.db");
        let mut a = args(&state, &db);
        a.dry_run = true;

        assert_eq!(execute(&a, &cfg()).await, EXIT_OK);
        assert!(!db.exists(), "the --db path must not be created");
        assert!(
            !state.join(aulos_store::import::MARKER_FILE).exists(),
            "a dry run writes no marker"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_real_run_writes_the_database_and_the_marker() {
        let root = tmp("real");
        let state = root.join("state");
        copy_fixture("v2", &state);
        let db = root.join("aulos.db");

        assert_eq!(execute(&args(&state, &db), &cfg()).await, EXIT_OK);
        assert!(db.is_file());
        assert!(state.join(aulos_store::import::MARKER_FILE).is_file());

        // Idempotence: the second run is refused, and the database survives.
        assert_eq!(execute(&args(&state, &db), &cfg()).await, EXIT_FAILED);
        assert!(db.is_file(), "refusing must not delete the database");

        // … unless forced.
        let mut forced = args(&state, &db);
        forced.force = true;
        assert_eq!(execute(&forced, &cfg()).await, EXIT_OK);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_corrupt_file_fails_and_removes_the_database() {
        let root = tmp("corrupt");
        let state = root.join("state");
        copy_fixture("corrupt", &state);
        let db = root.join("aulos.db");

        assert_eq!(execute(&args(&state, &db), &cfg()).await, EXIT_FAILED);
        assert!(!db.exists(), "the DB file must not exist afterwards");

        // `--skip-corrupt` commits everything else.
        let mut skip = args(&state, &db);
        skip.skip_corrupt = true;
        assert_eq!(execute(&skip, &cfg()).await, EXIT_OK);
        assert!(db.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_rendered_report_marks_a_dry_run_and_keeps_the_table_stable() {
        let report = ImportReport::new(PathBuf::from("/downloads/.metube"), 17);
        let wet = render(&report, false);
        let dry = render(&report, true);
        assert!(dry.starts_with("DRY RUN"), "{dry}");
        assert!(wet.contains("  imported_at: 17"), "{wet}");
        assert_eq!(
            dry.replace("DRY RUN — nothing was written\n", ""),
            wet,
            "the table itself must be identical"
        );
    }
}
