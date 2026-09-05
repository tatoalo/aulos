//! The shared harness for the subscription suite.
//!
//! Everything is local: a temporary directory, a SQLite file with a five-millisecond flush window,
//! the scripted `fake` provider (BRIEF §17), a real queue engine and pinned jitter. Nothing
//! touches the network.
//!
//! Time is the interesting part. The scheduler sleeps on `tokio::time`, and the manager stamps
//! `last_checked` / `next_due` from an [`aulos_core::Clock`], so the two must agree or a test
//! asserting "the next check is 2× the interval away" cannot be written. [`TokioClock`] derives
//! its wall clock from `tokio::time::Instant`, so under `tokio::time::pause()` advancing virtual
//! time advances both halves together.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::config::{RawEnv, load};
use aulos_core::subscription::{SubCmd, SubscriptionsHandle};
use aulos_core::{
    Clock, Config, DomainEvent, EventInbox, EventRouter, EventSender, SubId, SubscriberSpec,
    SubscriptionRecord, SubscriptionView, UnixMs, YtdlOptions,
};
use aulos_provider::fake::FakeProvider;
use aulos_provider::{ProgressMsg, Provider, Registry};
use aulos_queue::{Engine, EngineHandle};
use aulos_store::{Store, StoreOptions};
use aulos_subscriptions::{
    CheckFailure, CheckReport, Checker, Feed, FeedChecker, FixedJitter, Manager, StaticOptions,
    SubDeps,
};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use url::Url;

/// The epoch every harness clock starts at: `2026-03-04T00:00:00Z`, matching
/// [`aulos_core::clock::DEFAULT_FAKE_EPOCH_MS`] so snapshots across crates line up.
pub const EPOCH_MS: UnixMs = 1_772_582_400_000;

/// A clock whose wall half is derived from `tokio::time::Instant`.
///
/// This is what makes `tokio::time::pause()` usable here: `tokio::time::advance(2h)` moves both
/// the timers the scheduler sleeps on *and* the `now_ms()` the manager stamps records with.
#[derive(Debug)]
pub struct TokioClock {
    epoch_ms: UnixMs,
    base: tokio::time::Instant,
}

impl TokioClock {
    #[must_use]
    pub fn new(epoch_ms: UnixMs) -> Self {
        Self {
            epoch_ms,
            base: tokio::time::Instant::now(),
        }
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new(EPOCH_MS)
    }
}

impl Clock for TokioClock {
    fn now_ms(&self) -> UnixMs {
        let elapsed = tokio::time::Instant::now().duration_since(self.base);
        self.epoch_ms
            .saturating_add(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
    }

    fn instant(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }
}

/// A [`FeedChecker`] driven from a test rather than from a provider.
///
/// Records every call, reports the configured answer for a URL, and holds a permit for
/// `hold` before answering so a concurrency test can observe how many run at once.
#[derive(Debug, Default)]
pub struct ScriptedChecker {
    answers: Mutex<HashMap<String, Vec<ScriptedAnswer>>>,
    fallback: Mutex<Option<ScriptedAnswer>>,
    hold: Mutex<HashMap<String, Duration>>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    calls: Mutex<Vec<String>>,
    done: Mutex<Vec<String>>,
    live: AtomicUsize,
    peak: AtomicUsize,
}

/// One scripted outcome.
#[derive(Clone, Debug)]
pub enum ScriptedAnswer {
    /// A successful check with this many queued ids.
    Ok(Vec<Box<str>>),
    /// A failure.
    Fail(CheckFailure),
}

impl ScriptedChecker {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queues answers for `url`; the last one repeats once the queue is drained.
    pub fn script(&self, url: &str, answers: Vec<ScriptedAnswer>) {
        self.answers.lock().unwrap().insert(url.to_owned(), answers);
    }

    /// The answer for any URL with no script of its own.
    pub fn default_answer(&self, answer: ScriptedAnswer) {
        *self.fallback.lock().unwrap() = Some(answer);
    }

    /// Makes a check for `url` take `d` of (virtual) time.
    ///
    /// Beware: a `tokio::time::sleep` is a *timer*, and paused time auto-advances to the nearest
    /// timer whenever the runtime idles — so this cannot hold a check open. Use [`Self::gate`] for
    /// that.
    pub fn slow(&self, url: &str, d: Duration) {
        self.hold.lock().unwrap().insert(url.to_owned(), d);
    }

