//! Resolution, playlist expansion and the one documented runner-up fall-through
//! (DESIGN §8.4, §6.4).
//!
//! The shapes a resolution can take, and what each one does to the row the client already holds:
//!
//! | Result | Effect |
//! |---|---|
//! | one `Video` entry | **the same item id**: `SetResolved`, status → `queued`. One ~110-byte `delta`. |
//! | a `Playlist`, or several entries | **in-place promotion**: the row keeps its `id` *and* its `ord`, `kind` flips, children are inserted in batches of 100 |
//! | a `Redirect` | re-enter resolution, depth-capped, with the legacy `already`-URL guard |
//! | zero entries, or an entry with no usable id | `error` + the verbatim `Invalid/empty data was given.` |
//! | `Err(Unsupported)` with a `Ready` runner-up | **one** retry through the runner-up |
//! | any other `Err` | terminal |
//!
//! In-place promotion is the single best anti-flicker property in the protocol: `added` is an
//! upsert by id and the group keeps its sort key, so the row morphs instead of blinking.

use std::collections::VecDeque;
use std::sync::Arc;

use aulos_core::{
    AddReason, ErrorCode, FieldUpdate, GroupId, Item, ItemId, Kind, Status, WireError,
};
use aulos_provider::{EntryKind, MediaEntry, Provider, ProviderError, ProviderId, ResolveCtx};
use aulos_store::{Durability, WriteOp};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::cmd::{CHILD_BATCH, EngineCmd, ResolveMeta};
use crate::dedupe::{DedupeKey, canonical_key};
use crate::engine::{Engine, Expansion, ResolveSlot};
use crate::groups::GroupAcc;

/// The verbatim legacy message for an empty or unusable resolution (DESIGN §8.4, §11.7).
pub const INVALID_EMPTY_DATA: &str = "Invalid/empty data was given.";

/// The verbatim legacy message for a root `_type` outside the mappable set (DESIGN §8.4, §11.7).
#[must_use]
pub fn unsupported_resource(etype: &str) -> String {
    format!("Unsupported resource \"{etype}\"")
}

impl Engine {
    /// Spawns one resolution task (DESIGN §8.4).
    ///
    /// `meta` is `None` for a first attempt; a redirect or a runner-up retry passes the previous
    /// bookkeeping so the depth cap and the one-retry rule are enforced across attempts.
    pub(crate) async fn spawn_resolve(
        &mut self,
        id: ItemId,
        generation: u64,
        meta: Option<ResolveMeta>,
    ) {
        let Some(item) = self.cached(id) else {
            return;
        };
        let url = meta
            .as_ref()
            .and_then(|m| m.seen.last())
            .and_then(|u| url::Url::parse(u).ok())
            .unwrap_or_else(|| item.url.clone());

        let picked = {
            let registry = self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let forced = meta.as_ref().map(|m| m.provider.clone());
            let hint = forced.as_ref().or(item.request.provider_hint.as_ref());
            registry.pick(&url, hint).and_then(|selected| {
                let provider = registry.by_id(&selected.id).map(Arc::clone)?;
                Some((provider, selected))
            })
        };
        let Some((provider, selected)) = picked else {
            // "No provider matched" is a real outcome, and it is the `unsupported_url` case of
            // DESIGN §5 — a synthesised `Selected` would be a lie the engine could route a job to.
            self.fail_resolution(
                id,
                WireError::new(
                    ErrorCode::UnsupportedUrl,
                    unsupported_resource(url.as_str()),
                ),
            )
            .await;
            self.notify_resolved(id);
            return;
        };

        let mut next = meta.unwrap_or(ResolveMeta {
            generation,
            epoch: self.cancel_epoch,
            depth: 0,
            fell_through: false,
            provider: selected.id.clone(),
            runner_up: None,
            seen: Vec::new(),
        });
        next.generation = generation;
        next.epoch = self.cancel_epoch;
        next.provider = selected.id.clone();
        next.runner_up = selected
            .runner_up
            .as_ref()
            .filter(|_| selected.state.is_ready())
            .map(|(runner, _)| runner.clone());
        let here = Box::<str>::from(url.as_str());
        if !next.seen.contains(&here) {
            next.seen.push(here);
        }

        let cancel = self.shutdown.child_token();
        let job = ResolveJob {
            id,
            url,
            provider,
            item,
            pool: self.slots.resolve_pool(),
            ytdl: self.ytdl.load_full(),
            paths: self.cfg.paths.clone(),
            timeout: self.cfg.resolve_timeout_secs,
            cancel: cancel.clone(),
            tx: self.tx.clone(),
            meta: next,
        };
        let handle = tokio::spawn(job.run());
        if let Some(previous) = self.resolving.insert(
            id,
            ResolveSlot {
                handle,
                cancel,
                generation,
            },
        ) {
            previous.cancel.cancel();
            previous.handle.abort();
        }
    }

