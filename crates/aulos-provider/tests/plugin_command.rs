//! The `command` provider end to end against real child processes: the three `expect_output`
//! modes, the isolation policy, the hostile-plugin budget and watchdogs, discovery and hot
//! reload, and the circuit breaker (PLAN WP-10 acceptance list).
//!
//! Every plugin here is a `/bin/sh` script written into a temp directory, so the tests exercise
//! the real spawn path — cleared environment, own process group, rlimits, `execvp` — rather than a
//! mock of it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::FakeClock;
use aulos_core::error::ErrorCode;
use aulos_core::paths::RelPath;
use aulos_core::selection::{DownloadType, ProviderId};
use aulos_provider::command::{CommandPluginLoader, PluginEnv, scan};
use aulos_provider::registry::{ProviderState, Registry};
use aulos_provider::sink::{ProgressMsg, ProgressSinkFactory, Stage};
use aulos_provider::{Match, Provider, ProviderError};
use common::{Plugin, download_ctx, entry, paths, request, resolve_ctx};
use tokio_util::sync::CancellationToken;
use url::Url;

fn url() -> Url {
    Url::parse("https://example.test/watch/42").unwrap()
}

/// A manifest whose `[download]` runs `script` as `sh -c script sh <extra argv…>`.
fn dl_manifest(script: &str, extra: &[&str], tail: &str) -> String {
    let mut argv = vec![
        "\"/bin/sh\"".to_owned(),
        "\"-c\"".to_owned(),
        toml_str(script),
        "\"sh\"".to_owned(),
    ];
    argv.extend(extra.iter().map(|a| toml_str(a)));
    format!(
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"

[match]
hosts = ["example.test"]

[download]
command = [{}]
{tail}
"#,
        argv.join(", ")
    )
}

