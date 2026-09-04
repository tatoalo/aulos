//! WP-01 acceptance: the CLI surface the container depends on.

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
fn check_config_and_healthcheck_exit_zero() {
    for sub in ["check-config", "healthcheck"] {
        bin()
            .arg(sub)
            .assert()
            .success()
            .stdout(contains(format!("aulos-server {sub}: not implemented")));
    }
}

#[test]
fn import_requires_state_dir_and_db() {
    bin().arg("import").assert().failure();
    bin()
        .args([
            "import",
            "--state-dir",
            "/downloads/.metube",
            "--db",
            "/config/aulos.db",
        ])
        .assert()
        .success()
        .stdout(contains("aulos-server import: not implemented"));
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
