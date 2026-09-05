//! Community `[[hook]]`s end to end: the Plex GET, the Emby POST, the ntfy body, a command hook's
//! argv, debounced batching and the retry budget (DESIGN §13.4, BRIEF §13).
//!
//! Every manifest here goes through WP-10's real loader, so a change to the schema breaks these
//! tests rather than silently changing what a plugin author's file means.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::FakeClock;
use aulos_core::error::{ErrorCode, WireError};
use aulos_core::selection::DownloadType;
use aulos_core::status::{Status, TerminalStatus};
use aulos_hooks::hook::{BatchEntry, Hook};
use aulos_hooks::{HookDispatcher, HookRunner, HookStore, ManifestHook};
use aulos_provider::command::{HookSpec, load_manifest_with_env};
use common::{FakeStore, ItemBuilder, config, events, script, sink, until};
use wiremock::matchers::{body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Writes a `plugin.toml` into a fresh plugins directory and loads its `[[hook]]` tables.
fn hooks_from(toml: &str) -> (tempfile::TempDir, Vec<HookSpec>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin = dir.path().join("media");
    std::fs::create_dir_all(&plugin).expect("mkdir");
    std::fs::write(plugin.join("plugin.toml"), toml).expect("write the manifest");
    let manifest = load_manifest_with_env(&plugin, &|name| match name {
        "PLEX_TOKEN" => Some("plex-token".to_owned()),
        "EMBY_TOKEN" => Some("emby-token".to_owned()),
        _ => None,
    })
    .expect("the manifest must load");
    (dir, manifest.hooks)
}

/// One [`ManifestHook`] from a one-hook manifest.
fn one_hook(toml: &str) -> (tempfile::TempDir, ManifestHook) {
    let (dir, mut hooks) = hooks_from(toml);
    assert_eq!(hooks.len(), 1, "this helper is for a single-hook manifest");
    let plugins_dir = dir.path().to_path_buf();
    let hook = ManifestHook::new(hooks.remove(0), &plugins_dir);
    (dir, hook)
}

/// Runs one hook once over a finished item.
async fn run(
    hook: &ManifestHook,
    item: common::ItemBuilder,
    outcome: TerminalStatus,
) -> (Result<(), aulos_hooks::HookError>, Arc<FakeStore>) {
    let cfg = config(&[]);
    let view = item.view();
    let store = FakeStore::new();
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        cfg,
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let batch = vec![BatchEntry::from_view(&view, outcome)];
    let result = runner.run(hook, &view, &batch).await;
    (result, store)
}

const HEADER: &str = "manifest_version = 1\nname = \"t\"\nversion = \"1.0.0\"\n";

/// BRIEF §13 names the Plex GET explicitly; this is that request.
#[tokio::test]
async fn the_plex_get_carries_its_token_in_the_query_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections/3/refresh"))
        .and(query_param("X-Plex-Token", "plex-token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"plex\"\non = [\"finished\"]\n\
         http = {{ method = \"GET\", url = \"{}/library/sections/3/refresh?X-Plex-Token=${{PLEX_TOKEN}}\" }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    assert_eq!(&*hook.id(), "hook:media/plex");
    assert_eq!(hook.ordering(), 50, "the DESIGN §13.4 default");
    let (result, store) = run(
        &hook,
        ItemBuilder::finished("Clip"),
        TerminalStatus::Finished,
    )
    .await;
    result.expect("a 200 is a success");
    assert!(
        store.calls().is_empty(),
        "a community hook never touches item state"
    );
    server.verify().await;
}

#[tokio::test]
async fn the_emby_post_sends_its_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .and(header("x-emby-token", "emby-token"))
        .and(header("accept", "application/json"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"emby\"\non = [\"finished\"]\n\
         http = {{ method = \"POST\", url = \"{}/Library/Refresh\", \
         headers = {{ \"X-Emby-Token\" = \"${{EMBY_TOKEN}}\", Accept = \"application/json\" }} }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    let (result, _store) = run(
        &hook,
        ItemBuilder::finished("Clip"),
        TerminalStatus::Finished,
    )
    .await;
    result.expect("a 204 is a success");
    server.verify().await;
}

#[tokio::test]
async fn the_ntfy_post_renders_its_body_and_header_placeholders() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/my-aulos-topic"))
        .and(header("title", "Aulos: finished"))
        .and(body_string("Clip\nShow/Clip.mp4"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"ntfy\"\non = [\"finished\", \"error\"]\n\
         http = {{ method = \"POST\", url = \"{}/my-aulos-topic\", \
         headers = {{ Title = \"Aulos: {{status}}\" }}, \
         body = \"{{title}}\\n{{filename}}{{error_message}}\" }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    assert!(
        !hook.body_is_json(),
        "a plain-text body is inserted raw, not JSON-escaped"
    );
    let (result, _store) = run(
        &hook,
        ItemBuilder::finished("Clip").filename("Show/Clip.mp4"),
        TerminalStatus::Finished,
    )
    .await;
    result.expect("a 200 is a success");
    server.verify().await;
}

/// A failed download renders `{status}`, `{error_code}` and `{error_message}` instead.
#[tokio::test]
async fn an_error_outcome_renders_the_error_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string(
            "{\"status\":\"error\",\"code\":\"unavailable\",\"message\":\"Video unavailable\"}",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"webhook\"\non = [\"error\"]\n\
         http = {{ url = \"{}/hook\", \
         body = \"{{\\\"status\\\":\\\"{{status}}\\\",\\\"code\\\":\\\"{{error_code}}\\\",\\\"message\\\":\\\"{{error_message}}\\\"}}\" }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    assert!(
        hook.body_is_json(),
        "a JSON body selects JSON escaping for its values"
    );
    let item = ItemBuilder::finished("Clip").error(aulos_core::error::WireError::new(
        aulos_core::error::ErrorCode::Unavailable,
        "Video unavailable",
    ));
    let (result, _store) = run(&hook, item, TerminalStatus::Error).await;
    result.expect("a 200 is a success");
    server.verify().await;
}

/// A value containing a quote must not be able to break the JSON body it is inserted into.
#[tokio::test]
async fn a_json_body_escapes_its_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string("{\"t\":\"L'ultimo \\\"caso\\\"\"}"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"json\"\non = [\"finished\"]\n\
         http = {{ url = \"{}/hook\", body = \"{{\\\"t\\\":\\\"{{title}}\\\"}}\" }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    let (result, _store) = run(
        &hook,
        ItemBuilder::finished("L'ultimo \"caso\""),
        TerminalStatus::Finished,
    )
    .await;
    result.expect("a 200 is a success");
    server.verify().await;
}