    /// [`EngineCmd::Resolved`] (DESIGN §8.4).
    pub(crate) async fn handle_resolved(
        &mut self,
        id: ItemId,
        result: Result<Vec<MediaEntry>, ProviderError>,
        meta: ResolveMeta,
    ) {
        self.resolving.remove(&id);
        let Some(item) = self.cached(id) else {
            return;
        };
        if item.status != Status::Resolving {
            // Cancelled, deleted or already settled while the task was running.
            self.notify_resolved(id);
            return;
        }
        if self.cancel_epoch > meta.epoch {
            // A `CancelScope::All` landed while this was in flight. Note the epoch, not the
            // generation: a *later add* must not condemn this one's resolution.
            self.cancel_one(id).await;
            self.notify_resolved(id);
            return;
        }

        match result {
            Ok(entries) => self.on_entries(id, entries, meta).await,
            Err(e) => self.on_resolve_error(id, e, meta).await,
        }
        self.notify_resolved(id);
        self.schedule().await;
    }

    /// The success branch of DESIGN §8.4.
    async fn on_entries(&mut self, id: ItemId, entries: Vec<MediaEntry>, meta: ResolveMeta) {
        if entries.is_empty() {
            self.fail_resolution(id, empty_data(&meta.provider)).await;
            return;
        }
        if entries.len() > 1 {
            // Several top-level entries with no container wrapper: a playlist all the same. This
            // is what a flat extraction produces, and what the fake provider's `expand_playlist`
            // step returns.
            let title = self
                .cached(id)
                .map_or_else(|| Box::<str>::from(""), |i| i.title.clone());
            self.promote(id, title, entries, meta).await;
            return;
        }

        let mut iter = entries.into_iter();
        let Some(entry) = iter.next() else {
            self.fail_resolution(id, empty_data(&meta.provider)).await;
            return;
        };
        match entry.kind {
            EntryKind::Redirect { ref url } => {
                let url = url.clone();
                self.on_redirect(id, url, meta).await;
            }
            EntryKind::Playlist {
                ref title,
                ref entries,
            } => {
                let title = title.clone();
                let children = entries.clone();
                self.promote(id, title, children, meta).await;
            }
            EntryKind::Video => {
                if entry.media_id.trim().is_empty() && entry.url.as_str().is_empty() {
                    self.fail_resolution(id, empty_data(&meta.provider)).await;
                    return;
                }
                self.resolve_single(id, entry, &meta).await;
            }
        }
    }

