//! Existing releases, explicit selections and durable download-history checks.

use std::collections::HashSet;
use std::time::Duration;

use aulos_core::subscription::{
    ReleaseDownloadResult, ReleaseSkip, SubError, SubscriptionRecord, SubscriptionRelease,
    SubscriptionReleases,
};
use aulos_core::{ItemId, SubId};
use aulos_provider::{LiveStatus, MediaEntry};
use aulos_store::{Durability, ReleaseIdentity, ReleaseState, WriteOp, canonical_key};
use tokio::time::Instant;

use super::{Checker, media_id_of};

const PREVIEW_TTL: Duration = Duration::from_secs(600);
const MAX_PREVIEWS: usize = 16;

pub(super) struct Preview {
    id: Box<str>,
    subscription: SubId,
    expires_at: Instant,
    entries: Vec<MediaEntry>,
}

fn invalid(field: &str, message: &str) -> SubError {
    SubError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

impl Checker {
    pub(super) async fn states_for(
        &self,
        record: &SubscriptionRecord,
        entries: &[MediaEntry],
    ) -> Result<Vec<ReleaseState>, SubError> {
        let identities = {
            let registry = self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries
                .iter()
                .map(|entry| {
                    // Unsupported entries still reach queue validation individually.
                    let provider = registry.pick(&entry.url, None);
                    let provider_id = provider.as_ref().map_or("", |p| p.id.as_str());
                    ReleaseIdentity {
                        canonical_key: canonical_key(
                            provider_id,
                            entry.url.as_str(),
                            Some(&media_id_of(entry)),
                        ),
                        url_key: canonical_key(provider_id, entry.url.as_str(), None),
                    }
                })
                .collect::<Vec<_>>()
        };
        self.store
            .release_states(identities, record.selection.download_type.as_str())
            .await
            .map_err(|e| SubError::Other(e.to_string().into_boxed_str()))
    }

    pub(super) async fn browse_releases(
        &self,
        record: &SubscriptionRecord,
        limit: u32,
    ) -> Result<SubscriptionReleases, SubError> {
        let limit = limit.clamp(1, 1000);
        let mut feed = self
            .probe_limited(&record.url, 0, limit)
            .await
            .map_err(|e| SubError::Other(e.to_string().into_boxed_str()))?;
        let mut unique = HashSet::new();
        feed.entries
            .retain(|entry| unique.insert(media_id_of(entry)));
        feed.entries.truncate(limit as usize);
        let states = self.states_for(record, &feed.entries).await?;
        let items = feed
            .entries
            .iter()
            .zip(states)
            .map(|(entry, state)| SubscriptionRelease {
                id: media_id_of(entry),
                url: entry.url.as_str().into(),
                title: entry.title.clone(),
                duration: entry.hints.duration.filter(|v| v.is_finite() && *v >= 0.0),
                published_date: entry.hints.published_date.clone(),
                channel: entry
                    .hints
                    .uploader
                    .clone()
                    .or_else(|| entry.hints.channel_title.clone()),
                waiting_for_release: entry.live.is_upcoming() || entry.live == LiveStatus::IsLive,
                downloaded_at: state.downloaded_at,
                queued_item_id: state.queued_item_id,
            })
            .collect();
        // Selection needs identity and URL only. Do not retain provider state or large metadata.
        let entries = feed
            .entries
            .into_iter()
            .map(|entry| {
                let mut cached = MediaEntry::video(media_id_of(&entry), entry.title, entry.url);
                cached.live = entry.live;
                cached
            })
            .collect();
        let preview_id = ItemId::new().to_string().into_boxed_str();
        let now = self.clock.instant();
        let mut previews = self.previews.lock().await;
        previews.retain(|preview| preview.expires_at > now);
        while previews.len() >= MAX_PREVIEWS {
            previews.pop_front();
        }
        previews.push_back(Preview {
            id: preview_id.clone(),
            subscription: record.id.clone(),
            expires_at: now + PREVIEW_TTL,
            entries,
        });
        Ok(SubscriptionReleases {
            preview_id,
            subscription_id: record.id.clone(),
            items,
            limit,
            has_more: feed.limit_reached,
        })
    }

    pub(super) async fn queue_releases(
        &self,
        record: &SubscriptionRecord,
        preview_id: &str,
        ids: &[Box<str>],
        force: bool,
    ) -> Result<ReleaseDownloadResult, SubError> {
        if ids.len() > 1000 {
            return Err(invalid("ids", "Select at most 1000 releases"));
        }
        let (entries, ignored) = {
            let previews = self.previews.lock().await;
            let preview = previews
                .iter()
                .find(|p| {
                    p.id.as_ref() == preview_id
                        && p.subscription == record.id
                        && p.expires_at > self.clock.instant()
                })
                .ok_or_else(|| {
                    invalid(
                        "preview_id",
                        "This release list expired. Reload it and select again.",
                    )
                })?;
            let requested: HashSet<&str> = ids.iter().map(AsRef::as_ref).collect();
            let entries: Vec<_> = preview
                .entries
                .iter()
                .filter(|entry| requested.contains(entry.media_id.as_ref()))
                .cloned()
                .collect();
            if entries.len() != requested.len() {
                return Err(invalid(
                    "ids",
                    "A selected release is not in this list. Reload it and select again.",
                ));
            }
            let ignored: Vec<_> = preview
                .entries
                .iter()
                .filter(|entry| {
                    !requested.contains(entry.media_id.as_ref())
                        && !entry.live.is_upcoming()
                        && entry.live != LiveStatus::IsLive
                })
                .map(media_id_of)
                .collect();
            (entries, ignored)
        };
        // Serialize explicit selections with scheduled checks, then recheck current DB state.
        let _guard = self.queue_lock.lock().await;
        let record = self
            .store
            .subscription(&record.id)
            .await
            .map_err(|e| SubError::Other(e.to_string().into_boxed_str()))?
            .ok_or_else(|| SubError::NotFound(record.id.clone()))?;
        let states = self.states_for(&record, &entries).await?;
        let mut result = ReleaseDownloadResult::default();
        let entries: Vec<_> = entries
            .into_iter()
            .zip(states)
            .filter_map(|(entry, state)| {
                let reason = if state.queued_item_id.is_some() {
                    Some("already_queued")
                } else if state.downloaded_at.is_some() && !force {
                    Some("already_downloaded")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    result.skipped.push(ReleaseSkip {
                        id: media_id_of(&entry),
                        reason: reason.into(),
                    });
                    None
                } else {
                    Some(entry)
                }
            })
            .collect();
        (result.queued, result.errors) = self.queue(&record, &entries).await;
        let handled: Vec<_> = result
            .queued
            .iter()
            .cloned()
            .chain(result.skipped.iter().map(|skip| skip.id.clone()))
            .chain(ignored)
            .collect();
        if !handled.is_empty() {
            self.store
                .write(
                    vec![
                        WriteOp::MarkSeen {
                            sub: record.id.clone(),
                            ids: handled,
                            at: self.clock.now_ms(),
                        },
                        WriteOp::PruneSeen {
                            sub: record.id.clone(),
                            keep: self.cfg.subscription_max_seen_ids,
                        },
                    ],
                    Durability::Sync,
                )
                .await
                .map_err(|e| SubError::Other(e.to_string().into_boxed_str()))?;
        }
        Ok(result)
    }
}
