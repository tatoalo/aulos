//! The actor: one task, one `select!`, a 1 Hz tick (DESIGN §12.1).
//!
//! Everything the bot does is a message. Handlers never block: a `/config` press is a store write
//! and a transport call, a text message is one `EngineCmd::Add`, and the board is redrawn by the
//! tick rather than by whatever event happened to arrive. Legacy polled at 15 s and edited nothing;
//! this ticks at 1 Hz and lets the limiter decide what actually goes out.
//!
//! ```text
//!   long polling ──Incoming──►┌─────────────────────────────────┐──Transport──► Bot API
//!   EventRouter ──EventInbox─►│ TelegramActor: chats, boards,   │──EngineCmd──► aulos-queue
//!   1 Hz tick ───────────────►│ watches, limiter. No Mutex.     │──WriteOp────► aulos-store
//!                             └─────────────────────────────────┘
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use aulos_core::catalog::{BotFormat, FormatCatalog};
use aulos_core::clock::Clock;
use aulos_core::config::{Config, TelegramBoard};
use aulos_core::event::{DomainEvent, EventInbox};
use aulos_core::id::ItemId;
use aulos_core::item::{ItemView, Kind};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::status::Status;
use aulos_core::telegram::ChatConfig;
use aulos_store::{Durability, Store, WriteOp};
use indexmap::IndexMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::commands::{Command, Incoming, plan_message};
use crate::config_ui::{self, Keyboard, Screen};
use crate::limiter::{Limiter, NETWORK_ATTEMPTS};
use crate::render::{self, JobLine, Mark, notify};
use crate::transport::{MessageId, MockTransport, TeloxideTransport, TgError, Transport};
use crate::watch::WatchRegistry;

/// The tick period. Legacy polled its watchdogs every 15 s; the board needs finer resolution and
/// the limiter is what keeps the API calls inside budget.
pub const TICK: Duration = Duration::from_secs(1);

/// How long a terminal line lingers on the board before it drops (DESIGN §12.4).
pub const LINGER: Duration = Duration::from_secs(60);

/// How long after the last job ends the board is replaced by its summary (DESIGN §12.4).
pub const RETIRE_AFTER: Duration = Duration::from_secs(60);

/// The `Incoming` channel depth. A full queue means the actor is busy, and Telegram will redeliver.
pub const INCOMING_CAPACITY: usize = 256;

/// The knobs the bot runs on (DESIGN §12.1).
#[derive(Clone, Debug)]
pub struct TelegramConfig {
    /// `TELEGRAM_BOT_ENABLED`.
    pub enabled: bool,
    /// `TELEGRAM_BOT_TOKEN`. Never logged.
    pub token: String,
    /// `TELEGRAM_ALLOWED_CHAT_IDS`.
    pub allowed_chat_ids: Vec<i64>,
    /// `TELEGRAM_STALL_TIMEOUT_SECONDS`.
    pub stall_timeout_seconds: u64,
    /// `TELEGRAM_HARD_TIMEOUT_SECONDS`.
    pub hard_timeout_seconds: u64,
    /// `TELEGRAM_MAX_URLS_PER_MESSAGE`.
    pub max_urls_per_message: u32,
    /// `AULOS_TELEGRAM_BOARD`.
    pub board: TelegramBoard,
    /// `AULOS_TELEGRAM_EDIT_INTERVAL_MS`.
    pub edit_interval_ms: u64,
    /// `AULOS_TELEGRAM_WATCH_ALL`. Default `true` (BRIEF).
    pub watch_all: bool,
    /// `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT`, for a chat's first-access defaults.
    pub default_playlist_item_limit: u32,
    /// `OUTPUT_TEMPLATE_CHAPTER`, for a chat's first-access defaults.
    pub default_chapter_template: String,
}

