//! The shared harness every integration test in this crate runs on.
//!
//! Everything is local: a temporary directory, a SQLite file with a five-millisecond flush window,
//! the scripted `fake` provider (BRIEF §17) and a [`FakeClock`]. Nothing touches the network and
//! nothing waits on wall-clock time — the engine's deadlines all read the fake clock, and
//! [`EngineHandle::tick`] runs the maintenance pass on demand.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use aulos_core::config::{RawEnv, load};
use aulos_core::{
    Clock, Codec, Config, DomainEvent, DownloadRequest, DownloadType, EventInbox, EventRouter,
    EventSender, FakeClock, FormatId, Item, ItemId, ItemView, QualityId, Selection, SourceKind,
    SourceRef, Status, SubscriberSpec, YtdlOptions,
};
use aulos_provider::fake::{FakeProvider, Step, Timeline};
use aulos_provider::{Match, ProgressMsg, ProgressSinkFactory, Provider, ProviderId, Registry};
use aulos_queue::{Engine, EngineHandle, PreTerminalHooks, RecoveryReport};
use aulos_store::{Store, StoreOptions};
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// How long any harness wait may take before it gives up.
///
/// A wall-clock deadline rather than a poll count: a shared CI runner then gets exactly the same
/// budget as a quiet laptop, instead of a budget that shrinks with every scheduling delay. Every
/// step these tests wait on is milliseconds' work, so this is slack, not a measurement.
const WAIT_BUDGET: Duration = Duration::from_secs(15);

/// How often a wait re-reads what it is waiting for.
const POLL: Duration = Duration::from_millis(1);

/// A running engine plus everything a test needs to drive and observe it.
pub struct Harness {
    pub dir: TempDir,
    pub cfg: Arc<Config>,
    pub store: Store,
    pub registry: Arc<RwLock<Registry>>,
    pub clock: Arc<FakeClock>,
    pub handle: EngineHandle,
    pub events: Events,
    /// The `hooks` subscriber's view: `Finishing | Completed` (DESIGN §2.2.1). `Finishing` is
    /// deliberately absent from the aggregator's filter, so it can only be observed here.
    pub hooks: Events,
    pub sink: ProgressSinkFactory,
    pub progress: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<ProgressMsg>>>>,
    _engine: JoinHandle<()>,
    _router: JoinHandle<()>,
    _pump: Option<JoinHandle<()>>,
    _sender: EventSender,
}

/// How a harness is put together.
pub struct HarnessBuilder {
    env: Vec<(String, String)>,
    providers: Vec<Arc<dyn Provider>>,
    pre_terminal: Option<Arc<dyn PreTerminalHooks>>,
    pump: bool,
    progress_capacity: usize,
    seed: Vec<Item>,
    recover: bool,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            providers: Vec::new(),
            pre_terminal: None,
            pump: true,
            progress_capacity: 1_024,
            seed: Vec::new(),
            recover: false,
        }
    }
}

impl HarnessBuilder {
    /// Adds or overrides an environment variable.
    #[must_use]
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Registers a provider. Registration order breaks score ties (DESIGN §6.3).
    #[must_use]
    pub fn provider(mut self, p: Arc<dyn Provider>) -> Self {
        self.providers.push(p);
        self
    }

    /// Wires the pre-terminal hook gate (DESIGN §13).
    #[must_use]
    pub fn pre_terminal(mut self, hooks: Arc<dyn PreTerminalHooks>) -> Self {
        self.pre_terminal = Some(hooks);
        self
    }

    /// Leaves the progress channel unread, so a saturated channel drops frames.
    #[must_use]
    pub fn without_pump(mut self) -> Self {
        self.pump = false;
        self
    }

    /// Shrinks the progress channel.
    #[must_use]
    pub fn progress_capacity(mut self, n: usize) -> Self {
        self.progress_capacity = n;
        self
    }

    /// Inserts rows before the engine starts, for a boot-recovery test.
    #[must_use]
    pub fn seed(mut self, items: Vec<Item>) -> Self {
        self.seed = items;
        self.recover = true;
        self
    }

