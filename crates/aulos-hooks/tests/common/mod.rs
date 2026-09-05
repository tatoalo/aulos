//! The harness every `aulos-hooks` integration test shares.
//!
//! Its whole point is structural: there is **no SQLite and no engine** anywhere in this directory.
//! [`FakeStore`] is a `HashMap` behind the `aulos_core::ports::HookStore` port and
//! [`aulos_hooks::RecordingFinalizer`] stands in for `EngineCmd::HooksFinished`, which is the
//! proof that `aulos-hooks` needs neither an `aulos-store` nor an `aulos-queue` dependency
//! (DESIGN §3, PLAN WP-11).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)] // each test binary uses a different slice of the harness

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aulos_core::config::{Config, RawEnv, load};
use aulos_core::error::WireError;
use aulos_core::event::{DomainEvent, EventInbox, EventRouter, EventSender, SubscriberSpec};
use aulos_core::id::ItemId;
use aulos_core::item::{EntryBlob, Item, ItemView, Kind, ViewExtras};
use aulos_core::paths::{RelDir, RelPath};
use aulos_core::ports::{HookStore, PortError};
use aulos_core::request::DownloadRequest;
use aulos_core::selection::{Codec, DownloadType, FormatId, ProviderId, QualityId, Selection};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::status::{Status, TerminalStatus};
use aulos_hooks::hook::{Debounce, Hook, HookCtx, HookHealth, SkipReason};
use aulos_hooks::{HookError, HookPhase};
use aulos_provider::sink::{ProgressMsg, ProgressSinkFactory};
use tokio::sync::mpsc;
use url::Url;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// A config with the given overrides on top of the two directories every test needs.
///
/// # Panics
/// If the resulting environment does not load, which is a bug in the test.
pub fn config(pairs: &[(&str, &str)]) -> Arc<Config> {
    let mut env: Vec<(String, String)> = vec![
        ("STATE_DIR".to_owned(), "/tmp".to_owned()),
        ("DOWNLOAD_DIR".to_owned(), "/downloads".to_owned()),
    ];
    for (k, v) in pairs {
        env.push(((*k).to_owned(), (*v).to_owned()));
    }
    Arc::new(load(&RawEnv::from_pairs(env)).expect("the test config must load"))
}