/// A TOML basic string. The scripts contain quotes and backslashes, so this matters.
fn toml_str(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// expect_output — all three modes, success and failure
// ---------------------------------------------------------------------------

async fn run_download(
    plugin: &Plugin,
    folder: Option<&str>,
) -> Result<aulos_provider::Outcome, ProviderError> {
    let provider = plugin.provider();
    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let mut req = request(&url, DownloadType::Video, "mp4", "best");
    if let Some(f) = folder {
        req.folder = Some(aulos_core::paths::RelDir::parse(f).unwrap());
    }
    let e = entry(&url, "A Clip");
    let out_dir = match folder {
        Some(f) => p.download.join(f),
        None => p.download.clone(),
    };
    let ctx = download_ctx(&e, &req, out_dir, p.temp.clone(), CancellationToken::new());
    let (factory, _rx) = ProgressSinkFactory::channel();
    let sink = factory.for_item(ctx.item_id);
    provider.download(ctx, sink).await
}

#[tokio::test]
async fn expect_output_path_template_wants_the_file_to_exist() {
    let ok = Plugin::new(
        "example",
        &dl_manifest(
            r#"printf 'hello' > "$1""#,
            &["{out_path}"],
            "expect_output = \"path_template\"\noutput_ext = \"mp4\"",
        ),
    );
    let outcome = run_download(&ok, None).await.expect("a produced file");
    assert_eq!(outcome.size, Some(5));
    assert_eq!(
        outcome.filename.as_ref().map(RelPath::as_str),
        Some("A Clip.mp4")
    );

    // Exit 0 but no file: a contract violation, and the message says which path was expected.
    let missing = Plugin::new(
        "example",
        &dl_manifest("true", &[], "expect_output = \"path_template\""),
    );
    let e = run_download(&missing, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Contract);
    assert!(e.message().contains("path_template"), "{}", e.message());

    // An empty file is not a download either.
    let empty = Plugin::new(
        "example",
        &dl_manifest(
            r#": > "$1""#,
            &["{out_path}"],
            "expect_output = \"path_template\"",
        ),
    );
    assert_eq!(
        run_download(&empty, None).await.unwrap_err().code(),
        ErrorCode::Contract
    );
}

#[tokio::test]
async fn expect_output_result_frame_wants_the_frame() {
    let script = r#"printf 'hello' > "$1"; printf '{"t":"result","path":"%s","size":5}\n' "$1""#;
    let ok = Plugin::new(
        "example",
        &dl_manifest(
            script,
            &["{out_path}"],
            "expect_output = \"result_frame\"\noutput_ext = \"flac\"",
        ),
    );
    let outcome = run_download(&ok, None).await.expect("a result frame");
    assert_eq!(outcome.size, Some(5));
    assert_eq!(
        outcome.filename.as_ref().map(RelPath::as_str),
        Some("A Clip.flac")
    );

    // Exit 0, file written, but no frame printed.
    let silent = Plugin::new(
        "example",
        &dl_manifest(
            r#"printf 'hello' > "$1""#,
            &["{out_path}"],
            "expect_output = \"result_frame\"",
        ),
    );
    let e = run_download(&silent, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Contract);
    assert!(e.message().contains("result_frame"), "{}", e.message());

    // A frame naming a file that is not there.
    let lying = Plugin::new(
        "example",
        &dl_manifest(
            r#"printf '{"t":"result","path":"nope.mp4","size":1}\n'"#,
            &[],
            "expect_output = \"result_frame\"",
        ),
    );
    let e = run_download(&lying, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Contract);
    assert!(e.message().contains("nope.mp4"), "{}", e.message());
}

#[tokio::test]
async fn expect_output_newest_in_dir_takes_the_newest_file() {
    let ok = Plugin::new(
        "example",
        &dl_manifest(
            r#"printf 'aaaa' > "$1/one.bin"; printf 'bbbbbb' > "$1/two.bin""#,
            &["{out_dir}"],
            "expect_output = \"newest_in_dir\"",
        ),
    );
    let outcome = run_download(&ok, None).await.expect("a produced file");
    let name = outcome.filename.as_ref().map(RelPath::as_str).unwrap_or("");
    assert!(name.ends_with(".bin"), "{name}");
    assert!(outcome.size.is_some_and(|s| s > 0));

    // Exit 0 and nothing written at all.
    let nothing = Plugin::new(
        "example",
        &dl_manifest("true", &[], "expect_output = \"newest_in_dir\""),
    );
    let e = run_download(&nothing, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Contract);
    assert!(e.message().contains("newest_in_dir"), "{}", e.message());
}

#[tokio::test]
async fn a_produced_file_is_reported_relative_to_the_download_root() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            r#"printf 'hello' > "$1""#,
            &["{out_path}"],
            "expect_output = \"path_template\"",
        ),
    );
    let outcome = run_download(&plugin, Some("Series/Season 1"))
        .await
        .unwrap();
    assert_eq!(
        outcome.filename.as_ref().map(RelPath::as_str),
        Some("Series/Season 1/A Clip.mp4")
    );
}

