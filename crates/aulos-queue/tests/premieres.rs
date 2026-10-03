//! Subscription premieres wait for a recording without becoming failed downloads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use aulos_core::{Clock, ErrorCode, SourceKind, SourceRef, Status};
use aulos_provider::{
    DownloadCtx, LiveStatus, Match, MediaEntry, Outcome, ProgressSink, Provider, ProviderError,
    ResolveCtx,
};
use aulos_queue::Action;
use support::{Harness, fake, request};

struct Premiere {
    stage: AtomicUsize,
    downloads: AtomicUsize,
    resolutions: AtomicUsize,
    release_at: AtomicI64,
}

impl Premiere {
    fn new(stage: usize) -> Arc<Self> {
        Arc::new(Self {
            stage: AtomicUsize::new(stage),
            downloads: AtomicUsize::new(0),
            resolutions: AtomicUsize::new(0),
            release_at: AtomicI64::new(0),
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
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let at = self.release_at.load(Ordering::SeqCst);
        let retry_at = (at > 0).then_some(at);
        if self.stage.load(Ordering::SeqCst) == 0 {
            return Err(ProviderError::NotYetLive {
                message: "Premieres in 14 hours".into(),
                retry_at,
            });
        }
        let mut entry = MediaEntry::video("premiere", "Premiere", url.clone());
        entry.live = match self.stage.load(Ordering::SeqCst) {
            1 => LiveStatus::IsUpcoming { at: retry_at },
            2 => LiveStatus::IsLive,
            3 => LiveStatus::WasLive,
            _ => LiveStatus::NotLive,
        };
        Ok(vec![entry])
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        _: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        assert_eq!(ctx.source, SourceKind::Subscription);
        self.downloads.fetch_add(1, Ordering::SeqCst);
        let at = self.release_at.load(Ordering::SeqCst);
        match self.stage.load(Ordering::SeqCst) {
            0 | 1 => Err(ProviderError::NotYetLive {
                message: "Premieres in 14 hours".into(),
                retry_at: (at > 0).then_some(at),
            }),
            2 => Err(ProviderError::NotYetLive {
                message: "Premiere is live".into(),
                retry_at: None,
            }),
            3 => Err(ProviderError::NotYetLive {
                message: "Recording is processing".into(),
                retry_at: None,
            }),
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

#[tokio::test]
async fn announced_release_waits_until_due_then_polls_every_fifteen_minutes() {
    for stage in [0, 1] {
        let provider = Premiere::new(stage);
        let h = Harness::builder().provider(provider.clone()).build().await;
        let release = h.clock.now_ms() + 6 * 60 * 60 * 1_000;
        provider.release_at.store(release, Ordering::SeqCst);
        let id = add_subscription(&h).await;
        let first = waiting(&h, id).await;
        assert_eq!(first.error.as_ref().unwrap().retry_at, Some(release));
        assert!(first.msg.as_ref().unwrap().contains("06:00 UTC"));
        assert_eq!(first.attempt, 0);
        h.advance(Duration::from_secs(6 * 60 * 60 - 1)).await;
        assert_eq!(provider.resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(provider.downloads.load(Ordering::SeqCst), 0);

        provider.stage.store(2, Ordering::SeqCst);
        h.advance(Duration::from_secs(1)).await;
        h.until(id, "first due check", |i| {
            i.error
                .as_ref()
                .is_some_and(|e| e.retry_at == Some(release + 15 * 60 * 1_000))
        })
        .await;
        h.settle().await;
        let downloads = provider.downloads.load(Ordering::SeqCst);
        provider.stage.store(4, Ordering::SeqCst);
        h.advance(Duration::from_secs(15 * 60 - 1)).await;
        assert_eq!(provider.downloads.load(Ordering::SeqCst), downloads);
        h.advance(Duration::from_secs(1)).await;
        let done = h.until_status(id, Status::Finished).await;
        assert_eq!(done.attempt, 0);
        assert!(done.error.is_none());
    }
}

#[tokio::test]
async fn release_schedule_survives_restart_pause_and_resume() {
    for stage in [0, 1] {
        let provider = Premiere::new(stage);
        let h = Harness::builder().provider(provider.clone()).build().await;
        let release = h.clock.now_ms() + 6 * 60 * 60 * 1_000;
        provider.release_at.store(release, Ordering::SeqCst);
        let id = add_subscription(&h).await;
        let row = waiting(&h, id).await;
        let recovered = Harness::builder()
            .provider(provider.clone())
            .seed(vec![row])
            .recovering()
            .build()
            .await;
        recovered.settle().await;
        assert_eq!(provider.resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(provider.downloads.load(Ordering::SeqCst), 0);
        recovered
            .handle
            .actions(Action::Pause, vec![id], None)
            .await;
        recovered.advance(Duration::from_secs(60 * 60)).await;
        recovered
            .handle
            .actions(Action::Start, vec![id], None)
            .await;
        recovered
            .advance(Duration::from_secs(5 * 60 * 60 - 1))
            .await;
        assert_eq!(provider.resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(provider.downloads.load(Ordering::SeqCst), 0);
        provider.stage.store(4, Ordering::SeqCst);
        provider.release_at.store(0, Ordering::SeqCst);
        recovered.advance(Duration::from_secs(1)).await;
        recovered.until_status(id, Status::Finished).await;
    }
}

#[tokio::test]
async fn a_new_release_time_from_download_replaces_the_previous_schedule() {
    let provider = Premiere::new(1);
    let h = Harness::builder().provider(provider.clone()).build().await;
    let id = add_subscription(&h).await;
    waiting(&h, id).await;
    let release = h.clock.now_ms() + 6 * 60 * 60 * 1_000;
    provider.release_at.store(release, Ordering::SeqCst);
    h.advance(Duration::from_secs(15 * 60)).await;
    h.until(id, "updated release time", |i| {
        i.error
            .as_ref()
            .is_some_and(|e| e.retry_at == Some(release))
    })
    .await;
    h.advance(Duration::from_secs(5 * 60 * 60 + 45 * 60 - 1))
        .await;
    assert_eq!(provider.downloads.load(Ordering::SeqCst), 1);
    provider.stage.store(4, Ordering::SeqCst);
    h.advance(Duration::from_secs(1)).await;
    h.until_status(id, Status::Finished).await;
}

#[tokio::test]
async fn live_streams_and_past_release_times_start_with_fifteen_minute_checks() {
    for stage in [1, 2] {
        let provider = Premiere::new(stage);
        let h = Harness::builder().provider(provider.clone()).build().await;
        let now = h.clock.now_ms();
        provider.release_at.store(now - 1_000, Ordering::SeqCst);
        let id = add_subscription(&h).await;
        let row = waiting(&h, id).await;
        assert_eq!(
            row.error.as_ref().unwrap().retry_at,
            Some(now + 15 * 60 * 1_000)
        );
        assert!(row.msg.as_ref().unwrap().contains("in 15 minutes"));
        h.advance(Duration::from_secs(15 * 60 - 1)).await;
        assert_eq!(provider.downloads.load(Ordering::SeqCst), 0);
        provider.stage.store(4, Ordering::SeqCst);
        h.advance(Duration::from_secs(1)).await;
        h.until_status(id, Status::Finished).await;
    }
}
