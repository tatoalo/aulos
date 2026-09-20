//! The DESIGN §14.3 check-algorithm parity rules, against a real queue engine and a provider a
//! test writes the feed for. No network, no yt-dlp.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::subscription::{SubChanges, SubError, SubscriptionView};
use aulos_provider::LiveStatus;
use support::{Harness, ScriptProvider, entry, feed_url, unqueueable};

/// A harness whose only provider is `script`, with `name`'s feed already declared.
async fn with_feed(
    name: &str,
    entries: Vec<aulos_provider::MediaEntry>,
) -> (Harness, Arc<ScriptProvider>) {
    let provider = ScriptProvider::new();
    provider.set_feed(&feed_url(name), entries);
    let h = Harness::builder()
        .provider(Arc::clone(&provider) as Arc<dyn aulos_provider::Provider>)
        .build()
        .await;
    (h, provider)
}

/// DESIGN §14.3 step 8: subscribing marks everything currently visible seen **without** queueing,
/// except an upcoming premiere — which stays unseen so it is queued when the stream starts.
#[tokio::test(flavor = "multi_thread")]
async fn backfill_suppression_marks_everything_seen_without_queueing() {
    let mut soon = entry("premiere");
    soon.live = LiveStatus::IsUpcoming { at: Some(1) };
    let (h, _p) = with_feed("chan", vec![entry("a"), soon, entry("b")]).await;

    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    assert_eq!(&*view.name, "Scripted Feed");
    assert_eq!(view.seen_count, 2, "the premiere is not counted");

    let seen = h.seen(&view.id).await;
    let mut ids: Vec<&str> = seen.iter().map(|i| &**i).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["a", "b"]);

    h.settle().await;
    assert!(
        h.items().await.is_empty(),
        "subscribing must not queue anything"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subsequent_check_queues_only_unseen_entries_even_when_seen_entries_go_live() {
    let (h, provider) = with_feed("chan", vec![entry("a"), entry("b")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    h.settle().await;
    assert!(h.items().await.is_empty());

    // The channel gained "c", and "a" went live.
    let mut live_a = entry("a");
    live_a.live = LiveStatus::IsLive;
    provider.set_feed(&feed_url("chan"), vec![live_a, entry("b"), entry("c")]);

    h.check(vec![view.id.clone()]).await;
    let queued = wait_for_items(&h, 1).await;
    let mut urls: Vec<String> = queued.iter().map(|i| i.url.to_string()).collect();
    urls.sort();
    assert_eq!(
        urls,
        vec![format!("https://{}/watch/c", support::GOOD_HOST)],
        "seen entries are never requeued"
    );

    // Every queued item is attributed to the subscription (DESIGN §14.3 step 6).
    for item in &queued {
        assert_eq!(item.source.kind, aulos_core::SourceKind::Subscription);
        assert_eq!(item.source.reference.as_deref(), Some(view.id.as_str()));
    }

    let record = h
        .until(&view.id, "a successful check", |r| r.seen_count == 3)
        .await;
    assert_eq!(record.error, None);
    assert_eq!(record.consecutive_failures, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_videos_tab_never_falls_back_to_fifty_unseen_shorts() {
    let provider = ScriptProvider::new();
    let root = feed_url("chan");
    let videos = format!("{root}/videos");
    let shorts = format!("{root}/shorts");
    let tab = |url: &str| {
        let mut entry = entry(url);
        entry.url = url::Url::parse(url).unwrap();
        entry.kind = aulos_provider::EntryKind::Playlist {
            title: url.into(),
            entries: Vec::new(),
        };
        entry
    };
    provider.set_feed(&root, vec![tab(&videos), tab(&shorts)]);
    provider.set_feed(&videos, vec![entry("a"), entry("b")]);
    provider.set_feed(
        &shorts,
        (0..50).map(|i| entry(&format!("short{i}"))).collect(),
    );
    let h = Harness::builder()
        .provider(Arc::clone(&provider) as Arc<dyn aulos_provider::Provider>)
        .build()
        .await;
    let view = h.subscribe(&root).await.unwrap();
    assert_eq!(view.seen_count, 2);

    provider.set_feed(&videos, Vec::new());
    h.check(vec![view.id.clone()]).await;
    h.until(&view.id, "empty tab failure", |r| {
        r.consecutive_failures == 1
    })
    .await;
    assert!(h.items().await.is_empty());
    assert_eq!(h.seen(&view.id).await.len(), 2);
    assert_eq!(
        provider.resolve_count(),
        4,
        "the Shorts tab was never probed"
    );

    provider.set_feed(&videos, vec![entry("a"), entry("b"), entry("new")]);
    h.check(vec![view.id.clone()]).await;
    h.until(&view.id, "videos recovered", |r| r.seen_count == 3)
        .await;
    let items = h.items().await;
    assert_eq!(items.len(), 1);
    assert!(items[0].url.as_str().ends_with("/watch/new"));
}

/// DESIGN §14.3 step 6: entries that fail validation are **not** marked seen (so they retry) and
/// their first three messages join into `error` with `"; "`.
#[tokio::test(flavor = "multi_thread")]
async fn entries_that_fail_validation_stay_unseen_and_their_first_three_messages_join() {
    let (h, provider) = with_feed("chan", vec![entry("ok")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");

    provider.set_feed(
        &feed_url("chan"),
        vec![
            entry("ok"),
            unqueueable("x1"),
            unqueueable("x2"),
            unqueueable("x3"),
            unqueueable("x4"),
            entry("fresh"),
        ],
    );
    h.check(vec![view.id.clone()]).await;

    let record = h
        .until(&view.id, "the check to record its failures", |r| {
            r.error.is_some()
        })
        .await;
    let error = record.error.as_deref().expect("an error");
    assert_eq!(
        error.matches("; ").count(),
        2,
        "exactly three messages joined, got {error:?}"
    );
    assert!(
        error.starts_with("Unsupported resource \"https://rejected.test/watch/x"),
        "{error:?}"
    );

    let seen = h.seen(&view.id).await;
    for id in ["x1", "x2", "x3", "x4"] {
        assert!(!seen.contains(&Box::from(id)), "{id} must stay unseen");
    }
    assert!(seen.contains(&Box::from("fresh")), "the good one is seen");
    // A queue error is not a *check* failure, so the backoff counter stays at zero.
    assert_eq!(record.consecutive_failures, 0);
}

/// DESIGN §14.3 step 4: a single video is rejected with the exact legacy message, on subscribe and
/// on a later check — and on a check it **counts as a failure**, which legacy never did.
#[tokio::test(flavor = "multi_thread")]
async fn the_single_video_url_is_rejected_with_the_legacy_message_and_counts_as_a_failure() {
    let (h, provider) = with_feed("chan", vec![entry("a")]).await;

    // Subscribe to a URL the provider has no listing for: it resolves to one video.
    let err = h
        .subscribe(&feed_url("lonely"))
        .await
        .expect_err("a single video is not subscribable");
    assert_eq!(err, SubError::VideoOnly);
    assert_eq!(
        err.to_string(),
        "This URL points to a single video, not a channel or playlist. Use Download instead."
    );
    assert_eq!(err.code(), aulos_core::ErrorCode::ValidationFailed);

    // A live subscription whose feed stops being a feed backs off instead of hot-retrying.
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    provider.set_feed(&feed_url("chan"), Vec::new());
    h.check(vec![view.id.clone()]).await;

    let record = h
        .until(&view.id, "the failure to be recorded", |r| {
            r.consecutive_failures == 1
        })
        .await;
    assert_eq!(
        record.error.as_deref(),
        Some("This URL points to a single video, not a channel or playlist. Use Download instead.")
    );
    // The legacy bug: `last_checked` was left untouched on failure.
    assert!(record.last_checked.is_some());
    let next = record.next_due.expect("a next due time");
    let checked = record.last_checked.expect("last_checked");
    assert_eq!(
        next - checked,
        2 * 60 * 60 * 1_000,
        "one failure doubles the 60-minute interval"
    );
}

/// DESIGN §14.3 step 9: the unique-URL index and the in-flight `pending_urls` set, both with the
/// legacy message.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_url_is_rejected_with_the_legacy_message() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let first = h.subscribe(&feed_url("chan")).await.expect("subscribed");

    let again = h
        .subscribe(&feed_url("chan"))
        .await
        .expect_err("the same URL twice");
    assert_eq!(again, SubError::AlreadySubscribed);
    assert_eq!(again.to_string(), "This URL is already subscribed");
    assert_eq!(again.code(), aulos_core::ErrorCode::Conflict);

    // The trimmed form is the key, exactly as legacy's `_normalize_url`.
    let padded = h
        .subscribe(&format!("  {}  ", feed_url("chan")))
        .await
        .expect_err("whitespace is not a different URL");
    assert_eq!(padded, SubError::AlreadySubscribed);

    assert_eq!(h.list().await.len(), 1);
    assert_eq!(h.list().await[0].id, first.id);
}

/// The in-flight guard: two subscribes to the same brand-new URL race, and the loser is rejected
/// before either resolution finishes.
#[tokio::test(flavor = "multi_thread")]
async fn an_in_flight_duplicate_is_also_rejected() {
    let (h, provider) = with_feed("slow", vec![entry("a")]).await;
    // A slow resolve guarantees both commands reach the manager while the first is still probing,
    // so this exercises the in-flight `pending_urls` guard and not the persisted url index.
    provider.slow(Duration::from_millis(150));
    let url = feed_url("slow");

    let (a, b) = tokio::join!(h.subscribe(&url), h.subscribe(&url));
    let outcomes = [a, b];
    let ok = outcomes.iter().filter(|r| r.is_ok()).count();
    let rejected: Vec<&SubError> = outcomes.iter().filter_map(|r| r.as_ref().err()).collect();
    assert_eq!(ok, 1, "exactly one subscribe may win");
    assert_eq!(rejected.len(), 1);
    assert_eq!(*rejected[0], SubError::AlreadySubscribed);
    assert_eq!(h.list().await.len(), 1);
}

/// The legacy `_normalize_url` parity: surrounding whitespace is stripped, and the trimmed form
/// is the uniqueness key.
///
/// The legacy `Missing URL` branch it used to share a block with is now unreachable *by
/// construction* — [`aulos_core::SubCmd::Add`] carries a typed `Url`, which cannot be empty (see
/// the wave-2 note in `docs/INTEGRATION-NOTES.md`). It was already unreachable over HTTP:
/// `tests/v1_golden/MANIFEST.json` records that `parse_download_options` rejects a falsy `url`
/// with `missing 'url', 'download_type', or 'quality'` before the manager ever sees it. The
/// string itself is still pinned, in `aulos_api::v1::legacy`.
#[tokio::test(flavor = "multi_thread")]
async fn a_padded_url_is_normalised_and_is_its_own_uniqueness_key() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let url = feed_url("chan");
    let view = h
        .subscribe(&format!("  {url}\t"))
        .await
        .expect("a padded url subscribes");
    assert_eq!(&*view.url, url, "stored trimmed");
    assert_eq!(
        h.subscribe(&format!("\n{url}  ")).await.expect_err("dup"),
        SubError::AlreadySubscribed,
        "the trimmed form is the key"
    );
    assert_eq!(
        SubError::MissingUrl.to_string(),
        "Missing URL",
        "the legacy string is unchanged"
    );
}

/// A `name` in the create body names the subscription, and a blank one does not: the record is
/// then named after the probed feed, as it always was. Both shipped clients send the field, and
/// before it travelled in `SubCmd::Add` a subscription added as "Flow Test NASA" came back named
/// after the channel.
#[tokio::test(flavor = "multi_thread")]
async fn a_supplied_name_wins_and_a_blank_one_falls_back_to_the_feed() {
    use aulos_core::request::DownloadRequest;

    let (h, provider) = with_feed("chan", vec![entry("a")]).await;
    provider.set_feed(&feed_url("other"), vec![entry("b")]);
    provider.set_feed(&feed_url("third"), vec![entry("c")]);
    let request = |url: &str| {
        DownloadRequest::new(url::Url::parse(url).unwrap(), crate::support::selection())
    };

    let named = h
        .subscribe_with(request(&feed_url("chan")), None, Some("  Flow Test NASA  "))
        .await
        .expect("subscribed");
    assert_eq!(&*named.name, "Flow Test NASA", "trimmed, and it wins");
    assert_eq!(&*h.record(&named.id).await.name, "Flow Test NASA");

    // Blank is no name at all — `update`'s rule — so the feed names it.
    let blank = h
        .subscribe_with(request(&feed_url("other")), None, Some("   "))
        .await
        .expect("subscribed");
    assert_eq!(&*blank.name, "Scripted Feed");

    // And an absent one is what every v1 add sends.
    let absent = h
        .subscribe_with(request(&feed_url("third")), None, None)
        .await
        .expect("subscribed");
    assert_eq!(&*absent.name, "Scripted Feed");
}

/// The whole download template travels in `SubCmd::Add`, which is the v1 parity legacy's
/// `POST <p>subscribe` had: it accepted every one of these fields and `add_subscription` stored
/// them (legacy spec §7.4). Before the wave-2 integration pass the command carried only `url`,
/// `selection` and `folder`, and the rest were silently replaced by config defaults.
#[tokio::test(flavor = "multi_thread")]
async fn the_whole_download_template_and_the_interval_reach_the_record() {
    use aulos_core::request::{DownloadRequest, SubtitleLang, SubtitleMode};

    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let mut request = DownloadRequest::new(
        url::Url::parse(&feed_url("chan")).unwrap(),
        crate::support::selection(),
    );
    request.folder = Some(aulos_core::paths::RelDir::parse("Shows").unwrap());
    request.custom_name_prefix = "S01 - ".into();
    request.auto_start = false;
    request.playlist_item_limit = 7;
    request.split_by_chapters = true;
    request.chapter_template = "%(section_number)s".into();
    request.subtitle_language = SubtitleLang::parse("it").unwrap();
    request.subtitle_mode = SubtitleMode::PreferAuto;
    request.ytdl_options_presets = vec!["fast".into()];
    request.ytdl_options_overrides = serde_json::from_str(r#"{"noplaylist": true}"#).unwrap();

    let view = h
        .subscribe_with(request.clone(), Some(15), None)
        .await
        .expect("subscribed");
    assert_eq!(view.check_interval_minutes, 15, "the requested interval");

    let row = h.record(&view.id).await;
    assert_eq!(row.check_interval_minutes, 15);
    assert_eq!(row.folder, request.folder);
    assert_eq!(row.custom_name_prefix, request.custom_name_prefix);
    assert!(!row.auto_start);
    assert_eq!(row.playlist_item_limit, 7);
    assert!(row.split_by_chapters);
    assert_eq!(row.chapter_template, request.chapter_template);
    assert_eq!(row.subtitle_language, request.subtitle_language);
    assert_eq!(row.subtitle_mode, SubtitleMode::PreferAuto);
    assert_eq!(row.ytdl_options_presets, request.ytdl_options_presets);
    assert_eq!(row.ytdl_options_overrides, request.ytdl_options_overrides);
}

/// An empty `chapter_template` still means "use the configured default", and `None` for the
/// interval still means `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unset_template_field_still_falls_back_to_the_effective_config() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    let row = h.record(&view.id).await;
    assert!(
        !row.chapter_template.is_empty(),
        "the configured default was substituted"
    );
    assert_eq!(row.check_interval_minutes, 60, "the config default");
}

/// A URL nothing in the registry claims is the legacy `Could not resolve URL`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unclaimed_url_is_could_not_resolve() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let err = h
        .subscribe("https://nobody.test/@x")
        .await
        .expect_err("no provider");
    assert_eq!(err, SubError::CouldNotResolve);
    assert_eq!(err.to_string(), "Could not resolve URL");
}

/// DESIGN §14.3 step 10: `update` accepts only `enabled`, `check_interval_minutes` and `name`, and
/// an empty name is ignored exactly as legacy's `if "name" in changes and changes["name"]`.
#[tokio::test(flavor = "multi_thread")]
async fn update_accepts_only_the_three_legacy_fields() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");

    let updated = h
        .update(
            &view.id,
            SubChanges {
                enabled: Some(false),
                check_interval_minutes: Some(5),
                name: Some("Renamed".into()),
            },
        )
        .await
        .expect("updated");
    assert!(!updated.enabled);
    assert_eq!(updated.check_interval_minutes, 5);
    assert_eq!(&*updated.name, "Renamed");
    // Nothing else moved.
    assert_eq!(updated.url, view.url);
    assert_eq!(updated.format, view.format);
    assert_eq!(updated.folder, view.folder);

    // `max(1, n)`, as legacy.
    let clamped = h
        .update(
            &view.id,
            SubChanges {
                check_interval_minutes: Some(0),
                ..SubChanges::default()
            },
        )
        .await
        .expect("updated");
    assert_eq!(clamped.check_interval_minutes, 1);

    // An empty name is ignored, not stored.
    let kept = h
        .update(
            &view.id,
            SubChanges {
                name: Some("   ".into()),
                ..SubChanges::default()
            },
        )
        .await
        .expect("updated");
    assert_eq!(&*kept.name, "Renamed");

    // Unknown ids are 404, not 500.
    let missing = aulos_core::SubId::parse("01JCMISSING").expect("id");
    let err = h
        .update(&missing, SubChanges::default())
        .await
        .expect_err("no such subscription");
    assert_eq!(err.code(), aulos_core::ErrorCode::NotFound);
}

/// The v2 object is 16 keys and the v1 projection is exactly the legacy 13 with float-second
/// `last_checked` (DESIGN §14.1).
#[tokio::test(flavor = "multi_thread")]
async fn both_projections_have_their_documented_key_sets() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");

    let v2 = serde_json::to_value(&view).expect("json");
    let obj = v2.as_object().expect("object");
    assert_eq!(obj.len(), 16);
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected: Vec<&str> = SubscriptionView::V1_KEYS
        .iter()
        .chain(SubscriptionView::V2_ADDITIONAL_KEYS.iter())
        .copied()
        .collect();
    expected.sort_unstable();
    assert_eq!(keys, expected);

    let v1 = aulos_subscriptions::to_v1_dict(&view);
    let v1obj = v1.as_object().expect("object");
    assert_eq!(v1obj.len(), 13);
    let ms = view.last_checked.expect("last_checked");
    assert_eq!(
        v1obj["last_checked"].as_f64().expect("float seconds"),
        ms as f64 / 1_000.0
    );
}

/// The two PROTOCOL §5.9 envelopes, as this crate's producers see them.
#[tokio::test(flavor = "multi_thread")]
async fn the_two_websocket_envelopes_have_their_protocol_shape() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");

    let frame = aulos_subscriptions::v2_frame(11, &view);
    assert_eq!(frame["t"], "subscription");
    assert_eq!(frame["seq"], 11);
    assert_eq!(frame["subscription"]["id"], view.id.as_str());

    let removed = h.delete(vec![view.id.clone()]).await;
    assert_eq!(removed, vec![view.id.clone()]);
    let frame = aulos_subscriptions::v2_removed_frame(12, &removed);
    assert_eq!(frame["t"], "subscription_removed");
    assert!(frame["ids"].is_array(), "an array even for one deletion");
    assert_eq!(frame["ids"][0], view.id.as_str());

    h.settle().await;
    assert_eq!(h.events.removed(), vec![view.id]);
    assert!(h.list().await.is_empty());
}

/// The subscription row is gone from SQLite too, and its seen set cascaded with it.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_subscription_removes_its_row_and_its_seen_set() {
    let (h, _p) = with_feed("chan", vec![entry("a"), entry("b")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    assert_eq!(h.seen(&view.id).await.len(), 2);

    h.delete(vec![view.id.clone()]).await;
    h.settle().await;
    assert!(
        h.store
            .subscription(&view.id)
            .await
            .expect("read")
            .is_none()
    );
    assert!(h.seen(&view.id).await.is_empty());

    // Deleting an unknown id is a no-op, not an error.
    assert!(h.delete(vec![view.id]).await.is_empty());
}

/// A delete whose store write fails must leave the subscription exactly as it was: still listed,
/// still holding its URL, still scheduled. Dropping the in-memory state first would hide a row
/// that is still in SQLite — invisible until a restart loaded it again, by which time the operator
/// has re-subscribed and the feed is checked by two tasks with two seen sets.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_delete_leaves_the_subscription_in_place() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    h.settle().await;
    h.events.clear();

    // Every later write is `StoreError::Closed` — the transient-store-failure stand-in.
    let _ = h.store.close().await;

    let err = h
        .try_delete(vec![view.id.clone()])
        .await
        .expect_err("the delete must fail when the store cannot record it");
    assert!(
        matches!(err, SubError::Other(_)),
        "a store failure surfaces as Other, got {err:?}"
    );

    let listed = h.list().await;
    assert_eq!(
        listed.iter().map(|v| v.id.clone()).collect::<Vec<_>>(),
        vec![view.id.clone()],
        "a failed delete may not remove the subscription from the manager"
    );
    assert!(
        h.events.removed().is_empty(),
        "no subscription_removed event for a delete that did not happen"
    );
    // The URL index still holds it, so the operator cannot end up with a duplicate row.
    assert!(
        matches!(
            h.subscribe(&feed_url("chan")).await,
            Err(SubError::AlreadySubscribed)
        ),
        "the deleted-but-not-persisted url must still be taken"
    );
}

/// `checking` is `true` while a check runs and `false` afterwards — the observable half of
/// "`POST subscriptions/check` returns immediately" (DESIGN §14.2).
#[tokio::test(flavor = "multi_thread")]
async fn a_check_publishes_checking_true_then_false() {
    let (h, _p) = with_feed("chan", vec![entry("a")]).await;
    let view = h.subscribe(&feed_url("chan")).await.expect("subscribed");
    h.settle().await;
    h.events.clear();

    let job = h.check(vec![view.id.clone()]).await;
    assert!(!job.job_id.is_empty());
    assert_eq!(job.subscriptions, vec![view.id.clone()]);

    h.settle().await;
    let flags: Vec<bool> = h
        .events
        .changed()
        .iter()
        .filter(|v| v.id == view.id)
        .map(|v| v.checking)
        .collect();
    assert!(
        flags.windows(2).any(|w| w == [true, false]),
        "expected a true→false pair, got {flags:?}"
    );
}

/// Waits until at least `n` queue rows exist.
async fn wait_for_items(h: &Harness, n: usize) -> Vec<aulos_core::Item> {
    for _ in 0..2_000 {
        let rows = h.items().await;
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!(
        "timed out waiting for {n} queued item(s); saw {:?}",
        h.items().await.len()
    );
}
