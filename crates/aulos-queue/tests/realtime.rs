//! The realtime side end to end: the aggregator's cadence, the published snapshot, the replay ring
//! and `?since=` across a restart (DESIGN §15.1–15.3, §15.5).
//!
//! Everything here is local and deterministic: a temporary SQLite file for the durable `seq`
//! allocator, `tokio::time::pause()` for the cadence, and no network anywhere.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::config::{RawEnv, load};
use aulos_core::{
    AddReason, Clock, Codec, Config, DomainEvent, DownloadRequest, DownloadType, EventRouter,
    EventSender, FakeClock, FormatId, Item, ItemId, ItemView, Kind, QualityId, RemoveReason,
    Selection, Seq, SourceKind, SourceRef, Status, SubscriberSpec, ViewExtras, YtdlOptions,
};
use aulos_provider::{ProgressMsg, Registry};
use aulos_queue::{
    Aggregator, DeltaBatch, DeltaItem, Engine, EventHub, FrameBody, FrameKind, Resume, Ring,
    RingEntry, StateView, WireFrame,
};
use aulos_store::{Store, StoreOptions};
use proptest::prelude::*;
use serde_json::Value;
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn selection() -> Selection {
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").unwrap(),
        QualityId::parse("best").unwrap(),
    )
}

fn row(status: Status, ord: i64) -> Item {
    let url = url::Url::parse("https://fake.test/watch?v=1").unwrap();
    Item {
        id: ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord,
        url: url.clone(),
        canonical_key: "ytdlp:fake.test/1".into(),
        provider: None,
        media_id: None,
        title: "A title".into(),
        status,
        auto_start: true,
        msg: None,
        error: None,
        request: DownloadRequest::new(url, selection()),
        entry: None,
        filename: None,
        size: None,
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 1_757_000_000_000,
        started_at: None,
        finished_at: None,
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV2),
        children_total: None,
        clear_after: None,
    }
}

fn view(status: Status, ord: i64) -> Arc<ItemView> {
    Arc::new(ItemView::from_item(
        &row(status, ord),
        None,
        &ViewExtras::default(),
    ))
}

/// A running aggregator over a real durable `seq` allocator.
///
/// The engine is real but idle unless the rig was seeded: `Rig::new` builds it and keeps it
/// unspawned, which is enough to hold the `EngineCmd` channel open for the aggregator's forwards,
/// while `Rig::seeded` recovers the given rows and spawns it so a `Stage` forward can be observed
/// all the way to SQLite and back out as a frame.
struct Rig {
    _dir: tempfile::TempDir,
    store: Store,
    hub: EventHub,
    state: StateView,
    events: EventSender,
    progress: mpsc::Sender<ProgressMsg>,
    engine: aulos_queue::EngineHandle,
    rx: Receiver<Arc<WireFrame>>,
    _idle_engine: Option<Engine>,
    _router: JoinHandle<()>,
    _engine_task: Option<JoinHandle<()>>,
    _agg: JoinHandle<()>,
}

