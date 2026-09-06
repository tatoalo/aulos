//! Who gets told about which job, and the two watchdog warnings (DESIGN §12.5, §12.6).
//!
//! # Attribution is explicit
//!
//! Legacy tracked watches in a `dict` keyed by **URL** and learned the chat id from a
//! `contextvars.ContextVar` set around each `add` call. Two consequences, both bugs: adding the
//! same URL twice merged two jobs into one watch, and any job the bot did not create itself — a
//! web add, a subscription, a playlist child — was invisible. Here the watch is keyed by
//! [`ItemId`] and the chat comes from [`aulos_core::SourceRef`], which the engine copies onto
//! every playlist child.
//!
//! # Who the bot reports on
//!
//! An item reports on the channel that asked for it (DESIGN §12.6):
//!
//! | `source.kind` | Chats |
//! |---|---|
//! | `telegram` | the one chat in `source.ref`, if it is allow-listed — never the whole list |
//! | `subscription` | every allowed chat, **always** |
//! | anything else (`api_v2`, `api_v1`, `ios`, and the legacy `retry`/`restart` rows) | every allowed chat, but only with `AULOS_TELEGRAM_WATCH_ALL` |
//!
//! `AULOS_TELEGRAM_WATCH_ALL` defaults to **`false`** (decision 43, superseding the BRIEF's
//! `true`): an item added from the iOS app is announced by APNs (§25), and announcing it here too
//! is the duplicate the operator asked us to stop sending.
//!
//! A subscription is the exception because it has no originating channel — nobody typed the URL,
//! so there is nobody to report back to — and Telegram is the only push surface for background
//! work. Gating it on `watch_all` would make auto-downloads silent everywhere, which is the one
//! outcome nothing else in the system covers.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use aulos_core::id::ItemId;
use aulos_core::item::ItemView;
use aulos_core::source::SourceKind;
use tokio::time::Instant;

use crate::render::Mark;

/// The notification seam, declared in `aulos-core` where DESIGN §12.6 puts it.
///
/// Re-exported here because `aulos-telegram` is its first implementation.
pub use aulos_core::event::Notifier;

/// One job the bot is reporting on.
#[derive(Clone, Debug)]
pub struct Watched {
    /// The chats to notify.
    pub chats: BTreeSet<i64>,
    /// When the watch began — i.e. when the item was queued.
    pub started_at: Instant,
    /// When the item last *started running*, or `None` while it is merely queued.
    ///
    /// Both watchdogs measure download time, not time spent waiting behind
    /// `MAX_CONCURRENT_DOWNLOADS`, so they are held off entirely while this is `None`
    /// (DESIGN §12.5).
    pub running_since: Option<Instant>,
    /// The last time this job reported progress.
    pub last_progress_at: Instant,
    /// Chats already told it looks stalled.
    pub stall_notified: BTreeSet<i64>,
    /// Chats already told it is taking too long.
    pub timeout_notified: BTreeSet<i64>,
    /// The source URL, which is what both warnings quote.
    pub url: Arc<str>,
    /// The display title, which is what the two terminal messages quote.
    pub title: Arc<str>,
    /// The marker the board line carries, if any.
    pub mark: Option<Mark>,
}

/// One warning to deliver.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Warning {
    /// Which chat.
    pub chat: i64,
    /// Which item.
    pub id: ItemId,
    /// Stall or hard timeout.
    pub kind: Mark,
    /// The elapsed seconds the message quotes.
    pub secs: u64,
    /// The URL the message quotes.
    pub url: Arc<str>,
}

/// The per-job watch table and the two watchdogs (DESIGN §12.5).
#[derive(Debug)]
pub struct WatchRegistry {
    jobs: HashMap<ItemId, Watched>,
    stall: Duration,
    hard: Duration,
    watch_all: bool,
    allowed: Vec<i64>,
}

impl WatchRegistry {
    /// A registry with the two legacy timeouts, the `watch_all` knob and the allow-list that
    /// `watch_all` fans out to.
    #[must_use]
    pub fn new(stall_secs: u64, hard_secs: u64, watch_all: bool, allowed: Vec<i64>) -> Self {
        Self {
            jobs: HashMap::new(),
            stall: Duration::from_secs(stall_secs),
            hard: Duration::from_secs(hard_secs),
            watch_all,
            allowed,
        }
    }