/// A command hook substitutes at argv level and never through a shell (DESIGN §6.5.3).
#[tokio::test]
async fn a_command_hook_receives_its_argv_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin = dir.path().join("media");
    std::fs::create_dir_all(&plugin).expect("mkdir");
    let out = dir.path().join("argv.txt");
    let sh = script(
        &plugin,
        "organise.sh",
        &format!("for a in \"$@\"; do echo \"$a\"; done > {}", out.display()),
    );
    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"post-process\"\non = [\"finished\"]\n\
         when = {{ download_type = [\"video\"], folder_prefix = [\"Series/\"] }}\n\
         command = [\"{}\", \"{{filename}}\", \"{{folder}}\", \"{{title}}\"]\ntimeout_ms = 60000\n",
        sh.display()
    );
    std::fs::write(plugin.join("plugin.toml"), &toml).expect("write");
    let manifest = load_manifest_with_env(&plugin, &|_| None).expect("load");
    let mut hooks = manifest.hooks;
    let hook = ManifestHook::new(hooks.remove(0), dir.path());

    let item = ItemBuilder::finished("Titolo; rm -rf /")
        .filename("Series/Show/S01E01.mp4")
        .folder("Series/Show");
    let view = item.view();
    assert!(
        hook.applies(&view, TerminalStatus::Finished),
        "the when filters accept this item"
    );

    let store = FakeStore::new();
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        config(&[]),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    runner
        .run(
            &hook,
            &view,
            &[BatchEntry::from_view(&view, TerminalStatus::Finished)],
        )
        .await
        .expect("the script exits 0");

    let argv = std::fs::read_to_string(&out).expect("the script wrote its argv");
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        ["Series/Show/S01E01.mp4", "Series/Show", "Titolo; rm -rf /"],
        "one argv element per template element, shell metacharacters and all"
    );
}

/// A non-zero exit is a hook failure and nothing else.
#[tokio::test]
async fn a_failing_command_is_reported_and_the_item_is_unaffected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin = dir.path().join("media");
    std::fs::create_dir_all(&plugin).expect("mkdir");
    let sh = script(&plugin, "broken.sh", "echo 'no thanks' >&2; exit 9");
    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"broken\"\non = [\"finished\"]\nretries = 0\n\
         command = [\"{}\"]\n",
        sh.display()
    );
    std::fs::write(plugin.join("plugin.toml"), &toml).expect("write");
    let manifest = load_manifest_with_env(&plugin, &|_| None).expect("load");
    let mut hooks = manifest.hooks;
    let hook = ManifestHook::new(hooks.remove(0), dir.path());

    let (result, store) = run(
        &hook,
        ItemBuilder::finished("Clip"),
        TerminalStatus::Finished,
    )
    .await;
    let e = result.expect_err("exit 9 fails");
    assert!(e.to_string().contains("no thanks"), "{e}");
    assert!(store.calls().is_empty(), "the item is untouched");
}

