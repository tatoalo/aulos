//! Fill missing release titles without turning a playlist preview into an unbounded scan.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::ErrorCode;
use aulos_core::subscription::SubscriptionRecord;
use aulos_provider::{EntryKind, MediaEntry, ProviderError, ResolveCtx};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::Checker;

fn missing_title(entry: &MediaEntry) -> bool {
    let title = entry.title.trim();
    title.is_empty()
        || Url::parse(title).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
        || title.starts_with("youtube video #")
        || matches!(
            title.to_ascii_lowercase().as_str(),
            "[private video]" | "[deleted video]"
        )
}

fn needs_details(entry: &MediaEntry) -> bool {
    missing_title(entry)
        || entry
            .pre_error
            .as_ref()
            .is_some_and(|error| error.code != ErrorCode::NotYetLive)
        || matches!(
            entry
                .state
                .get("availability")
                .and_then(serde_json::Value::as_str),
            Some("private" | "needs_auth" | "subscriber_only" | "premium_only")
        )
}

impl Checker {
    pub(super) async fn resolve_release_titles(
        &self,
        record: &SubscriptionRecord,
        entries: &mut Vec<MediaEntry>,
    ) -> (u32, u32) {
        let mut pending = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| needs_details(entry).then_some(index))
            .collect::<Vec<_>>()
            .into_iter();
        let mut hidden: HashSet<usize> = pending.clone().collect();
        let mut unavailable = 0;
        let deadline =
            self.clock.instant() + Duration::from_secs(self.cfg.resolve_timeout_secs.clamp(1, 20));
        let mut tasks = JoinSet::new();

        loop {
            while tasks.len() < 4 && self.clock.instant() < deadline {
                let Some(index) = pending.next() else { break };
                let entry = &entries[index];
                let provider = {
                    let registry = self
                        .registry
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    registry
                        .pick(&entry.url, None)
                        .and_then(|choice| registry.by_id(&choice.id).cloned())
                };
                let Some(provider) = provider else { continue };
                let request = self.request_for(record, entry);
                let cfg = Arc::clone(&self.cfg);
                let options = self.ytdl.current();
                tasks.spawn(async move {
                    let cancel = CancellationToken::new();
                    let result = tokio::time::timeout_at(
                        deadline,
                        provider.resolve(
                            &request.url,
                            ResolveCtx {
                                item_id: aulos_core::ItemId::new(),
                                request: &request,
                                ytdl_options: options,
                                paths: &cfg.paths,
                                flat: false,
                                playlist_end: Some(1),
                                cancel: cancel.clone(),
                                deadline,
                            },
                        ),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(ProviderError::Timeout("Release details timed out".into()))
                    });
                    cancel.cancel();
                    (index, result)
                });
            }
            let Some(result) = tasks.join_next().await else {
                break;
            };
            let Ok((index, result)) = result else {
                continue;
            };
            match result {
                Ok(resolved) => {
                    if let Some(resolved) = resolved.into_iter().find(|entry| {
                        matches!(entry.kind, EntryKind::Video) && !missing_title(entry)
                    }) {
                        if let Some(error) = &resolved.pre_error
                            && error.code != ErrorCode::NotYetLive
                        {
                            if matches!(
                                error.code,
                                ErrorCode::AuthRequired
                                    | ErrorCode::Unavailable
                                    | ErrorCode::GeoRestricted
                            ) {
                                unavailable += 1;
                            }
                            continue;
                        }
                        // Keep the listing identity and URL stable for selection and history.
                        let entry = &mut entries[index];
                        entry.title = resolved.title;
                        entry.hints = resolved.hints;
                        entry.live = resolved.live;
                        entry.pre_error = resolved.pre_error;
                        hidden.remove(&index);
                    }
                }
                Err(
                    ProviderError::AuthRequired(_)
                    | ProviderError::Unavailable(_)
                    | ProviderError::GeoRestricted(_),
                ) => {
                    unavailable += 1;
                }
                Err(error) => {
                    tracing::debug!(subscription = %record.id, code = ?error.code(), "release details could not be loaded");
                }
            }
        }
        let unresolved = hidden.len() as u32 - unavailable;
        let mut index = 0;
        entries.retain(|_| {
            let keep = !hidden.contains(&index);
            index += 1;
            keep
        });
        (unavailable, unresolved)
    }
}