impl TelegramConfig {
    /// Reads the bot's knobs off the effective configuration.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.telegram_bot_enabled,
            token: cfg.telegram_bot_token.expose().clone(),
            allowed_chat_ids: cfg.telegram_allowed_chat_ids.clone(),
            stall_timeout_seconds: cfg.telegram_stall_timeout_seconds,
            hard_timeout_seconds: cfg.telegram_hard_timeout_seconds,
            max_urls_per_message: cfg.telegram_max_urls_per_message,
            board: cfg.telegram_board,
            edit_interval_ms: cfg.telegram_edit_interval_ms,
            watch_all: cfg.telegram_watch_all,
            default_playlist_item_limit: cfg.default_option_playlist_item_limit,
            default_chapter_template: cfg.default_chapter_template().to_owned(),
        }
    }

    /// A config for a test: enabled, one chat, the legacy timeouts.
    #[must_use]
    pub fn for_test(allowed: Vec<i64>) -> Self {
        Self {
            enabled: true,
            token: "0:test".to_owned(),
            allowed_chat_ids: allowed,
            stall_timeout_seconds: 180,
            hard_timeout_seconds: 7_200,
            max_urls_per_message: 10,
            board: TelegramBoard::Board,
            edit_interval_ms: 3_000,
            watch_all: true,
            default_playlist_item_limit: 0,
            default_chapter_template: String::new(),
        }
    }

    /// Whether `chat` may talk to the bot.
    #[must_use]
    pub fn is_allowed(&self, chat: i64) -> bool {
        self.allowed_chat_ids.contains(&chat)
    }

    /// The defaults a chat gets on first access — the legacy twelve keys verbatim.
    #[must_use]
    pub fn chat_defaults(&self) -> ChatConfig {
        ChatConfig::legacy_defaults(
            self.default_playlist_item_limit,
            &self.default_chapter_template,
        )
    }
}

/// Why the bot did not start (DESIGN §12.1). Each one is a single log line and a silent no-op,
/// exactly as legacy.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum TgInitError {
    /// `TELEGRAM_BOT_ENABLED=false`. Not a failure; the caller logs it at INFO.
    #[error("Telegram bot disabled")]
    Disabled,
    /// Enabled with no token.
    #[error("Telegram bot enabled but TELEGRAM_BOT_TOKEN is missing")]
    MissingToken,
    /// Enabled with an empty allow-list. Legacy refused to start too, and it is the right call: a
    /// bot with no allow-list answers anyone who finds it.
    #[error("Telegram bot enabled but TELEGRAM_ALLOWED_CHAT_IDS is empty")]
    NoAllowedChats,
}

/// What `healthz.components.telegram` reports (DESIGN §16.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TelegramHealth {
    /// Whether the actor is running.
    pub enabled: bool,
    /// How many chats have stored defaults.
    pub chats: usize,
    /// How many live boards there are.
    pub boards: usize,
    /// How many jobs are being reported on.
    pub watched_jobs: usize,
    /// `edits_throttled_total` — over-budget behaviour, made observable (DESIGN §12.4).
    pub edits_throttled_total: u64,
}

/// [`TelegramActor::health`] after the actor has been consumed by [`TelegramActor::spawn`].
///
/// [`TelegramActor::health`] needs `&self` and `spawn` takes the actor by value, so without this
/// `healthz.components.telegram` could only ever report the values the actor had at boot —
/// `edits_throttled_total` in particular would stop at `0` forever. The actor refreshes the cell
/// on every pass of its 1 Hz loop, so a reader of this handle is at most one tick behind.
#[derive(Clone, Debug)]
pub struct TelegramHealthHandle {
    cell: Arc<HealthCell>,
}

impl TelegramHealthHandle {
    /// The actor's last published health (DESIGN §16.3).
    #[must_use]
    pub fn health(&self) -> TelegramHealth {
        self.cell.load()
    }
}

/// The shared cell behind [`TelegramHealthHandle`]. Atomics rather than a lock: the writer is the
/// actor's own loop and the reader is the health publisher, and neither may ever wait on the other.
#[derive(Debug, Default)]
struct HealthCell {
    enabled: AtomicBool,
    chats: AtomicUsize,
    boards: AtomicUsize,
    watched_jobs: AtomicUsize,
    edits_throttled_total: AtomicU64,
}

impl HealthCell {
    fn store(&self, h: TelegramHealth) {
        self.enabled.store(h.enabled, Ordering::Relaxed);
        self.chats.store(h.chats, Ordering::Relaxed);
        self.boards.store(h.boards, Ordering::Relaxed);
        self.watched_jobs.store(h.watched_jobs, Ordering::Relaxed);
        self.edits_throttled_total
            .store(h.edits_throttled_total, Ordering::Relaxed);
    }