#[tokio::test]
async fn a_non_zero_exit_surfaces_the_stderr_tail() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            "echo 'login required: set BC_TOKEN' >&2; exit 3",
            &[],
            "expect_output = \"path_template\"",
        ),
    );
    let e = run_download(&plugin, None).await.unwrap_err();
    // DESIGN §6.5.3: a non-zero exit is `Other`, carrying the stderr tail the author needs.
    assert_eq!(e.code(), ErrorCode::Internal);
    let message = e.message();
    assert!(message.contains("exit code 3"), "{message}");
    assert!(
        message.contains("login required: set BC_TOKEN"),
        "{message}"
    );
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_reaches_the_sink_through_the_declared_source() {
    let script = concat!(
        r#"printf '\033[32m12.0%%\033[0m 1.2MiB / 10.0MiB  1.5MiB/s ETA 00:07\r' >&2; "#,
        r#"printf '47.5%% 4.75MiB / 10.0MiB  2.0MiB/s ETA 00:03\r' >&2; "#,
        r#"printf 'stage=mux\n' >&2; "#,
        r#"printf 'hello' > "$1""#
    );
    let manifest = dl_manifest(
        script,
        &["{out_path}"],
        concat!(
            "expect_output = \"path_template\"\n",
            "[progress]\n",
            "kind = \"regex\"\n",
            "source = \"stderr\"\n",
            "min_interval_ms = 0\n",
            "patterns = [\n",
            "  '(?P<percent>[\\d.]+)%\\s+(?P<downloaded>[\\d.]+\\s*[KMG]i?B)\\s*/\\s*(?P<total>[\\d.]+\\s*[KMG]i?B)',\n",
            "  '(?P<speed>[\\d.]+\\s*[KMG]i?B)/s',\n",
            "  'ETA\\s+(?P<eta>\\d{1,2}:\\d{2})',\n",
            "  'stage=(?P<status>fetch|mux|done)',\n",
            "]\n",
            "units = { downloaded = \"auto\", total = \"auto\", speed = \"auto\", eta = \"hms\" }\n",
            "status_map = { fetch = \"downloading\", mux = \"postprocessing\", done = \"finished\" }\n"
        ),
    );
    let plugin = Plugin::new("example", &manifest);
    let provider = plugin.provider();

    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let req = request(&url, DownloadType::Video, "mp4", "best");
    let e = entry(&url, "A Clip");
    let ctx = download_ctx(
        &e,
        &req,
        p.download.clone(),
        p.temp.clone(),
        CancellationToken::new(),
    );
    let item = ctx.item_id;
    let (factory, mut rx) = ProgressSinkFactory::channel();
    provider
        .download(ctx, factory.for_item(item))
        .await
        .expect("the download succeeds");

    let mut stages = Vec::new();
    let mut frames = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            ProgressMsg::Stage { stage, .. } => stages.push(stage),
            ProgressMsg::Progress { raw, .. } => frames.push(raw),
            ProgressMsg::File { .. } => {}
        }
    }
    assert_eq!(stages.first(), Some(&Stage::Preparing), "{stages:?}");
    assert!(
        stages.contains(&Stage::Postprocessing),
        "status_map must translate mux → postprocessing: {stages:?}"
    );
    let best = frames
        .iter()
        .filter_map(|f| f.downloaded_bytes)
        .fold(0.0_f64, f64::max);
    assert!(best >= 4.75 * 1024.0 * 1024.0, "{frames:?}");
    let total = frames
        .iter()
        .filter_map(|f| f.total_bytes)
        .fold(0.0, f64::max);
    assert_eq!(total, 10.0 * 1024.0 * 1024.0);
    assert!(frames.iter().any(|f| f.eta.is_some()), "{frames:?}");
}

#[tokio::test]
async fn a_json_lines_plugin_reports_progress_and_its_result() {
    let script = concat!(
        r#"printf '{"status":"downloading","downloaded":512,"total":2048,"speed":1024,"eta":3}\n'; "#,
        r#"printf '{"status":"postprocessing","msg":"merging"}\n'; "#,
        r#"printf 'hello' > "$1"; "#,
        r#"printf '{"t":"result","path":"%s","size":5}\n' "$1""#
    );
    let manifest = dl_manifest(
        script,
        &["{out_path}"],
        "expect_output = \"result_frame\"\n[progress]\nkind = \"json_lines\"\nmin_interval_ms = 0\n",
    );
    let plugin = Plugin::new("example", &manifest);
    let provider = plugin.provider();
    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let req = request(&url, DownloadType::Video, "mp4", "best");
    let e = entry(&url, "A Clip");
    let ctx = download_ctx(
        &e,
        &req,
        p.download.clone(),
        p.temp.clone(),
        CancellationToken::new(),
    );
    let item = ctx.item_id;
    let (factory, mut rx) = ProgressSinkFactory::channel();
    let outcome = provider
        .download(ctx, factory.for_item(item))
        .await
        .expect("the download succeeds");
    assert_eq!(outcome.size, Some(5));

    let mut saw_bytes = false;
    let mut saw_postprocessing = false;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            ProgressMsg::Progress { raw, .. } => {
                if raw.downloaded_bytes == Some(512.0) && raw.total_bytes == Some(2048.0) {
                    saw_bytes = true;
                }
            }
            ProgressMsg::Stage { stage, msg, .. } => {
                if stage == Stage::Postprocessing {
                    saw_postprocessing = true;
                    assert_eq!(msg.as_deref(), Some("merging"));
                }
            }
            ProgressMsg::File { .. } => {}
        }
    }
    assert!(saw_bytes, "the json_lines frame must reach the sink");
    assert!(saw_postprocessing);
}

// ---------------------------------------------------------------------------
// The hostile plugin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn infinite_stdout_is_killed_by_the_output_budget() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            "while : ; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
            &[],
            "expect_output = \"path_template\"\n[limits]\nmax_output_bytes = 4096\n",
        ),
    );
    let e = run_download(&plugin, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Contract);
    assert!(e.message().contains("output budget"), "{}", e.message());
}

