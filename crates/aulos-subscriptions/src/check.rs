//! The check algorithm: resolve a feed through the provider registry, work out what is new, and
//! queue it as one batch (DESIGN §14.3).
//!
//! Resolution goes through [`aulos_provider::Registry`] rather than straight to yt-dlp, so a
//! subscription works for a StreamingCommunity series or a `command` plugin feed too. For the
//! `ytdlp` provider the option layering is the **same order as everywhere else** — MeTube's keys
//! applied *after* the user's — because [`aulos_provider::Provider::resolve`] is the one code path;
//! the legacy asymmetry where `YTDL_OPTIONS` could break subscription extraction while leaving
//! normal adds intact cannot be reproduced.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use aulos_core::clock::Clock;
use aulos_core::config::Config;
use aulos_core::id::ItemId;
use aulos_core::request::DownloadRequest;
use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::subscription::SubscriptionRecord;
use aulos_core::ytdl_options::YtdlOptions;
use aulos_provider::{LiveStatus, MediaEntry, Registry, ResolveCtx};
use aulos_queue::{AddError, EngineHandle};
use aulos_store::Store;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::detect::{
    Classified, TAB_RECURSION_MAX_DEPTH, classify, is_media_entry, is_short, is_shorts_url,
    media_id_of,
};
use crate::model::{CheckFailure, CheckReport};

/// The throwaway selection a *metadata-only* resolution borrows.
///
/// `flat: true` extraction never looks at the format picker, so this only has to be a valid
/// [`Selection`]. `video/auto/any/best` is what an unconfigured add uses.
fn probe_selection() -> Selection {
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("any").unwrap_or_else(|_| unreachable!("\"any\" is a valid format id")),
        QualityId::parse("best").unwrap_or_else(|_| unreachable!("\"best\" is a valid quality id")),
    )
}

/// A resolved feed: its name and the entries that survived [`is_media_entry`].
#[derive(Clone, PartialEq, Debug)]
pub struct Feed {
    /// The container's title, when the provider supplied one.
    pub name: Option<Box<str>>,
    /// The media entries, in provider order.
    pub entries: Vec<MediaEntry>,
}

impl Feed {
    /// The media ids to mark seen when suppressing a backfill: every entry **except** an upcoming
    /// or ongoing premiere, which will be queued to wait for the finished recording.
    #[must_use]
    pub fn backfill_ids(&self) -> Vec<Box<str>> {
        self.entries
            .iter()
            .filter(|e| !e.live.is_upcoming() && e.live != LiveStatus::IsLive)
            .map(media_id_of)
            .collect()
    }
}

/// A live view of the merged yt-dlp option snapshot (DESIGN §17.2).
///
/// `aulos-server` holds the `ArcSwap<YtdlOptions>` that `YTDL_OPTIONS_FILE`'s watcher publishes
/// into and implements this over it, so a hot reload is visible to the next check. It is a trait
/// rather than the `ArcSwap` itself because DESIGN §3 does not budget `arc-swap` for this crate,
/// and one method is cheaper than a dependency.
pub trait OptionsSource: Send + Sync + std::fmt::Debug {
    /// The current snapshot.
    fn current(&self) -> Arc<YtdlOptions>;
}

/// An [`OptionsSource`] that never changes — what a test and `check-config` want.
#[derive(Clone, Debug)]
pub struct StaticOptions(pub Arc<YtdlOptions>);

impl Default for StaticOptions {
    fn default() -> Self {
        Self(Arc::new(YtdlOptions::empty()))
    }
}

impl OptionsSource for StaticOptions {
    fn current(&self) -> Arc<YtdlOptions> {
        Arc::clone(&self.0)
    }
}

