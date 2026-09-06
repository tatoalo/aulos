//! The shared harness for the Telegram suite.
//!
//! **There is no token anywhere in here.** The actor is built with
//! [`aulos_telegram::TelegramActor::with_transport`] over a [`MockTransport`], so every command
//! text, every callback text and the whole rate-limit ladder is asserted against recorded calls.
//! Starting a real bot from a dev machine would also long-poll against the live bot and get an
//! HTTP 409 (BRIEF, "Testing against the user's VPS").
//!
//! The actor is driven **by hand** — [`Harness::handle`], [`Harness::observe`],
//! [`Harness::tick`] — rather than spawned, so a test controls exactly when the 1 Hz pass runs.
//! [`Harness::spawn`] exists for the one test that proves the loop is wired.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use aulos_core::catalog::{FormatCatalog, ytdlp_catalog};
use aulos_core::config::{RawEnv, load};
use aulos_core::event::{DomainEvent, EventInbox, EventRouter, EventSender, SubscriberSpec};
use aulos_core::item::{Item, ItemView, Kind, ViewExtras};
use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::status::Status;
use aulos_core::{Clock, Config, FakeClock, ItemId, YtdlOptions};
use aulos_provider::{ProgressMsg, Provider, Registry};
use aulos_queue::{Engine, EngineHandle};
use aulos_store::{Store, StoreOptions};
use aulos_telegram::{MockTransport, TelegramActor, TelegramConfig};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The chat every test talks from unless it says otherwise.
pub const CHAT: i64 = 4_242;
/// A second allowed chat, for the fan-out tests.
pub const OTHER_CHAT: i64 = -100_777;

/// A driven actor plus everything a test needs to observe it.
pub struct Harness {
    pub dir: TempDir,
    pub cfg: Arc<Config>,
    pub store: Store,
    pub engine: EngineHandle,
    pub transport: Arc<MockTransport>,
    pub clock: Arc<FakeClock>,
    pub catalog: Arc<FormatCatalog>,
    pub actor: TelegramActor,
    pub events: EventSender,
    pub inbox: Option<EventInbox>,
    _engine: JoinHandle<()>,
    _router: JoinHandle<()>,
    _pump: JoinHandle<()>,
}

/// How a harness is put together.
pub struct HarnessBuilder {
    env: Vec<(String, String)>,
    tg: Option<TelegramConfig>,
    allowed: Vec<i64>,
    parked: bool,
}

impl Default for HarnessBuilder {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            tg: None,
            allowed: vec![CHAT, OTHER_CHAT],
            parked: false,
        }
    }
}

impl HarnessBuilder {
    #[must_use]
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Replaces the whole bot config.
    #[must_use]
    pub fn telegram(mut self, tg: TelegramConfig) -> Self {
        self.tg = Some(tg);
        self
    }

    /// Sets the allow-list.
    #[must_use]
    pub fn allowed(mut self, ids: Vec<i64>) -> Self {
        self.allowed = ids;
        self
    }

    /// Downloads that never finish, so an added item stays **live**.
    ///
    /// The dedupe index only holds non-terminal rows (DESIGN §8.5), and `EchoProvider` otherwise
    /// completes a download before the next statement runs — which would make any test about
    /// re-adding a queued URL a race against the engine's own tasks.
    #[must_use]
    pub fn parked_downloads(mut self) -> Self {
        self.parked = true;
        self
    }

    pub async fn build(self) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["downloads", "audio", "temp", "state"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
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

        let mut registry = Registry::new();
        // The real `ytdlp` catalog, so an `audio`/`m4a` selection validates. `FakeProvider`'s own
        // catalog is a two-format stub, which would reject anything the `cfg:` grammar can reach
        // beyond `any`/`best`.
        let echo = if self.parked {
            EchoProvider::parked()
        } else {
            EchoProvider::new()
        };
        registry.register(Arc::new(echo) as Arc<dyn Provider>);
        let registry = Arc::new(RwLock::new(registry));

        let clock = Arc::new(FakeClock::default());
        let (mut router, events) = EventRouter::new(4_096);
        let inbox = router.subscribe(SubscriberSpec::telegram());
        let router_task = router.spawn();

        let (progress_tx, mut progress_rx) = mpsc::channel::<ProgressMsg>(1_024);
        let (engine, engine_handle) = Engine::new(
            store.clone(),
            registry,
            Arc::clone(&cfg),
            Arc::new(ArcSwap::from_pointee(YtdlOptions::empty())),
            Arc::clone(&clock) as Arc<dyn Clock>,
            events.clone(),
            progress_tx,
        );
        let engine_task = engine.spawn();
        let pump = tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });

        let tg = Arc::new(self.tg.unwrap_or_else(|| TelegramConfig {
            default_chapter_template: cfg.default_chapter_template().to_owned(),
            ..TelegramConfig::for_test(self.allowed)
        }));
        let transport = MockTransport::new();
        let catalog = Arc::new(ytdlp_catalog());
        let mut actor = TelegramActor::with_transport(
            Arc::clone(&tg),
            store.clone(),
            engine_handle.clone(),
            Arc::clone(&catalog),
            Arc::clone(&clock) as Arc<dyn Clock>,
            Arc::clone(&transport) as Arc<dyn aulos_telegram::Transport>,
        )
        .expect("the actor must build");
        actor.load().await.expect("load");

        Harness {
            dir,
            cfg,
            store,
            engine: engine_handle,
            transport,
            clock,
            catalog,
            actor,
            events,
            inbox: Some(inbox),
            _engine: engine_task,
            _router: router_task,
            _pump: pump,
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

    /// Feeds one update.
    pub async fn handle(&mut self, update: aulos_telegram::Incoming) {
        self.actor.handle(update).await;
    }

    /// Feeds one domain event.
    pub async fn observe(&mut self, event: &DomainEvent) {
        self.actor.observe(event).await;
    }

    /// Runs the 1 Hz pass now.
    pub async fn tick(&mut self) {
        self.actor.tick_now().await;
    }

    /// Advances the fake clock and runs a tick.
    pub async fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.tick().await;
    }

    /// Spawns the real loop, returning the sender the polling adapter would use.
    pub fn spawn(self) -> (mpsc::Sender<aulos_telegram::Incoming>, JoinHandle<()>) {
        let tx = self.actor.incoming();
        let inbox = self.inbox;
        let actor = self.actor;
        let handle = actor.spawn(inbox.expect("the inbox is taken once"));
        (tx, handle)
    }

    /// Every persisted queue row.
    pub async fn items(&self) -> Vec<Item> {
        self.store
            .items(aulos_store::ItemFilter::default())
            .await
            .unwrap()
            .rows
    }

    /// The stored chat configs.
    pub async fn chat_configs(&self) -> std::collections::HashMap<i64, aulos_core::ChatConfig> {
        self.store.telegram_chats().await.unwrap()
    }

    /// Waits until at least `n` queue rows exist.
    pub async fn wait_for_items(&self, n: usize) -> Vec<Item> {
        for _ in 0..2_000 {
            let rows = self.items().await;
            if rows.len() >= n {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("timed out waiting for {n} queued item(s)");
    }
}

/// `video/auto/any/best`.
#[must_use]
pub fn selection() -> Selection {
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("any").unwrap(),
        QualityId::parse("best").unwrap(),
    )
}

