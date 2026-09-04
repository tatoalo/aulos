//! `assert_cmd` against `python/ytdlp_runner.py` itself: exit codes, the ordering guarantees and
//! the `--replay` mode (DESIGN §9.1 "exit codes", §9.2, §9.3).
//!
//! With fd 3 closed — which is what happens when a person runs the shim by hand, and what
//! `assert_cmd` does — the protocol falls back to the *original* stdout, duplicated away before
//! fd 1 is pointed at `/dev/null`. The isolation property still holds (plugin chatter on fd 1
//! goes nowhere), and the transcript is readable without a harness. That fallback is what makes
//! this file possible; `transport.rs` covers the real fd-3 path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::{Value, json};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python/ytdlp_runner.py")
}

fn python() -> String {
    std::env::var("AULOS_TEST_PYTHON").unwrap_or_else(|_| "python3".to_owned())
}

fn have_python() -> bool {
    StdCommand::new(python())
        .arg("-c")
        .arg("import sys")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

macro_rules! skip_without_python {
    () => {
        if !have_python() {
            eprintln!("skipping: no {} on PATH", python());
            return;
        }
    };
}

/// Runs the shim with `stdin`, the stubbed `yt_dlp` on `PYTHONPATH`, and fd 3 closed.
fn run(stdin: &str) -> std::process::Output {
    let mut cmd = Command::new(python());
    cmd.arg(shim())
        .env("PYTHONPATH", fixtures().join("pystub"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .write_stdin(stdin.to_owned());
    cmd.output().expect("the shim must be runnable")
}

/// Parses the transcript the shim printed, ignoring any non-JSON line.
///
/// Nothing *should* be on stdout but frames — the stub and its noisy plugin both print there on
/// import and both are swallowed — so an unparseable line here is itself a finding.
fn frames(out: &std::process::Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!("stdout carried a non-frame line {l:?}: {e}");
            })
        })
        .collect()
}

fn kinds(out: &std::process::Output) -> Vec<String> {
    frames(out)
        .iter()
        .map(|f| f["t"].as_str().unwrap_or("?").to_owned())
        .collect()
}

fn job(extra: &Value) -> String {
    let mut base = json!({ "v": 1, "protocol": 1, "job_id": "cli" });
    let map = base.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        map.insert(k.clone(), v.clone());
    }
    format!("{base}\n")
}

#[test]
fn a_selftest_exits_zero_with_hello_and_result() {
    skip_without_python!();
    let out = run(&job(&json!({ "mode": "selftest" })));
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(kinds(&out), ["hello", "result", "bye"]);

    let f = frames(&out);
    assert_eq!(f[0]["protocol"], 1);
    assert_eq!(f[0]["n"], 1);
    assert_eq!(f[0]["v"], 1);
    assert_eq!(f[0]["yt_dlp"], "2026.8.30.232658.dev0");
    assert_eq!(f[1]["ok"], true);
    // `bye` counts itself, and `n` is gap-free from 1.
    assert_eq!(f[2]["n"], 3);
    assert_eq!(f[2]["frames"], 3);
    // Neither the stub's import-time print nor the noisy plugin's reached fd 1.
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("NOISE"),
        "stdout must be the protocol and nothing else"
    );
}

#[test]
fn a_malformed_job_exits_two_and_still_writes_a_transcript() {
    skip_without_python!();
    for stdin in ["not json at all\n", "[1,2,3]\n", "\n", ""] {
        let out = run(stdin);
        assert_eq!(out.status.code(), Some(2), "stdin = {stdin:?}");
        let f = frames(&out);
        // The ordering guarantee holds even here: hello first, one terminator, bye last.
        assert_eq!(kinds(&out), ["hello", "error", "bye"], "stdin = {stdin:?}");
        assert_eq!(f[1]["code"], "bad_job", "stdin = {stdin:?}");
    }
}

#[test]
fn a_protocol_mismatch_exits_sixty_four() {
    skip_without_python!();
    let out = run(&job(&json!({ "mode": "selftest", "protocol": 2 })));
    assert_eq!(out.status.code(), Some(64));
    let f = frames(&out);
    assert_eq!(kinds(&out), ["hello", "error", "bye"]);
    assert_eq!(f[1]["code"], "bad_job");
    assert!(
        f[1]["message"].as_str().unwrap().contains("protocol 2"),
        "{}",
        f[1]["message"]
    );
    // `hello` still reported the protocol the shim *does* speak, so the parent can say which.
    assert_eq!(f[0]["protocol"], 1);
}

#[test]
fn an_unknown_coercion_names_the_key_and_is_a_bad_job() {
    skip_without_python!();
    let out = run(&job(&json!({
        "mode": "download",
        "url": "https://stub.test/x",
        "options": { "impersonate": "chrome" },
        "coerce": { "impersonate": "NotAThing" }
    })));
    assert_eq!(out.status.code(), Some(2));
    let f = frames(&out);
    assert_eq!(f[1]["t"], "error");
    assert_eq!(f[1]["code"], "bad_job");
    let msg = f[1]["message"].as_str().unwrap();
    assert!(msg.contains("NotAThing"), "{msg}");
    assert!(msg.contains("impersonate"), "{msg}");
}

#[test]
fn an_unknown_mode_and_a_missing_url_are_both_bad_jobs() {
    skip_without_python!();
    for extra in [
        json!({ "mode": "teleport" }),
        json!({ "mode": "download" }),
        json!({ "mode": "extract" }),
        json!({ "mode": "outtmpl" }),
        json!({ "mode": "download", "url": "https://stub.test/x", "options": [] }),
    ] {
        let out = run(&job(&extra));
        assert_eq!(out.status.code(), Some(2), "{extra}");
        let f = frames(&out);
        assert_eq!(f[1]["code"], "bad_job", "{extra}");
    }
}

#[test]
fn replay_re_emits_a_recorded_transcript_verbatim() {
    skip_without_python!();
    let transcript = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/download_ok.jsonl");
    let out = Command::new(python())
        .arg(shim())
        .arg("--replay")
        .arg(&transcript)
        .output()
        .unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        std::fs::read_to_string(&transcript).unwrap(),
        "--replay must be byte-faithful: it is how the transport is exercised offline"
    );
}

#[test]
fn replay_without_a_path_is_a_usage_error_and_replay_of_a_missing_file_is_internal() {
    skip_without_python!();
    let out = Command::new(python())
        .arg(shim())
        .arg("--replay")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    let out = Command::new(python())
        .arg(shim())
        .arg("--replay")
        .arg("/nonexistent/transcript.jsonl")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot replay"));
}

#[test]
fn the_shim_is_syntactically_valid_under_the_target_interpreter() {
    skip_without_python!();
    // `py_compile` is also a CI gate (DESIGN §18.4); having it here means a syntax error fails
    // `cargo test` too, rather than only the python job.
    Command::new(python())
        .arg("-m")
        .arg("py_compile")
        .arg(shim())
        .assert()
        .success();
}

#[test]
fn the_standalone_contract_test_passes() {
    skip_without_python!();
    // The same script CI runs as `python crates/aulos-provider-ytdlp/tests/shim_contract.py`
    // (DESIGN §18.4). Running it from `cargo test` keeps the two from drifting.
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/shim_contract.py");
    let out = Command::new(python()).arg(&script).output().unwrap();
    assert!(
        out.status.success(),
        "shim_contract.py failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