/// What the manager needs from a feed, behind a trait so the scheduler tests need no provider.
#[async_trait]
pub trait FeedChecker: Send + Sync + std::fmt::Debug {
    /// Resolves `url` and reports what it is. Used by `subscribe` for the name and the backfill.
    ///
    /// # Errors
    /// [`CheckFailure::VideoOnly`] for a single video or an empty listing, and whatever the
    /// provider or the registry reported.
    async fn probe(&self, url: &Url) -> Result<Feed, CheckFailure>;

    /// One full check: resolve, diff against the seen set, queue what is new.
    ///
    /// # Errors
    /// Everything [`Self::probe`] can fail with, plus [`CheckFailure::Store`] and
    /// [`CheckFailure::Engine`].
    async fn check(&self, record: &SubscriptionRecord) -> Result<CheckReport, CheckFailure>;
}

/// The production [`FeedChecker`]: the provider registry plus one `EngineCmd::Add` per check.
pub struct Checker {
    store: Store,
    registry: Arc<RwLock<Registry>>,
    cfg: Arc<Config>,
    ytdl: Arc<dyn OptionsSource>,
    engine: EngineHandle,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Checker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checker")
            .field("scan", &self.cfg.subscription_scan_playlist_end)
            .finish_non_exhaustive()
    }
}

