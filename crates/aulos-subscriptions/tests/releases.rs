//! Existing-release browsing, selection and durable duplicate protection.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;

use aulos_core::subscription::{ReleaseDownloadResult, SubCmd, SubError, SubscriptionReleases};
use aulos_core::{DownloadRequest, Item, ItemId, Kind, SourceKind, SourceRef, Status, SubId};
use aulos_store::{Durability, WriteOp, canonical_key};
use support::{Harness, ScriptProvider, entry, feed_url, selection};
use tokio::sync::oneshot;

async fn browse(h: &Harness, id: &SubId, limit: u32) -> SubscriptionReleases {
    let (ack, rx) = oneshot::channel();
    h.handle
        .send(SubCmd::Releases {
            id: id.clone(),
            limit,
            ack,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap()
}
async fn submit(
    h: &Harness,
    listing: &SubscriptionReleases,
    ids: &[&str],
    force: bool,
) -> Result<ReleaseDownloadResult, SubError> {
    let (ack, rx) = oneshot::channel();
    h.handle
        .send(SubCmd::DownloadReleases {
            id: listing.subscription_id.clone(),
            preview_id: listing.preview_id.clone(),
            ids: ids.iter().map(|id| Box::from(*id)).collect(),
            force,
            ack,
        })
        .await
        .unwrap();
    rx.await.unwrap()
}
async fn setup(
    entries: Vec<aulos_provider::MediaEntry>,
) -> (Harness, std::sync::Arc<ScriptProvider>, SubId) {
    let provider = ScriptProvider::new();
    provider.set_feed(&feed_url("one"), entries);
    let h = Harness::builder().provider(provider.clone()).build().await;
    let mut request = DownloadRequest::new(feed_url("one").parse().unwrap(), selection());
    request.auto_start = false;
    let sub = h
        .subscribe_with(request, None, Some("Lecture series"))
        .await
        .unwrap();
    (h, provider, sub.id)
}
async fn remembered(h: &Harness, media: &str, status: Status) -> ItemId {
    let e = entry(media);
    let id = ItemId::new();
    let item = Item {
        id,
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord: 900,
        url: e.url.clone(),
        canonical_key: canonical_key("script", e.url.as_str(), Some(media)),
        provider: None,
        media_id: Some(media.into()),
        title: e.title,
        status,
        auto_start: false,
        msg: None,
        error: None,
        request: DownloadRequest::new(e.url, selection()),
        entry: None,
        filename: None,
        size: None,
        chapter_files: vec![],
        subtitle_files: vec![],
        created_at: 1,
        started_at: None,
        finished_at: Some(42),
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV2),
        children_total: None,
        clear_after: None,
    };
    h.store
        .write(
            vec![WriteOp::InsertItems { items: vec![item] }],
            Durability::Sync,
        )
        .await
        .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn browsing_is_read_only_and_selection_keeps_the_subscription_name() {
    let mut video = entry("one");
    video.hints.duration = Some(61.0);
    video.hints.published_date = Some("2026-11-17".into());
    video.hints.uploader = Some("Lecturer".into());
    let (h, _, id) = setup(vec![video, entry("two")]).await;
    let seen = h.seen(&id).await;
    let listing = browse(&h, &id, 100).await;
    assert_eq!(listing.items.len(), 2);
    assert_eq!(listing.items[0].duration, Some(61.0));
    assert_eq!(
        listing.items[0].published_date.as_deref(),
        Some("2026-11-17")
    );
    assert_eq!(listing.items[0].channel.as_deref(), Some("Lecturer"));
    assert_eq!(seen, h.seen(&id).await);
    assert!(h.items().await.is_empty());
    let result = submit(&h, &listing, &["one"], false).await.unwrap();
    assert_eq!(result.queued, [Box::<str>::from("one")]);
    assert!(result.errors.is_empty());
    let items = h.items().await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source.name.as_deref(), Some("Lecture series"));
    h.delete(vec![id]).await;
    assert_eq!(
        h.items().await[0].source.name.as_deref(),
        Some("Lecture series")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_survives_deleted_items_and_requires_an_explicit_override() {
    let (h, _, id) = setup(vec![entry("old")]).await;
    let item = remembered(&h, "old", Status::Finished).await;
    h.store
        .write(vec![WriteOp::DeleteItems(vec![item])], Durability::Sync)
        .await
        .unwrap();
    let listing = browse(&h, &id, 100).await;
    assert_eq!(listing.items[0].downloaded_at, Some(42));
    let skipped = submit(&h, &listing, &["old"], false).await.unwrap();
    assert!(skipped.queued.is_empty());
    assert_eq!(skipped.skipped[0].reason.as_ref(), "already_downloaded");
    let forced = submit(&h, &listing, &["old"], true).await.unwrap();
    assert_eq!(forced.queued.len(), 1);
    assert!(forced.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn force_never_duplicates_an_active_item() {
    let (h, _, id) = setup(vec![entry("old")]).await;
    let item = remembered(&h, "old", Status::Queued).await;
    let listing = browse(&h, &id, 100).await;
    assert_eq!(listing.items[0].queued_item_id, Some(item));
    assert!(listing.items[0].downloaded_at.is_none());
    let result = submit(&h, &listing, &["old"], true).await.unwrap();
    assert!(result.queued.is_empty());
    assert_eq!(result.skipped[0].reason.as_ref(), "already_queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn future_only_marks_existing_releases_but_leaves_upcoming_releases_eligible() {
    let (h, provider, id) = setup(vec![entry("old")]).await;
    let mut upcoming = entry("upcoming");
    upcoming.live = aulos_provider::LiveStatus::IsUpcoming { at: None };
    provider.set_feed(
        &feed_url("one"),
        vec![entry("old"), entry("ignored"), upcoming],
    );
    let listing = browse(&h, &id, 100).await;
    assert_eq!(h.seen(&id).await.len(), 1);
    submit(&h, &listing, &[], false).await.unwrap();
    let seen = h.seen(&id).await;
    assert!(seen.contains("ignored"));
    assert!(!seen.contains("upcoming"));
    assert!(h.items().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_checks_skip_successful_manual_downloads() {
    let (h, provider, id) = setup(vec![entry("old")]).await;
    let item = remembered(&h, "manual", Status::Finished).await;
    h.store
        .write(vec![WriteOp::DeleteItems(vec![item])], Durability::Sync)
        .await
        .unwrap();
    provider.set_feed(&feed_url("one"), vec![entry("old"), entry("manual")]);
    h.check(vec![id.clone()]).await;
    h.until(&id, "successful manual release handled", |r| {
        r.seen_count == 2
    })
    .await;
    assert!(h.items().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn preview_ids_and_selected_ids_are_validated_and_limits_are_reported() {
    let (h, _, id) = setup(vec![entry("one"), entry("two")]).await;
    let mut listing = browse(&h, &id, 1).await;
    assert_eq!(listing.items.len(), 1);
    assert!(listing.has_more);
    assert!(matches!(
        submit(&h, &listing, &["not-listed"], false).await,
        Err(SubError::Invalid { .. })
    ));
    listing.preview_id = "missing-preview".into();
    assert!(matches!(
        submit(&h, &listing, &[], false).await,
        Err(SubError::Invalid { .. })
    ));
    assert!(h.items().await.is_empty());
}