fn config_in(dir: &std::path::Path, overrides: &[(&str, &str)]) -> Config {
    for sub in ["downloads", "audio", "temp", "state"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let path = |sub: &str| dir.join(sub).to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = vec![
        ("STATE_DIR".into(), path("state")),
        ("DOWNLOAD_DIR".into(), path("downloads")),
        ("AUDIO_DOWNLOAD_DIR".into(), path("audio")),
        ("TEMP_DIR".into(), path("temp")),
        (
            "AULOS_DB_PATH".into(),
            dir.join("state/aulos.db").to_string_lossy().into_owned(),
        ),
        ("AULOS_DB_FLUSH_MS".into(), "5".into()),
    ];
    env.extend(
        overrides
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    load(&RawEnv::from_pairs(env)).expect("the config must load")
}

impl Rig {
    async fn new(overrides: &[(&str, &str)]) -> Self {
        Self::build(overrides, Vec::new()).await
    }

    async fn seeded(overrides: &[(&str, &str)], seed: Vec<Item>) -> Self {
        Self::build(overrides, seed).await
    }

    async fn build(overrides: &[(&str, &str)], seed: Vec<Item>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(config_in(dir.path(), overrides));
        let store = Store::open(
            StoreOptions::from_config(&cfg)
                .with_flush_ms(5)
                .with_readers(1),
        )
        .unwrap();
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(1_757_000_000_000));

        let (mut router, events) = EventRouter::new(4_096);
        let inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();

        let (progress, progress_rx) = mpsc::channel::<ProgressMsg>(1_024);
        let (mut engine, handle) = Engine::new(
            store.clone(),
            Arc::new(RwLock::new(Registry::new())),
            Arc::clone(&cfg),
            Arc::new(ArcSwap::from_pointee(YtdlOptions::empty())),
            Arc::clone(&clock),
            events.clone(),
            progress.clone(),
        );

        let hub = EventHub::new(store.seq_allocator(), aulos_core::BootId::new(), &cfg);
        let rx = hub.subscribe();
        let (agg, state) = Aggregator::new(hub.clone(), Arc::clone(&cfg), Arc::clone(&clock));
        let agg_task = agg.spawn(progress_rx, inbox, handle.clone());

        let (idle_engine, engine_task) = if seed.is_empty() {
            (Some(engine), None)
        } else {
            store
                .write(
                    vec![aulos_store::WriteOp::InsertItems { items: seed }],
                    aulos_store::Durability::Sync,
                )
                .await
                .unwrap();
            engine.recover().await.unwrap();
            (None, Some(engine.spawn()))
        };

        // Let `run` initialise its tick phase before the test's clock reads start.
        tokio::task::yield_now().await;

        Self {
            _dir: dir,
            store,
            hub,
            state,
            events,
            progress,
            engine: handle,
            rx,
            _idle_engine: idle_engine,
            _router: router_task,
            _engine_task: engine_task,
            _agg: agg_task,
        }
    }

    async fn added(&self, views: &[Arc<ItemView>]) {
        self.events
            .publish(DomainEvent::Added(views.to_vec(), AddReason::Created))
            .await;
    }

    async fn changed(&self, view: &Arc<ItemView>) {
        self.events
            .publish(DomainEvent::StatusChanged {
                id: view.id,
                from: view.status,
                to: view.status,
                view: Arc::clone(view),
            })
            .await;
    }

    async fn progress(&self, id: ItemId, downloaded: f64) {
        self.progress
            .send(ProgressMsg::Progress {
                id,
                raw: aulos_core::RawProgress {
                    downloaded_bytes: Some(downloaded),
                    total_bytes: Some(1_000.0),
                    ..aulos_core::RawProgress::default()
                },
            })
            .await
            .unwrap();
    }

    /// The next frame, as `(kind, elapsed since `start`, parsed body)`.
    async fn next_frame(&mut self, start: Instant) -> (FrameKind, Duration, Value) {
        let frame = tokio::time::timeout(Duration::from_secs(30), self.rx.recv())
            .await
            .expect("a frame must arrive")
            .expect("the hub must stay open");
        (
            frame.kind,
            start.elapsed(),
            serde_json::from_str(frame.as_str()).unwrap(),
        )
    }

    fn try_frame(&mut self) -> Option<FrameKind> {
        self.rx.try_recv().ok().map(|f| f.kind)
    }
}

// ---------------------------------------------------------------------------
// cadence
// ---------------------------------------------------------------------------

/// The two halves of DESIGN §15.1's promptness rule in one deterministic run: a state change is
/// flushed inside `AULOS_WS_URGENT_MS`, a numeric change waits for `AULOS_WS_BATCH_MS`, and the
/// urgent flush does **not** move the periodic tick — the numeric delta lands on the tick's
/// original phase (200 ms), not 25 ms later.
#[tokio::test(start_paused = true)]
async fn a_state_change_is_prompt_a_numeric_change_is_batched_and_the_tick_never_moves() {
    let mut rig = Rig::new(&[("AULOS_WS_BATCH_MS", "200"), ("AULOS_WS_URGENT_MS", "25")]).await;
    let start = Instant::now();
    let v = view(Status::Downloading, 1);
    rig.added(std::slice::from_ref(&v)).await;

    let (kind, at, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Added);
    assert!(
        at < Duration::from_millis(50),
        "an `added` is urgent, not batched: {at:?}"
    );
    assert_eq!(body["items"][0]["id"], v.id.to_string());

    rig.progress(v.id, 500.0).await;
    let (kind, at, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Delta);
    assert!(
        (Duration::from_millis(200)..Duration::from_millis(225)).contains(&at),
        "the numeric delta must land on the tick's original phase, not 25 ms after the urgent \
         flush: {at:?}"
    );
    assert_eq!(body["items"][0]["percent"], 50.0);
    assert!(
        body["items"][0].get("msg").is_none(),
        "absent means unchanged"
    );
}

/// A frame that changes only `msg` — the shim's `phase` frame (DESIGN §9.3) and the SC engine's
/// `"Starting N_m3u8DL-RE download..."` transition (§10.5) — is prompt, and a numeric frame in the
/// same window rides along without having pulled the tick forward itself.
#[tokio::test(start_paused = true)]
async fn a_msg_only_change_is_flushed_within_the_urgent_deadline() {
    let mut rig = Rig::new(&[("AULOS_WS_BATCH_MS", "2000"), ("AULOS_WS_URGENT_MS", "25")]).await;
    let start = Instant::now();
    let v = view(Status::Downloading, 1);
    rig.added(std::slice::from_ref(&v)).await;
    let (kind, _, _) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Added);

    // A numeric change alone: nothing may be emitted for a long time.
    rig.progress(v.id, 100.0).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        rig.try_frame().is_none(),
        "a numeric-only change waits for the 2 s tick"
    );

    let sent_at = Instant::now();
    let mut with_msg = (*v).clone();
    with_msg.msg = Some(Arc::from("Starting N_m3u8DL-RE download..."));
    rig.changed(&Arc::new(with_msg)).await;
    let (kind, _, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Delta);
    assert!(
        sent_at.elapsed() < Duration::from_millis(60),
        "text is urgent: {:?}",
        sent_at.elapsed()
    );
    assert_eq!(body["items"][0]["msg"], "Starting N_m3u8DL-RE download...");
    assert_eq!(
        body["items"][0]["percent"], 10.0,
        "the batched number rides along with the urgent flush"
    );
}