/// PLAN WP-11: "A non-2xx retries then gives up, and the item is unaffected."
#[tokio::test]
async fn a_non_2xx_is_retried_and_then_given_up_on() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .expect(2)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"flaky\"\non = [\"finished\"]\nretries = 1\n\
         http = {{ url = \"{}/hook\" }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    assert_eq!(hook.spec().retries, 1);
    // The backoff is DESIGN §13.4's 2 s, which this test pays once.
    let (result, store) = run(
        &hook,
        ItemBuilder::finished("Clip"),
        TerminalStatus::Finished,
    )
    .await;
    let e = result.expect_err("two 502s give up");
    assert!(e.to_string().contains("502"), "{e}");
    assert!(store.calls().is_empty());
    server.verify().await;
}

/// A 4xx is final: retrying a rejected request just annoys the target.
#[tokio::test]
async fn a_4xx_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"forbidden\"\non = [\"finished\"]\nretries = 2\n\
         http = {{ url = \"{}/hook\" }}\n",
        server.uri()
    );
    let (_dir, hook) = one_hook(&toml);
    let (result, _store) = run(
        &hook,
        ItemBuilder::finished("Clip"),
        TerminalStatus::Finished,
    )
    .await;
    result.expect_err("403 fails");
    server.verify().await;
}

/// PLAN WP-11: "Debounced batching produces one call with the right `{count}` and
/// `{titles_json}`."
#[tokio::test]
async fn debounced_batching_renders_count_and_titles_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batch"))
        .and(body_string(
            "{\"count\":3,\"titles\":[\"Clip 0\",\"Clip 1\",\"Clip 2\"],\"files\":[\"Clip.mp4\",\"Clip.mp4\",\"Clip.mp4\"]}",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"batch\"\non = [\"finished\"]\n\
         debounce_ms = 150\nmax_wait_ms = 3000\n\
         http = {{ url = \"{}/batch\", \
         body = \"{{\\\"count\\\":{{count}},\\\"titles\\\":{{titles_json}},\\\"files\\\":{{filenames_json}}}}\" }}\n",
        server.uri()
    );
    let (dir, mut hooks) = hooks_from(&toml);
    let hook: Arc<dyn Hook> = Arc::new(ManifestHook::new(hooks.remove(0), dir.path()));

    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook)],
        Arc::new(FakeClock::default()),
    );
    let health = dispatcher.health_handle();
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());
    for i in 0..3 {
        ev.completed(&ItemBuilder::finished(&format!("Clip {i}")).view())
            .await;
    }
    assert!(
        until(|| health
            .health()
            .stat("hook:media/batch")
            .is_some_and(|s| s.runs_total == 1))
        .await,
        "{:?}",
        health.health()
    );
    drop(ev.tx);
    task.await.expect("clean stop");

    let requests: Vec<Request> = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "three completions, one call");
    server.verify().await;
}

/// A debounced batch's single-item tokens and its batch tokens describe the **same** event: the
/// first one of the window. Regression test for a representative that was overwritten by every
/// later arrival, which rendered `{title}` from the last event while `{status}` and
/// `{error_message}` still came from the first — a notification that named a failed download and
/// reported the successful one's status (DESIGN §13.4).
#[tokio::test]
async fn a_debounced_batch_renders_its_first_event_not_its_last() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/batch"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"batch\"\non = [\"finished\", \"error\"]\n\
         debounce_ms = 150\nmax_wait_ms = 3000\n\
         http = {{ url = \"{}/batch\", \
         body = \"title={{title}} titles={{titles_json}} status={{status}} message={{error_message}}\" }}\n",
        server.uri()
    );
    let (dir, mut hooks) = hooks_from(&toml);
    let hook: Arc<dyn Hook> = Arc::new(ManifestHook::new(hooks.remove(0), dir.path()));

    let dispatcher = HookDispatcher::with_hooks(
        config(&[]),
        vec![Arc::clone(&hook)],
        Arc::new(FakeClock::default()),
    );
    let health = dispatcher.health_handle();
    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, FakeStore::new());

    // Alpha finishes first; Beta fails inside the same window.
    ev.completed(&ItemBuilder::finished("Alpha").view()).await;
    ev.completed(
        &ItemBuilder::finished("Beta")
            .status(Status::Error)
            .error(WireError::new(ErrorCode::Unavailable, "Video unavailable"))
            .view(),
    )
    .await;

    assert!(
        until(|| health
            .health()
            .stat("hook:media/batch")
            .is_some_and(|s| s.runs_total == 1))
        .await,
        "{:?}",
        health.health()
    );
    drop(ev.tx);
    task.await.expect("clean stop");

    let requests: Vec<Request> = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "two events, one coalesced call");
    let body = String::from_utf8(requests[0].body.clone()).expect("utf-8 body");
    assert_eq!(
        body, "title=Alpha titles=[\"Alpha\",\"Beta\"] status=finished message=",
        "the single-item tokens and titles_json[0] must be the same event"
    );
    server.verify().await;
}