    fn load(&self) -> TelegramHealth {
        TelegramHealth {
            enabled: self.enabled.load(Ordering::Relaxed),
            chats: self.chats.load(Ordering::Relaxed),
            boards: self.boards.load(Ordering::Relaxed),
            watched_jobs: self.watched_jobs.load(Ordering::Relaxed),
            edits_throttled_total: self.edits_throttled_total.load(Ordering::Relaxed),
        }
    }
}

/// One job's row plus when it should leave the board.
#[derive(Clone, Debug)]
struct BoardEntry {
    line: JobLine,
    drop_at: Option<Instant>,
}

/// One chat's live board (DESIGN §12.1).
#[derive(Debug, Default)]
struct ChatBoard {
    message: Option<MessageId>,
    jobs: IndexMap<ItemId, BoardEntry>,
    last_rendered: String,
    dirty: bool,
    /// When the last job ended, or `None` while any is still running.
    idle_since: Option<Instant>,
    finished: usize,
    failed: usize,
    canceled: usize,
}

impl ChatBoard {
    fn lines(&self) -> Vec<JobLine> {
        self.jobs.values().map(|e| e.line.clone()).collect()
    }

    fn any_active(&self) -> bool {
        self.jobs.values().any(|e| !e.line.status.is_terminal())
    }
}

/// The Telegram actor (DESIGN §12.1).
pub struct TelegramActor {
    cfg: Arc<TelegramConfig>,
    store: Store,
    engine: aulos_queue::EngineHandle,
    formats: Vec<BotFormat>,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn Transport>,
    chats: HashMap<i64, ChatConfig>,
    boards: HashMap<i64, ChatBoard>,
    watches: WatchRegistry,
    limiter: Limiter,
    incoming_tx: mpsc::Sender<Incoming>,
    incoming_rx: mpsc::Receiver<Incoming>,
    /// The bot `new` built its own transport around, kept so the long-polling loop can have one.
    /// `None` when the actor was assembled over a supplied transport (every test, and any future
    /// non-`teloxide` transport).
    bot: Option<teloxide::Bot>,
    health: Arc<HealthCell>,
}

impl std::fmt::Debug for TelegramActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramActor")
            .field("chats", &self.chats.len())
            .field("boards", &self.boards.len())
            .field("watched", &self.watches.len())
            .finish_non_exhaustive()
    }
}

impl TelegramActor {
    /// Builds the actor, applying the DESIGN §12.1 startup gating.
    ///
    /// # Errors
    /// [`TgInitError::Disabled`] when `TELEGRAM_BOT_ENABLED` is false, and the two configuration
    /// failures. The caller logs one line and carries on: a missing token must not stop the server.
    // `catalog` is taken by `Arc` because that is the DESIGN §12.1 signature and because
    // `aulos-server` holds it as one; only the nine-entry projection is kept.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        cfg: Arc<TelegramConfig>,
        store: Store,
        engine: aulos_queue::EngineHandle,
        catalog: Arc<FormatCatalog>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, TgInitError> {
        if !cfg.enabled {
            return Err(TgInitError::Disabled);
        }
        if cfg.token.trim().is_empty() {
            return Err(TgInitError::MissingToken);
        }
        if cfg.allowed_chat_ids.is_empty() {
            return Err(TgInitError::NoAllowedChats);
        }
        let bot = teloxide::Bot::new(&cfg.token);
        let transport = Arc::new(TeloxideTransport::new(bot.clone()));
        let mut actor = Self::assemble(cfg, store, engine, &catalog, clock, transport);
        actor.bot = Some(bot);
        Ok(actor)
    }