#[tokio::test(start_paused = true)]
async fn an_idle_server_emits_nothing_for_ten_ticks() {
    let mut rig = Rig::new(&[("AULOS_WS_BATCH_MS", "50")]).await;
    tokio::time::sleep(Duration::from_millis(50 * 10 + 25)).await;
    assert!(rig.try_frame().is_none(), "silence means nothing changed");
    assert_eq!(rig.hub.frames_published(), 0);
    assert!(rig.state.load().is_empty());
}

/// The whole point of the published snapshot: a connecting client is served from one atomic load,
/// and the aggregator structurally cannot read the database — it holds no `Store`.
#[tokio::test(start_paused = true)]
async fn five_hundred_items_reach_the_published_snapshot_in_ord_order() {
    let rig = Rig::new(&[("AULOS_WS_BATCH_MS", "50")]).await;
    let views: Vec<Arc<ItemView>> = (0..500)
        .map(|i| view(Status::Queued, i64::from(500 - i)))
        .collect();
    rig.added(&views).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let published = rig.state.load();
    assert_eq!(published.items.len(), 500);
    assert_eq!(published.counts.queued, 500);
    assert!(published.seq <= rig.hub.head());
    let ords: Vec<i64> = published.items.iter().map(|v| v.ord).collect();
    let mut sorted = ords.clone();
    sorted.sort_unstable();
    assert_eq!(ords, sorted);
    assert_eq!(published.items[0].ord, 1);
}

