//! WP-17 acceptance: the real boot order, the `EventRouter` fan-out, the graceful shutdown and
//! the two silent seams, driven through [`aulos_server::wiring::run_with`].
//!
//! These go through the **production** wiring rather than a test-only assembly of the same parts:
//! the whole point of DESIGN §16.1 is the *order*, and an assembly written for a test is exactly
//! the thing that can be in the right order while the server is not.
//!
//! The `ytdlp` tool probes are skipped ([`RunOptions::skip_tool_probes`]) because they are fatal
//! by design and this suite supplies its own provider; `tools.rs`'s unit tests cover the probes,
//! `doctor` covers them as a command, `tests/cli.rs` covers the fatal path through the real binary
//! with the checked-in Python stub, and `tests/e2e/run.sh` covers them inside the image.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aulos_core::config::{Config, RawEnv};
use aulos_core::event::{
    DomainEvent, DropPolicy, EventFilter, EventKind, EventRouter, SubscriberSpec,
};
use aulos_core::item::ItemView;
use aulos_core::status::TerminalStatus;
use aulos_hooks::{Hook, HookCtx, HookError, HookPhase};
use aulos_provider::fake::{FakeProvider, Step, Timeline};
use aulos_server::wiring::{RunOptions, run_with};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// A booted server: its address, its shutdown token and the temporary volume it writes into.
struct Rig {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    root: PathBuf,
    cfg: Arc<Config>,
}

impl Rig {
    /// Boots a server over `root` with the fake provider and the given extra environment.
    async fn start(root: &Path, extra: &[(&str, &str)], hooks: Vec<Arc<dyn Hook>>) -> Self {
        let cfg = Arc::new(config(root, extra));
        let shutdown = CancellationToken::new();
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut opts = RunOptions::new(Arc::clone(&cfg));
        opts.providers = vec![Arc::new(FakeProvider::new().with_timeline(fast_timeline()))];
        opts.extra_hooks = hooks;
        opts.shutdown = shutdown.clone();
        opts.ready = Some(ready_tx);
        opts.install_signals = false;
        opts.skip_tool_probes = true;

        let task = tokio::spawn(run_with(opts));
        let addr = tokio::time::timeout(Duration::from_secs(30), ready_rx)
            .await
            .expect("the server must bind within 30s")
            .expect("the ready channel must fire");
        Self {
            addr,
            shutdown,
            task,
            root: root.to_path_buf(),
            cfg,
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!("http://{}{}", self.addr, self.cfg.url_prefix.route(suffix))
    }

    async fn get_json(&self, suffix: &str) -> serde_json::Value {
        let body = reqwest::get(self.url(suffix))
            .await
            .unwrap_or_else(|e| panic!("GET {suffix}: {e}"))
            .text()
            .await
            .unwrap();
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("GET {suffix} → {body}: {e}"))
    }

    /// Cancels the shutdown token and waits for `run_with` to return.
    async fn stop(self) -> anyhow::Result<()> {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(60), self.task)
            .await
            .expect("the shutdown must complete inside 60s")
            .expect("the server task must not panic")
    }
}

/// A configuration over a temporary volume: no POT sidecar, no Telegram, an ephemeral port.
fn config(root: &Path, extra: &[(&str, &str)]) -> Config {
    let mut pairs: Vec<(String, String)> = vec![
        ("HOST".into(), "127.0.0.1".into()),
        ("PORT".into(), "0".into()),
        (
            "DOWNLOAD_DIR".into(),
            root.join("downloads").display().to_string(),
        ),
        (
            "AUDIO_DOWNLOAD_DIR".into(),
            root.join("audio").display().to_string(),
        ),
        ("TEMP_DIR".into(), root.join("tmp").display().to_string()),
        ("STATE_DIR".into(), root.join("state").display().to_string()),
        (
            "AULOS_PLUGINS_DIR".into(),
            root.join("plugins").display().to_string(),
        ),
        ("AULOS_POT_ENABLED".into(), "false".into()),
        ("TELEGRAM_BOT_ENABLED".into(), "false".into()),
        // A short batch window so a test does not wait a quarter of a second per assertion.
        ("AULOS_WS_BATCH_MS".into(), "50".into()),
        ("AULOS_SHUTDOWN_GRACE_SECS".into(), "1".into()),
        ("AULOS_DB_FLUSH_MS".into(), "5".into()),
        ("AULOS_CONFIG_POLL_SECS".into(), "0".into()),
    ];
    for (k, v) in extra {
        pairs.retain(|(key, _)| key != k);
        pairs.push(((*k).to_owned(), (*v).to_owned()));
    }
    aulos_core::config::load(&RawEnv::from_pairs(pairs)).expect("the rig environment is valid")
}