#[tokio::test(start_paused = true)]
async fn a_silent_plugin_is_killed_by_the_stall_watchdog() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            "sleep 86400",
            &[],
            "expect_output = \"path_template\"\n[limits]\ndownload_stall_secs = 1\n",
        ),
    );
    let e = run_download(&plugin, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Timeout);
    assert!(e.message().contains("no output for 1s"), "{}", e.message());
}

#[tokio::test(start_paused = true)]
async fn a_long_running_plugin_is_killed_by_the_hard_timeout() {
    // A plugin that keeps printing never stalls, so only the hard timeout can stop it.
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            "while : ; do printf 'tick\\n'; sleep 1; done",
            &[],
            concat!(
                "expect_output = \"path_template\"\n",
                "[limits]\n",
                "download_stall_secs = 3600\n",
                "download_hard_timeout_secs = 2\n",
                "max_output_bytes = 1048576\n"
            ),
        ),
    );
    let e = run_download(&plugin, None).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Timeout);
    assert!(e.message().contains("hard timeout"), "{}", e.message());
}

#[tokio::test]
async fn rlimit_fsize_stops_a_plugin_writing_a_huge_file() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            r#"dd if=/dev/zero of="$1" bs=4096 count=256 2>/dev/null"#,
            &["{out_path}"],
            concat!(
                "expect_output = \"path_template\"\n",
                "[limits]\n",
                // `RLIMIT_FSIZE`: DESIGN §6.5.3 requires it; `limits.file_size_bytes` is the key.
                "file_size_bytes = 8192\n"
            ),
        ),
    );
    let e = run_download(&plugin, None).await.unwrap_err();
    // `dd` is killed by SIGXFSZ, so the plugin exits non-zero.
    assert_eq!(e.code(), ErrorCode::Internal, "{}", e.message());
    assert!(e.message().contains("the plugin failed"), "{}", e.message());
}

#[tokio::test]
async fn a_cancel_kills_the_process_group() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest("sleep 86400", &[], "expect_output = \"path_template\""),
    );
    let provider = plugin.provider();
    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let req = request(&url, DownloadType::Video, "mp4", "best");
    let e = entry(&url, "A Clip");
    let cancel = CancellationToken::new();
    let ctx = download_ctx(&e, &req, p.download.clone(), p.temp.clone(), cancel.clone());
    let item = ctx.item_id;
    let (factory, _rx) = ProgressSinkFactory::channel();
    cancel.cancel();
    let err = provider
        .download(ctx, factory.for_item(item))
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Canceled);
    let _ = item;
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_environment_is_cleared_except_for_pass_and_set() {
    // The script writes its own environment into the output file, so the test can read it back.
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            r#"env > "$1"; printf 'x' >> "$1""#,
            &["{out_path}"],
            concat!(
                "expect_output = \"path_template\"\n",
                "[env]\n",
                "pass = [\"PATH\"]\n",
                "set = { PYTHONUNBUFFERED = \"1\", TITLE = \"{title}\" }\n",
                "[headers]\n",
                "Referer = \"https://example.test/\"\n"
            ),
        ),
    );
    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let req = request(&url, DownloadType::Video, "mp4", "best");
    let e = entry(&url, "A Clip");
    let provider = plugin.provider();
    let ctx = download_ctx(
        &e,
        &req,
        p.download.clone(),
        p.temp.clone(),
        CancellationToken::new(),
    );
    let item = ctx.item_id;
    let (factory, _rx) = ProgressSinkFactory::channel();
    provider
        .download(ctx, factory.for_item(item))
        .await
        .expect("the download succeeds");

    let dumped = std::fs::read_to_string(p.download.join("A Clip.mp4")).unwrap();
    let vars: Vec<&str> = dumped.lines().collect();
    assert!(
        vars.iter().any(|l| l.starts_with("PYTHONUNBUFFERED=1")),
        "{dumped}"
    );
    assert!(vars.contains(&"TITLE=A Clip"), "{dumped}");
    // Every `[headers]` entry is exposed as `AULOS_HEADER_<NAME>` (DESIGN §6.5.1).
    assert!(
        vars.contains(&"AULOS_HEADER_REFERER=https://example.test/"),
        "{dumped}"
    );
    // `PATH` was passed; nothing else from the server's environment is there. `sh` sets `PWD`
    // and friends itself, so the assertion is about a variable only the parent had.
    assert!(vars.iter().any(|l| l.starts_with("PATH=")), "{dumped}");
    assert!(
        !vars.iter().any(|l| l.starts_with("CARGO_PKG_NAME=")),
        "the server's own environment leaked: {dumped}"
    );
}