    /// Blocks every check of `url` until the returned [`tokio::sync::Notify`] is notified.
    ///
    /// A `Notify` is not a timer, so paused time cannot skip past it — which is what makes "the
    /// slow one still holds its permit while the others finish" assertable.
    pub fn gate(&self, url: &str) -> Arc<tokio::sync::Notify> {
        let gate = Arc::new(tokio::sync::Notify::new());
        self.gates
            .lock()
            .unwrap()
            .insert(url.to_owned(), Arc::clone(&gate));
        gate
    }

    /// Every URL whose check ran to completion, in order.
    #[must_use]
    pub fn completed(&self) -> Vec<String> {
        self.done.lock().unwrap().clone()
    }

    /// Every URL checked, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// How many checks ran at the same time, at most.
    #[must_use]
    pub fn peak_concurrency(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn next_answer(&self, url: &str) -> ScriptedAnswer {
        let mut answers = self.answers.lock().unwrap();
        if let Some(queue) = answers.get_mut(url) {
            if queue.len() > 1 {
                return queue.remove(0);
            }
            if let Some(last) = queue.first() {
                return last.clone();
            }
        }
        drop(answers);
        self.fallback
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(ScriptedAnswer::Ok(Vec::new()))
    }
}

#[async_trait::async_trait]
impl FeedChecker for ScriptedChecker {
    async fn probe(&self, url: &Url) -> Result<Feed, CheckFailure> {
        self.calls.lock().unwrap().push(url.to_string());
        match self.next_answer(url.as_str()) {
            ScriptedAnswer::Fail(f) => Err(f),
            ScriptedAnswer::Ok(_) => Ok(Feed {
                name: Some("Scripted".into()),
                entries: Vec::new(),
            }),
        }
    }

    async fn check(&self, record: &SubscriptionRecord) -> Result<CheckReport, CheckFailure> {
        let url = record.url.to_string();
        self.calls.lock().unwrap().push(url.clone());
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);

        let hold = self.hold.lock().unwrap().get(&url).copied();
        if let Some(d) = hold {
            tokio::time::sleep(d).await;
        }
        let gate = self.gates.lock().unwrap().get(&url).map(Arc::clone);
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.done.lock().unwrap().push(url.clone());

        match self.next_answer(&url) {
            ScriptedAnswer::Fail(f) => Err(f),
            ScriptedAnswer::Ok(queued) => Ok(CheckReport {
                new_total: queued.len(),
                queued,
                errors: Vec::new(),
            }),
        }
    }
}

/// What [`HarnessBuilder::build`] decides in one step: the checker, and the engine plumbing that
/// only the real [`Checker`] needs.
type Wiring = (
    Arc<dyn FeedChecker>,
    Option<EngineHandle>,
    Option<JoinHandle<()>>,
    Option<JoinHandle<()>>,
);

/// A running manager plus everything a test needs to drive and observe it.
pub struct Harness {
    pub dir: TempDir,
    pub cfg: Arc<Config>,
    pub store: Store,
    pub clock: Arc<TokioClock>,
    pub handle: SubscriptionsHandle,
    pub events: Events,
    pub engine: Option<EngineHandle>,
    pub scripted: Option<Arc<ScriptedChecker>>,
    _manager: JoinHandle<()>,
    _router: JoinHandle<()>,
    _engine: Option<JoinHandle<()>>,
    _pump: Option<JoinHandle<()>>,
    _sender: EventSender,
}

/// How a harness is put together.
pub struct HarnessBuilder {
    env: Vec<(String, String)>,
    providers: Vec<Arc<dyn Provider>>,
    scripted: Option<Arc<ScriptedChecker>>,
    jitter: Option<f64>,
    seed: Vec<SubscriptionRecord>,
    seen: Vec<(SubId, Vec<Box<str>>)>,
    dir: Option<TempDir>,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            providers: Vec::new(),
            scripted: None,
            jitter: Some(0.5),
            seed: Vec::new(),
            seen: Vec::new(),
            dir: None,
        }
    }
}

impl HarnessBuilder {
    #[must_use]
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    #[must_use]
    pub fn provider(mut self, p: Arc<dyn Provider>) -> Self {
        self.providers.push(p);
        self
    }