    /// Runs [`Engine::recover`] before spawning.
    #[must_use]
    pub fn recovering(mut self) -> Self {
        self.recover = true;
        self
    }

    /// Builds and spawns everything.
    pub async fn build(self) -> Harness {
        let (h, _) = self.build_reporting().await;
        h
    }

    /// Builds and spawns everything, also returning what boot recovery found.
    pub async fn build_reporting(self) -> (Harness, Option<RecoveryReport>) {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["downloads", "audio", "temp", "state"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        let mut env: Vec<(String, String)> = vec![
            ("STATE_DIR".into(), path(dir.path(), "state")),
            ("DOWNLOAD_DIR".into(), path(dir.path(), "downloads")),
            ("AUDIO_DOWNLOAD_DIR".into(), path(dir.path(), "audio")),
            ("TEMP_DIR".into(), path(dir.path(), "temp")),
            ("AULOS_DB_PATH".into(), path(dir.path(), "state/aulos.db")),
            ("AULOS_DB_FLUSH_MS".into(), "5".into()),
            ("CUSTOM_DIRS".into(), "true".into()),
            ("CREATE_CUSTOM_DIRS".into(), "true".into()),
        ];
        env.extend(self.env);
        let cfg = Arc::new(load(&RawEnv::from_pairs(env)).expect("the harness config must load"));

        let store = Store::open(
            StoreOptions::from_config(&cfg)
                .with_flush_ms(5)
                .with_readers(2)
                .with_busy_timeout_ms(500),
        )
        .unwrap();
        if !self.seed.is_empty() {
            store
                .write(
                    vec![aulos_store::WriteOp::InsertItems { items: self.seed }],
                    aulos_store::Durability::Sync,
                )
                .await
                .unwrap();
        }

        let mut registry = Registry::new();
        let providers = if self.providers.is_empty() {
            vec![Arc::new(fake()) as Arc<dyn Provider>]
        } else {
            self.providers
        };
        for p in providers {
            registry.register(p);
        }
        let registry = Arc::new(RwLock::new(registry));

        let clock = Arc::new(FakeClock::default());
        let (mut router, sender) = EventRouter::new(4_096);
        let inbox = router.subscribe(SubscriberSpec::aggregator());
        let hooks_inbox = router.subscribe(SubscriberSpec::hooks());
        let router_task = router.spawn();

        let (progress_tx, progress_rx) =
            tokio::sync::mpsc::channel::<ProgressMsg>(self.progress_capacity);
        let sink = ProgressSinkFactory::new(progress_tx.clone());

        let (mut engine, handle) = Engine::new(
            store.clone(),
            Arc::clone(&registry),
            Arc::clone(&cfg),
            Arc::new(ArcSwap::from_pointee(YtdlOptions::empty())),
            Arc::clone(&clock) as Arc<dyn Clock>,
            sender.clone(),
            progress_tx,
        );
        if let Some(hooks) = self.pre_terminal {
            engine = engine.with_pre_terminal(hooks);
        }
        let report = if self.recover {
            Some(engine.recover().await.expect("recovery"))
        } else {
            None
        };
        let engine_task = engine.spawn();

        let progress = Arc::new(Mutex::new(None));
        let pump = if self.pump {
            let pump_handle = handle.clone();
            let beats = handle.heartbeats().clone();
            let pump_clock = Arc::clone(&clock);
            let mut rx = progress_rx;
            Some(tokio::spawn(async move {
                // This is the aggregator's job in production (DESIGN §15.1): bump the heartbeat on
                // every frame, and forward the two lossless kinds to the engine.
                while let Some(msg) = rx.recv().await {
                    beats.frame(msg.item_id(), pump_clock.now_ms());
                    match msg {
                        ProgressMsg::Progress { .. } => {}
                        ProgressMsg::Stage { id, stage, msg } => {
                            pump_handle.stage(id, stage, msg).await;
                        }
                        ProgressMsg::File { id, slot, file } => {
                            pump_handle.file(id, slot, file).await;
                        }
                    }
                }
            }))
        } else {
            *progress.lock().unwrap() = Some(progress_rx);
            None
        };

        let harness = Harness {
            dir,
            cfg,
            store,
            registry,
            clock,
            handle,
            events: Events::spawn(inbox),
            hooks: Events::spawn(hooks_inbox),
            sink,
            progress,
            _engine: engine_task,
            _router: router_task,
            _pump: pump,
            _sender: sender,
        };
        (harness, report)
    }
}

impl Harness {
    /// A harness with the default fake provider and nothing seeded.
    pub async fn new() -> Self {
        Self::builder().build().await
    }