// ---------------------------------------------------------------------------
// resolve
// ---------------------------------------------------------------------------

fn resolve_manifest(script: &str, tail: &str) -> String {
    format!(
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"

[match]
hosts = ["example.test"]

[capabilities]
resolve = true

[resolve]
command = ["/bin/sh", "-c", {}, "sh", "{{url}}"]
{tail}

[download]
command = ["/bin/sh", "-c", "true"]
"#,
        toml_str(script)
    )
}

async fn run_resolve(plugin: &Plugin) -> Result<Vec<aulos_provider::MediaEntry>, ProviderError> {
    let provider = plugin.provider();
    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let req = request(&url, DownloadType::Video, "mp4", "best");
    let ctx = resolve_ctx(&req, &p, CancellationToken::new());
    provider.resolve(&url, ctx).await
}

#[tokio::test]
async fn json_lines_resolve_builds_a_group_and_its_children() {
    let script = concat!(
        r#"printf '{"t":"group","media_id":"g1","title":"Album","expected":2}\n'; "#,
        r#"printf '{"t":"entry","media_id":"e1","url":"https://example.test/track/1","title":"One"}\n'; "#,
        r#"printf '{"url":"https://example.test/track/2","title":"Two"}\n'; "#,
        r#"printf '{"t":"note","message":"skipped one"}\n'"#
    );
    let plugin = Plugin::new(
        "example",
        &resolve_manifest(script, "format = \"json_lines\""),
    );
    let entries = run_resolve(&plugin).await.expect("entries");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_playlist());
    assert_eq!(entries[0].children().len(), 2);
    assert_eq!(&*entries[0].children()[1].title, "Two");
}

#[tokio::test]
async fn a_json_document_resolve_is_accepted() {
    let script = r#"printf '[{"title":"a","url":"https://example.test/a"},{"title":"b","url":"https://example.test/b"}]\n'"#;
    let plugin = Plugin::new("example", &resolve_manifest(script, "format = \"json\""));
    let entries = run_resolve(&plugin).await.expect("entries");
    assert_eq!(entries.len(), 2);
    assert_eq!(&*entries[0].title, "a");
}

#[tokio::test]
async fn an_error_frame_becomes_the_typed_error_it_names() {
    let script =
        r#"printf '{"t":"error","code":"geo_restricted","message":"not in your country"}\n'"#;
    let plugin = Plugin::new("example", &resolve_manifest(script, ""));
    let e = run_resolve(&plugin).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::GeoRestricted);
    assert_eq!(e.message(), "not in your country");
}

#[tokio::test]
async fn a_resolve_that_prints_nothing_is_unsupported_so_the_engine_may_fall_through() {
    let plugin = Plugin::new("example", &resolve_manifest("true", ""));
    let e = run_resolve(&plugin).await.unwrap_err();
    // DESIGN §6.4: `Unsupported` is the one variant the engine retries through the runner-up.
    assert_eq!(e.code(), ErrorCode::UnsupportedUrl);
}

#[tokio::test(start_paused = true)]
async fn a_resolve_that_never_finishes_hits_its_deadline() {
    let plugin = Plugin::new(
        "example",
        &resolve_manifest("sleep 86400", "\n[limits]\nresolve_timeout_secs = 1\n"),
    );
    let e = run_resolve(&plugin).await.unwrap_err();
    assert_eq!(e.code(), ErrorCode::Timeout);
    assert!(e.message().contains("resolve command"), "{}", e.message());
}

#[tokio::test]
async fn a_plugin_without_resolve_gets_a_synthetic_entry() {
    let plugin = Plugin::new(
        "example",
        &dl_manifest("true", &[], "expect_output = \"path_template\""),
    );
    let entries = run_resolve(&plugin).await.expect("a synthetic entry");
    assert_eq!(entries.len(), 1);
    assert_eq!(&*entries[0].title, "42");
    assert!(!entries[0].is_playlist());
}