/// A synthetic [`ItemView`], so an event test needs no engine.
#[must_use]
pub fn view(id: ItemId, title: &str, status: Status, source: SourceRef) -> ItemView {
    let request = aulos_core::request::DownloadRequest::new(
        url::Url::parse("https://a.test/watch/1").unwrap(),
        selection(),
    );
    let item = Item {
        id,
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord: 1,
        url: request.url.clone(),
        canonical_key: "k".into(),
        provider: None,
        media_id: None,
        title: title.into(),
        status,
        auto_start: true,
        msg: None,
        error: None,
        request,
        entry: None,
        filename: None,
        size: None,
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 0,
        started_at: None,
        finished_at: None,
        attempt: 0,
        source,
        children_total: None,
        clear_after: None,
    };
    ItemView::from_item(&item, None, &ViewExtras::default())
}

/// The same, attributed to `chat`.
#[must_use]
pub fn tg_view(id: ItemId, title: &str, status: Status, chat: i64) -> ItemView {
    view(
        id,
        title,
        status,
        SourceRef::with_ref(SourceKind::Telegram, chat.to_string()),
    )
}

/// An `Added` event for one view.
#[must_use]
pub fn added(view: &ItemView) -> DomainEvent {
    added_batch(
        std::slice::from_ref(view),
        aulos_core::event::AddReason::Created,
    )
}

/// An `Added` event for a whole batch, under the reason its producer would have used.
///
/// Boot recovery publishes one event carrying the entire working set under
/// `AddReason::Recovered` (DESIGN §8.9 step 7), which is a shape no `added()` call can express.
#[must_use]
pub fn added_batch(views: &[ItemView], reason: aulos_core::event::AddReason) -> DomainEvent {
    DomainEvent::Added(views.iter().map(|v| Arc::new(v.clone())).collect(), reason)
}

/// A `StatusChanged` event for one view.
#[must_use]
pub fn changed(view: &ItemView, from: Status) -> DomainEvent {
    DomainEvent::StatusChanged {
        id: view.id,
        from,
        to: view.status,
        view: Arc::new(view.clone()),
    }
}

/// A `Completed` event for one view.
#[must_use]
pub fn completed(view: &ItemView) -> DomainEvent {
    DomainEvent::Completed(Arc::new(view.clone()))
}

/// A provider that resolves every URL to itself and finishes instantly, over the **real** `ytdlp`
/// catalog so every selection the `cfg:` grammar can produce validates.
pub struct EchoProvider {
    id: aulos_provider::ProviderId,
    catalog: Arc<FormatCatalog>,
    /// Never return from `download`, so the item never reaches a terminal status.
    parked: bool,
}

impl EchoProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: aulos_provider::ProviderId::parse("echo").unwrap(),
            catalog: Arc::new(ytdlp_catalog()),
            parked: false,
        }
    }

    /// The same provider, but its downloads never finish. See
    /// [`HarnessBuilder::parked_downloads`].
    #[must_use]
    pub fn parked() -> Self {
        Self {
            parked: true,
            ..Self::new()
        }
    }
}

#[async_trait::async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> aulos_provider::ProviderId {
        self.id.clone()
    }

    fn matches(&self, _url: &url::Url) -> aulos_provider::Match {
        aulos_provider::Match::Strong(aulos_provider::SCORE_HOST_SUFFIX)
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        url: &url::Url,
        _ctx: aulos_provider::ResolveCtx<'_>,
    ) -> Result<Vec<aulos_provider::MediaEntry>, aulos_provider::ProviderError> {
        Ok(vec![aulos_provider::MediaEntry::video(
            "echo",
            url.path(),
            url.clone(),
        )])
    }

    async fn download(
        &self,
        _ctx: aulos_provider::DownloadCtx<'_>,
        _sink: aulos_provider::ProgressSink,
    ) -> Result<aulos_provider::Outcome, aulos_provider::ProviderError> {
        if self.parked {
            // `pending` rather than a long sleep: it never touches the timer, so a
            // `start_paused` runtime does not auto-advance around it.
            std::future::pending::<()>().await;
        }
        Ok(aulos_provider::Outcome::default())
    }
}