    /// One `Video` entry ⇒ the same item id (DESIGN §8.4).
    async fn resolve_single(&mut self, id: ItemId, entry: MediaEntry, meta: &ResolveMeta) {
        let Some(before) = self.cached(id) else {
            return;
        };
        let provider = meta.provider.clone();
        let media_id = (!entry.media_id.trim().is_empty()).then(|| entry.media_id.clone());
        let key = canonical_key(&provider, &entry.url, media_id.as_deref());
        let blob = crate::entry::compact_entry(&provider, &entry);
        let title = if entry.title.trim().is_empty() {
            Box::<str>::from(entry.url.as_str())
        } else {
            entry.title.clone()
        };

        if !self
            .apply(
                vec![WriteOp::SetResolved {
                    id,
                    provider: provider.clone(),
                    media_id: media_id.clone(),
                    title: title.clone(),
                    entry: blob.clone(),
                    canonical_key: key.clone(),
                }],
                Durability::Batched,
            )
            .await
        {
            return;
        }

        // Both keys stay in the index: the URL-derived one the item was added under, so
        // re-adding the same URL while it is still queued is caught, and the `media_id`-derived
        // one, so `youtu.be/x` and `youtube.com/watch?v=x` collapse onto it (DESIGN §8.5). This is
        // the legacy bug this closes: legacy checked only `queue`, so re-adding a *pending* URL
        // created a second entry that silently replaced the first.
        self.dedupe.insert(
            DedupeKey::new(key.clone(), before.request.selection.clone()),
            id,
        );
        self.patch(id, |item| {
            item.provider = Some(provider.clone());
            item.media_id = media_id.clone();
            item.title = title.clone();
            item.entry = blob.clone();
            item.canonical_key = key.clone();
        });

        let (error, auto_start) = match pre_error_of(&entry) {
            // A pre-download problem is **not** terminal: the row lands in `queued` with
            // `auto_start = false` and a populated `error` (DESIGN §8.4).
            Some(e) => (FieldUpdate::Set(e), false),
            // The **row's** flag, not the request's: they agree at insert (`crate::add`), and
            // they stop agreeing exactly when a `start` arrives while the row is still resolving
            // (PROTOCOL §4.2). Reading the request back here is what used to park such an item as
            // `queued(auto_start = false)` for ever.
            None => (FieldUpdate::Clear, before.auto_start),
        };
        if !self
            .write_status(
                id,
                Status::Queued,
                FieldUpdate::Clear,
                error,
                Some(auto_start),
            )
            .await
        {
            return;
        }
        if auto_start {
            self.enqueue(id);
        }
    }

    /// A `Playlist` (or a multi-entry answer) ⇒ in-place promotion (DESIGN §8.4, §8.6).
    async fn promote(
        &mut self,
        id: ItemId,
        title: Box<str>,
        entries: Vec<MediaEntry>,
        meta: ResolveMeta,
    ) {
        let Some(parent) = self.cached(id) else {
            return;
        };
        let limit = parent.request.playlist_item_limit;
        let mut children: VecDeque<MediaEntry> = entries.into();
        if limit > 0 {
            children.truncate(limit as usize);
        }
        let total = u32::try_from(children.len()).unwrap_or(u32::MAX);
        let provider = meta.provider.clone();
        let title = if title.trim().is_empty() {
            parent.title.clone()
        } else {
            title
        };

        // `PromoteToGroup` writes `kind`, `children_total` and `title` only, so without the
        // `SetStatus` the persisted row would keep the `resolving` status (and any fall-through
        // `msg`) the cache is about to drop: a restart at this point would then classify a
        // perfectly good group as an interrupted resolution (DESIGN §8.9). `sync_group_status`
        // cannot repair it either — it short-circuits, because the cached status already equals
        // the accumulator's roll-up.
        let at = self.clock.now_ms();
        if !self
            .apply(
                vec![
                    WriteOp::SetResolved {
                        id,
                        provider: provider.clone(),
                        media_id: None,
                        title: title.clone(),
                        entry: None,
                        canonical_key: parent.canonical_key.clone(),
                    },
                    WriteOp::PromoteToGroup {
                        id,
                        children_total: total,
                        title: title.clone(),
                    },
                    WriteOp::SetStatus {
                        id,
                        status: Status::Queued,
                        msg: FieldUpdate::Clear,
                        error: FieldUpdate::Clear,
                        auto_start: None,
                        at,
                    },
                ],
                Durability::Batched,
            )
            .await
        {
            return;
        }

        self.patch(id, |item| {
            item.kind = Kind::Group;
            item.provider = Some(provider.clone());
            item.title = title.clone();
            item.children_total = Some(total);
            item.status = Status::Queued;
            item.msg = None;
            item.error = None;
        });
        self.groups.insert(id, GroupAcc::new(total));
        // A group never downloads, so it holds no dedupe entry: re-adding the same playlist URL
        // while it is expanding is a legitimate second add of whatever it resolves to now.
        self.dedupe.retain(|_, v| *v != id);

        self.expansions.insert(
            id,
            Expansion {
                generation: meta.generation,
                epoch: meta.epoch,
                remaining: children,
                next_index: 1,
                provider,
                first_batch: true,
            },
        );
        self.handle_expand_next(id).await;
    }