    /// Builds the actor over a supplied transport, skipping the token check.
    ///
    /// This is what the suite uses: WP-16's acceptance list requires the whole command, callback
    /// and rate-limit surface to be asserted against a mocked transport, never a real token.
    ///
    /// # Errors
    /// [`TgInitError::Disabled`] and [`TgInitError::NoAllowedChats`] still apply; the token is not
    /// consulted, because there is no bot to give it to.
    #[allow(clippy::needless_pass_by_value)] // the same `Arc<FormatCatalog>` signature as `new`
    pub fn with_transport(
        cfg: Arc<TelegramConfig>,
        store: Store,
        engine: aulos_queue::EngineHandle,
        catalog: Arc<FormatCatalog>,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, TgInitError> {
        if !cfg.enabled {
            return Err(TgInitError::Disabled);
        }
        if cfg.allowed_chat_ids.is_empty() {
            return Err(TgInitError::NoAllowedChats);
        }
        Ok(Self::assemble(
            cfg, store, engine, &catalog, clock, transport,
        ))
    }

    #[allow(clippy::needless_pass_by_value)] // every argument is stored on `Self`
    fn assemble(
        cfg: Arc<TelegramConfig>,
        store: Store,
        engine: aulos_queue::EngineHandle,
        catalog: &FormatCatalog,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CAPACITY);
        let watches = WatchRegistry::new(
            cfg.stall_timeout_seconds,
            cfg.hard_timeout_seconds,
            cfg.watch_all,
            cfg.allowed_chat_ids.clone(),
        );
        let limiter = Limiter::new(cfg.edit_interval_ms, Arc::clone(&clock));
        let me = Self {
            formats: catalog.bot_formats(),
            cfg,
            store,
            engine,
            clock,
            transport,
            chats: HashMap::new(),
            boards: HashMap::new(),
            watches,
            limiter,
            incoming_tx,
            incoming_rx,
            bot: None,
            health: Arc::new(HealthCell::default()),
        };
        me.publish_health();
        me
    }

    /// The sender the long-polling loop (or a test) pushes updates into.
    #[must_use]
    pub fn incoming(&self) -> mpsc::Sender<Incoming> {
        self.incoming_tx.clone()
    }

    /// The bot [`TelegramActor::new`] built, for [`crate::poll_updates`].
    ///
    /// `new` owns the token, so it is the only place a `teloxide::Bot` can be made without the
    /// caller reproducing the DESIGN §12.1 startup gating; the long-polling loop needs that same
    /// bot. `None` after [`TelegramActor::with_transport`], which has no token and no bot.
    #[must_use]
    pub fn bot(&self) -> Option<teloxide::Bot> {
        self.bot.clone()
    }

    /// A handle that reports [`Self::health`] after `self` has been consumed by [`Self::spawn`].
    ///
    /// Take it **before** `spawn`, the way `HookDispatcher::health_handle` is taken.
    #[must_use]
    pub fn health_handle(&self) -> TelegramHealthHandle {
        TelegramHealthHandle {
            cell: Arc::clone(&self.health),
        }
    }

    /// Refreshes what [`Self::health_handle`] reports. Called on every pass of the actor's loop.
    fn publish_health(&self) {
        self.health.store(self.health());
    }

    /// Loads the per-chat defaults out of `telegram_chats` (DESIGN §12.2, §7.6.5).
    ///
    /// # Errors
    /// [`aulos_store::StoreError`] when the table cannot be read.
    pub async fn load(&mut self) -> Result<usize, aulos_store::StoreError> {
        self.chats = self.store.telegram_chats().await?;
        tracing::info!(chats = self.chats.len(), "Telegram chat configs loaded");
        self.publish_health();
        Ok(self.chats.len())
    }

    /// `healthz.components.telegram`.
    #[must_use]
    pub fn health(&self) -> TelegramHealth {
        TelegramHealth {
            enabled: self.cfg.enabled,
            chats: self.chats.len(),
            boards: self.boards.len(),
            watched_jobs: self.watches.len(),
            edits_throttled_total: self.limiter.throttled_total(),
        }
    }

    /// The `select!` loop: updates, domain events and the 1 Hz tick.
    pub async fn run(mut self, mut events: EventInbox) {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::info!("Telegram bot started");
        loop {
            tokio::select! {
                update = self.incoming_rx.recv() => match update {
                    Some(update) => self.on_incoming(update).await,
                    None => break,
                },
                event = events.recv() => match event {
                    Some(event) => self.on_event(&event).await,
                    None => break,
                },
                _ = ticker.tick() => self.on_tick().await,
            }
            // One pass, one refresh: the health publisher reads a handle, not the actor.
            self.publish_health();
        }
        tracing::info!("Telegram bot stopped");
    }

    /// Spawns [`Self::run`].
    pub fn spawn(self, events: EventInbox) -> JoinHandle<()> {
        tokio::spawn(self.run(events))
    }

    /// Runs one tick now. The suite drives the board through this rather than waiting a second.
    pub async fn tick_now(&mut self) {
        self.on_tick().await;
    }

    /// Handles one update on the current task. Used by the suite.
    pub async fn handle(&mut self, update: Incoming) {
        self.on_incoming(update).await;
    }