#[tokio::test(start_paused = true)]
async fn a_terminal_transition_produces_completed_then_removal_produces_removed() {
    let mut rig = Rig::new(&[("AULOS_WS_BATCH_MS", "50")]).await;
    let start = Instant::now();
    let v = view(Status::Downloading, 1);
    rig.added(std::slice::from_ref(&v)).await;
    assert_eq!(rig.next_frame(start).await.0, FrameKind::Added);

    let mut done = (*v).clone();
    done.status = Status::Finished;
    done.filename = Some(Arc::from("A title.mp4"));
    done.size = Some(103_809_024);
    rig.events
        .publish(DomainEvent::Completed(Arc::new(done)))
        .await;
    let (kind, _, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Completed);
    assert_eq!(body["items"][0]["status"], "finished");
    assert_eq!(body["items"][0]["percent"], 100.0);
    assert_eq!(body["items"][0]["size"], 103_809_024_u64);

    rig.events
        .publish(DomainEvent::Removed {
            ids: vec![v.id],
            reason: RemoveReason::Expired,
        })
        .await;
    let (kind, _, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Removed);
    assert_eq!(body["reason"], "expired");
    assert_eq!(body["ids"][0], v.id.to_string());
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(rig.state.load().is_empty());
}

/// The two lossless progress kinds are **not** batched into a frame: the aggregator forwards them
/// to the engine, which persists them and republishes, and the resulting `delta` is what the client
/// sees. This is the whole round trip — provider → aggregator → engine → SQLite → hub — with the
/// aggregator standing in for the pump WP-12's tests used.
#[tokio::test(start_paused = true)]
async fn a_stage_message_is_forwarded_to_the_engine_and_comes_back_as_a_delta() {
    let mut parked = row(Status::Queued, 1);
    parked.auto_start = false;
    let id = parked.id;
    let mut rig = Rig::seeded(&[("AULOS_WS_BATCH_MS", "50")], vec![parked]).await;

    // Boot recovery publishes the working set, which becomes the aggregator's baseline.
    let start = Instant::now();
    let (kind, _, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Added);
    assert_eq!(body["items"][0]["id"], id.to_string());
    assert_eq!(body["items"][0]["status"], "queued");
    assert_eq!(body["items"][0]["auto_start"], false);

    rig.progress
        .send(ProgressMsg::Stage {
            id,
            stage: aulos_provider::Stage::Preparing,
            msg: Some("Preparing".into()),
        })
        .await
        .unwrap();

    let (kind, _, body) = rig.next_frame(start).await;
    assert_eq!(kind, FrameKind::Delta, "the stage arrives as a patch");
    assert_eq!(body["items"][0]["id"], id.to_string());
    assert_eq!(body["items"][0]["status"], "preparing");
    assert_eq!(body["items"][0]["msg"], "Preparing");
    assert!(
        body["items"][0].get("percent").is_none(),
        "nothing numeric moved, so no numeric key is on the wire"
    );

    // And it is persisted, which is why it went through the engine at all.
    let persisted = rig.store.item(id).await.unwrap().expect("the row");
    assert_eq!(persisted.status, Status::Preparing);
    assert_eq!(persisted.msg.as_deref(), Some("Preparing"));
    assert_eq!(rig.state.load().items[0].status, Status::Preparing);
}

/// DESIGN §4.7/§8.11: the aggregator bumps the per-job heartbeat on **every** progress frame it
/// receives, whatever the frame carries and whether or not the row is still tracked. That is the
/// entire input to the stall watchdog.
#[tokio::test(start_paused = true)]
async fn every_progress_frame_bumps_the_stall_watchdogs_heartbeat() {
    let rig = Rig::new(&[]).await;
    let tracked = view(Status::Downloading, 1);
    let untracked = ItemId::new();
    let beat_a = rig.engine.heartbeats().arm(tracked.id, 0);
    let beat_b = rig.engine.heartbeats().arm(untracked, 0);
    rig.added(std::slice::from_ref(&tracked)).await;

    rig.progress(tracked.id, 10.0).await;
    rig.progress(untracked, 10.0).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(beat_a.frames(), 1);
    assert_eq!(
        beat_b.frames(),
        1,
        "a frame for a row the aggregator does not track is still liveness"
    );
    assert!(beat_a.last_ms() > 0 && beat_b.last_ms() > 0);
}

// ---------------------------------------------------------------------------
// `?since=` across a restart
// ---------------------------------------------------------------------------