#[tokio::test]
async fn stdin_json_hands_the_whole_request_to_the_plugin() {
    let script = r#"cat > "$1"; printf 'x' >> "$1""#;
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            script,
            &["{out_path}"],
            "expect_output = \"path_template\"\nstdin = \"json\"\n",
        ),
    );
    let base = tempfile::tempdir().unwrap();
    let p = paths(base.path());
    let url = url();
    let req = request(&url, DownloadType::Video, "mp4", "best");
    let e = entry(&url, "A Clip");
    let provider = plugin.provider();
    let ctx = download_ctx(
        &e,
        &req,
        p.download.clone(),
        p.temp.clone(),
        CancellationToken::new(),
    );
    let item = ctx.item_id;
    let (factory, _rx) = ProgressSinkFactory::channel();
    provider
        .download(ctx, factory.for_item(item))
        .await
        .expect("the download succeeds");
    let written = std::fs::read_to_string(p.download.join("A Clip.mp4")).unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(written.lines().next().unwrap()).expect("one JSON line");
    assert_eq!(payload["url"], url.to_string());
    assert_eq!(payload["title"], "A Clip");
    assert_eq!(payload["download_type"], "video");
    assert_eq!(payload["quality"], "best");
    // The stdin payload describes the same file the argv `{out_path}` named: same extension, same
    // path. A payload built from a second, extension-less context would say `""` and `A Clip`.
    assert_eq!(payload["output_ext"], "mp4");
    assert_eq!(
        payload["out_path"],
        p.download.join("A Clip.mp4").to_string_lossy().as_ref()
    );
}

/// DESIGN §6.5.4: a plugin that takes the whole request on stdin and writes where the payload's
/// `out_path` says. The server then looks for that same path, so the two must agree — including
/// the extension `expect_output = "path_template"` appends.
#[tokio::test]
async fn a_plugin_that_writes_the_stdin_out_path_satisfies_path_template() {
    // No `{out_path}` in argv at all: the only path the plugin knows is the one on stdin.
    let script = r#"p=$(sed -n 's/.*"out_path":"\([^"]*\)".*/\1/p'); printf 'hi' > "$p""#;
    let plugin = Plugin::new(
        "example",
        &dl_manifest(
            script,
            &[],
            "expect_output = \"path_template\"\nstdin = \"json\"\noutput_ext = \"flac\"\n",
        ),
    );
    let outcome = run_download(&plugin, None).await.expect("a produced file");
    assert_eq!(
        outcome.filename.as_ref().map(RelPath::as_str),
        Some("A Clip.flac")
    );
    assert_eq!(outcome.size, Some(2));
}

// ---------------------------------------------------------------------------
// matches()
// ---------------------------------------------------------------------------

#[test]
fn the_score_table_of_design_6_3_is_what_matches_returns() {
    let plugin = Plugin::new(
        "example",
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"

[match]
hosts              = ["example.test"]
host_regex         = '^([a-z0-9-]+\.)?example\.test$'
path_regex         = '^/(watch|titles)/'
exclude_path_regex = '^/embed/'
schemes            = ["https"]

[download]
command = ["/bin/sh", "-c", "true"]
"#,
    );
    let provider = plugin.provider();
    let m = |s: &str| provider.matches(&Url::parse(s).unwrap());

    // host + path ⇒ 250.
    assert_eq!(m("https://example.test/watch/42"), Match::Strong(250));
    // host only ⇒ 150 (the regex hit), since `hosts` alone would be 100.
    assert_eq!(m("https://example.test/other"), Match::Strong(150));
    assert_eq!(m("https://sub.example.test/watch/1"), Match::Strong(250));
    // the veto wins over everything.
    assert_eq!(m("https://example.test/embed/1"), Match::No);
    // a scheme outside the list, and an unrelated host.
    assert_eq!(m("http://example.test/watch/42"), Match::No);
    assert_eq!(m("https://elsewhere.test/watch/42"), Match::No);
    // a host that merely *contains* the suffix is not a suffix match.
    assert_eq!(m("https://notexample.test/watch/1"), Match::No);

    // `hosts` on its own scores 100.
    let suffix_only = Plugin::new(
        "example",
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"
[match]
hosts = ["example.test"]
[download]
command = ["/bin/sh", "-c", "true"]
"#,
    );
    let p = suffix_only.provider();
    assert_eq!(
        p.matches(&Url::parse("https://example.test/x").unwrap()),
        Match::Strong(100)
    );
    assert_eq!(
        p.matches(&Url::parse("http://a.example.test/x").unwrap()),
        Match::Strong(100),
        "http is in the default scheme list"
    );

    // `priority` overrides the derived score, which is how a plugin outranks StreamingCommunity.
    let forced = Plugin::new(
        "example",
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"
[match]
hosts    = ["example.test"]
priority = 201
[download]
command = ["/bin/sh", "-c", "true"]
"#,
    );
    assert_eq!(
        forced
            .provider()
            .matches(&Url::parse("https://example.test/x").unwrap()),
        Match::Strong(201)
    );
}