    /// Handles one domain event on the current task. Used by the suite.
    pub async fn observe(&mut self, event: &DomainEvent) {
        self.on_event(event).await;
    }

    // -----------------------------------------------------------------------
    // updates
    // -----------------------------------------------------------------------

    async fn on_incoming(&mut self, update: Incoming) {
        let chat = update.chat();
        if !self.cfg.is_allowed(chat) {
            // Legacy: silently ignored, plus one WARN line including the chat id.
            tracing::warn!("Rejected Telegram update from unauthorized chat {chat}");
            return;
        }
        match update {
            Incoming::Command { command, .. } => self.on_command(chat, command).await,
            Incoming::Callback {
                message,
                query_id,
                data,
                ..
            } => self.on_callback(chat, message, &query_id, &data).await,
            Incoming::Text { text, .. } => self.on_text(chat, &text).await,
        }
    }

    async fn on_command(&mut self, chat: i64, command: Command) {
        match command {
            Command::Start => self.send(chat, config_ui::START_TEXT, None).await,
            Command::Config => {
                let cfg = self.chat_config(chat).await;
                let text = config_ui::config_text(&cfg);
                let keyboard = config_ui::main_keyboard(&cfg);
                self.send(chat, &text, Some(&keyboard)).await;
            }
        }
    }

    async fn on_callback(&mut self, chat: i64, message: MessageId, query_id: &str, data: &str) {
        // Legacy answered the query first, unconditionally, then edited.
        if let Err(e) = self.transport.answer_callback_query(query_id).await {
            tracing::debug!("answerCallbackQuery failed: {e}");
        }
        let mut cfg = self.chat_config(chat).await;
        let applied = config_ui::apply_callback(data, &mut cfg, &self.formats);
        if applied.changed {
            self.chats.insert(chat, cfg.clone());
            self.persist_chat(chat, &cfg).await;
        }
        let Some(screen) = applied.screen else {
            return;
        };
        let (text, keyboard) = self.screen(&cfg, &screen);
        if let Err(e) = self
            .transport
            .edit_message_text(chat, message, &text, keyboard.as_ref())
            .await
        {
            tracing::debug!("editMessageText failed: {e}");
        }
    }

    /// The text and keyboard of one `cfg:` screen.
    fn screen(&self, cfg: &ChatConfig, screen: &Screen) -> (String, Option<Keyboard>) {
        match *screen {
            Screen::Main => (
                config_ui::config_text(cfg),
                Some(config_ui::main_keyboard(cfg)),
            ),
            Screen::Format => (
                config_ui::SELECT_FORMAT.to_owned(),
                Some(config_ui::format_keyboard(&self.formats)),
            ),
            Screen::Quality => (
                config_ui::SELECT_QUALITY.to_owned(),
                Some(config_ui::quality_keyboard(&self.formats, &cfg.format)),
            ),
            Screen::Limit => (
                config_ui::SELECT_LIMIT.to_owned(),
                Some(config_ui::limit_keyboard()),
            ),
        }
    }