    /// Inserts one batch of an expansion's children (DESIGN §8.4).
    pub(crate) async fn handle_expand_next(&mut self, group: GroupId) {
        let Some(state) = self.expansions.get_mut(&group) else {
            return;
        };
        if self.cancel_epoch > state.epoch {
            self.expansions.remove(&group);
            return;
        }
        let provider = state.provider.clone();
        let first_batch = state.first_batch;
        state.first_batch = false;
        let mut batch: Vec<MediaEntry> = Vec::with_capacity(CHILD_BATCH);
        let mut indices: Vec<u32> = Vec::with_capacity(CHILD_BATCH);
        while batch.len() < CHILD_BATCH {
            let Some(entry) = state.remaining.pop_front() else {
                break;
            };
            indices.push(state.next_index);
            state.next_index += 1;
            batch.push(entry);
        }
        let more = !state.remaining.is_empty();

        let Some(parent) = self.cached(group) else {
            self.expansions.remove(&group);
            return;
        };
        if batch.is_empty() {
            self.expansions.remove(&group);
            self.sync_group_status(group).await;
            return;
        }

        let now = self.clock.now_ms();
        let mut items: Vec<Item> = Vec::with_capacity(batch.len());
        for (entry, index) in batch.into_iter().zip(indices) {
            items.push(self.child_row(&parent, &provider, &entry, index, now));
        }
        if !self
            .apply(
                vec![WriteOp::InsertItems {
                    items: items.clone(),
                }],
                Durability::Batched,
            )
            .await
        {
            self.expansions.remove(&group);
            return;
        }

        let mut views = Vec::with_capacity(items.len() + 1);
        let mut startable = Vec::with_capacity(items.len());
        for item in items {
            let id = item.id;
            let auto_start = item.auto_start;
            let arc = self.cache_insert(item);
            views.push(self.view(&arc));
            if auto_start {
                startable.push(id);
            }
        }
        // The first flush carries the updated group view too, so a client that only ever saw an
        // `item` row learns it is now a `group` in the same frame as its first children.
        if first_batch && let Some(view) = self.view_of(group) {
            views.insert(0, view);
        }
        self.publish_added(views, AddReason::Expanded).await;
        for id in startable {
            self.enqueue(id);
        }
        self.sync_group_status(group).await;
        self.schedule().await;

        if more {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(EngineCmd::ExpandNext { group }).await;
            });
        } else {
            self.expansions.remove(&group);
        }
    }

    /// One child row, inheriting the parent's request (DESIGN §8.4).
    fn child_row(
        &self,
        parent: &Item,
        provider: &ProviderId,
        entry: &MediaEntry,
        index: u32,
        now: i64,
    ) -> Item {
        let media_id = (!entry.media_id.trim().is_empty()).then(|| entry.media_id.clone());
        let mut request = parent.request.clone();
        request.url = entry.url.clone();
        let pre_error = pre_error_of(entry);
        // A `pre_error` child is `queued(auto_start = false)` with a populated error — never
        // `status = error`, which would put an upcoming livestream in the shipped client's Failed
        // section and stop it ever starting (DESIGN §8.4).
        // The parent's live flag, for the same reason `resolve_single` reads the row rather than
        // the request: a `start` sent while the playlist was still resolving has to reach the
        // children it is about to produce.
        let auto_start = pre_error.is_none() && parent.auto_start;
        let title = if entry.title.trim().is_empty() {
            Box::<str>::from(entry.url.as_str())
        } else {
            entry.title.clone()
        };
        Item {
            id: ItemId::new(),
            kind: Kind::Item,
            group_id: Some(parent.id),
            group_index: Some(index),
            ord: self.store.next_ord(),
            url: entry.url.clone(),
            canonical_key: canonical_key(provider, &entry.url, media_id.as_deref()),
            provider: Some(provider.clone()),
            media_id,
            title,
            status: Status::Queued,
            auto_start,
            msg: None,
            error: pre_error,
            request,
            entry: crate::entry::compact_entry(provider, entry),
            filename: None,
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: now,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: parent.source.clone(),
            children_total: None,
            clear_after: None,
        }
    }

    /// A `Redirect` ⇒ re-enter resolution, depth-capped (DESIGN §8.4).
    async fn on_redirect(&mut self, id: ItemId, url: url::Url, mut meta: ResolveMeta) {
        let here = Box::<str>::from(url.as_str());
        let looped = meta.seen.contains(&here);
        meta.depth += 1;
        if looped || meta.depth > self.cfg.resolve_max_depth {
            let message = if looped {
                format!("Redirect loop at {url}")
            } else {
                format!("Too many redirects resolving {url}")
            };
            self.fail_resolution(
                id,
                WireError::new(ErrorCode::UnsupportedUrl, message)
                    .with_provider(meta.provider.as_arc(), None),
            )
            .await;
            return;
        }
        meta.seen.push(here);
        let generation = meta.generation;
        self.spawn_resolve(id, generation, Some(meta)).await;
    }

    /// The failure branch, including the one documented fall-through (DESIGN §8.4, §6.4).
    async fn on_resolve_error(&mut self, id: ItemId, err: ProviderError, meta: ResolveMeta) {
        let may_fall_through = matches!(err, ProviderError::Unsupported(_))
            && self.cfg.resolve_fallthrough
            && !meta.fell_through;
        if may_fall_through
            && let Some(runner_up) = meta.runner_up.clone()
            && self
                .provider_of(&runner_up)
                .is_some_and(|s| s.degraded.is_none())
        {
            let msg = format!("Retrying with {runner_up}");
            tracing::info!(item = %id, from = %meta.provider, to = %runner_up, "{msg}");
            self.write_status(
                id,
                Status::Resolving,
                FieldUpdate::Set(msg.into_boxed_str()),
                FieldUpdate::Keep,
                None,
            )
            .await;
            let generation = meta.generation;
            let next = ResolveMeta {
                fell_through: true,
                provider: runner_up,
                runner_up: None,
                ..meta
            };
            self.spawn_resolve(id, generation, Some(next)).await;
            return;
        }
        self.fail_resolution(id, err.to_wire(&meta.provider, None))
            .await;
    }

    /// A resolution that ends the item.
    pub(crate) async fn fail_resolution(&mut self, id: ItemId, error: WireError) {
        self.terminate(id, Status::Error, FieldUpdate::Set(error))
            .await;
    }
}