/// A config whose `DOWNLOAD_DIR` and `AUDIO_DOWNLOAD_DIR` are `dir`.
///
/// # Panics
/// If the resulting environment does not load.
pub fn config_rooted(dir: &Path, pairs: &[(&str, &str)]) -> Arc<Config> {
    let root = dir.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = vec![
        ("STATE_DIR".to_owned(), root.clone()),
        ("DOWNLOAD_DIR".to_owned(), root.clone()),
        ("AUDIO_DOWNLOAD_DIR".to_owned(), root),
    ];
    for (k, v) in pairs {
        env.push(((*k).to_owned(), (*v).to_owned()));
    }
    Arc::new(load(&RawEnv::from_pairs(env)).expect("the test config must load"))
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

/// A builder for the one wire shape the dispatcher works from.
///
/// `Clone` so a test can derive two views of the **same** item — one `postprocessing` for the
/// `Finishing` event and one `finished` for the `Completed` one — without minting a second id.
#[derive(Clone)]
pub struct ItemBuilder {
    item: Item,
}

impl ItemBuilder {
    /// A `finished` video item with a filename and a size.
    ///
    /// # Panics
    /// If one of the fixed ids or URLs in it is invalid, which is a bug in the harness.
    #[must_use]
    pub fn finished(title: &str) -> Self {
        let url = Url::parse("https://sc.test/it/watch/9?e=77").expect("url");
        let selection = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").expect("format"),
            QualityId::parse("best").expect("quality"),
        );
        let request = DownloadRequest::new(url.clone(), selection);
        Self {
            item: Item {
                id: ItemId::new(),
                kind: Kind::Item,
                group_id: None,
                group_index: None,
                ord: 1,
                url,
                canonical_key: "test".into(),
                provider: Some(ProviderId::parse("ytdlp").expect("provider")),
                media_id: Some("sc_9_77".into()),
                title: title.into(),
                status: Status::Finished,
                auto_start: true,
                msg: None,
                error: None,
                request,
                entry: None,
                filename: Some(RelPath::parse("Clip.mp4").expect("filename")),
                size: Some(1_024),
                chapter_files: Vec::new(),
                subtitle_files: Vec::new(),
                created_at: 1_788_480_000_000,
                started_at: None,
                finished_at: Some(1_788_480_001_000),
                attempt: 0,
                source: SourceRef::bare(SourceKind::ApiV2),
                children_total: None,
                clear_after: None,
            },
        }
    }

    /// Overrides the status.
    #[must_use]
    pub fn status(mut self, status: Status) -> Self {
        self.item.status = status;
        self
    }

    /// Overrides the provider.
    ///
    /// # Panics
    /// On an invalid provider id.
    #[must_use]
    pub fn provider(mut self, provider: &str) -> Self {
        self.item.provider = Some(ProviderId::parse(provider).expect("provider"));
        self
    }

    /// Overrides the produced file.
    ///
    /// # Panics
    /// On an invalid relative path.
    #[must_use]
    pub fn filename(mut self, filename: &str) -> Self {
        self.item.filename = Some(RelPath::parse(filename).expect("filename"));
        self
    }

    /// Removes the produced file.
    #[must_use]
    pub fn no_file(mut self) -> Self {
        self.item.filename = None;
        self
    }

    /// Overrides the size.
    #[must_use]
    pub fn size(mut self, size: Option<u64>) -> Self {
        self.item.size = size;
        self
    }

    /// Overrides the request's `folder`.
    ///
    /// # Panics
    /// On an invalid relative directory.
    #[must_use]
    pub fn folder(mut self, folder: &str) -> Self {
        self.item.request.folder = Some(RelDir::parse(folder).expect("folder"));
        self
    }

    /// Overrides the format/quality pair.
    ///
    /// # Panics
    /// On an invalid catalog id.
    #[must_use]
    pub fn selection(mut self, download_type: DownloadType, format: &str, quality: &str) -> Self {
        self.item.request.selection = Selection::new(
            download_type,
            Codec::Auto,
            FormatId::parse(format).expect("format"),
            QualityId::parse(quality).expect("quality"),
        );
        self
    }

    /// Sets the terminal error.
    #[must_use]
    pub fn error(mut self, error: WireError) -> Self {
        self.item.error = Some(error);
        self
    }

    /// Sets the entry blob (only for a direct hook call; the dispatcher reads the port).
    #[must_use]
    pub fn entry(mut self, entry: EntryBlob) -> Self {
        self.item.entry = Some(entry);
        self
    }

    /// The item's id.
    #[must_use]
    pub fn id(&self) -> ItemId {
        self.item.id
    }

    /// The persisted row.
    #[must_use]
    pub fn build(self) -> Item {
        self.item
    }

    /// The wire view.
    #[must_use]
    pub fn view(self) -> Arc<ItemView> {
        Arc::new(ItemView::from_item(
            &self.item,
            None,
            &ViewExtras::default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// The store port
// ---------------------------------------------------------------------------

/// One call through the [`HookStore`] port.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Call {
    /// `entry_blob`.
    EntryBlob(ItemId),
    /// `drop_entry_blob`.
    DropEntryBlob(ItemId),
    /// `set_size`.
    SetSize(ItemId, u64),
}

/// A `HashMap`-backed [`HookStore`] that records every call.
#[derive(Debug, Default)]
pub struct FakeStore {
    blobs: Mutex<HashMap<ItemId, EntryBlob>>,
    calls: Mutex<Vec<Call>>,
    fail: Mutex<Option<PortError>>,
}

impl FakeStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A store holding one blob.
    #[must_use]
    pub fn with_blob(id: ItemId, blob: EntryBlob) -> Arc<Self> {
        let store = Self::default();
        store.set_blob(id, blob);
        Arc::new(store)
    }

    /// Inserts a blob.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    pub fn set_blob(&self, id: ItemId, blob: EntryBlob) {
        self.blobs.lock().expect("lock").insert(id, blob);
    }

    /// Makes every write fail with `error`.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    pub fn fail_with(&self, error: PortError) {
        *self.fail.lock().expect("lock") = Some(error);
    }

    /// Every call so far, in order.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("lock").clone()
    }

    /// The calls that are writes — what a built-in is documented to make.
    #[must_use]
    pub fn writes(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| !matches!(c, Call::EntryBlob(_)))
            .collect()
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("lock").push(call);
    }

    fn check(&self) -> Result<(), PortError> {
        match self.fail.lock().expect("lock").clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[async_trait::async_trait]
impl HookStore for FakeStore {
    async fn entry_blob(&self, id: ItemId) -> Result<Option<EntryBlob>, PortError> {
        self.record(Call::EntryBlob(id));
        self.check()?;
        Ok(self.blobs.lock().expect("lock").get(&id).cloned())
    }

    async fn drop_entry_blob(&self, id: ItemId) -> Result<(), PortError> {
        self.record(Call::DropEntryBlob(id));
        self.check()?;
        self.blobs.lock().expect("lock").remove(&id);
        Ok(())
    }

    async fn set_size(&self, id: ItemId, size: u64) -> Result<(), PortError> {
        self.record(Call::SetSize(id, size));
        self.check()
    }
}

// ---------------------------------------------------------------------------
// Test hooks
// ---------------------------------------------------------------------------

/// What a [`ScriptHook`] does when it runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behaviour {
    /// Succeed immediately.
    Ok,
    /// Fail immediately.
    Fail,
    /// Panic, to prove the dispatcher survives it.
    Panic,
    /// Sleep forever, to prove the per-hook timeout fires.
    Hang,
}

