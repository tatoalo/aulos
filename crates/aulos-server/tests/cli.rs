//! WP-01 acceptance: the CLI surface the container depends on.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt as _;
use predicates::str::contains;

fn bin() -> Command {
    // `cargo_bin` panics only when the binary was not built, which cannot happen under
    // `cargo test`.
    #[allow(clippy::expect_used)]
    Command::cargo_bin("aulos-server").expect("the aulos-server binary is built by cargo test")
}

#[test]
fn a_bare_invocation_runs_serve() {
    // The entrypoint execs `aulos-server "$@"`; with no CMD there is no argument at all.
    assert_serve_runs_and_stops_on_sigterm(&[]);
}

#[test]
fn explicit_serve_runs_serve() {
    // `CMD ["serve"]` in the image passes it explicitly; both paths must behave identically.
    assert_serve_runs_and_stops_on_sigterm(&["serve"]);
}

/// `serve` must stay alive (the container `HEALTHCHECK` needs a live process) and exit 0 on the
/// `SIGTERM` that `tini` forwards (DESIGN §16.4).
fn assert_serve_runs_and_stops_on_sigterm(args: &[&str]) {
    use std::io::{BufRead as _, BufReader};
    use std::process::{Command as Proc, Stdio};

    let exe = assert_cmd::cargo::cargo_bin("aulos-server");
    let mut child = Proc::new(exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning aulos-server failed: {e}"));

    let mut line = String::new();
    let stdout = child.stdout.take();
    match stdout {
        Some(out) => {
            let read = BufReader::new(out).read_line(&mut line);
            assert!(read.is_ok(), "reading the announce line failed");
        }
        None => panic!("stdout was not piped"),
    }
    assert!(
        line.contains("aulos-server serve: not implemented"),
        "unexpected announce line: {line:?}"
    );

    // Still running: a stub that exited would make the image permanently unhealthy.
    assert!(
        matches!(child.try_wait(), Ok(None)),
        "serve must not exit on its own"
    );

    let sent = Proc::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    assert!(matches!(sent, Ok(s) if s.success()), "SIGTERM failed");

    let status = child.wait();
    assert!(
        matches!(&status, Ok(s) if s.success()),
        "serve must exit 0 on SIGTERM, got {status:?}"
    );
}

#[test]
fn doctor_exits_zero_with_the_not_implemented_line() {
    bin()
        .arg("doctor")
        .assert()
        .success()
        .stdout(contains("aulos-server doctor: not implemented"));
}

#[test]
fn healthcheck_exits_zero_with_the_not_implemented_line() {
    bin()
        .arg("healthcheck")
        .assert()
        .success()
        .stdout(contains("aulos-server healthcheck: not implemented"));
}

#[test]
fn import_requires_state_dir_and_db() {
    bin().arg("import").assert().failure();
    bin()
        .args(["import", "--state-dir", "/downloads/.metube"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// WP-05: `import` and `check-config`
// ---------------------------------------------------------------------------

/// A fixture directory copied out of `aulos-store`'s corpus, so the checked-in files stay
/// pristine and the marker file lands somewhere disposable.
fn state_dir(fixture: &str, into: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../aulos-store/tests/fixtures/state")
        .join(fixture);
    let to = into.join("state");
    std::fs::create_dir_all(&to).unwrap_or_else(|e| panic!("mkdir: {e}"));
    for entry in std::fs::read_dir(&from).unwrap_or_else(|e| panic!("{}: {e}", from.display())) {
        let entry = entry.unwrap_or_else(|e| panic!("{e}"));
        if entry.path().is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))
                .unwrap_or_else(|e| panic!("copy: {e}"));
        }
    }
    to
}

/// A disposable working directory. `tempfile` is not a dependency of this crate, and one
/// process-scoped directory per test is enough.
fn workdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aulos-cli-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
    dir
}