#[tokio::test]
async fn own_slots_follows_uses_global_slot() {
    let global = Plugin::new(
        "example",
        &dl_manifest(
            "true",
            &[],
            "expect_output = \"path_template\"\n[limits]\nmax_concurrent = 4\n",
        ),
    );
    assert_eq!(global.provider().own_slots(), None);

    let own = Plugin::new(
        "example",
        &dl_manifest(
            "true",
            &[],
            concat!(
                "expect_output = \"path_template\"\n",
                "[limits]\n",
                "max_concurrent = 4\n",
                "uses_global_slot = false\n"
            ),
        ),
    );
    assert_eq!(own.provider().own_slots(), Some(4));
}

// ---------------------------------------------------------------------------
// Discovery and hot reload
// ---------------------------------------------------------------------------

const MIN: &str = r#"
manifest_version = 1
name    = "Min"
version = "1.0.0"
[match]
hosts = ["min.test"]
[download]
command = ["/bin/sh", "-c", "true"]
"#;

#[test]
fn a_scan_finds_every_manifest_and_reports_what_it_could_not_load() {
    let root = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("alpha", MIN.to_owned()),
        ("beta", MIN.replace("min.test", "beta.test")),
        // A hook-only manifest contributes a hook and no provider.
        (
            "hooks",
            "manifest_version = 1\nname = \"H\"\nversion = \"1\"\n[[hook]]\nid = \"ping\"\non = [\"finished\"]\nhttp = { url = \"https://h/x\" }\n".to_owned(),
        ),
        // A broken one is visible, not silent.
        ("broken", "manifest_version = 1\nname = \"B\"\nversion = \"1\"\n[match]\nhosts = [\"broken.test\"]\n".to_owned()),
        // A directory with no manifest at all is simply not a plugin.
        ("notaplugin", String::new()),
    ] {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if !body.is_empty() {
            std::fs::write(dir.join("plugin.toml"), body).unwrap();
        }
    }

    let scan = scan(root.path());
    let ids: Vec<String> = scan
        .plugins
        .iter()
        .map(|p| p.provider.id().as_str().to_owned())
        .collect();
    assert_eq!(
        ids,
        ["command:alpha", "command:beta", "command:broken"],
        "sorted, and the broken one is registered degraded"
    );
    assert_eq!(scan.hooks.len(), 1);
    assert_eq!(&*scan.hooks[0].id, "hook:hooks/ping");
    assert_eq!(scan.failed.len(), 1);
    assert_eq!(&*scan.failed[0].name, "broken");
    assert!(scan.failed[0].reason.contains("[download] section"));

    // The degraded stand-in still claims the URLs its `[match]` names (DESIGN §6.4).
    let broken = scan
        .plugins
        .iter()
        .find(|p| p.provider.id().as_str() == "command:broken")
        .unwrap();
    assert!(broken.degraded.is_some());
    assert_eq!(
        broken
            .provider
            .matches(&Url::parse("https://broken.test/x").unwrap()),
        Match::Strong(100)
    );
    assert_eq!(
        broken
            .provider
            .matches(&Url::parse("https://elsewhere.test/x").unwrap()),
        Match::No
    );
}