    /// A configurable harness.
    #[must_use]
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder::default()
    }

    /// Adds one URL and returns its id.
    pub async fn add(&self, url: &str) -> ItemId {
        let out = self
            .handle
            .add(vec![request(url)], SourceRef::bare(SourceKind::ApiV2))
            .await
            .expect("the add must be accepted");
        *out.ids.first().expect("one id")
    }

    /// Adds one request and returns the whole outcome.
    pub async fn add_request(
        &self,
        request: DownloadRequest,
    ) -> Result<aulos_queue::AddOutcome, aulos_queue::AddError> {
        self.handle
            .add(vec![request], SourceRef::bare(SourceKind::ApiV2))
            .await
    }

    /// The persisted row, or `None`.
    pub async fn item(&self, id: ItemId) -> Option<Item> {
        self.store.item(id).await.unwrap()
    }

    /// Waits until the persisted row satisfies `pred`, or panics at [`WAIT_BUDGET`].
    pub async fn until(&self, id: ItemId, what: &str, pred: impl Fn(&Item) -> bool) -> Item {
        let deadline = Instant::now() + WAIT_BUDGET;
        loop {
            if let Some(item) = self.store.item(id).await.unwrap()
                && pred(&item)
            {
                return item;
            }
            if Instant::now() >= deadline {
                let found = self.store.item(id).await.unwrap();
                panic!("timed out waiting for {what}; the row is {found:?}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Waits until the row reaches `status`.
    ///
    /// For a terminal status it also waits for the row's `Completed` event to have been fanned out:
    /// the persisted write and the event reach observers on different paths, and a test that reads
    /// `events.completed()` right after seeing the row would otherwise race the router by a few
    /// hundred microseconds (seen as a flake on CI).
    pub async fn until_status(&self, id: ItemId, status: Status) -> Item {
        let item = self
            .until(id, &format!("status {status}"), |i| i.status == status)
            .await;
        if status.is_terminal() {
            self.events
                .until(
                    "completed",
                    |e| matches!(e, DomainEvent::Completed(v) if v.id == id),
                )
                .await;
        }
        item
    }

    /// Waits until the row leaves `resolving`.
    pub async fn until_resolved(&self, id: ItemId) -> Item {
        self.until(id, "resolution", |i| i.status != Status::Resolving)
            .await
    }

    /// Waits until `pred` holds over the whole persisted queue.
    pub async fn until_all(&self, what: &str, pred: impl Fn(&[Item]) -> bool) -> Vec<Item> {
        let deadline = Instant::now() + WAIT_BUDGET;
        loop {
            let rows = self.rows().await;
            if pred(&rows) {
                return rows;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}; the queue holds {} rows",
                    rows.len()
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Every persisted row, `ord` ascending.
    pub async fn rows(&self) -> Vec<Item> {
        self.store
            .items(aulos_store::ItemFilter::default())
            .await
            .unwrap()
            .rows
    }

    /// The children of one group, `group_index` ascending.
    pub async fn children(&self, group: ItemId) -> Vec<Item> {
        let mut rows = self
            .store
            .items(
                aulos_store::ItemFilter::default().with_group(aulos_store::GroupScope::Of(group)),
            )
            .await
            .unwrap()
            .rows;
        rows.sort_by_key(|i| i.group_index);
        rows
    }

    /// Lets the engine drain everything queued behind the commands sent so far.
    ///
    /// One `tick` round-trip is enough: the channel is FIFO, so an ack from a command sent after
    /// the ones under test means they have all been handled.
    pub async fn settle(&self) {
        for _ in 0..8 {
            self.handle.tick().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Moves the fake clock and runs the maintenance pass.
    pub async fn advance(&self, by: Duration) {
        self.clock.advance(by);
        self.handle.tick().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    /// Waits until a path is gone, or panics at [`WAIT_BUDGET`].
    ///
    /// A delete's unlinks run on a blocking pool rather than on the engine task (DESIGN §8.2, §8.10
    /// — a bulk clear is tens of thousands of syscalls and nothing waits on their result), so
    /// "the file is gone" is an eventual assertion, not an immediate one.
    pub async fn until_gone(&self, path: &Path) {
        let deadline = Instant::now() + WAIT_BUDGET;
        loop {
            if !path.exists() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {} to be removed",
                path.display()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// The absolute download root.
    #[must_use]
    pub fn download_dir(&self) -> PathBuf {
        self.cfg.paths.download.clone()
    }

    /// The absolute scratch root.
    #[must_use]
    pub fn temp_dir(&self) -> PathBuf {
        self.cfg.paths.temp.clone()
    }

    /// The per-job scratch directory the engine gives an item.
    #[must_use]
    pub fn job_temp_dir(&self, id: ItemId) -> PathBuf {
        self.temp_dir().join(id.to_string())
    }
}

/// A background collector over one [`EventInbox`].
pub struct Events {
    seen: Arc<Mutex<Vec<Arc<DomainEvent>>>>,
    _task: JoinHandle<()>,
}

impl Events {
    fn spawn(mut inbox: EventInbox) -> Self {
        let seen: Arc<Mutex<Vec<Arc<DomainEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            while let Some(ev) = inbox.recv().await {
                sink.lock().unwrap().push(ev);
            }
        });
        Self { seen, _task: task }
    }

    /// Everything published so far, in order.
    pub fn all(&self) -> Vec<Arc<DomainEvent>> {
        self.seen.lock().unwrap().clone()
    }

    /// Forgets everything seen so far.
    pub fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// How many events match.
    pub fn count(&self, pred: impl Fn(&DomainEvent) -> bool) -> usize {
        self.all().iter().filter(|e| pred(e)).count()
    }

    /// Whether any event matches.
    pub fn any(&self, pred: impl Fn(&DomainEvent) -> bool) -> bool {
        self.count(pred) > 0
    }

    /// Waits until at least one event matches, or panics at [`WAIT_BUDGET`].
    pub async fn until(&self, what: &str, pred: impl Fn(&DomainEvent) -> bool) {
        self.until_count(what, 1, pred).await;
    }

    /// Waits until at least `n` events match, or panics at [`WAIT_BUDGET`].
    ///
    /// The router fans out on its own task, so an event is always a little behind the store write
    /// that produced it. A test that reads the recorded events after waiting on a *row* has to
    /// wait for the event too, or it races the router (DESIGN §15.1).
    pub async fn until_count(&self, what: &str, n: usize, pred: impl Fn(&DomainEvent) -> bool) {
        let deadline = Instant::now() + WAIT_BUDGET;
        loop {
            let seen = self.count(&pred);
            if seen >= n {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {n} {what} events, saw {seen}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Every `added` batch, as `(ids, reason)`.
    pub fn added(&self) -> Vec<(Vec<ItemId>, aulos_core::AddReason)> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::Added(views, reason) => {
                    Some((views.iter().map(|v| v.id).collect(), *reason))
                }
                _ => None,
            })
            .collect()
    }

    /// Every `removed` batch, as `(ids, reason)`.
    pub fn removed(&self) -> Vec<(Vec<ItemId>, aulos_core::RemoveReason)> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::Removed { ids, reason } => Some((ids.clone(), *reason)),
                _ => None,
            })
            .collect()
    }

    /// Every `completed` view.
    pub fn completed(&self) -> Vec<Arc<ItemView>> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::Completed(v) => Some(Arc::clone(v)),
                _ => None,
            })
            .collect()
    }

    /// Every `finishing` view (DESIGN §13 — hooks only, never on the wire).
    pub fn finishing(&self) -> Vec<Arc<ItemView>> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::Finishing(v) => Some(Arc::clone(v)),
                _ => None,
            })
            .collect()
    }

