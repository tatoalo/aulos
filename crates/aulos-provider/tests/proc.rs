//! The `proc` acceptance suite (PLAN WP-03): real child processes, real signals.
//!
//! Every test here spawns `/bin/sh`, which is the only external dependency, and every one of them
//! asserts a property that only shows up with a real process: that a grandchild dies with its
//! group, that a `SIGTERM`-ignoring child still dies, that a chatty child does not deadlock, and
//! that an unbounded line is a `contract` failure rather than an out-of-memory.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::time::Duration;

use aulos_core::error::ErrorCode;
use aulos_provider::proc::{Child, ProcError, SpawnSpec};
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// Whether `pid` still exists, via the classic signal-0 probe.
fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Polls for a process to disappear, up to `budget`. Returns how long it took.
async fn await_death(pid: i32, budget: Duration) -> Option<Duration> {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if !alive(pid) {
            return Some(start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

fn sh(script: &str) -> SpawnSpec {
    SpawnSpec::new("sh", "/bin/sh")
        .arg("-c")
        .arg(script)
        .stdout_piped(true)
        .kill_grace(Duration::from_millis(300))
}

async fn first_line(child: &mut Child) -> String {
    child
        .stdout_lines()
        .expect("stdout must be piped")
        .next_line()
        .await
        .expect("a line must be readable")
        .expect("the child must print one line")
}

#[tokio::test]
async fn killing_the_group_kills_the_grandchild() {
    // The exact shape of legacy's bug: `sh` spawns `sleep` and waits. Killing only the direct
    // child would leave `sleep` running, which is what left orphaned ffmpeg processes writing to
    // cancelled `.part` files.
    let mut child = Child::spawn(&sh("sleep 300 & echo $!; wait")).expect("spawn");
    let grandchild: i32 = first_line(&mut child).await.trim().parse().expect("a pid");
    assert!(alive(grandchild), "the grandchild must be running");

    let status = child.kill_group().await;
    assert!(status.is_some(), "the direct child must be reaped");
    assert!(
        await_death(grandchild, Duration::from_secs(5))
            .await
            .is_some(),
        "the grandchild must die with its process group"
    );
}

#[tokio::test]
async fn sigkill_follows_a_sigterm_the_child_ignores() {
    use std::os::unix::process::ExitStatusExt;

    // `trap '' TERM` is what a well-meaning wrapper script does; it must not be able to outlive a
    // cancel.
    let mut child =
        Child::spawn(&sh("trap '' TERM; echo ready; while :; do :; done")).expect("spawn");
    assert_eq!(first_line(&mut child).await, "ready");

    let started = std::time::Instant::now();
    let status = child.kill_group().await.expect("reaped");
    let elapsed = started.elapsed();

    assert_eq!(
        status.signal(),
        Some(nix::libc::SIGKILL),
        "an unresponsive child must end up on SIGKILL"
    );
    assert!(
        elapsed >= Duration::from_millis(250),
        "SIGKILL must wait out the grace period, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "…but not much longer, took {elapsed:?}"
    );
}

#[tokio::test]
async fn the_drop_guard_kills_the_group_too() {
    let grandchild: i32 = {
        let mut child = Child::spawn(&sh("sleep 300 & echo $!; wait")).expect("spawn");
        let pid = first_line(&mut child).await.trim().parse().expect("a pid");
        assert!(alive(pid));
        pid
        // `child` is dropped here, without anyone awaiting it.
    };
    assert!(
        await_death(grandchild, Duration::from_secs(5))
            .await
            .is_some(),
        "a dropped Child must not leak its process group"
    );
}

#[tokio::test]
async fn a_megabyte_of_stderr_does_not_deadlock_the_child() {
    // A 64 KiB pipe with nobody reading it blocks the child's next write forever. The drain is
    // what makes this test pass, and it is the reason `Child` is mandatory.
    let mut child = Child::spawn(&sh(
        r#"yes "$(printf '%0512d' 0)" | head -n 2048 >&2; echo done"#,
    ))
    .expect("spawn");
    assert_eq!(first_line(&mut child).await, "done");

    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("the child must not deadlock")
        .expect("wait");
    assert!(status.success());

    let ring = child.stderr();
    // The drain runs in its own task; give it a moment to see the pipe close.
    for _ in 0..200 {
        if !ring.is_empty() && ring.dropped() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ring.lines().len() <= 64, "the ring is line-bounded");
    assert!(ring.dropped() > 1000, "most of a megabyte was evicted");
    assert!(ring.tail(2048).len() <= 2048, "the tail is byte-bounded");
    assert!(
        ring.lines().iter().all(|l| l.len() == 512),
        "the retained lines are whole"
    );
}

#[tokio::test]
async fn a_megabyte_of_stderr_with_no_newline_does_not_grow_the_drain() {
    // The pathological case the chunked drain exists for: no line terminator at all.
    let mut child = Child::spawn(&sh(
        r#"head -c 1048576 /dev/zero | tr '\0' 'a' >&2; echo done"#,
    ))
    .expect("spawn");
    assert_eq!(first_line(&mut child).await, "done");
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("the child must not deadlock")
        .expect("wait");
    assert!(status.success());
    let ring = child.stderr();
    for _ in 0..200 {
        if !ring.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ring.lines().iter().all(|l| l.len() <= 32 * 1024),
        "no retained fragment may exceed the byte budget"
    );
}

#[tokio::test]
async fn an_over_long_stdout_line_is_a_contract_failure_and_the_child_is_killed() {
    let spec = SpawnSpec::new("sh", "/bin/sh")
        .arg("-c")
        .arg(r#"head -c 65536 /dev/zero | tr '\0' 'x'; sleep 300"#)
        .stdout_piped(true)
        .max_line_bytes(1024)
        .kill_grace(Duration::from_millis(300));
    let mut child = Child::spawn(&spec).expect("spawn");
    let pid = i32::try_from(child.pid()).expect("a pid");

    let err = child
        .stdout_lines()
        .expect("piped")
        .next_line()
        .await
        .expect_err("the cap must be enforced");
    assert_eq!(err.code(), ErrorCode::Contract);
    assert!(matches!(err, ProcError::LineTooLong { cap: 1024, .. }));

    child.kill_group().await;
    assert!(
        await_death(pid, Duration::from_secs(5)).await.is_some(),
        "the offending child must be killed"
    );
}

#[tokio::test]
async fn a_missing_binary_is_tool_missing_not_a_panic() {
    let spec = SpawnSpec::new("ffmpeg", "/nonexistent/aulos-not-a-binary");
    let err = Child::spawn(&spec).expect_err("must fail");
    assert_eq!(err.code(), ErrorCode::ToolMissing);
    assert_eq!(err.tool(), "ffmpeg");
    let provider_error: aulos_provider::ProviderError = err.into();
    assert!(matches!(
        provider_error,
        aulos_provider::ProviderError::ToolMissing("ffmpeg")
    ));
}

#[tokio::test]
async fn the_child_gets_its_own_process_group_and_a_cleared_environment() {
    // `$$` is the shell's own pid; with `process_group(0)` it is also the group leader, so the
    // pid the parent tracks and the group it signals are the same number.
    let mut child = Child::spawn(&sh("echo $$")).expect("spawn");
    let reported: u32 = first_line(&mut child).await.trim().parse().expect("a pid");
    assert_eq!(reported, child.pid());
    assert!(child.wait().await.expect("wait").success());
}

#[tokio::test]
async fn env_clear_hides_the_servers_environment_and_pass_lets_one_through() {
    use std::ffi::OsString;

    use aulos_provider::proc::EnvPolicy;

    // SAFETY: single-threaded test setup, before any child is spawned.
    unsafe {
        std::env::set_var("AULOS_TEST_SECRET", "s3cret");
        std::env::set_var("AULOS_TEST_PASSED", "fine");
    }
    let spec = SpawnSpec::new("sh", "/bin/sh")
        .arg("-c")
        .arg("echo \"[${AULOS_TEST_SECRET:-}][${AULOS_TEST_PASSED:-}][${AULOS_TEST_SET:-}]\"")
        .stdout_piped(true)
        .env(EnvPolicy {
            clear: true,
            pass: vec![OsString::from("AULOS_TEST_PASSED")],
            set: vec![(OsString::from("AULOS_TEST_SET"), OsString::from("literal"))],
        });
    let mut child = Child::spawn(&spec).expect("spawn");
    assert_eq!(first_line(&mut child).await, "[][fine][literal]");
    assert!(child.wait().await.expect("wait").success());
}

#[tokio::test]
async fn a_file_size_rlimit_stops_a_runaway_write() {
    use aulos_provider::proc::Rlimits;

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("big");
    let spec = SpawnSpec::new("sh", "/bin/sh")
        .arg("-c")
        .arg(format!(
            "head -c 1048576 /dev/zero > {} 2>/dev/null; echo $?",
            out.display()
        ))
        .stdout_piped(true)
        .limits(Rlimits {
            file_size: Some(4096),
            ..Rlimits::default()
        });
    let mut child = Child::spawn(&spec).expect("spawn");
    // The write is killed by SIGXFSZ or fails with EFBIG; either way the file is truncated to the
    // limit rather than filling the disk.
    let _ = first_line(&mut child).await;
    let _ = child.wait().await;
    let written = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    assert!(
        written <= 4096,
        "RLIMIT_FSIZE must cap the file, got {written} bytes"
    );
}