/// A scripted hook that appends to a shared log every time it runs.
pub struct ScriptHook {
    id: Arc<str>,
    ordering: i16,
    phase: HookPhase,
    debounce: Debounce,
    timeout: Duration,
    behaviour: Behaviour,
    skip: Option<SkipReason>,
    log: Arc<Mutex<Vec<String>>>,
    batches: Arc<Mutex<Vec<Vec<String>>>>,
}

impl ScriptHook {
    /// A post-terminal hook that succeeds.
    #[must_use]
    pub fn new(id: &str, ordering: i16, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            id: Arc::from(id),
            ordering,
            phase: HookPhase::PostTerminal,
            debounce: Debounce::NONE,
            timeout: Duration::from_secs(30),
            behaviour: Behaviour::Ok,
            skip: None,
            log,
            batches: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Moves it to the pre-terminal phase.
    #[must_use]
    pub fn pre_terminal(mut self) -> Self {
        self.phase = HookPhase::PreTerminal;
        self
    }

    /// Gives it a debounce window.
    #[must_use]
    pub fn debounced(mut self, window: Duration, max_wait: Duration) -> Self {
        self.debounce = Debounce::capped(window, max_wait);
        self
    }

    /// Changes what `run` does.
    #[must_use]
    pub fn behaving(mut self, behaviour: Behaviour) -> Self {
        self.behaviour = behaviour;
        self
    }

    /// Makes it decline every event, naming `reason` — the shape of a hook whose gate never
    /// opens, which is what `runs_total: 0` alone cannot express.
    #[must_use]
    pub fn skipping(mut self, reason: &'static str) -> Self {
        self.skip = Some(SkipReason::new(reason));
        self
    }

    /// Shortens the dispatcher's outer bound.
    #[must_use]
    pub fn timing_out_after(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The titles of every batch this hook was invoked with.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn batches(&self) -> Vec<Vec<String>> {
        self.batches.lock().expect("lock").clone()
    }

    /// How many times it ran.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.batches.lock().expect("lock").len()
    }
}

#[async_trait::async_trait]
impl Hook for ScriptHook {
    fn id(&self) -> Arc<str> {
        Arc::clone(&self.id)
    }

    fn ordering(&self) -> i16 {
        self.ordering
    }

    fn phase(&self) -> HookPhase {
        self.phase
    }

    fn debounce(&self) -> Debounce {
        self.debounce
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool {
        self.skip_reason(item, outcome).is_none()
    }

    fn skip_reason(&self, _item: &ItemView, _outcome: TerminalStatus) -> Option<SkipReason> {
        self.skip.clone()
    }

    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError> {
        self.log.lock().expect("lock").push(self.id.to_string());
        self.batches
            .lock()
            .expect("lock")
            .push(ctx.batch.iter().map(|b| b.title.to_string()).collect());
        match self.behaviour {
            Behaviour::Ok => Ok(()),
            Behaviour::Fail => Err(HookError::other("scripted failure")),
            Behaviour::Panic => panic!("scripted panic"),
            Behaviour::Hang => {
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }

    fn health(&self) -> HookHealth {
        HookHealth::ok()
    }
}

/// Wraps a real hook and records its id in a shared log every time it runs, so a test can assert
/// the observed order of the built-ins without reimplementing them.
pub struct RecordingHook {
    inner: Arc<dyn Hook>,
    log: Arc<Mutex<Vec<String>>>,
}

impl RecordingHook {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Hook>, log: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self { inner, log })
    }
}

#[async_trait::async_trait]
impl Hook for RecordingHook {
    fn id(&self) -> Arc<str> {
        self.inner.id()
    }

    fn ordering(&self) -> i16 {
        self.inner.ordering()
    }

    fn phase(&self) -> HookPhase {
        self.inner.phase()
    }

    fn debounce(&self) -> Debounce {
        self.inner.debounce()
    }

    fn timeout(&self) -> Duration {
        self.inner.timeout()
    }

    fn wants_entry(&self) -> bool {
        self.inner.wants_entry()
    }

    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool {
        self.inner.applies(item, outcome)
    }

    fn skip_reason(&self, item: &ItemView, outcome: TerminalStatus) -> Option<SkipReason> {
        // Forwarded, or the wrapper would flatten every wrapped hook's reason to the generic one.
        self.inner.skip_reason(item, outcome)
    }

