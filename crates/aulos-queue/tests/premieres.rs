//! Subscription premieres wait for a recording without becoming failed downloads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aulos_core::{ErrorCode, SourceKind, SourceRef, Status};
use aulos_provider::{
    DownloadCtx, LiveStatus, Match, MediaEntry, Outcome, ProgressSink, Provider, ProviderError,
    ResolveCtx,
};
use aulos_queue::Action;
use support::{Harness, fake, request};

struct Premiere {
    stage: AtomicUsize,
    downloads: AtomicUsize,
}

impl Premiere {
    fn new(stage: usize) -> Arc<Self> {
        Arc::new(Self {
            stage: AtomicUsize::new(stage),
            downloads: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Premiere {
    fn id(&self) -> aulos_provider::ProviderId {
        fake().id()
    }
    fn matches(&self, url: &url::Url) -> Match {
        fake().matches(url)
    }
    fn catalog(&self) -> Arc<aulos_core::FormatCatalog> {
        fake().catalog()
    }

    async fn resolve(
        &self,
        url: &url::Url,
        _: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        if self.stage.load(Ordering::SeqCst) == 0 {
            return Err(ProviderError::NotYetLive("Premieres in 14 hours".into()));
        }
        let mut entry = MediaEntry::video("premiere", "Premiere", url.clone());
        entry.live = LiveStatus::IsUpcoming { at: None };
        Ok(vec![entry])
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        _: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        assert_eq!(ctx.source, SourceKind::Subscription);
        self.downloads.fetch_add(1, Ordering::SeqCst);
        match self.stage.load(Ordering::SeqCst) {
            0 | 1 => Err(ProviderError::NotYetLive("Premieres in 14 hours".into())),
            2 => Err(ProviderError::NotYetLive("Premiere is live".into())),
            3 => Err(ProviderError::NotYetLive("Recording is processing".into())),
            _ => Ok(Outcome::default()),
        }
    }
}

async fn add_subscription(h: &Harness) -> aulos_core::ItemId {
    h.handle
        .add(
            vec![request("https://fake.test/watch/premiere")],
            SourceRef::with_ref(SourceKind::Subscription, "channel"),
        )
        .await
        .unwrap()
        .ids[0]
}

async fn waiting(h: &Harness, id: aulos_core::ItemId) -> aulos_core::Item {
    let row = h
        .until(id, "waiting for the recording", |i| {
            i.status == Status::Queued
                && i.error
                    .as_ref()
                    .is_some_and(|e| e.code == ErrorCode::NotYetLive)
        })
        .await;
    h.settle().await;
    row
}

#[tokio::test]
async fn a_premiere_waits_through_resolution_live_and_processing_without_spending_retries() {
    let provider = Premiere::new(0);
    let h = Harness::builder()
        .provider(provider.clone())
        .env("AULOS_AUTO_RETRY_MAX", "0")
        .build()
        .await;
    let id = add_subscription(&h).await;
    let first = waiting(&h, id).await;
    assert!(first.auto_start);
    assert!(first.provider.is_none());
    assert_eq!(first.attempt, 0);
    h.advance(Duration::from_secs(14 * 60)).await;
    assert_eq!(provider.downloads.load(Ordering::SeqCst), 0);

    provider.stage.store(1, Ordering::SeqCst);
    h.advance(Duration::from_secs(60)).await;
    h.until(id, "premiere metadata", |i| {
        i.provider.is_some() && i.status == Status::Queued
    })
    .await;
    h.settle().await;
    assert_eq!(provider.downloads.load(Ordering::SeqCst), 0);

    for stage in 2..=3 {
        provider.stage.store(stage, Ordering::SeqCst);
        h.advance(Duration::from_secs(15 * 60)).await;
        let expected = if stage == 2 {
            "Premiere is live"
        } else {
            "Recording is processing"
        };
        h.until(id, expected, |i| {
            i.error.as_ref().is_some_and(|e| &*e.message == expected)
        })
        .await;
        let row = waiting(&h, id).await;
        assert_eq!(row.attempt, 0);
        assert!(row.finished_at.is_none());
        assert_eq!(provider.downloads.load(Ordering::SeqCst), stage - 1);
        assert!(h.events.completed().is_empty());
    }

    provider.stage.store(4, Ordering::SeqCst);
    h.advance(Duration::from_secs(15 * 60)).await;
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.attempt, 0);
    assert!(done.error.is_none());
    assert_eq!(h.events.added().len(), 1);
}

#[tokio::test]
async fn a_waiting_subscription_survives_restart_and_cancel_stops_its_rechecks() {
    let provider = Premiere::new(1);
    let h = Harness::builder().provider(provider.clone()).build().await;
    let id = add_subscription(&h).await;
    let row = waiting(&h, id).await;

    let recovered = Harness::builder()
        .provider(provider.clone())
        .seed(vec![row])
        .recovering()
        .build()
        .await;
    waiting(&recovered, id).await;
    recovered
        .handle
        .actions(Action::Cancel, vec![id], None)
        .await;
    let attempts = provider.downloads.load(Ordering::SeqCst);
    provider.stage.store(4, Ordering::SeqCst);
    recovered.advance(Duration::from_secs(30 * 60)).await;
    assert_eq!(recovered.item(id).await.unwrap().status, Status::Canceled);
    assert_eq!(provider.downloads.load(Ordering::SeqCst), attempts);
}