#[test]
fn reload_commands_adds_updates_and_removes() {
    let root = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), body).unwrap();
    };
    write("alpha", MIN);
    write("beta", &MIN.replace("min.test", "beta.test"));

    let loader = Arc::new(CommandPluginLoader::with_env(PluginEnv::default()));
    let mut registry = Registry::new();
    registry.set_command_loader(loader.clone());

    let report = registry.reload_commands(root.path());
    assert_eq!(report.added.len(), 2, "{report:?}");
    assert!(report.updated.is_empty());
    assert!(report.failed.is_empty());
    assert_eq!(registry.len(), 2);

    // An unchanged directory is neither added nor updated.
    let report = registry.reload_commands(root.path());
    assert!(report.is_empty(), "{report:?}");

    // A changed manifest is an update, and the new version is what the registry holds.
    write("alpha", &MIN.replace("1.0.0", "2.0.0"));
    let report = registry.reload_commands(root.path());
    assert_eq!(report.updated.len(), 1, "{report:?}");
    assert!(report.added.is_empty());

    // A removed directory is a removal.
    std::fs::remove_dir_all(root.path().join("beta")).unwrap();
    let report = registry.reload_commands(root.path());
    assert_eq!(report.removed.len(), 1, "{report:?}");
    assert_eq!(registry.len(), 1);

    // A newly broken manifest becomes a `failed` entry *and* a degraded provider.
    write("alpha", "manifest_version = 9\n");
    let report = registry.reload_commands(root.path());
    assert_eq!(report.failed.len(), 1, "{report:?}");
    let id = ProviderId::parse("command:alpha").unwrap();
    match registry.state_of(&id) {
        Some(ProviderState::Degraded { reason, .. }) => {
            assert!(reason.contains("unsupported manifest_version"), "{reason}");
        }
        other => panic!("expected a degraded provider, got {other:?}"),
    }

    // The loader also remembers the hooks it saw, so a reload refreshes `aulos-hooks`.
    write(
        "hooks",
        "manifest_version = 1\nname = \"H\"\nversion = \"1\"\n[[hook]]\nid = \"ping\"\non = [\"finished\"]\nhttp = { url = \"https://h/x\" }\n",
    );
    registry.reload_commands(root.path());
    assert_eq!(loader.hooks().len(), 1);
    assert_eq!(&*loader.hooks()[0].id, "hook:hooks/ping");
}

#[test]
fn a_degraded_plugin_still_wins_its_urls_and_never_falls_through() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("broken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nname = \"B\"\nversion = \"1\"\n[match]\nhosts = [\"broken.test\"]\n",
    )
    .unwrap();

    let mut registry = Registry::new();
    registry.set_command_loader(Arc::new(CommandPluginLoader::new()));
    registry.reload_commands(root.path());

    let selected = registry
        .pick(&Url::parse("https://broken.test/x").unwrap(), None)
        .expect("the degraded plugin still matches");
    assert_eq!(selected.id.as_str(), "command:broken");
    assert!(!selected.state.is_ready());
    assert!(
        selected
            .state
            .reason()
            .is_some_and(|r| r.contains("download"))
    );
}

#[test]
fn five_failures_in_ten_minutes_degrade_a_healthy_plugin() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("alpha");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), MIN).unwrap();

    let clock = Arc::new(FakeClock::default());
    let mut registry = Registry::with_clock(clock.clone());
    registry.set_command_loader(Arc::new(CommandPluginLoader::new()));
    registry.reload_commands(root.path());
    let id = ProviderId::parse("command:alpha").unwrap();
    assert!(registry.state_of(&id).is_some_and(ProviderState::is_ready));

    for i in 0..4 {
        assert!(
            !registry.record_failure(&id, "boom"),
            "failure {i} must not trip the breaker yet"
        );
        clock.advance(Duration::from_secs(10));
    }
    assert!(registry.record_failure(&id, "boom"), "the fifth trips it");

    // Degraded, but still selectable — an item routed here fails `provider_degraded` rather than
    // silently falling through to `ytdlp` (DESIGN §6.4).
    let selected = registry
        .pick(&Url::parse("https://min.test/x").unwrap(), None)
        .expect("a degraded provider still matches");
    assert_eq!(selected.id, id);
    assert!(!selected.state.is_ready());
    assert_eq!(selected.state.reason(), Some("boom"));
}