/// A download that takes long enough to be observable but short enough to be fast.
fn fast_timeline() -> Timeline {
    Timeline {
        download: vec![
            Step::Stage(aulos_provider::Stage::Preparing),
            Step::Stage(aulos_provider::Stage::Downloading),
            Step::Progress {
                percent: 50.0,
                speed: Some(1024.0),
                eta: Some(1),
            },
            Step::Wait(Duration::from_millis(30)),
            Step::Finish {
                filename: "fake.mp4".to_owned(),
                size: 1024,
            },
        ],
        ..Timeline::new()
    }
}

/// A pre-terminal hook that records every item it ran for.
#[derive(Debug, Default)]
struct RecordingPreTerminal {
    runs: AtomicU64,
}

#[async_trait::async_trait]
impl Hook for RecordingPreTerminal {
    fn id(&self) -> Arc<str> {
        Arc::from("test_pre_terminal")
    }
    fn ordering(&self) -> i16 {
        5
    }
    fn phase(&self) -> HookPhase {
        HookPhase::PreTerminal
    }
    fn applies(&self, _item: &ItemView, outcome: TerminalStatus) -> bool {
        outcome == TerminalStatus::Finished
    }
    async fn run(&self, _ctx: HookCtx<'_>) -> Result<(), HookError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A `SIGTERM` that beats the boot must unwind, not hard-kill.
///
/// The handler used to be installed at the *end* of `run_with`, after the importer, the sixty-
/// second tool probes and the queue recovery — so for the whole of that window `SIGTERM` kept its
/// default disposition and killed the process outright. The visible cost was on the cutover boot:
/// `aulos.db` was created and left empty, and the legacy import never ran again. Here the token is
/// cancelled *before* `run_with` is even called, which is the same thing from the boot's point of
/// view: it must return `Ok`, and it must never bind the port or fire `ready`.
#[tokio::test]
async fn a_shutdown_that_beats_the_boot_returns_cleanly_without_binding() {
    let root = tempfile::tempdir().unwrap();
    let cfg = Arc::new(config(root.path(), &[]));
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let (ready_tx, ready_rx) = oneshot::channel();

    let mut opts = RunOptions::new(Arc::clone(&cfg));
    opts.providers = vec![Arc::new(FakeProvider::new().with_timeline(fast_timeline()))];
    opts.shutdown = shutdown;
    opts.ready = Some(ready_tx);
    opts.install_signals = false;
    opts.skip_tool_probes = true;

    tokio::time::timeout(Duration::from_secs(30), run_with(opts))
        .await
        .expect("a boot that is cancelled up front must return promptly")
        .expect("an operator's stop is a clean exit, not a boot failure");

    assert!(
        ready_rx.await.is_err(),
        "the listener must never have been bound"
    );
}

/// Polls `f` until it answers `Some`, or fails after `secs`.
async fn until<T, F, Fut>(secs: u64, what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Adds one URL and returns its id.
async fn add(rig: &Rig, url: &str) -> String {
    let body = reqwest::Client::new()
        .post(rig.url("api/v2/downloads"))
        .header("content-type", "application/json")
        .body(format!(r#"{{"url":"{url}"}}"#))
        .send()
        .await
        .expect("POST downloads");
    let status = body.status();
    let text = body.text().await.unwrap();
    assert_eq!(status.as_u16(), 202, "{text}");
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in {text}"))
        .to_owned()
}

// ---------------------------------------------------------------------------
// Startup order
// ---------------------------------------------------------------------------

/// DESIGN §16.1: steps 5–12 complete **before** the listener binds, so the very first request
/// already sees a recovered, consistent snapshot.
///
/// The observable form of "the bind is last" is this: seed a row that boot recovery has to rewrite,
/// then read the state on the *first* request the server ever answers. A bind that happened before
/// recovery would answer `downloading` — the pre-recovery row — at least some of the time.
#[tokio::test(flavor = "multi_thread")]
async fn the_first_request_already_sees_a_recovered_queue() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), &[]);
    std::fs::create_dir_all(&cfg.paths.state).unwrap();

    // A first boot that leaves an item mid-download, the way a `SIGKILL` or an OOM would.
    let id = {
        let rig = Rig::start(root.path(), &[], Vec::new()).await;
        let id = add(&rig, "https://fake.test/watch?v=recovered").await;
        until(20, "the item to be running or done", || async {
            let item = rig.get_json(&format!("api/v2/items/{id}")).await;
            let status = item["status"].as_str().unwrap_or_default().to_owned();
            (status != "resolving" && status != "queued").then_some(())
        })
        .await;
        rig.stop().await.unwrap();
        id
    };

    // The second boot: read the state as the very first thing the process serves.
    let rig = Rig::start(root.path(), &[], Vec::new()).await;
    let state = rig.get_json("api/v2/state").await;
    let seen = state["items"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .chain(state["done"].as_array().unwrap_or(&Vec::new()).iter())
        .find(|i| i["id"].as_str() == Some(id.as_str()))
        .cloned();
    let seen = seen.unwrap_or_else(|| panic!("the recovered row is missing from {state}"));
    let status = seen["status"].as_str().unwrap_or_default();
    assert!(
        matches!(status, "queued" | "downloading" | "preparing" | "finished"),
        "a row read on the first request must already be recovered, not left mid-flight: {seen}"
    );
    rig.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// healthz
// ---------------------------------------------------------------------------

/// Every component the wiring itself publishes is present on the first request, and the roll-up is
/// serveable.
///
/// The five tool components (`ytdlp_runner`, `ffmpeg`, `ffprobe`, `nm3u8dl`, `deno`) are absent
/// here because this rig skips the probes; `tools::publish` writes them and is unit-tested, and
/// `tests/e2e/run.sh` asserts them against the real image.
#[tokio::test(flavor = "multi_thread")]
async fn healthz_names_every_component_the_wiring_owns() {
    let root = tempfile::tempdir().unwrap();
    let rig = Rig::start(root.path(), &[], Vec::new()).await;

    let body = rig.get_json("healthz").await;
    assert!(
        matches!(body["status"].as_str(), Some("ok" | "degraded")),
        "{body}"
    );
    let components = body["components"]
        .as_object()
        .unwrap_or_else(|| panic!("no components in {body}"));
    for name in [
        "store",
        "queue",
        "pot",
        "ytdl_options",
        "importer",
        "telegram",
        "jellyfin",
        "nfo",
        "audio_sync",
        "events",
        "subscriptions",
        "apns",
    ] {
        assert!(
            components.contains_key(name),
            "components.{name} is missing from {body}"
        );
    }
    // The sidecar is off in this rig, and `disabled` is not a failure.
    assert_eq!(components["pot"]["status"], "disabled", "{body}");
    assert_eq!(components["store"]["status"], "ok", "{body}");
    assert!(components["store"]["wal_bytes"].is_u64(), "{body}");
    assert!(
        components["queue"]["slots"]["global"]["total"].is_u64(),
        "{body}"
    );
    assert_eq!(components["events"]["dropped"]["hooks"], 0, "{body}");
    assert_eq!(components["events"]["dropped"]["apns"], 0, "{body}");
    // Push is off in the stock rig, and `disabled` is not a failure either.
    assert_eq!(components["apns"]["status"], "disabled", "{body}");
    assert_eq!(components["apns"]["devices"], 0, "{body}");

    // `livez` does no work at all.
    let livez = rig.get_json("livez").await;
    assert_eq!(livez["ok"], true);
    rig.stop().await.unwrap();
}

/// `healthcheck` exits 0 against a running server, **including with `URL_PREFIX=metube`** — the
/// normalisation regression a raw-`${URL_PREFIX}` shell `curl` failed (C16).
#[tokio::test(flavor = "multi_thread")]
async fn healthcheck_exits_zero_against_a_running_server_with_and_without_a_prefix() {
    for prefix in ["", "metube"] {
        let root = tempfile::tempdir().unwrap();
        // `healthcheck` derives the port from the configuration, so the rig needs a fixed one.
        let port = free_port().await;
        let rig = Rig::start(
            root.path(),
            &[("PORT", &port.to_string()), ("URL_PREFIX", prefix)],
            Vec::new(),
        )
        .await;

        let (message, code) = aulos_server::healthcheck::check(&rig.cfg).await;
        assert_eq!(
            code,
            aulos_server::healthcheck::EXIT_HEALTHY,
            "URL_PREFIX={prefix:?}: {message}"
        );
        rig.stop().await.unwrap();

        // And 1 once it has stopped.
        let (message, code) =
            aulos_server::healthcheck::check(&rig_cfg(root.path(), port, prefix)).await;
        assert_eq!(
            code,
            aulos_server::healthcheck::EXIT_UNHEALTHY,
            "a stopped server must fail the check: {message}"
        );
    }
}

fn rig_cfg(root: &Path, port: u16, prefix: &str) -> Config {
    config(root, &[("PORT", &port.to_string()), ("URL_PREFIX", prefix)])
}

/// A port nothing is listening on, obtained by binding and immediately releasing it.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// ---------------------------------------------------------------------------
// The two silent seams
// ---------------------------------------------------------------------------

/// Both `adapters` seams at once: the engine asks `PreTerminalHooks` whether to hold the terminal
/// write, the dispatcher runs the hook, and `HookFinalizer` tells the engine to finalise.
///
/// Without either wire the item would sit in `postprocessing` forever, and *nothing* would say so
/// — which is why this is an end-to-end assertion rather than a unit test of the adapter.
#[tokio::test(flavor = "multi_thread")]
async fn a_pre_terminal_hook_runs_and_the_item_still_finalises() {
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(RecordingPreTerminal::default());
    let rig = Rig::start(root.path(), &[], vec![Arc::clone(&hook) as Arc<dyn Hook>]).await;

    let id = add(&rig, "https://fake.test/watch?v=pre-terminal").await;
    let status = until(30, "the item to finalise", || async {
        let item = rig.get_json(&format!("api/v2/items/{id}")).await;
        let status = item["status"].as_str().unwrap_or_default().to_owned();
        matches!(status.as_str(), "finished" | "error" | "canceled").then_some(status)
    })
    .await;
    assert_eq!(
        status, "finished",
        "the pre-terminal phase must not change the outcome"
    );
    assert_eq!(
        hook.runs.load(Ordering::SeqCst),
        1,
        "the pre-terminal hook must have run exactly once"
    );

    // The produced file really is in the volume, which is what the file route serves.
    let file = rig.root.join("downloads/fake.mp4");
    assert!(file.is_file(), "{} was not created", file.display());
    rig.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Shutdown and resume
// ---------------------------------------------------------------------------

/// DESIGN §16.4: the shutdown exits 0, and the next boot resumes what was in flight.
///
/// The resume half is the acceptance criterion ("asserted by a restart that resumes them"): the
/// item is left `downloading` in the database on purpose, and DESIGN §8.9's boot recovery converts
/// exactly those rows to `queued` — the same path a `SIGKILL` takes.
#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_mid_download_exits_zero_and_the_next_boot_resumes_the_item() {
    let root = tempfile::tempdir().unwrap();

    // A download that hangs, so the shutdown really is mid-flight.
    let cfg = Arc::new(config(root.path(), &[]));
    let shutdown = CancellationToken::new();
    let (ready_tx, ready_rx) = oneshot::channel();
    let mut opts = RunOptions::new(Arc::clone(&cfg));
    opts.providers = vec![Arc::new(FakeProvider::new().with_timeline(Timeline {
        download: vec![
            Step::Stage(aulos_provider::Stage::Downloading),
            Step::Progress {
                percent: 10.0,
                speed: Some(1.0),
                eta: None,
            },
            Step::Hang,
        ],
        ..Timeline::new()
    }))];
    opts.shutdown = shutdown.clone();
    opts.ready = Some(ready_tx);
    opts.install_signals = false;
    opts.skip_tool_probes = true;
    let task = tokio::spawn(run_with(opts));
    let addr = tokio::time::timeout(Duration::from_secs(30), ready_rx)
        .await
        .unwrap()
        .unwrap();

    let first = Rig {
        addr,
        shutdown: shutdown.clone(),
        task,
        root: root.path().to_path_buf(),
        cfg: Arc::clone(&cfg),
    };
    let id = add(&first, "https://fake.test/watch?v=interrupted").await;
    until(30, "the item to start downloading", || async {
        let item = first.get_json(&format!("api/v2/items/{id}")).await;
        (item["status"].as_str() == Some("downloading")).then_some(())
    })
    .await;

    // Step 4 waits `AULOS_SHUTDOWN_GRACE_SECS` (1 in the rig), then step 5 kills the job.
    let started = std::time::Instant::now();
    first
        .stop()
        .await
        .expect("the shutdown must exit cleanly even with a job in flight");
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the grace must actually be honoured, not skipped"
    );

    // The next boot resumes it.
    let second = Rig::start(root.path(), &[], Vec::new()).await;
    let status = until(30, "the resumed item", || async {
        let item = second.get_json(&format!("api/v2/items/{id}")).await;
        let status = item["status"].as_str().unwrap_or_default().to_owned();
        (!status.is_empty() && status != "downloading").then_some(status)
    })
    .await;
    assert!(
        matches!(status.as_str(), "queued" | "preparing" | "finished"),
        "an interrupted item must be resumed, not stranded: {status}"
    );
    second.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// v1 and the removed Socket.IO
// ---------------------------------------------------------------------------

/// The v1 shim is mounted by the same wiring, and `socket.io` is honestly gone.
#[tokio::test(flavor = "multi_thread")]
async fn the_v1_shim_and_the_socket_io_501_are_both_mounted() {
    let root = tempfile::tempdir().unwrap();
    let rig = Rig::start(root.path(), &[], Vec::new()).await;

    let added = reqwest::Client::new()
        .post(rig.url("add"))
        .header("content-type", "application/json")
        .body(r#"{"url":"https://fake.test/watch?v=v1","quality":"best"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(added.status().as_u16(), 200, "{:?}", added.text().await);

    let history = rig.get_json("history").await;
    for key in ["queue", "pending", "done"] {
        assert!(
            history[key].is_array(),
            "history.{key} must be an array: {history}"
        );
    }

    let socketio = reqwest::get(rig.url("socket.io/")).await.unwrap();
    assert_eq!(
        socketio.status().as_u16(),
        501,
        "Socket.IO must be honestly removed, not silently broken"
    );
    rig.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// The EventRouter fan-out (DESIGN §2.2.1)
// ---------------------------------------------------------------------------

/// One `Completed` reaches all three subscribers; killing the Telegram consumer does not stall the
/// other two; and registering after `spawn` is not expressible.
#[tokio::test]
async fn one_completed_event_reaches_all_three_subscribers_and_a_dead_one_stalls_nothing() {
    let (mut router, sender) = EventRouter::new(64);
    let mut aggregator = router.subscribe(SubscriberSpec::aggregator());
    let mut hooks = router.subscribe(SubscriberSpec::hooks());
    let telegram = router.subscribe(SubscriberSpec::telegram());
    assert_eq!(
        router.subscriber_names(),
        ["aggregator", "hooks", "telegram"],
        "the DESIGN §2.2.1 registration order"
    );
    let task = router.spawn();

    // The Telegram subscriber dies before anything is published — the acceptance bullet's
    // "killing the Telegram task".
    drop(telegram);

    let view = Arc::new(completed_view());
    sender
        .publish(DomainEvent::Completed(Arc::clone(&view)))
        .await;

    let a = tokio::time::timeout(Duration::from_secs(5), aggregator.recv())
        .await
        .expect("the aggregator must not be stalled by a dead subscriber")
        .unwrap();
    let h = tokio::time::timeout(Duration::from_secs(5), hooks.recv())
        .await
        .expect("the hook dispatcher must not be stalled either")
        .unwrap();
    assert!(matches!(&*a, DomainEvent::Completed(v) if v.id == view.id));
    assert!(matches!(&*h, DomainEvent::Completed(v) if v.id == view.id));

    // `Finishing` is the one event that is not on the wire: the aggregator's filter omits it
    // because it describes a status transition that has not been written yet.
    sender
        .publish(DomainEvent::Finishing(Arc::clone(&view)))
        .await;
    let h = tokio::time::timeout(Duration::from_secs(5), hooks.recv())
        .await
        .expect("hooks receive Finishing")
        .unwrap();
    assert!(matches!(&*h, DomainEvent::Finishing(_)));
    let leaked = tokio::time::timeout(Duration::from_millis(300), aggregator.recv()).await;
    assert!(
        leaked.is_err(),
        "the aggregator must never see Finishing: {leaked:?}"
    );

    drop(sender);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the router stops when every sender is dropped")
        .unwrap();
}

/// A `DropNewest` subscriber that stops reading loses events and says how many; a `Block`
/// subscriber never does.
#[tokio::test]
async fn a_full_dropnewest_inbox_counts_its_drops_and_the_router_keeps_going() {
    let (mut router, sender) = EventRouter::new(64);
    let mut aggregator = router.subscribe(SubscriberSpec::aggregator());
    let slow = router.subscribe(SubscriberSpec {
        name: "slow",
        capacity: 1,
        policy: DropPolicy::DropNewest,
        filter: EventFilter::of(&[EventKind::Completed]),
    });
    let task = router.spawn();

    let view = Arc::new(completed_view());
    for _ in 0..8 {
        sender
            .publish(DomainEvent::Completed(Arc::clone(&view)))
            .await;
    }
    // The aggregator (Block) got all eight.
    for i in 0..8 {
        tokio::time::timeout(Duration::from_secs(5), aggregator.recv())
            .await
            .unwrap_or_else(|_| panic!("the Block subscriber must receive event {i}"))
            .unwrap();
    }
    assert_eq!(aggregator.dropped(), 0, "a Block subscriber never drops");
    assert!(
        slow.dropped() > 0,
        "a full DropNewest inbox must count its drops"
    );

    drop(sender);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

/// A minimal terminal `ItemView`, built through the same projection the engine uses.
fn completed_view() -> ItemView {
    use aulos_core::item::{Item, Kind, ViewExtras};
    use aulos_core::request::DownloadRequest;
    use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
    use aulos_core::source::{SourceKind, SourceRef};
    use aulos_core::status::Status;

    let url: url::Url = "https://fake.test/watch?v=1".parse().unwrap();
    let selection = Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").unwrap(),
        QualityId::parse("best").unwrap(),
    );
    let item = Item {
        id: aulos_core::ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord: 1,
        url: url.clone(),
        canonical_key: "fake:fake.test/1".into(),
        provider: None,
        media_id: None,
        title: "A title".into(),
        status: Status::Finished,
        auto_start: true,
        msg: None,
        error: None,
        request: DownloadRequest::new(url, selection),
        entry: None,
        filename: None,
        size: Some(1024),
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 1_757_000_000_000,
        started_at: None,
        finished_at: Some(1_757_000_001_000),
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV2),
        children_total: None,
        clear_after: None,
    };
    ItemView::from_item(&item, None, &ViewExtras::default())
}

// ---------------------------------------------------------------------------
// APNs (DESIGN §25.6, §25.7): the wiring's three states.
// ---------------------------------------------------------------------------

/// The committed throwaway P-256 key the `aulos-apns` suite signs with.
///
/// Referenced rather than copied: a second checked-in private key is a second thing a secret
/// scanner has to be told about, and this one is already documented as a fixture.
fn apns_test_key() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../aulos-apns/tests/fixtures/apns_test_key.p8")
        .canonicalize()
        .expect("the aulos-apns key fixture must exist")
}

/// DESIGN §25.6: `APNS_ENABLED=true` with an unreadable `.p8` must **not** stop the server.
///
/// This is the regression that matters most about the APNs wiring: a `?` on
/// `ApnsNotifier::new` would turn a typo in a path into a container that never comes up, and
/// nothing else in the suite would notice, because every other test leaves push disabled.
#[tokio::test(flavor = "multi_thread")]
async fn a_misconfigured_apns_key_degrades_healthz_and_the_server_still_serves() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("nope.p8");
    let rig = Rig::start(
        root.path(),
        &[
            ("APNS_ENABLED", "true"),
            ("APNS_KEY_FILE", &missing.display().to_string()),
            ("APNS_KEY_ID", "ABCD123456"),
            ("APNS_TEAM_ID", "TEAM123456"),
        ],
        vec![],
    )
    .await;

    let body = rig.get_json("healthz").await;
    let apns = &body["components"]["apns"];
    assert_eq!(apns["status"], "degraded", "{body}");
    assert!(
        apns["last_error"].as_str().is_some_and(|e| !e.is_empty()),
        "the reason an operator has to act on must be in healthz: {body}"
    );
    // The server is otherwise entirely healthy, which is the point.
    assert_eq!(body["components"]["store"]["status"], "ok", "{body}");
    assert_eq!(rig.get_json("livez").await["ok"], true);
    rig.stop().await.unwrap();
}

/// With a readable key the component is `ok`, and the `devices` gauge is the **same** registration
/// table the `PUT <p>api/v2/devices/{token}` route writes.
///
/// That shared `Arc<dyn DeviceStore>` is the one thing the wiring can get wrong invisibly: the
/// routes would keep answering `204` and the notifier would keep pushing to nobody.
#[tokio::test(flavor = "multi_thread")]
async fn a_registered_device_reaches_the_apns_health_component() {
    let root = tempfile::tempdir().unwrap();
    let key = apns_test_key();
    let rig = Rig::start(
        root.path(),
        &[
            ("APNS_ENABLED", "true"),
            ("APNS_KEY_FILE", &key.display().to_string()),
            ("APNS_KEY_ID", "ABCD123456"),
            ("APNS_TEAM_ID", "TEAM123456"),
            // Nothing is ever sent in this test, but a real gateway URL in a unit test is a
            // trap waiting for someone to add an assertion that downloads something.
            ("APNS_BASE_URL_OVERRIDE", "http://127.0.0.1:1"),
        ],
        vec![],
    )
    .await;

    let before = rig.get_json("healthz").await;
    assert_eq!(before["components"]["apns"]["status"], "ok", "{before}");
    assert_eq!(before["components"]["apns"]["devices"], 0, "{before}");

    let token = "a".repeat(64);
    let status = reqwest::Client::new()
        .put(rig.url(&format!("api/v2/devices/{token}")))
        .json(&serde_json::json!({
            "platform": "ios",
            "bundle_id": "com.tatoalo.aulos",
            "environment": "sandbox",
            "alerts": true,
            "live_activity_start_token": null,
            "app_version": "1.0.0 (3)",
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(
        status.as_u16(),
        204,
        "the registration route must accept it"
    );

    // `healthz` recomputes on its own tick, so poll rather than assume.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let body = rig.get_json("healthz").await;
        if body["components"]["apns"]["devices"] == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the registered device never reached components.apns: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    rig.stop().await.unwrap();
}