/// The `when.*` allow-lists (DESIGN §13.4), including "an item with no provider fails a
/// `when.provider` filter rather than passing it".
#[test]
fn the_when_filters_are_the_documented_three_axes() {
    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"filtered\"\non = [\"finished\"]\n\
         when = {{ provider = [\"streamingcommunity\"], download_type = [\"video\"], folder_prefix = [\"Series/\"] }}\n\
         http = {{ url = \"http://nowhere.invalid/hook\" }}\n"
    );
    let (_dir, hook) = one_hook(&toml);

    let ok = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .folder("Series/Show")
        .view();
    assert!(hook.applies(&ok, TerminalStatus::Finished));
    assert!(
        !hook.applies(&ok, TerminalStatus::Error),
        "`on` still gates it"
    );

    let wrong_provider = ItemBuilder::finished("Clip").folder("Series/Show").view();
    assert!(!hook.applies(&wrong_provider, TerminalStatus::Finished));

    let wrong_folder = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .folder("Films")
        .view();
    assert!(!hook.applies(&wrong_folder, TerminalStatus::Finished));

    let wrong_type = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .folder("Series/Show")
        .selection(DownloadType::Audio, "mp3", "best")
        .view();
    assert!(!hook.applies(&wrong_type, TerminalStatus::Finished));
}

/// An unfiltered hook runs for every item, which is the normal shape.
#[test]
fn an_unfiltered_hook_applies_to_everything_it_fires_on() {
    let toml = format!(
        "{HEADER}\n[[hook]]\nid = \"all\"\non = [\"finished\", \"error\", \"canceled\"]\n\
         http = {{ url = \"http://nowhere.invalid/hook\" }}\n"
    );
    let (_dir, hook) = one_hook(&toml);
    let view = ItemBuilder::finished("Clip").view();
    for outcome in TerminalStatus::ALL {
        assert!(hook.applies(&view, outcome), "{outcome:?}");
    }
}

/// BRIEF §13's community hook surface, as shipped: `plugins/examples/media-server-hooks` must load
/// into the four hooks the design documents, with the debounce values it documents.
#[test]
fn the_shipped_example_manifest_loads_into_four_hooks() {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/examples/media-server-hooks");
    let manifest = load_manifest_with_env(&dir, &|name| match name {
        "PLEX_TOKEN" => Some("plex-token".to_owned()),
        "EMBY_TOKEN" => Some("emby-token".to_owned()),
        _ => None,
    })
    .expect("the shipped example must load");
    let hooks: Vec<ManifestHook> = manifest
        .hooks
        .into_iter()
        .map(|h| ManifestHook::new(h, Path::new("/config/plugins")))
        .collect();
    let ids: Vec<String> = hooks.iter().map(|h| h.id().to_string()).collect();
    assert_eq!(
        ids,
        [
            "hook:media-server-hooks/plex",
            "hook:media-server-hooks/emby",
            "hook:media-server-hooks/ntfy",
            "hook:media-server-hooks/post-process"
        ]
    );
    let plex = &hooks[0];
    assert_eq!(plex.debounce().window, Duration::from_secs(30));
    assert_eq!(plex.debounce().max_wait, Duration::from_secs(300));
    assert!(
        plex.spec().action.summary().contains("plex-token"),
        "${{PLEX_TOKEN}} is interpolated at load time: {}",
        plex.spec().action.summary()
    );
    let emby = &hooks[1];
    assert_eq!(
        emby.debounce().max_wait,
        Duration::from_secs(300),
        "max_wait defaults to 10 × debounce_ms"
    );
    let ntfy = &hooks[2];
    assert!(!ntfy.debounce().is_armed(), "one push per outcome");
    let post = &hooks[3];
    assert_eq!(post.spec().timeout_ms, 60_000);
    assert!(!post.spec().when.is_unfiltered());
}