/// The verbatim empty-resolution error, attributed to the provider that produced it.
fn empty_data(provider: &ProviderId) -> WireError {
    WireError::new(ErrorCode::UnsupportedUrl, INVALID_EMPTY_DATA)
        .with_provider(provider.as_arc(), None)
}

/// The `pre_error` a resolved entry carries, as the row's `error` (DESIGN §8.4).
///
/// The text is preserved verbatim, including
/// `Live stream is scheduled to start at {ts:%Y-%m-%d %H:%M:%S %z}`; only the code is decided
/// here — `not_yet_live` for an upcoming stream, whatever the provider said otherwise.
#[must_use]
pub fn pre_error_of(entry: &MediaEntry) -> Option<WireError> {
    let error = entry.pre_error.clone()?;
    if entry.live.is_upcoming() && error.code != ErrorCode::NotYetLive {
        return Some(WireError::new(
            ErrorCode::NotYetLive,
            Arc::clone(&error.message),
        ));
    }
    Some(error)
}

/// One resolution, running off the engine task (DESIGN §8.4).
struct ResolveJob {
    id: ItemId,
    url: url::Url,
    provider: Arc<dyn Provider>,
    item: Arc<Item>,
    pool: Arc<Semaphore>,
    ytdl: Arc<aulos_core::YtdlOptions>,
    paths: aulos_core::Paths,
    timeout: u64,
    cancel: CancellationToken,
    tx: mpsc::Sender<EngineCmd>,
    meta: ResolveMeta,
}