    /// Uses a [`ScriptedChecker`] instead of the real one, so no provider is involved.
    #[must_use]
    pub fn scripted(mut self, c: Arc<ScriptedChecker>) -> Self {
        self.scripted = Some(c);
        self
    }

    /// Pins the jitter sample. `0.5` is "no offset" for the ±10 % window.
    #[must_use]
    pub fn jitter(mut self, sample: f64) -> Self {
        self.jitter = Some(sample);
        self
    }

    /// Uses the real [`aulos_subscriptions::RandJitter`] — for the tests whose subject *is* the
    /// spread.
    #[must_use]
    pub fn rand_jitter(mut self) -> Self {
        self.jitter = None;
        self
    }

    /// Inserts subscription rows before the manager loads, for a restart test.
    #[must_use]
    pub fn seed(mut self, records: Vec<SubscriptionRecord>) -> Self {
        self.seed = records;
        self
    }

    /// Pre-marks media ids as seen for one subscription.
    #[must_use]
    pub fn seen(mut self, sub: SubId, ids: Vec<Box<str>>) -> Self {
        self.seen.push((sub, ids));
        self
    }

    /// Reuses an existing directory, so a "restart" keeps the same database file.
    #[must_use]
    pub fn reusing(mut self, dir: TempDir) -> Self {
        self.dir = Some(dir);
        self
    }

    pub async fn build(self) -> Harness {
        let dir = match self.dir {
            Some(d) => d,
            None => {
                let d = tempfile::tempdir().unwrap();
                for sub in ["downloads", "audio", "temp", "state"] {
                    std::fs::create_dir_all(d.path().join(sub)).unwrap();
                }
                d
            }
        };
        let root = dir.path().to_path_buf();
        let p = |sub: &str| root.join(sub).to_string_lossy().into_owned();

        let mut env: Vec<(String, String)> = vec![
            ("STATE_DIR".into(), p("state")),
            ("DOWNLOAD_DIR".into(), p("downloads")),
            ("AUDIO_DOWNLOAD_DIR".into(), p("audio")),
            ("TEMP_DIR".into(), p("temp")),
            ("AULOS_DB_PATH".into(), p("state/aulos.db")),
            ("AULOS_DB_FLUSH_MS".into(), "1".into()),
        ];
        env.extend(self.env);
        let cfg = Arc::new(load(&RawEnv::from_pairs(env)).expect("the harness config must load"));

        let store = Store::open(
            StoreOptions::from_config(&cfg)
                .with_flush_ms(1)
                .with_readers(2)
                .with_busy_timeout_ms(500),
        )
        .unwrap();

        let mut ops = Vec::new();
        for record in self.seed {
            ops.push(aulos_store::WriteOp::UpsertSubscription(Box::new(record)));
        }
        for (sub, ids) in self.seen {
            ops.push(aulos_store::WriteOp::MarkSeen {
                sub,
                ids,
                at: EPOCH_MS,
            });
        }
        if !ops.is_empty() {
            store
                .write(ops, aulos_store::Durability::Sync)
                .await
                .unwrap();
        }

        let clock = Arc::new(TokioClock::default());
        let (mut router, sender) = EventRouter::new(4_096);
        let inbox = router.subscribe(SubscriberSpec::aggregator());
        let router_task = router.spawn();

        // The engine is only wired when the real `Checker` is in play; a scripted checker never
        // touches it, and spawning one anyway would make every scheduler test slower for nothing.
        let (checker, engine_handle, engine_task, pump): Wiring =
            if let Some(scripted) = self.scripted.clone() {
                (scripted, None, None, None)
            } else {
                let mut registry = Registry::new();
                let providers = if self.providers.is_empty() {
                    vec![Arc::new(FakeProvider::new()) as Arc<dyn Provider>]
                } else {
                    self.providers
                };
                for provider in providers {
                    registry.register(provider);
                }
                let registry = Arc::new(RwLock::new(registry));

                let (progress_tx, mut progress_rx) = mpsc::channel::<ProgressMsg>(1_024);
                let (engine, handle) = Engine::new(
                    store.clone(),
                    Arc::clone(&registry),
                    Arc::clone(&cfg),
                    Arc::new(ArcSwap::from_pointee(YtdlOptions::empty())),
                    Arc::clone(&clock) as Arc<dyn Clock>,
                    sender.clone(),
                    progress_tx,
                );
                let engine_task = engine.spawn();
                let pump = tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });
                let checker = Arc::new(Checker::new(
                    store.clone(),
                    registry,
                    Arc::clone(&cfg),
                    Arc::new(StaticOptions::default()),
                    handle.clone(),
                    Arc::clone(&clock) as Arc<dyn Clock>,
                ));
                (checker, Some(handle), Some(engine_task), Some(pump))
            };

        let (handle, rx) = SubscriptionsHandle::channel(64);
        let deps = SubDeps::new(
            store.clone(),
            Arc::clone(&cfg),
            Arc::clone(&clock) as Arc<dyn Clock>,
            sender.clone(),
            checker,
        )
        .with_jitter(match self.jitter {
            Some(sample) => Arc::new(FixedJitter(sample)) as Arc<dyn aulos_subscriptions::Jitter>,
            None => Arc::new(aulos_subscriptions::RandJitter),
        });
        let mut manager = Manager::new(deps, rx);
        manager.load().await.expect("load");
        let manager_task = manager.spawn();

        Harness {
            dir,
            cfg,
            store,
            clock,
            handle,
            events: Events::spawn(inbox),
            engine: engine_handle,
            scripted: self.scripted,
            _manager: manager_task,
            _router: router_task,
            _engine: engine_task,
            _pump: pump,
            _sender: sender,
        }
    }
}