    /// Every `status_changed` view for one id, in order.
    pub fn changes(&self, id: ItemId) -> Vec<Arc<ItemView>> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::StatusChanged { id: got, view, .. } if *got == id => {
                    Some(Arc::clone(view))
                }
                _ => None,
            })
            .collect()
    }

    /// Every notice, as `(code, item)`.
    pub fn notices(&self) -> Vec<(String, Option<ItemId>)> {
        self.all()
            .iter()
            .filter_map(|e| e.as_notice().map(|n| (n.code.to_string(), n.id)))
            .collect()
    }
}

/// The selection every fixture uses: `video / auto / mp4 / best`.
#[must_use]
pub fn selection() -> Selection {
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").unwrap(),
        QualityId::parse("best").unwrap(),
    )
}

/// A request for one URL, with every legacy default.
#[must_use]
pub fn request(url: &str) -> DownloadRequest {
    DownloadRequest::new(url::Url::parse(url).unwrap(), selection())
}

/// The default fake provider: `Strong(200)` on `fake.test`, one synthetic video per URL, and the
/// built-in preparing → downloading → 100 % → a small file script.
#[must_use]
pub fn fake() -> FakeProvider {
    FakeProvider::from_toml("id = \"fake\"\nscore = 200\nhosts = [\"fake.test\"]\n").unwrap()
}