    /// DESIGN §12.3: one message becomes **one** `EngineCmd::Add`.
    async fn on_text(&mut self, chat: i64, text: &str) {
        let cfg = self.chat_config(chat).await;
        let plan = plan_message(text, &cfg, self.cfg.max_urls_per_message);
        if plan.is_silent() {
            return;
        }
        if let Some(msg) = &plan.too_many {
            self.send(chat, msg, None).await;
        }
        if let Some(msg) = &plan.ignored {
            self.send(chat, msg, None).await;
        }
        if plan.requests.is_empty() {
            return;
        }

        let count = plan.requests.len();
        let urls: Vec<String> = plan
            .requests
            .iter()
            .map(|r| r.url.as_str().to_owned())
            .collect();
        // Attribution by explicit `source`, not a contextvar: every job the bot creates is
        // attributable, playlist children included (they inherit the parent's `source`).
        let source = SourceRef::with_ref(SourceKind::Telegram, chat.to_string());
        match self.engine.add(plan.requests, source).await {
            Ok(outcome) => {
                let queued = count.saturating_sub(outcome.duplicates.len());
                self.send(chat, &notify::queued(queued.max(outcome.ids.len())), None)
                    .await;
            }
            Err(e) => {
                // The batch is all-or-nothing, so one bad URL fails the message. Report which.
                let index = match &e {
                    aulos_queue::AddError::Invalid { index, .. }
                    | aulos_queue::AddError::Duplicate { index, .. } => *index,
                    _ => 0,
                };
                let url = urls.get(index).cloned().unwrap_or_default();
                let text = notify::failures(&[(url, e.to_string())]);
                self.send(chat, &text, None).await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // events
    // -----------------------------------------------------------------------

    async fn on_event(&mut self, event: &DomainEvent) {
        match event {
            DomainEvent::Added(views, _) => {
                let now = self.clock.instant();
                for view in views {
                    let chats = self.watches.watch(view, now);
                    for chat in chats {
                        self.board_upsert(chat, view);
                    }
                }
            }
            DomainEvent::StatusChanged { id, view, .. } => {
                if let Some(watched) = self.watches.get(*id) {
                    let chats: Vec<i64> = watched.chats.iter().copied().collect();
                    let now = self.clock.instant();
                    // The two watchdogs of DESIGN §12.5 time the *download*: an item still waiting
                    // behind `MAX_CONCURRENT_DOWNLOADS` (or paused back into the queue) is parked,
                    // so neither fires on it.
                    if view.status.is_running() {
                        self.watches.touch(*id, now);
                    } else {
                        self.watches.park(*id, now);
                    }
                    self.watches.retitle(view);
                    for chat in chats {
                        self.board_upsert(chat, view);
                    }
                }
            }
            DomainEvent::Completed(view) => self.on_completed(view).await,
            DomainEvent::Removed { ids, .. } => {
                for id in ids {
                    self.watches.finish(*id);
                    for board in self.boards.values_mut() {
                        if board.jobs.shift_remove(id).is_some() {
                            board.dirty = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The two terminal messages of DESIGN §12.5. `canceled` is deliberately silent.
    async fn on_completed(&mut self, view: &ItemView) {
        let Some(watched) = self.watches.finish(view.id) else {
            return;
        };
        let now = self.clock.instant();
        let title = if view.title.trim().is_empty() {
            Arc::clone(&watched.title)
        } else {
            Arc::clone(&view.title)
        };
        let text = match view.status {
            Status::Finished => Some(notify::finished(&title, view.filename.as_deref())),
            Status::Error => {
                let message = view
                    .msg
                    .as_deref()
                    .filter(|m| !m.trim().is_empty())
                    .or_else(|| view.error.as_ref().map(|e| &*e.message));
                Some(notify::failed(&title, message))
            }
            // Parity: a cancellation is silent, the watch is simply dropped.
            _ => None,
        };

        for chat in &watched.chats {
            if let Some(text) = &text {
                self.send(*chat, text, None).await;
            }
            self.board_upsert(*chat, view);
            if let Some(board) = self.boards.get_mut(chat) {
                match view.status {
                    Status::Finished => board.finished += 1,
                    Status::Error => board.failed += 1,
                    _ => board.canceled += 1,
                }
                if let Some(entry) = board.jobs.get_mut(&view.id) {
                    entry.drop_at = Some(now + LINGER);
                }
                // The retirement clock starts when the last job ends, not when the last line
                // finally expires — otherwise the summary is two minutes late.
                if !board.any_active() && board.idle_since.is_none() {
                    board.idle_since = Some(now);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // the tick
    // -----------------------------------------------------------------------

    async fn on_tick(&mut self) {
        let now = self.clock.instant();

        // The two watchdogs. In board mode they are sent as separate messages — they are alerts,
        // not state — and the board line gains its marker (DESIGN §12.5).
        for warning in self.watches.due_warnings(now) {
            let text = match warning.kind {
                Mark::Stalled => notify::stalled(warning.secs, &warning.url),
                Mark::Timeout => notify::hard_timeout(warning.secs, &warning.url),
            };
            self.send(warning.chat, &text, None).await;
            if let Some(board) = self.boards.get_mut(&warning.chat)
                && let Some(entry) = board.jobs.get_mut(&warning.id)
            {
                entry.line.mark = Some(warning.kind);
                board.dirty = true;
            }
        }

        if self.cfg.board != TelegramBoard::Board {
            return;
        }

        // Expire lingering terminal lines, then decide which boards to redraw or retire.
        let mut retire: Vec<i64> = Vec::new();
        let chats: Vec<i64> = self.boards.keys().copied().collect();
        for chat in chats {
            let Some(board) = self.boards.get_mut(&chat) else {
                continue;
            };
            let before = board.jobs.len();
            board
                .jobs
                .retain(|_, e| e.drop_at.is_none_or(|at| at > now));
            if board.jobs.len() != before {
                board.dirty = true;
            }
            if board.any_active() {
                board.idle_since = None;
            } else if board.idle_since.is_none() {
                board.idle_since = Some(now);
            }
            if board
                .idle_since
                .is_some_and(|since| now.saturating_duration_since(since) >= RETIRE_AFTER)
            {
                retire.push(chat);
            }
        }
        for chat in retire {
            self.retire(chat).await;
        }

        let chats: Vec<i64> = self.boards.keys().copied().collect();
        for chat in chats {
            self.redraw(chat).await;
        }
    }

    /// Redraws one chat's board, if it changed and the budget allows.
    async fn redraw(&mut self, chat: i64) {
        let now_ms = self.clock.now_ms();
        let Some(board) = self.boards.get(&chat) else {
            return;
        };
        if !board.dirty || board.jobs.is_empty() {
            return;
        }
        let lines = board.lines();
        // An unchanged render issues **no** API call at all: Telegram rejects an unmodified edit
        // with a 400, and that 400 counts against the chat's rate budget (DESIGN §12.4). Only the
        // *body* is compared — the `updated HH:MM:SS` footer moves every tick, so comparing the
        // whole message would make every tick a change and there would be no guard at all.
        let body = render::render_body(&lines);
        if body == board.last_rendered {
            if let Some(board) = self.boards.get_mut(&chat) {
                board.dirty = false;
            }
            return;
        }
        let text = render::render_board(&lines, now_ms);
        if let Err(reason) = self.limiter.acquire(chat) {
            tracing::trace!("board edit for {chat} deferred: {reason:?}");
            return;
        }
        let message = board.message;
        match message {
            Some(id) => {
                if self.edit(chat, id, &text).await {
                    self.mark_rendered(chat, body);
                }
            }
            None => match self.transport.send_message(chat, &text, None).await {
                Ok(id) => {
                    self.limiter.record_sent(chat);
                    if let Some(board) = self.boards.get_mut(&chat) {
                        board.message = Some(id);
                        board.last_rendered = body;
                        board.dirty = false;
                    }
                }
                Err(e) => self.after_failure(chat, &e).await,
            },
        }
    }

    fn mark_rendered(&mut self, chat: i64, body: String) {
        if let Some(board) = self.boards.get_mut(&chat) {
            board.last_rendered = body;
            board.dirty = false;
        }
    }

    /// One edit, with the DESIGN §12.4 failure ladder.
    async fn edit(&mut self, chat: i64, message: MessageId, text: &str) -> bool {
        for attempt in 1..=NETWORK_ATTEMPTS {
            match self
                .transport
                .edit_message_text(chat, message, text, None)
                .await
            {
                Ok(()) => {
                    self.limiter.record_sent(chat);
                    return true;
                }
                Err(TgError::NotModified) => {
                    // Telegram is the authority on whether the message changed; treat it as sent.
                    self.limiter.record_sent(chat);
                    return true;
                }
                Err(e @ TgError::Network(_)) if attempt < NETWORK_ATTEMPTS => {
                    tracing::debug!("board edit attempt {attempt} failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
                }
                Err(e) => {
                    self.after_failure(chat, &e).await;
                    return false;
                }
            }
        }
        false
    }

    /// `429` escalates the chat's interval and sleeps; anything else drops **this** edit — the next
    /// tick carries newer data, and stale edits are never queued.
    async fn after_failure(&mut self, chat: i64, error: &TgError) {
        if let TgError::RetryAfter(d) = error {
            let sleep = self.limiter.record_retry_after(chat, *d);
            tracing::warn!("Telegram asked chat {chat} to wait {sleep:?}");
            tokio::time::sleep(sleep).await;
        } else {
            tracing::debug!("dropping a Telegram edit for {chat}: {error}");
        }
    }

    /// Replaces a finished board with its summary and forgets it.
    async fn retire(&mut self, chat: i64) {
        let Some(board) = self.boards.remove(&chat) else {
            return;
        };
        let text = render::retired_text(board.finished, board.failed, board.canceled);
        // Nothing was ever drawn (every job was too short to reach a tick) ⇒ say nothing.
        if let Some(id) = board.message
            && let Err(e) = self
                .transport
                .edit_message_text(chat, id, &text, None)
                .await
        {
            tracing::debug!("could not retire the board for {chat}: {e}");
        }
    }

    // -----------------------------------------------------------------------
    // plumbing
    // -----------------------------------------------------------------------

    /// Adds or updates one item's row on one chat's board.
    fn board_upsert(&mut self, chat: i64, view: &ItemView) {
        if self.cfg.board != TelegramBoard::Board {
            return;
        }
        let board = self.boards.entry(chat).or_default();
        let mark = board.jobs.get(&view.id).and_then(|e| e.line.mark);
        let line = JobLine {
            id: view.id,
            title: Arc::clone(&view.title),
            status: view.status,
            percent: view.percent,
            speed: view.speed,
            eta: view.eta,
            group: (view.kind == Kind::Group).then(|| {
                (
                    view.children_done.unwrap_or(0),
                    view.children_total.unwrap_or(0),
                )
            }),
            mark,
        };
        let drop_at = board.jobs.get(&view.id).and_then(|e| e.drop_at);
        let terminal = view.status.is_terminal();
        board.jobs.insert(view.id, BoardEntry { line, drop_at });
        board.dirty = true;
        if !terminal {
            board.idle_since = None;
        }
    }

    /// A chat's stored defaults, creating and persisting them on first access (legacy
    /// `_get_chat_config`, which also wrote the file).
    async fn chat_config(&mut self, chat: i64) -> ChatConfig {
        if let Some(cfg) = self.chats.get(&chat) {
            return cfg.clone();
        }
        let defaults = self.cfg.chat_defaults();
        self.chats.insert(chat, defaults.clone());
        self.persist_chat(chat, &defaults).await;
        defaults
    }

    async fn persist_chat(&self, chat: i64, cfg: &ChatConfig) {
        if let Err(e) = self
            .store
            .write(
                vec![WriteOp::UpsertTelegramChat {
                    chat_id: chat,
                    config: cfg.clone(),
                }],
                Durability::Batched,
            )
            .await
        {
            tracing::error!("Failed to persist the Telegram config for {chat}: {e}");
        }
    }

    /// One `sendMessage`, with the legacy log line on failure.
    async fn send(&self, chat: i64, text: &str, keyboard: Option<&Keyboard>) {
        if let Err(e) = self.transport.send_message(chat, text, keyboard).await {
            tracing::error!("Failed to send Telegram message to {chat}: {e}");
        }
    }
}

/// A no-token actor for a caller that only needs the types (the `doctor` CLI, a smoke test).
///
/// Kept next to the actor so the `MockTransport` re-export has an obvious home.
#[must_use]
pub fn mock_transport() -> Arc<MockTransport> {
    MockTransport::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_startup_gating_messages_are_the_legacy_lines() {
        assert_eq!(TgInitError::Disabled.to_string(), "Telegram bot disabled");
        assert_eq!(
            TgInitError::MissingToken.to_string(),
            "Telegram bot enabled but TELEGRAM_BOT_TOKEN is missing"
        );
        assert_eq!(
            TgInitError::NoAllowedChats.to_string(),
            "Telegram bot enabled but TELEGRAM_ALLOWED_CHAT_IDS is empty"
        );
    }

    #[test]
    fn the_allow_list_is_an_exact_membership_test() {
        let cfg = TelegramConfig::for_test(vec![-100_123, 7]);
        assert!(cfg.is_allowed(7));
        assert!(cfg.is_allowed(-100_123));
        assert!(!cfg.is_allowed(8));
        assert!(!cfg.is_allowed(0));
    }

    #[test]
    fn chat_defaults_are_the_legacy_twelve_keys() {
        let cfg = TelegramConfig {
            default_playlist_item_limit: 5,
            default_chapter_template: "%(title)s.%(ext)s".to_owned(),
            ..TelegramConfig::for_test(vec![1])
        };
        let d = cfg.chat_defaults();
        assert_eq!(&*d.format, "mp4");
        assert_eq!(&*d.quality, "best");
        assert_eq!(d.playlist_item_limit, 5);
        assert_eq!(&*d.chapter_template, "%(title)s.%(ext)s");
    }
}