impl Harness {
    pub async fn new() -> Self {
        Self::builder().build().await
    }

    #[must_use]
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder::default()
    }

    /// `SubCmd::Add` with the harness defaults.
    ///
    /// `url` must parse: [`SubCmd::Add`] carries a typed `Url`, so the API layer is what rejects
    /// a missing or malformed one (the wave-2 note in `docs/INTEGRATION-NOTES.md`).
    pub async fn subscribe(
        &self,
        url: &str,
    ) -> Result<SubscriptionView, aulos_core::subscription::SubError> {
        let (ack, reply) = oneshot::channel();
        self.handle
            .send(SubCmd::Add {
                request: Box::new(aulos_core::request::DownloadRequest::new(
                    url::Url::parse(url).expect("the harness subscribes to a parseable url"),
                    selection(),
                )),
                check_interval_minutes: None,
                ack,
            })
            .await
            .unwrap();
        reply.await.unwrap().map(|v| *v)
    }

    /// `SubCmd::Add` with a caller-supplied template and interval.
    pub async fn subscribe_with(
        &self,
        request: aulos_core::request::DownloadRequest,
        check_interval_minutes: Option<u32>,
    ) -> Result<SubscriptionView, aulos_core::subscription::SubError> {
        let (ack, reply) = oneshot::channel();
        self.handle
            .send(SubCmd::Add {
                request: Box::new(request),
                check_interval_minutes,
                ack,
            })
            .await
            .unwrap();
        reply.await.unwrap().map(|v| *v)
    }

    /// `SubCmd::List`.
    pub async fn list(&self) -> Vec<SubscriptionView> {
        let (ack, reply) = oneshot::channel();
        self.handle.send(SubCmd::List { ack }).await.unwrap();
        reply.await.unwrap().unwrap()
    }

    /// `SubCmd::Update`.
    pub async fn update(
        &self,
        id: &SubId,
        changes: aulos_core::subscription::SubChanges,
    ) -> Result<SubscriptionView, aulos_core::subscription::SubError> {
        let (ack, reply) = oneshot::channel();
        self.handle
            .send(SubCmd::Update {
                id: id.clone(),
                changes: Box::new(changes),
                ack,
            })
            .await
            .unwrap();
        reply.await.unwrap().map(|v| *v)
    }

    /// `SubCmd::Delete`.
    pub async fn delete(&self, ids: Vec<SubId>) -> Vec<SubId> {
        self.try_delete(ids).await.unwrap()
    }

    /// `SubCmd::Delete`, keeping the error — what a test that makes the store fail needs.
    pub async fn try_delete(
        &self,
        ids: Vec<SubId>,
    ) -> Result<Vec<SubId>, aulos_core::subscription::SubError> {
        let (ack, reply) = oneshot::channel();
        self.handle.send(SubCmd::Delete { ids, ack }).await.unwrap();
        reply.await.unwrap()
    }

    /// `SubCmd::Check`.
    pub async fn check(&self, ids: Vec<SubId>) -> aulos_core::subscription::CheckJob {
        let (ack, reply) = oneshot::channel();
        self.handle.send(SubCmd::Check { ids, ack }).await.unwrap();
        reply.await.unwrap().unwrap()
    }

    /// `SubCmd::Health`.
    pub async fn health(&self) -> aulos_core::subscription::SubsHealth {
        let (ack, reply) = oneshot::channel();
        self.handle.send(SubCmd::Health { ack }).await.unwrap();
        reply.await.unwrap()
    }

    /// The persisted record.
    pub async fn record(&self, id: &SubId) -> SubscriptionRecord {
        self.store.subscription(id).await.unwrap().expect("the row")
    }

    /// The persisted seen set.
    pub async fn seen(&self, id: &SubId) -> std::collections::HashSet<Box<str>> {
        self.store.seen(id).await.unwrap()
    }

    /// Every queue row.
    pub async fn items(&self) -> Vec<aulos_core::Item> {
        self.store
            .items(aulos_store::ItemFilter::default())
            .await
            .unwrap()
            .rows
    }

    /// Lets the manager drain everything queued behind the commands sent so far. A `Health`
    /// round-trip is enough: the channel is FIFO.
    pub async fn settle(&self) {
        for _ in 0..12 {
            let _ = self.health().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Waits until `pred` holds over the persisted record, or panics.
    pub async fn until(
        &self,
        id: &SubId,
        what: &str,
        pred: impl Fn(&SubscriptionRecord) -> bool,
    ) -> SubscriptionRecord {
        for _ in 0..2_000 {
            if let Some(r) = self.store.subscription(id).await.unwrap()
                && pred(&r)
            {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let found = self.store.subscription(id).await.unwrap();
        panic!("timed out waiting for {what}; the row is {found:?}");
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

    pub fn all(&self) -> Vec<Arc<DomainEvent>> {
        self.seen.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// Every `SubscriptionChanged` view, in order.
    pub fn changed(&self) -> Vec<Arc<SubscriptionView>> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::SubscriptionChanged(v) => Some(Arc::clone(v)),
                _ => None,
            })
            .collect()
    }

    /// Every `SubscriptionRemoved` id, in order.
    pub fn removed(&self) -> Vec<SubId> {
        self.all()
            .iter()
            .filter_map(|e| match &**e {
                DomainEvent::SubscriptionRemoved(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }
}

/// `video/auto/any/best`.
#[must_use]
pub fn selection() -> aulos_core::Selection {
    use aulos_core::{Codec, DownloadType, FormatId, QualityId, Selection};
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("any").unwrap(),
        QualityId::parse("best").unwrap(),
    )
}

/// A record, ready to be seeded.
#[must_use]
pub fn record(id: &str, url: &str) -> SubscriptionRecord {
    SubscriptionRecord::new(
        SubId::parse(id).unwrap(),
        "Seeded",
        Url::parse(url).unwrap(),
        selection(),
    )
}

// ---------------------------------------------------------------------------
// A provider a test writes the feed for.
// ---------------------------------------------------------------------------

/// The host [`ScriptProvider`] claims. Anything else is `Match::No`, which is how a test makes one
/// entry fail the engine's `Unsupported resource` validation while the rest of the batch succeeds.
pub const GOOD_HOST: &str = "good.test";

/// A provider whose resolve result a test sets and changes between checks.
///
/// [`FakeProvider`] is scripted from TOML and its `expand_playlist` step always produces the same
/// synthetic children, which cannot express "the channel gained one video" or "this entry is live
/// now" — both of which the DESIGN §14.3 parity rules turn on.
pub struct ScriptProvider {
    id: aulos_provider::ProviderId,
    catalog: Arc<aulos_core::catalog::FormatCatalog>,
    feeds: Mutex<HashMap<String, Vec<aulos_provider::MediaEntry>>>,
    delay: Mutex<Option<Duration>>,
    resolves: AtomicUsize,
}

impl ScriptProvider {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            id: aulos_provider::ProviderId::parse("script").unwrap(),
            catalog: Arc::new(aulos_core::catalog::ytdlp_catalog()),
            feeds: Mutex::new(HashMap::new()),
            delay: Mutex::new(None),
            resolves: AtomicUsize::new(0),
        })
    }

    /// Declares `url` a container listing `entries`. Replaces any previous listing.
    pub fn set_feed(&self, url: &str, entries: Vec<aulos_provider::MediaEntry>) {
        self.feeds.lock().unwrap().insert(url.to_owned(), entries);
    }

    /// Makes every resolve take `d`, so a race is deterministic.
    pub fn slow(&self, d: Duration) {
        *self.delay.lock().unwrap() = Some(d);
    }

    /// How many times `resolve` has been called.
    #[must_use]
    pub fn resolve_count(&self) -> usize {
        self.resolves.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for ScriptProvider {
    fn id(&self) -> aulos_provider::ProviderId {
        self.id.clone()
    }

    fn matches(&self, url: &Url) -> aulos_provider::Match {
        let host = url.host_str().unwrap_or_default().to_lowercase();
        if host == GOOD_HOST || host.ends_with(&format!(".{GOOD_HOST}")) {
            aulos_provider::Match::Strong(aulos_provider::SCORE_HOST_SUFFIX)
        } else {
            aulos_provider::Match::No
        }
    }

    fn catalog(&self) -> Arc<aulos_core::catalog::FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        url: &Url,
        _ctx: aulos_provider::ResolveCtx<'_>,
    ) -> Result<Vec<aulos_provider::MediaEntry>, aulos_provider::ProviderError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let delay = *self.delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let listed = self.feeds.lock().unwrap().get(url.as_str()).cloned();
        match listed {
            Some(entries) => {
                // Mirror `aulos-provider-ytdlp`: a container is ONE entry whose kind nests the
                // children (DESIGN §6.1), not a flat vector.
                let mut parent =
                    aulos_provider::MediaEntry::video("feed", "Scripted Feed", url.clone());
                parent.kind = aulos_provider::EntryKind::Playlist {
                    title: "Scripted Feed".into(),
                    entries,
                };
                Ok(vec![parent])
            }
            None => Ok(vec![aulos_provider::MediaEntry::video(
                "single",
                "A single video",
                url.clone(),
            )]),
        }
    }

    async fn download(
        &self,
        _ctx: aulos_provider::DownloadCtx<'_>,
        _sink: aulos_provider::ProgressSink,
    ) -> Result<aulos_provider::Outcome, aulos_provider::ProviderError> {
        Ok(aulos_provider::Outcome::default())
    }
}

/// A child entry on [`GOOD_HOST`].
#[must_use]
pub fn entry(id: &str) -> aulos_provider::MediaEntry {
    aulos_provider::MediaEntry::video(
        id,
        format!("Video {id}"),
        Url::parse(&format!("https://{GOOD_HOST}/watch/{id}")).unwrap(),
    )
}

/// A child entry the engine will reject: nothing in the registry claims its host, so `Add` answers
/// `Unsupported resource "…"` for exactly that index.
#[must_use]
pub fn unqueueable(id: &str) -> aulos_provider::MediaEntry {
    aulos_provider::MediaEntry::video(
        id,
        format!("Video {id}"),
        Url::parse(&format!("https://rejected.test/watch/{id}")).unwrap(),
    )
}

/// The feed URL a test subscribes to.
#[must_use]
pub fn feed_url(name: &str) -> String {
    format!("https://{GOOD_HOST}/@{name}")
}

/// Lets every spawned task and both of the store's thread pools make progress **without**
/// advancing virtual time.
///
/// The real sleep is the point: under `tokio::time::pause()` a `tokio::time::sleep` is
/// auto-advanced and consumes no wall-clock time at all, so the store's writer and reader OS
/// threads never get a chance to answer. Alternating `yield_now` with a short real sleep gives the
/// runtime and those threads a turn each while the virtual clock stands still.
pub async fn spin() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// [`spin`], but stops as soon as `pred` holds. Panics if it never does.
pub async fn spin_until(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..4_000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_micros(200));
    }
    panic!("timed out waiting for {what}");
}

/// A seeded record whose schedule is **parked** (`enabled = false`).
///
/// Most scheduler tests want this. Paused `tokio` time auto-advances to the nearest timer whenever
/// the runtime idles, so an enabled subscription's own timer fires again and again while a test is
/// merely giving the store's threads a turn. A parked task waits on a `watch` channel and arms no
/// timer, and `SubCmd::Check` runs it anyway — legacy's `check_now(ids)` did too — so a test can
/// drive exactly as many checks as it means to.
#[must_use]
pub fn parked(id: &str, url: &str) -> SubscriptionRecord {
    let mut r = record(id, url);
    r.enabled = false;
    r
}