/// `seq` is durable and reserve-first, so a restart continues above whatever the previous process
/// handed out; and because the `boot_id` changed, a client's old cursor is answered with a
/// snapshot rather than a delta it could not apply.
#[tokio::test]
async fn since_is_correct_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_in(dir.path(), &[]);

    let (before_head, before_boot) = {
        let store = Store::open(StoreOptions::from_config(&cfg).with_flush_ms(5)).unwrap();
        let hub = EventHub::new(store.seq_allocator(), aulos_core::BootId::new(), &cfg);
        for _ in 0..3 {
            hub.publish_frame(FrameBody::Completed {
                items: vec![view(Status::Finished, 1)],
            });
        }
        let head = hub.head();
        let boot = hub.boot_id();
        assert!(matches!(hub.resume(head, Some(boot)), Resume::UpToDate));
        (head, boot)
    };

    let store = Store::open(StoreOptions::from_config(&cfg).with_flush_ms(5)).unwrap();
    let hub = EventHub::new(store.seq_allocator(), aulos_core::BootId::new(), &cfg);
    assert!(
        hub.head() >= before_head,
        "a durable allocator never re-issues a sequence: {} then {}",
        before_head,
        hub.head()
    );
    assert_ne!(hub.boot_id(), before_boot);
    assert!(
        matches!(hub.resume(before_head, Some(before_boot)), Resume::Snapshot),
        "the boot id moved, so the old cursor is not resumable"
    );
    assert!(
        matches!(hub.resume(before_head, None), Resume::Snapshot),
        "and it is below the fresh ring's floor as well"
    );
    assert!(matches!(
        hub.resume(hub.head(), Some(hub.boot_id())),
        Resume::UpToDate
    ));
    assert!(
        matches!(
            hub.resume(Seq(hub.head().0 + 1_000), Some(hub.boot_id())),
            Resume::Snapshot
        ),
        "a cursor above the head — a restored database — is a snapshot, never an empty delta"
    );
}

// ---------------------------------------------------------------------------
// the resume merge
// ---------------------------------------------------------------------------

/// PROTOCOL §7's apply algorithm, as an oracle: `added`/`completed` upsert, `removed` deletes,
/// `delta` patches only what it mentions and never creates a record.
#[derive(Clone, Debug, Default, PartialEq)]
struct ClientState(HashMap<ItemId, Value>);

impl ClientState {
    fn apply(&mut self, body: &FrameBody) {
        match body {
            FrameBody::Added { items, .. } | FrameBody::Completed { items } => {
                for view in items {
                    self.0
                        .insert(view.id, serde_json::to_value(&**view).unwrap());
                }
            }
            FrameBody::Removed { ids, .. } => {
                for id in ids {
                    self.0.remove(id);
                }
            }
            FrameBody::Delta(batch) => {
                for patch in &batch.items {
                    let Some(Value::Object(row)) = self.0.get_mut(&patch.id) else {
                        continue;
                    };
                    for (key, value) in &patch.fields {
                        row.insert((*key).to_owned(), value.clone());
                    }
                }
            }
            FrameBody::Other { .. } => {}
        }
    }
}

fn entry(seq: u64, body: FrameBody) -> RingEntry {
    let kind = body.kind();
    RingEntry {
        seq: Seq(seq),
        wire: Arc::new(WireFrame {
            seq: Seq(seq),
            kind,
            text: bytes::Bytes::from_static(b"{}"),
        }),
        body: Arc::new(body),
    }
}

/// One scripted frame, so `proptest` can shrink a window rather than a JSON blob.
#[derive(Clone, Debug)]
enum Step {
    Add(usize),
    Complete(usize),
    Remove(usize, u8),
    Patch(usize, f64, bool),
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        (0usize..4).prop_map(Step::Add),
        (0usize..4).prop_map(Step::Complete),
        (0usize..4, 0u8..4).prop_map(|(i, r)| Step::Remove(i, r)),
        (0usize..4, 0.0f64..100.0, any::<bool>()).prop_map(|(i, p, m)| Step::Patch(i, p, m)),
    ];
    proptest::collection::vec(step, 0..24)
}