impl Checker {
    /// Builds a checker over the shared registry, options snapshot, store and engine.
    #[must_use]
    pub fn new(
        store: Store,
        registry: Arc<RwLock<Registry>>,
        cfg: Arc<Config>,
        ytdl: Arc<dyn OptionsSource>,
        engine: EngineHandle,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            registry,
            cfg,
            ytdl,
            engine,
            clock,
        }
    }

    /// One flat resolution through the registry (DESIGN §14.3 step 1).
    async fn resolve_flat(&self, url: &Url) -> Result<Vec<MediaEntry>, CheckFailure> {
        let provider = {
            // The registry is behind a `std::sync::RwLock`, so the guard must not cross an await.
            let guard = self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let selected = guard
                .pick(url, None)
                .ok_or_else(|| CheckFailure::NoProvider(url.as_str().into()))?;
            guard
                .by_id(&selected.id)
                .cloned()
                .ok_or_else(|| CheckFailure::NoProvider(url.as_str().into()))?
        };

        // A subscription check has no queue item, so resolution borrows a throwaway request built
        // from the same defaults an unconfigured add would use. Only `playlist_item_limit` (which
        // the caller overrides with `playlist_end`) and the option presets read it back.
        let request = DownloadRequest::new(url.clone(), probe_selection());
        let scan = self.cfg.subscription_scan_playlist_end.max(1);
        let timeout = std::time::Duration::from_secs(self.cfg.resolve_timeout_secs.max(1));
        let ctx = ResolveCtx {
            item_id: ItemId::new(),
            request: &request,
            ytdl_options: self.ytdl.current(),
            paths: &self.cfg.paths,
            flat: true,
            playlist_end: Some(scan),
            cancel: CancellationToken::new(),
            deadline: self.clock.instant() + timeout,
        };
        provider
            .resolve(url, ctx)
            .await
            .map_err(|e| CheckFailure::Provider(e.message().into_boxed_str()))
    }

    /// Resolve → classify → filter → tab recursion, at most [`TAB_RECURSION_MAX_DEPTH`] deep.
    async fn probe_at(&self, url: &Url, depth: u32) -> Result<Feed, CheckFailure> {
        if is_shorts_url(url) {
            return Ok(Feed {
                name: None,
                entries: Vec::new(),
            });
        }
        let entries = self.resolve_flat(url).await?;
        match classify(entries) {
            Classified::SingleVideo => Err(CheckFailure::VideoOnly),
            Classified::Redirect(target) => {
                if depth >= TAB_RECURSION_MAX_DEPTH {
                    return Err(CheckFailure::VideoOnly);
                }
                Box::pin(self.probe_at(&target, depth + 1)).await
            }
            Classified::Feed { name, entries } => {
                let media: Vec<MediaEntry> = entries
                    .iter()
                    .filter(|e| is_media_entry(e))
                    .cloned()
                    .collect();
                if !media.is_empty() {
                    return Ok(Feed {
                        name,
                        entries: media.into_iter().filter(|entry| !is_short(entry)).collect(),
                    });
                }
                if depth < TAB_RECURSION_MAX_DEPTH
                    && let Some(child) = entries.iter().find(|entry| !is_short(entry))
                {
                    let feed = Box::pin(self.probe_at(&child.url, depth + 1)).await?;
                    return Ok(Feed {
                        name: feed.name.or(name),
                        entries: feed.entries,
                    });
                }
                if !entries.is_empty() && entries.iter().all(is_short) {
                    return Ok(Feed {
                        name,
                        entries: Vec::new(),
                    });
                }
                // A container that lists nothing downloadable is the legacy "no longer resolves to
                // a subscribable feed" case, which shares the single-video message.
                Err(CheckFailure::VideoOnly)
            }
        }
    }

    /// The [`DownloadRequest`] one newly-seen entry is queued with.
    fn request_for(&self, record: &SubscriptionRecord, entry: &MediaEntry) -> DownloadRequest {
        let chapter_template: Box<str> = if record.chapter_template.is_empty() {
            self.cfg.default_chapter_template().into()
        } else {
            record.chapter_template.clone()
        };
        DownloadRequest {
            url: entry.url.clone(),
            selection: record.selection.clone(),
            // `folder == ""` already became `None` on the record (DESIGN §14.3 step 11), so
            // downloads land in the base dir exactly as in legacy.
            folder: record.folder.clone(),
            custom_name_prefix: record.custom_name_prefix.clone(),
            playlist_item_limit: record.playlist_item_limit,
            auto_start: record.auto_start,
            split_by_chapters: record.split_by_chapters,
            chapter_template,
            subtitle_language: record.subtitle_language.clone(),
            subtitle_mode: record.subtitle_mode,
            ytdl_options_presets: record.ytdl_options_presets.clone(),
            ytdl_options_overrides: record.ytdl_options_overrides.clone(),
            provider_hint: None,
        }
    }

    /// Sends the batch, dropping any request the engine rejects and retrying the rest.
    ///
    /// DESIGN §14.3 step 6 wants **one** `EngineCmd::Add`, and parity wants an entry that fails
    /// validation left unseen with its message collected. `EngineCmd::Add` is all-or-nothing, so
    /// the happy path is one round trip and each rejection costs exactly one more.
    async fn queue(
        &self,
        record: &SubscriptionRecord,
        entries: &[MediaEntry],
    ) -> (Vec<Box<str>>, Vec<Box<str>>) {
        let source = SourceRef::with_ref(SourceKind::Subscription, record.id.as_str());
        let cap = usize::try_from(self.cfg.max_batch_urls.max(1)).unwrap_or(usize::MAX);
        let mut queued = Vec::new();
        let mut errors = Vec::new();

        for chunk in entries.chunks(cap) {
            let mut pending: Vec<(Box<str>, DownloadRequest)> = chunk
                .iter()
                .map(|e| (media_id_of(e), self.request_for(record, e)))
                .collect();

            // At most one drop per request, so the loop is bounded by `pending.len()`.
            while !pending.is_empty() {
                let requests: Vec<DownloadRequest> =
                    pending.iter().map(|(_, r)| r.clone()).collect();
                match self.engine.add(requests, source.clone()).await {
                    Ok(_) => {
                        queued.extend(pending.into_iter().map(|(id, _)| id));
                        break;
                    }
                    Err(AddError::Invalid { index, errors: es }) => {
                        let (_, request) = &pending[index.min(pending.len() - 1)];
                        let msg = es.first().map_or_else(
                            || format!("Queueing failed for {}", request.url),
                            |e| e.message.to_string(),
                        );
                        tracing::warn!(
                            subscription = record.id.as_str(),
                            url = %request.url,
                            "subscription queueing failed: {msg}"
                        );
                        errors.push(msg.into_boxed_str());
                        pending.remove(index.min(pending.len() - 1));
                    }
                    Err(AddError::Duplicate { index, existing_id }) => {
                        // Strict dedupe mode: the item already exists, so the entry has been
                        // handled and must be marked seen or it is re-queued every check.
                        tracing::debug!(
                            subscription = record.id.as_str(),
                            existing = %existing_id,
                            "subscription entry already queued"
                        );
                        let (id, _) = pending.remove(index.min(pending.len() - 1));
                        queued.push(id);
                    }
                    Err(other) => {
                        // `TooManyUrls` cannot happen (the batch is chunked) and `Unavailable`
                        // means shutdown: report once and stop, leaving everything unseen.
                        errors.push(other.to_string().into_boxed_str());
                        break;
                    }
                }
            }
        }
        (queued, errors)
    }
}