    /// Whether this item is one the bot reports on (DESIGN §12.6).
    ///
    /// Kept deliberately as "`chats_for` would name somebody", so the predicate and the routing
    /// can never disagree: a `true` here that produced an empty chat set would register a watch
    /// nothing can ever deliver, and the watchdogs would tick for it forever.
    #[must_use]
    pub fn interested(&self, item: &ItemView) -> bool {
        match item.source.kind {
            // Its own chat still has to be allow-listed; `chats_for` is the authority.
            SourceKind::Telegram => !self.chats_for(item).is_empty(),
            SourceKind::Subscription => true,
            _ => self.watch_all,
        }
    }

    /// The chats an item's events go to (DESIGN §12.6).
    ///
    /// Three rules, in order:
    ///
    /// - **Telegram-sourced** → the one chat in `source.ref`, and nowhere else. `source` is
    ///   written once at the add and never rewritten, so a retried or recovered item still knows
    ///   which chat asked (DESIGN §4.4).
    /// - **Subscription-sourced** → every allowed chat, whatever the knob says. An auto-download
    ///   has no originating channel and Telegram is its only push surface; gating it would make
    ///   background work silent everywhere.
    /// - **anything else** (web, `curl`, the iOS app) → every allowed chat only with
    ///   `AULOS_TELEGRAM_WATCH_ALL`, which is off by default so an iOS add is announced by APNs
    ///   alone (§25).
    #[must_use]
    pub fn chats_for(&self, item: &ItemView) -> BTreeSet<i64> {
        match item.source.kind {
            SourceKind::Telegram => item
                .source
                .reference
                .as_deref()
                .and_then(|r| r.parse::<i64>().ok())
                .filter(|c| self.allowed.contains(c))
                .into_iter()
                .collect(),
            SourceKind::Subscription => self.allowed.iter().copied().collect(),
            _ if self.watch_all => self.allowed.iter().copied().collect(),
            _ => BTreeSet::new(),
        }
    }

    /// Starts (or extends) a watch on `item`. Returns the chats now watching it.
    pub fn watch(&mut self, item: &ItemView, now: Instant) -> BTreeSet<i64> {
        let chats = self.chats_for(item);
        if chats.is_empty() {
            return chats;
        }
        let running = item.status.is_running();
        let entry = self.jobs.entry(item.id).or_insert_with(|| Watched {
            chats: BTreeSet::new(),
            started_at: now,
            running_since: running.then_some(now),
            last_progress_at: now,
            stall_notified: BTreeSet::new(),
            timeout_notified: BTreeSet::new(),
            url: Arc::clone(&item.url),
            title: Arc::clone(&item.title),
            mark: None,
        });
        entry.chats.extend(chats.iter().copied());
        entry.title = Arc::clone(&item.title);
        chats
    }

    /// Records that a watched job is **running** and made progress, which is what keeps the stall
    /// watchdog honest.
    ///
    /// The first such call also starts the hard-timeout clock: an item that is only waiting behind
    /// `MAX_CONCURRENT_DOWNLOADS` is not late, so neither watchdog may run until it starts.
    pub fn touch(&mut self, id: ItemId, now: Instant) {
        if let Some(w) = self.jobs.get_mut(&id) {
            w.running_since.get_or_insert(now);
            w.last_progress_at = now;
        }
    }

    /// Records that a watched job is **not** running — still queued or resolving, or paused back
    /// into the queue. Both watchdogs are held off until it runs again, and both clocks restart
    /// from the moment it does.
    pub fn park(&mut self, id: ItemId, now: Instant) {
        if let Some(w) = self.jobs.get_mut(&id) {
            w.running_since = None;
            w.last_progress_at = now;
        }
    }

    /// Refreshes the stored title as resolution learns it.
    pub fn retitle(&mut self, item: &ItemView) {
        if let Some(w) = self.jobs.get_mut(&item.id) {
            w.title = Arc::clone(&item.title);
        }
    }