/// A command with a clean environment: the importer reads `AULOS_*` from it, and a developer
/// machine must not be able to change what these tests assert.
fn clean() -> Command {
    let mut cmd = bin();
    cmd.env_clear();
    cmd
}

#[test]
fn import_dry_run_reports_without_writing() {
    let work = workdir("dry");
    let state = state_dir("v2", &work);
    let db = work.join("aulos.db");

    clean()
        .args([
            "import",
            "--state-dir",
            &state.display().to_string(),
            "--db",
            &db.display().to_string(),
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(
            contains("DRY RUN — nothing was written")
                .and(contains("queue.json"))
                .and(contains("items: queued=3 finished=1 error=2 canceled=0")),
        );

    assert!(!db.exists(), "a dry run must not create the database");
    assert!(
        !state.join(".aulos-imported").exists(),
        "a dry run must not write the marker"
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn import_writes_the_database_then_refuses_to_run_twice() {
    let work = workdir("real");
    let state = state_dir("v2", &work);
    let db = work.join("aulos.db");
    let argv = [
        "import".to_owned(),
        "--state-dir".to_owned(),
        state.display().to_string(),
        "--db".to_owned(),
        db.display().to_string(),
    ];

    clean()
        .args(&argv)
        .assert()
        .success()
        .stdout(contains("legacy import from").and(contains("errors: 0")));
    assert!(db.is_file(), "the database must exist");
    assert!(state.join(".aulos-imported").is_file());

    // Idempotence, and the database survives the refusal.
    clean()
        .args(&argv)
        .assert()
        .failure()
        .stderr(contains("--force"));
    assert!(db.is_file());

    clean().args(&argv).arg("--force").assert().success();
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn import_of_a_corrupt_state_dir_fails_and_skip_corrupt_rescues_it() {
    let work = workdir("corrupt");
    let state = state_dir("corrupt", &work);
    let db = work.join("aulos.db");
    let argv = [
        "import".to_owned(),
        "--state-dir".to_owned(),
        state.display().to_string(),
        "--db".to_owned(),
        db.display().to_string(),
    ];

    clean()
        .args(&argv)
        .assert()
        .failure()
        .stdout(contains("file_invalid: queue.json"))
        .stderr(contains("import failed"));
    assert!(
        !db.exists(),
        "the database must not exist after a rolled-back import"
    );

    clean()
        .args(&argv)
        .arg("--skip-corrupt")
        .assert()
        .success()
        .stdout(contains("file_skipped: queue.json"));
    assert!(db.is_file());
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn check_config_exits_zero_on_a_valid_environment_and_redacts_secrets() {
    clean()
        .arg("check-config")
        .env("DOWNLOAD_DIR", "/downloads")
        .env("TELEGRAM_BOT_TOKEN", "123456:super-secret")
        .assert()
        .success()
        .stdout(
            contains("effective configuration")
                .and(contains("TELEGRAM_BOT_TOKEN = «redacted»"))
                .and(contains("configuration is valid"))
                .and(contains("super-secret").not()),
        );
}

#[test]
fn check_config_exits_one_with_a_table_on_an_invalid_environment() {
    clean()
        .arg("check-config")
        .env("PORT", "eighty")
        .env("AULOS_TYPO_HERE", "1")
        .assert()
        .code(1)
        .stdout(
            contains("configuration is INVALID")
                .and(contains("PORT"))
                .and(contains("AULOS_TYPO_HERE")),
        );
}

#[test]
fn cut_subcommands_are_not_offered() {
    // BRIEF scope trims: `print-schema` and `repair-ids` are CUT for v1.0.
    for cut in ["print-schema", "repair-ids"] {
        bin().arg(cut).assert().failure();
    }
}

#[test]
fn help_and_version_work() {
    bin().arg("--help").assert().success().stdout(
        contains("serve")
            .and(contains("check-config"))
            .and(contains("import"))
            .and(contains("doctor"))
            .and(contains("healthcheck")),
    );
    bin().arg("--version").assert().success();
}