proptest! {
    /// The structural property behind `resume`: folding a window and applying the result must leave
    /// a client in exactly the state it would have reached by applying every original frame in
    /// order. Includes the two explicit cases DESIGN §15.3 calls out — added-then-removed drops
    /// both, and a `completed` supersedes an earlier delta.
    #[test]
    fn merging_a_window_is_equivalent_to_replaying_it(script in steps()) {
        let ids: Vec<ItemId> = (0..4).map(|_| ItemId::new()).collect();
        let reasons = [
            RemoveReason::Deleted,
            RemoveReason::Cleared,
            RemoveReason::Expired,
            RemoveReason::Replaced,
        ];
        let mut ring = Ring::new(4_096, 1 << 24, Seq(0));
        let mut replayed = ClientState::default();
        let mut bodies: Vec<FrameBody> = Vec::new();

        for (n, step) in script.iter().enumerate() {
            let seq = n as u64 + 1;
            let body = match step {
                Step::Add(i) => {
                    let mut v = (*view(Status::Queued, *i as i64)).clone();
                    v.id = ids[*i];
                    FrameBody::Added { reason: AddReason::Created, items: vec![Arc::new(v)] }
                }
                Step::Complete(i) => {
                    let mut v = (*view(Status::Finished, *i as i64)).clone();
                    v.id = ids[*i];
                    FrameBody::Completed { items: vec![Arc::new(v)] }
                }
                Step::Remove(i, r) => FrameBody::Removed {
                    reason: reasons[usize::from(*r) % reasons.len()],
                    ids: vec![ids[*i]],
                },
                Step::Patch(i, percent, with_msg) => {
                    let mut patch = DeltaItem::new(ids[*i]);
                    patch.fields.insert("percent", (*percent).into());
                    if *with_msg {
                        patch.fields.insert("msg", Value::from(format!("step {n}")));
                    }
                    FrameBody::Delta(Arc::new(DeltaBatch { ts: 0, items: vec![patch] }))
                }
            };
            replayed.apply(&body);
            bodies.push(body.clone());
            ring.push(entry(seq, body));
        }

        let fold = ring.merge_after(Seq(0));
        let mut merged = ClientState::default();
        if let Some((reason, items)) = &fold.added {
            merged.apply(&FrameBody::Added { reason: *reason, items: items.clone() });
        }
        if !fold.completed.is_empty() {
            merged.apply(&FrameBody::Completed { items: fold.completed.clone() });
        }
        for (reason, ids) in &fold.removed {
            merged.apply(&FrameBody::Removed { reason: *reason, ids: ids.clone() });
        }
        if !fold.delta.is_empty() {
            merged.apply(&FrameBody::Delta(Arc::new(DeltaBatch {
                ts: 0,
                items: fold.delta.clone(),
            })));
        }

        prop_assert_eq!(&merged, &replayed, "script: {:?}", script);
        prop_assert_eq!(fold.to, Seq(script.len() as u64));
        // At most one `removed` frame per distinct reason, in the fixed order.
        let order: Vec<RemoveReason> = fold.removed.iter().map(|(r, _)| *r).collect();
        let mut expected: Vec<RemoveReason> = aulos_queue::REASON_ORDER
            .iter()
            .copied()
            .filter(|r| order.contains(r))
            .collect();
        expected.dedup();
        prop_assert_eq!(order, expected);
        prop_assert!(bodies.len() == script.len());
    }
}

#[test]
fn a_ring_burst_of_large_added_frames_is_bounded_by_bytes() {
    let mut ring = Ring::new(512, 4 * 1_024 * 1_024, Seq(0));
    for seq in 1..=2_000u64 {
        let body = FrameBody::Added {
            reason: AddReason::Expanded,
            items: vec![view(Status::Queued, 1)],
        };
        let kind = body.kind();
        ring.push(RingEntry {
            seq: Seq(seq),
            wire: Arc::new(WireFrame {
                seq: Seq(seq),
                kind,
                text: bytes::Bytes::from(vec![b'x'; 64 * 1_024]),
            }),
            body: Arc::new(body),
        });
    }
    assert!(ring.len() <= 512, "the frame bound: {}", ring.len());
    assert!(
        ring.bytes() <= 4 * 1_024 * 1_024,
        "the byte bound: {} bytes",
        ring.bytes()
    );
    assert!(ring.len() < 512, "64 KiB frames hit the byte bound first");
    assert!(ring.floor() > Seq(0));
}