    /// Drops a watch, returning it so the caller can send the terminal message.
    pub fn finish(&mut self, id: ItemId) -> Option<Watched> {
        self.jobs.remove(&id)
    }

    /// The watch for `id`, if any.
    #[must_use]
    pub fn get(&self, id: ItemId) -> Option<&Watched> {
        self.jobs.get(&id)
    }

    /// How many jobs are being watched.
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// Whether nothing is being watched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// The warnings that have come due, **once per chat per job** (DESIGN §12.5).
    ///
    /// Neither warning cancels the download — parity. Each one is recorded as sent before it is
    /// returned, so a caller that drops a message does not get it again; that matches legacy,
    /// where the `add` to `stall_notified` happened under the lock before the send.
    ///
    /// A job that has not started running is skipped by both watchdogs: a 200-item subscription
    /// batch behind `MAX_CONCURRENT_DOWNLOADS=3` (or any playlist, with `AULOS_TELEGRAM_WATCH_ALL`
    /// on) would otherwise fire ~197 "stalled" messages into every allowed chat three minutes
    /// later.
    pub fn due_warnings(&mut self, now: Instant) -> Vec<Warning> {
        let mut out = Vec::new();
        for (id, w) in &mut self.jobs {
            let Some(running_since) = w.running_since else {
                continue;
            };
            let since_progress = now.saturating_duration_since(w.last_progress_at);
            let elapsed = now.saturating_duration_since(running_since);
            let stalled = since_progress > self.stall;
            let overrun = elapsed > self.hard;
            for chat in &w.chats {
                if stalled && !w.stall_notified.contains(chat) {
                    w.stall_notified.insert(*chat);
                    out.push(Warning {
                        chat: *chat,
                        id: *id,
                        kind: Mark::Stalled,
                        secs: since_progress.as_secs(),
                        url: Arc::clone(&w.url),
                    });
                }
                if overrun && !w.timeout_notified.contains(chat) {
                    w.timeout_notified.insert(*chat);
                    out.push(Warning {
                        chat: *chat,
                        id: *id,
                        kind: Mark::Timeout,
                        secs: elapsed.as_secs(),
                        url: Arc::clone(&w.url),
                    });
                }
            }
            if stalled {
                w.mark = Some(Mark::Stalled);
            }
            if overrun {
                w.mark = Some(Mark::Timeout);
            }
        }
        // Deterministic order, so a test can assert the list rather than a set.
        out.sort_by(|a, b| (a.chat, a.kind.glyph()).cmp(&(b.chat, b.kind.glyph())));
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use aulos_core::source::SourceRef;
    use aulos_core::status::Status;

    use super::*;

    /// A minimal `ItemView` with the attribution under test.
    fn item(source: SourceRef) -> ItemView {
        let mut v = view_template();
        v.source = source;
        v
    }

    /// A view in a given lifecycle state, which is what the two watchdogs key off.
    fn item_in(source: SourceRef, status: Status, auto_start: bool) -> ItemView {
        let mut v = item(source);
        v.status = status;
        v.auto_start = auto_start;
        v
    }

    fn view_template() -> ItemView {
        // `ItemView` has no constructor (it is built by the engine from an `Item`), so a test
        // builds one from an `Item`'s projection to stay in step with the real shape.
        let request = aulos_core::request::DownloadRequest::new(
            url::Url::parse("https://a.test/watch/1").expect("url"),
            aulos_core::selection::Selection::new(
                aulos_core::selection::DownloadType::Video,
                aulos_core::selection::Codec::Auto,
                aulos_core::selection::FormatId::parse("any").expect("format"),
                aulos_core::selection::QualityId::parse("best").expect("quality"),
            ),
        );
        let item = aulos_core::item::Item {
            id: ItemId::new(),
            kind: aulos_core::item::Kind::Item,
            group_id: None,
            group_index: None,
            ord: 1,
            url: request.url.clone(),
            canonical_key: "k".into(),
            provider: None,
            media_id: None,
            title: "A clip".into(),
            status: Status::Downloading,
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
            source: SourceRef::bare(SourceKind::ApiV2),
            children_total: None,
            clear_after: None,
        };
        ItemView::from_item(&item, None, &aulos_core::item::ViewExtras::default())
    }

    fn registry(watch_all: bool) -> WatchRegistry {
        WatchRegistry::new(180, 7_200, watch_all, vec![7, 8])
    }

    #[tokio::test]
    async fn a_telegram_job_goes_only_to_the_chat_that_asked() {
        let r = registry(true);
        let it = item(SourceRef::with_ref(SourceKind::Telegram, "7"));
        assert_eq!(r.chats_for(&it), BTreeSet::from([7]));
        assert!(r.interested(&it));
    }

    #[tokio::test]
    async fn a_telegram_job_from_a_chat_outside_the_allow_list_goes_nowhere() {
        let r = registry(true);
        let it = item(SourceRef::with_ref(SourceKind::Telegram, "999"));
        assert!(r.chats_for(&it).is_empty());
    }

    /// DESIGN §12.6: the knob governs only the kinds that have another channel of their own.
    #[tokio::test]
    async fn watch_all_decides_whether_a_web_add_is_visible() {
        let api = item(SourceRef::bare(SourceKind::ApiV2));

        let on = registry(true);
        assert!(on.interested(&api));
        assert_eq!(on.chats_for(&api), BTreeSet::from([7, 8]));

        let off = registry(false);
        assert!(
            !off.interested(&api),
            "the default: web adds report nowhere"
        );
        assert!(off.chats_for(&api).is_empty());

        // A Telegram job is always visible, whatever the knob says.
        let tg = item(SourceRef::with_ref(SourceKind::Telegram, "8"));
        assert!(off.interested(&tg));
        assert_eq!(off.chats_for(&tg), BTreeSet::from([8]));
    }

    /// A subscription has no originating channel, and Telegram is the only push surface for
    /// background work — so it fans out with the knob **off** (DESIGN §12.6).
    #[tokio::test]
    async fn a_subscription_is_reported_even_with_the_knob_off() {
        let sub = item(SourceRef::with_ref(SourceKind::Subscription, "01JC"));
        for watch_all in [false, true] {
            let r = registry(watch_all);
            assert!(r.interested(&sub), "AULOS_TELEGRAM_WATCH_ALL={watch_all}");
            assert_eq!(r.chats_for(&sub), BTreeSet::from([7, 8]));
        }
    }

    /// The operator's rule: a video added from the phone is APNs' to announce (§25), so the bot
    /// says nothing about it unless an operator asks for everything.
    #[tokio::test]
    async fn an_ios_add_is_the_phones_to_report_not_the_bots() {
        let ios = item(SourceRef::bare(SourceKind::Ios));

        let off = registry(false);
        assert!(!off.interested(&ios));
        assert!(off.chats_for(&ios).is_empty());

        let on = registry(true);
        assert!(on.interested(&ios), "the all-knob is still an escape hatch");
        assert_eq!(on.chats_for(&ios), BTreeSet::from([7, 8]));
    }

    /// `interested` and `chats_for` may never disagree: a watch nothing can deliver to would tick
    /// the watchdogs forever.
    #[tokio::test]
    async fn interested_never_promises_a_chat_that_routing_will_not_produce() {
        for watch_all in [false, true] {
            let r = registry(watch_all);
            for it in [
                item(SourceRef::bare(SourceKind::ApiV2)),
                item(SourceRef::bare(SourceKind::ApiV1)),
                item(SourceRef::bare(SourceKind::Ios)),
                item(SourceRef::with_ref(SourceKind::Subscription, "01JC")),
                item(SourceRef::with_ref(SourceKind::Telegram, "7")),
                // A chat outside the allow-list is the case the two used to disagree on.
                item(SourceRef::with_ref(SourceKind::Telegram, "999")),
                item(SourceRef::bare(SourceKind::Retry)),
                item(SourceRef::bare(SourceKind::Restart)),
            ] {
                assert_eq!(
                    r.interested(&it),
                    !r.chats_for(&it).is_empty(),
                    "{:?} with AULOS_TELEGRAM_WATCH_ALL={watch_all}",
                    it.source
                );
            }
        }
    }

    #[tokio::test]
    async fn watching_is_keyed_by_item_id_not_url() {
        let mut r = registry(true);
        let now = Instant::now();
        let a = item(SourceRef::with_ref(SourceKind::Telegram, "7"));
        let b = item(SourceRef::with_ref(SourceKind::Telegram, "8"));
        assert_ne!(a.id, b.id);
        assert_eq!(a.url, b.url, "the same URL twice");

        r.watch(&a, now);
        r.watch(&b, now);
        assert_eq!(r.len(), 2, "two jobs, not one merged watch");
        assert_eq!(r.get(a.id).expect("a").chats, BTreeSet::from([7]));
        assert_eq!(r.get(b.id).expect("b").chats, BTreeSet::from([8]));

        let finished = r.finish(a.id).expect("a was watched");
        assert_eq!(finished.chats, BTreeSet::from([7]));
        assert_eq!(r.len(), 1);
        assert!(r.finish(a.id).is_none(), "idempotent");
    }

    #[tokio::test]
    async fn an_unwatched_item_registers_nothing() {
        let mut r = registry(false);
        let api = item(SourceRef::bare(SourceKind::ApiV2));
        assert!(r.watch(&api, Instant::now()).is_empty());
        assert!(r.is_empty());
    }

    /// DESIGN §12.5: each warning fires once per chat per job, and neither cancels anything.
    #[tokio::test(start_paused = true)]
    async fn the_two_warnings_fire_once_per_chat_per_job() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7, 8]);
        let api = item(SourceRef::bare(SourceKind::ApiV2));
        let start = Instant::now();
        r.watch(&api, start);

        assert!(r.due_warnings(start).is_empty(), "nothing at once");
        assert!(
            r.due_warnings(start + Duration::from_secs(180)).is_empty(),
            "the threshold is exclusive, as legacy's `>` was"
        );

        let stalls = r.due_warnings(start + Duration::from_secs(181));
        assert_eq!(stalls.len(), 2, "one per chat: {stalls:?}");
        assert!(stalls.iter().all(|w| w.kind == Mark::Stalled));
        assert_eq!(stalls[0].chat, 7);
        assert_eq!(stalls[1].chat, 8);
        assert_eq!(stalls[0].secs, 181);
        assert_eq!(&*stalls[0].url, "https://a.test/watch/1");

        assert!(
            r.due_warnings(start + Duration::from_secs(600)).is_empty(),
            "already told"
        );

        let overruns = r.due_warnings(start + Duration::from_secs(7_201));
        assert_eq!(overruns.len(), 2);
        assert!(overruns.iter().all(|w| w.kind == Mark::Timeout));
        assert_eq!(overruns[0].secs, 7_201);

        assert!(
            r.due_warnings(start + Duration::from_secs(10_000))
                .is_empty()
        );
        assert_eq!(r.len(), 1, "neither warning cancels the download");
        assert_eq!(
            r.get(api.id).expect("still watched").mark,
            Some(Mark::Timeout)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progress_resets_the_stall_clock_but_not_the_hard_timeout() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let api = item(SourceRef::bare(SourceKind::ApiV2));
        let start = Instant::now();
        r.watch(&api, start);

        // Progress at +170 s pushes the stall deadline out.
        r.touch(api.id, start + Duration::from_secs(170));
        assert!(r.due_warnings(start + Duration::from_secs(300)).is_empty());
        assert_eq!(
            r.due_warnings(start + Duration::from_secs(360)).len(),
            1,
            "170 + 181 has now passed with no further progress"
        );

        // The hard timeout is measured from the start, so progress does not postpone it.
        let overrun = r.due_warnings(start + Duration::from_secs(7_201));
        assert_eq!(overrun.len(), 1);
        assert_eq!(overrun[0].kind, Mark::Timeout);
    }

    /// Regression (ops-5): a merely-*queued* item is not stalled, it is waiting behind
    /// `MAX_CONCURRENT_DOWNLOADS`. Letting the watchdogs run from `Added` fanned one bogus
    /// "stalled" message per queued item out to every allowed chat three minutes after a
    /// subscription batch landed — and a subscription fans out whatever `AULOS_TELEGRAM_WATCH_ALL`
    /// says, so flipping that default did not retire this regression.
    #[tokio::test(start_paused = true)]
    async fn neither_watchdog_fires_on_an_item_that_has_not_started() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7, 8]);
        let start = Instant::now();

        let queued = item_in(SourceRef::bare(SourceKind::ApiV2), Status::Queued, true);
        // `auto_start = false` is the legacy *pending* bucket: also not started, also silent.
        let pending = item_in(SourceRef::bare(SourceKind::ApiV2), Status::Queued, false);
        r.watch(&queued, start);
        r.watch(&pending, start);
        assert_eq!(r.len(), 2);

        assert!(r.due_warnings(start + Duration::from_secs(181)).is_empty());
        assert!(
            r.due_warnings(start + Duration::from_secs(7_201))
                .is_empty()
        );
        assert!(r.get(queued.id).expect("watched").running_since.is_none());
        assert_eq!(
            r.get(queued.id).expect("watched").mark,
            None,
            "and the board line carries no warning glyph either"
        );
    }

    /// …and both clocks start from the moment it does run.
    #[tokio::test(start_paused = true)]
    async fn both_clocks_start_when_the_item_leaves_the_queue() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let start = Instant::now();
        let it = item_in(SourceRef::bare(SourceKind::ApiV2), Status::Queued, true);
        r.watch(&it, start);

        // An hour behind the concurrency limit, then it starts.
        let began = start + Duration::from_secs(3_600);
        r.touch(it.id, began);
        assert_eq!(r.get(it.id).expect("watched").running_since, Some(began));

        assert!(
            r.due_warnings(began + Duration::from_secs(180)).is_empty(),
            "the stall clock runs from the start of work, not from the add"
        );
        let stalls = r.due_warnings(began + Duration::from_secs(181));
        assert_eq!(stalls.len(), 1);
        assert_eq!(stalls[0].secs, 181);

        assert!(
            r.due_warnings(start + Duration::from_secs(7_201))
                .is_empty(),
            "the hard timeout is measured from `running_since` too"
        );
        let overruns = r.due_warnings(began + Duration::from_secs(7_201));
        assert_eq!(overruns.len(), 1);
        assert_eq!(overruns[0].kind, Mark::Timeout);
        assert_eq!(overruns[0].secs, 7_201);
    }

    /// A pause writes `Downloading → Queued(auto_start = false)`; the item is queued again, so
    /// both watchdogs stop until it resumes.
    #[tokio::test(start_paused = true)]
    async fn parking_a_running_item_holds_both_watchdogs_off() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let start = Instant::now();
        let it = item(SourceRef::bare(SourceKind::ApiV2));
        r.watch(&it, start);
        assert_eq!(
            r.get(it.id).expect("watched").running_since,
            Some(start),
            "the template view is already `Downloading`"
        );

        r.park(it.id, start + Duration::from_secs(60));
        assert!(
            r.due_warnings(start + Duration::from_secs(9_999))
                .is_empty()
        );

        // Resuming restarts both clocks from the resume, not from the original add.
        let resumed = start + Duration::from_secs(10_000);
        r.touch(it.id, resumed);
        assert!(
            r.due_warnings(resumed + Duration::from_secs(180))
                .is_empty()
        );
        assert_eq!(r.due_warnings(resumed + Duration::from_secs(181)).len(), 1);
    }

    #[tokio::test]
    async fn a_retitle_updates_what_the_terminal_message_will_quote() {
        let mut r = registry(true);
        let mut it = item(SourceRef::with_ref(SourceKind::Telegram, "7"));
        r.watch(&it, Instant::now());
        assert_eq!(&*r.get(it.id).expect("watched").title, "A clip");
        it.title = "The real title".into();
        r.retitle(&it);
        assert_eq!(&*r.get(it.id).expect("watched").title, "The real title");
    }

    #[test]
    fn the_marks_carry_the_documented_glyphs() {
        assert_eq!(Mark::Stalled.glyph(), "⚠️");
        assert_eq!(Mark::Timeout.glyph(), "⏱️");
    }
}