#[async_trait]
impl FeedChecker for Checker {
    async fn probe(&self, url: &Url) -> Result<Feed, CheckFailure> {
        if is_shorts_url(url) {
            return Err(CheckFailure::ShortsExcluded);
        }
        self.probe_at(url, 0).await
    }

    async fn check(&self, record: &SubscriptionRecord) -> Result<CheckReport, CheckFailure> {
        let feed = self.probe_at(&record.url, 0).await?;
        let seen = self
            .store
            .seen(&record.id)
            .await
            .map_err(|e| CheckFailure::Store(e.to_string().into_boxed_str()))?;

        let new: Vec<MediaEntry> = feed
            .entries
            .iter()
            .filter(|e| !seen.contains(&media_id_of(e)))
            .cloned()
            .collect();

        let new_total = new.len();
        let (queued, errors) = if new.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            self.queue(record, &new).await
        };
        tracing::info!(
            subscription = record.id.as_str(),
            name = &*record.name,
            new = new_total,
            queued = queued.len(),
            failed = errors.len(),
            "subscription check finished"
        );
        Ok(CheckReport {
            queued,
            new_total,
            errors,
        })
    }
}

#[cfg(test)]
mod tests {
    use aulos_provider::{EntryHints, EntryKind, MediaEntry};

    use super::*;

    fn video(id: &str) -> MediaEntry {
        MediaEntry::video(
            id,
            id,
            Url::parse(&format!("https://x.test/v/{id}")).expect("url"),
        )
    }

    #[test]
    fn backfill_ids_skip_upcoming_premieres() {
        let mut upcoming = video("soon");
        upcoming.live = LiveStatus::IsUpcoming { at: Some(42) };
        let mut live = video("now");
        live.live = LiveStatus::IsLive;
        let feed = Feed {
            name: Some("Chan".into()),
            entries: vec![video("a"), upcoming, live, video("b")],
        };
        let backfill = feed.backfill_ids();
        let ids: Vec<&str> = backfill.iter().map(|i| &**i).collect();
        assert_eq!(ids, vec!["a", "b"], "premieres stay unseen until queued");
    }

    #[test]
    fn a_feed_keeps_provider_order() {
        let mut parent = video("p");
        parent.kind = EntryKind::Playlist {
            title: "Series".into(),
            entries: vec![video("c1"), video("c2"), video("c3")],
        };
        let Classified::Feed { entries, .. } = classify(vec![parent]) else {
            panic!("expected a feed");
        };
        let ids: Vec<&str> = entries.iter().map(|e| &*e.media_id).collect();
        assert_eq!(ids, vec!["c1", "c2", "c3"]);
    }

    #[test]
    fn hints_do_not_leak_into_the_media_id() {
        let mut e = video("c1");
        e.hints = EntryHints {
            playlist_index: Some(7),
            ..EntryHints::default()
        };
        assert_eq!(&*media_id_of(&e), "c1");
    }
}
