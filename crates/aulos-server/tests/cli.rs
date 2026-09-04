//! WP-01/WP-05/WP-17 acceptance: the CLI surface the container depends on, exercised through the
//! real binary.
//!
//! The `serve` tests point `PYTHONPATH` at `aulos-provider-ytdlp`'s checked-in `pystub` fixture,
//! so the DESIGN §16.1 step 9 shim handshake — which is **fatal** when `python3` + `yt-dlp` are
//! missing, because `ytdlp` is the fallback provider for every URL — succeeds on a machine that
//! has no real yt-dlp installed. The shim itself is the production one; only the `yt_dlp` module
//! it imports is the stub, which is exactly the seam `aulos-provider-ytdlp`'s own transport tests
//! use.

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

/// The stubbed `yt_dlp` package the shim imports, so a `serve` boot needs no real yt-dlp.
fn pystub() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../aulos-provider-ytdlp/tests/fixtures/pystub")
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

/// A bare invocation really takes the `serve` path, whatever the machine has installed.
///
/// The discriminator is the exit code: invalid configuration exits **2** from `serve` (BRIEF §15)
/// and **1** from `check-config`, so the same broken environment tells the two apart without
/// depending on a listener, a port or an interpreter.
#[test]
fn a_bare_invocation_takes_the_serve_path_and_exits_two_on_bad_config() {
    clean()
        .env("PORT", "eighty")
        .assert()
        .code(2)
        .stderr(contains("configuration is invalid").and(contains("PORT")));
    clean()
        .arg("check-config")
        .env("PORT", "eighty")
        .assert()
        .code(1);
}

/// `serve` must bind, announce itself, and exit 0 on the `SIGTERM` that `tini` forwards
/// (DESIGN §16.1 step 16, §16.4).
fn assert_serve_runs_and_stops_on_sigterm(args: &[&str]) {
    use std::io::{BufRead as _, BufReader};
    use std::process::{Command as Proc, Stdio};

    let work = workdir("serve");
    let exe = assert_cmd::cargo::cargo_bin("aulos-server");
    let mut child = Proc::new(exe)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("PYTHONPATH", pystub())
        // The image sets it too. Without it two concurrent boots both compile the stub package
        // into a shared `__pycache__`, which is wasted work in a test that runs the shim once.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("HOST", "127.0.0.1")
        .env("PORT", "0")
        .env("DOWNLOAD_DIR", work.join("downloads"))
        .env("TEMP_DIR", work.join("tmp"))
        .env("STATE_DIR", work.join("state"))
        .env("AULOS_PLUGINS_DIR", work.join("plugins"))
        .env("AULOS_POT_ENABLED", "false")
        .env("TELEGRAM_BOT_ENABLED", "false")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawning aulos-server failed: {e}"));

    // stderr is drained on a thread rather than left in the pipe: the boot log is well over a
    // page, and a full pipe would block the child *before* it announced itself — the read below
    // would then hang forever instead of failing. Draining it also means the log is available to
    // put in the panic message, which is the only way a boot failure here is diagnosable.
    let errors = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let sink = std::sync::Arc::clone(&errors);
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if let Ok(mut buf) = sink.lock() {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        });
    }
    let log = || {
        errors
            .lock()
            .map(|buf| buf.clone())
            .unwrap_or_else(|_| "<stderr unavailable>".to_owned())
    };

    let mut line = String::new();
    let stdout = child.stdout.take();
    match stdout {
        Some(out) => {
            let read = BufReader::new(out).read_line(&mut line);
            assert!(read.is_ok(), "reading the announce line failed: {}", log());
        }
        None => panic!("stdout was not piped"),
    }
    assert!(
        line.starts_with("aulos-server ") && line.contains("listening on 127.0.0.1:"),
        "unexpected announce line: {line:?}; stderr was:\n{}",
        log()
    );
    assert!(
        line.contains("(v1 shim: on)"),
        "the announce line must state the shim (DESIGN §16.1 step 16): {line:?}"
    );

    // Still running: a process that exited would make the image permanently unhealthy.
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
    let _ = std::fs::remove_dir_all(&work);
}

/// Steps 5–12 really are before the bind: the announce line is printed only once the database
/// exists, so a reader of stdout knows the queue is already consistent.
#[test]
fn the_announce_line_comes_after_the_database_is_open() {
    use std::io::{BufRead as _, BufReader};
    use std::process::{Command as Proc, Stdio};

    let work = workdir("order");
    let state = work.join("state");
    let exe = assert_cmd::cargo::cargo_bin("aulos-server");
    let mut child = Proc::new(exe)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("PYTHONPATH", pystub())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("HOST", "127.0.0.1")
        .env("PORT", "0")
        .env("DOWNLOAD_DIR", work.join("downloads"))
        .env("TEMP_DIR", work.join("tmp"))
        .env("STATE_DIR", &state)
        .env("AULOS_PLUGINS_DIR", work.join("plugins"))
        .env("AULOS_POT_ENABLED", "false")
        .env("TELEGRAM_BOT_ENABLED", "false")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn: {e}"));

    let mut line = String::new();
    if let Some(out) = child.stdout.take() {
        let _ = BufReader::new(out).read_line(&mut line);
    }
    assert!(line.contains("listening on"), "{line:?}");
    assert!(
        state.join("aulos.db").is_file(),
        "the database must exist before the listener is announced"
    );

    let _ = Proc::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&work);
}

/// `doctor` prints the table and exits 0 when the required tools answer.
#[test]
fn doctor_reports_every_tool_and_exits_zero_when_the_shim_answers() {
    clean()
        .arg("doctor")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("PYTHONPATH", pystub())
        .assert()
        .success()
        .stdout(
            contains("required:")
                .and(contains("yt-dlp"))
                .and(contains("optional:"))
                .and(contains("ffmpeg"))
                .and(contains("nm3u8dl"))
                .and(contains("deno"))
                .and(contains("all required tools are present")),
        );
}

/// A required tool removed from `PATH` makes `doctor` exit non-zero, which is what the image's own
/// smoke step relies on.
#[test]
fn doctor_exits_non_zero_when_python_is_not_on_path() {
    let empty = workdir("nopath");
    clean()
        .arg("doctor")
        .env("PATH", &empty)
        .assert()
        .code(1)
        .stdout(contains("a REQUIRED tool is missing"));
    let _ = std::fs::remove_dir_all(&empty);
}

/// `healthcheck` against a stopped server exits 1 and says which URL it tried.
#[test]
fn healthcheck_exits_one_against_a_stopped_server() {
    clean()
        .arg("healthcheck")
        .env("PORT", "1")
        .assert()
        .code(1)
        .stderr(contains("http://127.0.0.1:1/healthz"));
}

/// The C16 regression, as a URL: the subcommand normalises `URL_PREFIX` where a shell
/// interpolating `${URL_PREFIX}` produced `…:8081metubehealthz`.
#[test]
fn healthcheck_normalises_a_slashless_url_prefix() {
    clean()
        .arg("healthcheck")
        .env("PORT", "1")
        .env("URL_PREFIX", "metube")
        .assert()
        .code(1)
        .stderr(contains("http://127.0.0.1:1/metube/healthz"));
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