    fn health(&self) -> HookHealth {
        self.inner.health()
    }

    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError> {
        self.log
            .lock()
            .expect("lock")
            .push(self.inner.id().to_string());
        self.inner.run(ctx).await
    }
}

/// A [`aulos_hooks::HookFinalizer`] that appends `HooksFinished` to a shared log, so a test can
/// assert the interleaving of hook runs and engine commands in one sequence — the "fake engine
/// that records the command sequence" of PLAN WP-11.
pub struct LoggingFinalizer {
    log: Arc<Mutex<Vec<String>>>,
    ids: Mutex<Vec<ItemId>>,
}

impl LoggingFinalizer {
    /// A finalizer writing into `log`.
    #[must_use]
    pub fn new(log: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
        Arc::new(Self {
            log,
            ids: Mutex::new(Vec::new()),
        })
    }

    /// The ids finalised so far, in order.
    ///
    /// # Panics
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn ids(&self) -> Vec<ItemId> {
        self.ids.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl aulos_hooks::HookFinalizer for LoggingFinalizer {
    async fn hooks_finished(&self, id: ItemId) {
        self.ids.lock().expect("lock").push(id);
        self.log
            .lock()
            .expect("lock")
            .push("HooksFinished".to_owned());
    }
}

/// A shared string log.
#[must_use]
pub fn log() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

/// Reads a shared string log.
///
/// # Panics
/// If a previous holder of the lock panicked.
#[must_use]
pub fn read_log(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().expect("lock").clone()
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A router plus the `hooks` inbox, wired exactly as DESIGN §2.2.1 specifies.
pub struct Events {
    /// The publisher.
    pub tx: EventSender,
    /// The dispatcher's inbox.
    pub inbox: Option<EventInbox>,
    /// The router task.
    pub router: tokio::task::JoinHandle<()>,
}

/// Wires an [`EventRouter`] with the `hooks` subscriber and spawns it.
#[must_use]
pub fn events() -> Events {
    events_with(SubscriberSpec::hooks(), 4096)
}

/// Wires an [`EventRouter`] with an explicit subscriber spec, for the drop test.
#[must_use]
pub fn events_with(spec: SubscriberSpec, capacity: usize) -> Events {
    let (mut router, tx) = EventRouter::new(capacity);
    let inbox = router.subscribe(spec);
    Events {
        tx,
        inbox: Some(inbox),
        router: router.spawn(),
    }
}

impl Events {
    /// Takes the inbox, to hand it to `HookDispatcher::run`.
    ///
    /// # Panics
    /// If called twice.
    pub fn inbox(&mut self) -> EventInbox {
        self.inbox.take().expect("the inbox is taken once")
    }

    /// Publishes a `Finishing` event.
    pub async fn finishing(&self, view: &Arc<ItemView>) {
        self.tx
            .publish(DomainEvent::Finishing(Arc::clone(view)))
            .await;
    }

    /// Publishes a `Completed` event.
    pub async fn completed(&self, view: &Arc<ItemView>) {
        self.tx
            .publish(DomainEvent::Completed(Arc::clone(view)))
            .await;
    }
}

/// A progress sink factory plus its receiver, so a test can read what a hook reported.
#[must_use]
pub fn sink() -> (ProgressSinkFactory, mpsc::Receiver<ProgressMsg>) {
    ProgressSinkFactory::channel()
}

/// Drains everything currently queued on a progress channel.
pub fn drain(rx: &mut mpsc::Receiver<ProgressMsg>) -> Vec<ProgressMsg> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        out.push(msg);
    }
    out
}

// ---------------------------------------------------------------------------
// Scheduling helpers
// ---------------------------------------------------------------------------

/// Lets every currently runnable task make progress. Works under both real and paused time.
pub async fn settle() {
    for _ in 0..24 {
        tokio::task::yield_now().await;
    }
}

/// Polls `f` until it is true, yielding and then sleeping in between. Returns whether it became
/// true.
///
/// Only for tests running on real time: the sleeps would auto-advance a paused clock and fire a
/// debounce window early. A paused-time test drives `tokio::time::advance` itself and uses
/// [`settle`].
pub async fn until(mut f: impl FnMut() -> bool) -> bool {
    for i in 0..1_000 {
        if f() {
            return true;
        }
        if i < 32 {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    f()
}

// ---------------------------------------------------------------------------
// Fake media tools
// ---------------------------------------------------------------------------

/// Writes an executable shell script and returns its path.
///
/// # Panics
/// If the script cannot be written or made executable.
#[must_use]
pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the script");
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

/// Whether a real tool is on `PATH`, so a test can skip rather than fail on a machine without
/// ffmpeg. The `audio_sync` behaviour tests use scripts and never need this.
#[must_use]
pub fn have_tool(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