/// A fake provider under a chosen id and match, matching every host.
pub fn fake_named(id: &str, answer: Match) -> FakeProvider {
    FakeProvider::from_toml(&format!("id = \"{id}\"\n"))
        .unwrap()
        .with_match(answer)
}

/// A provider whose download hangs until cancelled (DESIGN §20: a real hang, no timer).
#[must_use]
pub fn hanging() -> FakeProvider {
    let mut timeline = Timeline::new();
    timeline.download = vec![
        Step::Stage(aulos_provider::Stage::Preparing),
        Step::Stage(aulos_provider::Stage::Downloading),
        Step::Hang,
    ];
    fake().with_timeline(timeline)
}

/// A provider that resolves a URL containing `playlist` into `count` synthetic children, and
/// every other URL into one plain video.
///
/// Scoped by `url_regex` on purpose: a provider that expanded *every* URL would turn each of a
/// test's ordinary adds into a group as well.
#[must_use]
pub fn expanding(count: usize) -> FakeProvider {
    FakeProvider::from_toml(&format!(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        url_regex = "playlist"
        resolve = [{{ kind = "expand_playlist", count = {count} }}]

        [[timeline]]
        resolve = []
    "#
    ))
    .unwrap()
}

/// A pre-terminal gate that claims every item, with one label.
pub struct AlwaysPreTerminal(pub &'static str);

impl PreTerminalHooks for AlwaysPreTerminal {
    fn label_for(&self, _view: &ItemView) -> Option<Box<str>> {
        Some(Box::from(self.0))
    }
}

/// A pre-terminal gate that claims nothing.
pub struct NeverPreTerminal;

impl PreTerminalHooks for NeverPreTerminal {
    fn label_for(&self, _view: &ItemView) -> Option<Box<str>> {
        None
    }
}

/// A path inside the harness directory, as a string.
fn path(root: &Path, sub: &str) -> String {
    root.join(sub).to_string_lossy().into_owned()
}

/// A `provider_id` from a literal.
#[must_use]
pub fn pid(id: &str) -> ProviderId {
    ProviderId::parse(id).unwrap()
}

/// Groups a status histogram over a row set, for the recovery assertions.
#[must_use]
pub fn histogram(rows: &[Item]) -> HashMap<(Status, bool), usize> {
    let mut out: HashMap<(Status, bool), usize> = HashMap::new();
    for row in rows {
        *out.entry((row.status, row.auto_start)).or_default() += 1;
    }
    out
}