impl ResolveJob {
    /// Acquires a pool permit, calls `resolve` under the deadline, and reports back.
    async fn run(self) {
        let Ok(_permit) = Arc::clone(&self.pool).acquire_owned().await else {
            return; // shutdown closed the pool
        };
        let window = std::time::Duration::from_secs(self.timeout.max(1));
        let deadline = tokio::time::Instant::now() + window;
        let playlist_end = (self.item.request.playlist_item_limit > 0)
            .then_some(self.item.request.playlist_item_limit);
        let ctx = ResolveCtx {
            item_id: self.id,
            request: &self.item.request,
            ytdl_options: Arc::clone(&self.ytdl),
            paths: &self.paths,
            flat: false,
            playlist_end,
            cancel: self.cancel.clone(),
            deadline,
        };
        let result =
            match tokio::time::timeout_at(deadline, self.provider.resolve(&self.url, ctx)).await {
                Ok(r) => r,
                Err(_) => Err(ProviderError::Timeout(format!(
                    "resolution timed out after {}s",
                    window.as_secs()
                ))),
            };
        let _ = self
            .tx
            .send(EngineCmd::Resolved {
                id: self.id,
                result,
                meta: Box::new(self.meta),
            })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use aulos_provider::entry::LiveStatus;

    use super::*;

    fn url() -> url::Url {
        url::Url::parse("https://example.test/v").unwrap()
    }

    #[test]
    fn the_two_verbatim_legacy_strings_are_undecorated() {
        assert_eq!(INVALID_EMPTY_DATA, "Invalid/empty data was given.");
        assert_eq!(
            unsupported_resource("url_result"),
            "Unsupported resource \"url_result\""
        );
        let e = empty_data(&ProviderId::parse("ytdlp").unwrap());
        assert_eq!(&*e.message, INVALID_EMPTY_DATA);
        assert_eq!(e.code, ErrorCode::UnsupportedUrl);
        assert_eq!(e.provider.as_deref(), Some("ytdlp"));
    }

    #[test]
    fn an_upcoming_stream_keeps_its_text_and_gets_the_not_yet_live_code() {
        let mut entry = MediaEntry::video("x", "Premiere", url());
        entry.live = LiveStatus::IsUpcoming { at: Some(17) };
        entry.pre_error = Some(WireError::new(
            ErrorCode::Unavailable,
            "Live stream is scheduled to start at 2026-09-04 18:00:00 +0000",
        ));
        let mapped = pre_error_of(&entry).expect("a pre_error");
        assert_eq!(mapped.code, ErrorCode::NotYetLive);
        assert_eq!(
            &*mapped.message, "Live stream is scheduled to start at 2026-09-04 18:00:00 +0000",
            "the legacy text is preserved byte for byte"
        );

        // An entry-level `msg` on a non-live entry keeps the provider's own code.
        let mut other = MediaEntry::video("y", "Clip", url());
        other.pre_error = Some(WireError::new(ErrorCode::UnsupportedUrl, "odd"));
        assert_eq!(
            pre_error_of(&other).map(|e| e.code),
            Some(ErrorCode::UnsupportedUrl)
        );
        assert!(pre_error_of(&MediaEntry::video("z", "z", url())).is_none());
    }
}
