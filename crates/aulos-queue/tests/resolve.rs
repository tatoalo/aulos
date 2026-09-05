//! Resolution, in-place group promotion, the runner-up fall-through, `pre_error` children,
//! `cancel-resolve` and `WaitResolved` (DESIGN §8.4, §6.4, §11.2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Instant;

use aulos_core::{AddReason, ErrorCode, Kind, Status};
use aulos_provider::fake::{FakeProvider, Step, Timeline};
use aulos_provider::{Match, Provider};
use aulos_queue::CancelScope;
use support::{Harness, expanding, fake, request};

#[tokio::test]
async fn a_single_video_keeps_its_id_and_produces_no_membership_change() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/solo").await;
    let resolved = h.until_resolved(id).await;
    assert_eq!(resolved.id, id, "the same item id (DESIGN §8.4)");
    assert_eq!(resolved.kind, Kind::Item);
    assert_eq!(resolved.group_id, None);
    assert!(h.events.removed().is_empty(), "no removed frame");
    assert_eq!(
        h.events.added().len(),
        1,
        "one `added`, from the add itself — resolution adds no membership"
    );
}

#[tokio::test]
async fn a_playlist_is_promoted_in_place_keeping_its_id_and_ord() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(3)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let id = h.add("https://fake.test/playlist/one").await;
    let before = h.item(id).await.unwrap();
    assert_eq!(before.kind, Kind::Item);
    let ord = before.ord;

    let group = h
        .until(id, "group promotion", |i| i.kind == Kind::Group)
        .await;
    assert_eq!(group.id, id, "the group keeps the anchor's id");
    assert_eq!(group.ord, ord, "and its ord");
    assert_eq!(group.children_total, Some(3));
    assert!(
        h.events.removed().is_empty(),
        "no `removed` event: the row morphs, it does not blink"
    );

    let children = h.until_all("three children", |rows| rows.len() == 4).await;
    let kids: Vec<_> = children.iter().filter(|i| i.group_id == Some(id)).collect();
    assert_eq!(kids.len(), 3);
    for (n, kid) in kids.iter().enumerate() {
        assert_eq!(kid.group_index, Some(u32::try_from(n).unwrap() + 1));
        assert_eq!(kid.status, Status::Queued);
        assert!(
            kid.provider.is_some(),
            "a child is inserted already resolved"
        );
        assert!(kid.ord > ord, "children sort after their group");
    }

    // The first `added` is the add; the second carries the promoted group plus its children.
    let added = h.events.added();
    assert_eq!(added[0].1, AddReason::Created);
    assert_eq!(added[1].1, AddReason::Expanded);
    assert_eq!(
        added[1].0[0], id,
        "the group view leads the expansion frame"
    );
}

#[tokio::test]
async fn a_five_hundred_item_expansion_is_cheap_and_schedulable_immediately() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(500)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let mut req = request("https://fake.test/playlist/big");
    // Parked, so the transaction count measures the *expansion* and not the 500 downloads that
    // would otherwise start behind it.
    req.auto_start = false;
    let before = h.store.job_count();
    let started = Instant::now();
    let id = h.add_request(req).await.unwrap().ids[0];
    h.until(id, "the group", |i| i.kind == Kind::Group).await;
    let first_children = h.until_all("the first batch", |rows| rows.len() > 1).await;
    let latency = started.elapsed();
    assert!(
        latency.as_millis() < 100,
        "the first children must be schedulable within 100 ms, took {latency:?}"
    );
    assert!(first_children.len() >= 2);

    let all = h.until_all("every child", |rows| rows.len() == 501).await;
    assert_eq!(all.len(), 501, "the group plus 500 children");
    let transactions = h.store.job_count() - before;
    assert!(
        transactions <= 8,
        "a 500-item expansion must cost at most 8 store transactions, took {transactions}"
    );
    // Children are inserted in batches of 100, one `added` frame per batch (DESIGN §8.4), and the
    // first frame also carries the promoted group view.
    let expanded: Vec<usize> = h
        .events
        .added()
        .into_iter()
        .filter(|(_, reason)| *reason == AddReason::Expanded)
        .map(|(ids, _)| ids.len())
        .collect();
    assert_eq!(expanded, vec![101, 100, 100, 100, 100], "{expanded:?}");
    assert_eq!(
        all.iter().filter(|i| i.group_id == Some(id)).count(),
        500,
        "and every child is there"
    );
}

/// Promotion writes the group's own status, not just its `kind` and `children_total`.
///
/// The cache and the wire said `queued` while the persisted row still said `resolving` (with
/// whatever `msg` the resolution left behind). Anything reading the row rather than the published
/// snapshot disagreed with the socket, and a restart at that point had boot recovery re-queue a
/// perfectly good group as an interrupted resolution.
#[tokio::test]
async fn a_promoted_group_persists_its_status_not_just_its_shape() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(2)))
        .build()
        .await;
    let mut req = request("https://fake.test/playlist/persisted");
    // The pending bucket, so the children never leave `queued` and the roll-up stays put.
    req.auto_start = false;
    let id = h.add_request(req).await.unwrap().ids[0];
    h.until_all("the children", |rows| rows.len() == 3).await;
    h.settle().await;

    let row = h.item(id).await.unwrap();
    assert_eq!(row.kind, Kind::Group);
    assert_eq!(
        row.status,
        Status::Queued,
        "the persisted group agrees with the snapshot"
    );
    assert_eq!(row.msg, None, "and carries no leftover resolution message");
    assert_eq!(row.error, None);
}

#[tokio::test]
async fn the_playlist_item_limit_truncates_the_children() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(20)))
        .build()
        .await;
    let mut req = request("https://fake.test/playlist/capped");
    req.playlist_item_limit = 5;
    let out = h.add_request(req).await.unwrap();
    let id = out.ids[0];
    let group = h.until(id, "the group", |i| i.kind == Kind::Group).await;
    assert_eq!(group.children_total, Some(5));
    let children = h.until_all("five children", |rows| rows.len() == 6).await;
    assert_eq!(
        children.iter().filter(|i| i.group_id == Some(id)).count(),
        5
    );
    for kid in children.iter().filter(|i| i.group_id == Some(id)) {
        assert_eq!(
            kid.request.playlist_item_limit, 5,
            "the limit is also on each child's request, so `playlistend` reaches yt-dlp"
        );
    }
}

#[tokio::test]
async fn a_playlist_child_that_duplicates_another_child_is_not_deduped() {
    // `expand_playlist` gives every child a distinct URL, so the duplicate has to be built by
    // hand: two entries with the same media id and URL, as a channel page listing a video twice.
    let toml = r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        url_regex = "twice"
        resolve = [{ kind = "expand_playlist", count = 2 }]
    "#;
    let provider = FakeProvider::from_toml(toml).unwrap();
    let h = Harness::builder()
        .provider(Arc::new(provider))
        .build()
        .await;
    let id = h.add("https://fake.test/playlist/twice").await;
    let rows = h.until_all("both children", |rows| rows.len() == 3).await;
    let kids: Vec<_> = rows.iter().filter(|i| i.group_id == Some(id)).collect();
    assert_eq!(
        kids.len(),
        2,
        "children bypass dedupe entirely (DESIGN §8.5)"
    );
}

#[tokio::test]
async fn zero_entries_produce_the_verbatim_legacy_message() {
    let toml = r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "expand_playlist", count = 0 }]
    "#;
    let h = Harness::builder()
        .provider(Arc::new(FakeProvider::from_toml(toml).unwrap()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/empty").await;
    let row = h.until_status(id, Status::Error).await;
    let error = row.error.expect("an error");
    assert_eq!(error.code, ErrorCode::UnsupportedUrl);
    assert_eq!(&*error.message, "Invalid/empty data was given.");
    assert_eq!(error.provider.as_deref(), Some("fake"));
}

#[tokio::test]
async fn an_unmappable_resource_produces_the_verbatim_legacy_message() {
    // The shim emits `Unsupported resource "<etype>"` as an `error` frame, which reaches the engine
    // as `ProviderError::Unsupported` with that text verbatim (DESIGN §8.4, §9.6).
    let toml = r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "fail", code = "unsupported_url" }]
    "#;
    let h = Harness::builder()
        .provider(Arc::new(FakeProvider::from_toml(toml).unwrap()))
        .env("AULOS_RESOLVE_FALLTHROUGH", "false")
        .build()
        .await;
    let id = h.add("https://fake.test/watch/odd").await;
    let row = h.until_status(id, Status::Error).await;
    assert_eq!(row.error.unwrap().code, ErrorCode::UnsupportedUrl);

    // And the engine's own version of the string, for a URL no provider claims at all.
    assert_eq!(
        aulos_queue::resolve::unsupported_resource("url_result"),
        "Unsupported resource \"url_result\""
    );
}

#[tokio::test]
async fn a_pre_error_child_is_queued_not_failed() {
    let h = Harness::builder()
        .provider(Arc::new(upcoming_provider()))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/premiere").await;
    let row = h.until_resolved(id).await;
    assert_eq!(
        row.status,
        Status::Queued,
        "an upcoming livestream is queued, never `error` (DESIGN §8.4)"
    );
    assert!(
        !row.auto_start,
        "and parked, so nothing tries to download it"
    );
    let error = row.error.expect("the pre_error is on the row");
    assert_eq!(error.code, ErrorCode::NotYetLive);
    assert_eq!(
        &*error.message, "Live stream is scheduled to start at 2026-09-04 18:00:00 +0000",
        "the legacy text is preserved byte for byte"
    );

    // It is never auto-retried: it is not a failed item at all.
    h.settle().await;
    assert_eq!(h.item(id).await.unwrap().attempt, 0);

    // And `start` runs it.
    let result = h
        .handle
        .actions(aulos_queue::Action::Start, vec![id], None)
        .await;
    assert_eq!(result.applied, vec![id]);
    h.until_status(id, Status::Finished).await;
}

#[tokio::test]
async fn the_runner_up_fall_through_retries_once_and_only_for_unsupported() {
    // A `Strong(200)` provider that always answers `Unsupported`, plus the `Weak(1)` catch-all.
    let first = FakeProvider::from_toml(
        r#"
        id = "sc"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "fail", code = "unsupported_url" }]
    "#,
    )
    .unwrap();
    let fallback = FakeProvider::from_toml(
        r#"
        id = "ytdlp"
        score = 1
        strong = false
    "#,
    )
    .unwrap();
    let h = Harness::builder()
        .provider(Arc::new(first))
        .provider(Arc::new(fallback))
        .build()
        .await;

    let id = h.add("https://fake.test/watch/fall").await;
    let row = h.until_resolved(id).await;
    assert_eq!(row.status, Status::Queued, "the runner-up succeeded");
    assert_eq!(
        row.provider.as_ref().map(aulos_core::ProviderId::as_str),
        Some("ytdlp")
    );
    let msgs: Vec<String> = h
        .events
        .changes(id)
        .iter()
        .filter_map(|v| v.msg.as_deref().map(str::to_owned))
        .collect();
    assert!(
        msgs.iter().any(|m| m == "Retrying with ytdlp"),
        "the fall-through is recorded on the item: {msgs:?}"
    );
}

#[tokio::test]
async fn a_non_unsupported_failure_is_terminal_with_no_fall_through() {
    for code in ["auth_required", "provider_degraded"] {
        let first = FakeProvider::from_toml(&format!(
            r#"
            id = "sc"
            score = 200
            hosts = ["fake.test"]

            [[timeline]]
            resolve = [{{ kind = "fail", code = "{code}" }}]
        "#
        ))
        .unwrap();
        let h = Harness::builder()
            .provider(Arc::new(first))
            .provider(Arc::new(
                FakeProvider::from_toml("id = \"ytdlp\"\nscore = 1\nstrong = false\n").unwrap(),
            ))
            .build()
            .await;
        let id = h.add("https://fake.test/watch/hard").await;
        let row = h.until_status(id, Status::Error).await;
        assert_eq!(
            row.provider.as_ref().map(aulos_core::ProviderId::as_str),
            None,
            "{code}: no provider was ever recorded, so nothing ran"
        );
        assert!(
            !h.events
                .changes(id)
                .iter()
                .any(|v| v.msg.as_deref() == Some("Retrying with ytdlp")),
            "{code} must not fall through"
        );
    }
}

#[tokio::test]
async fn fall_through_can_be_switched_off_and_never_happens_twice() {
    let unsupported = |id: &str, score: u8, strong: bool| {
        FakeProvider::from_toml(&format!(
            r#"
            id = "{id}"
            score = {score}
            strong = {strong}

            [[timeline]]
            resolve = [{{ kind = "fail", code = "unsupported_url" }}]
        "#
        ))
        .unwrap()
    };

    // Off: even `Unsupported` is terminal.
    let off = Harness::builder()
        .provider(Arc::new(unsupported("sc", 200, true)))
        .provider(Arc::new(
            FakeProvider::from_toml("id = \"ytdlp\"\nscore = 1\nstrong = false\n").unwrap(),
        ))
        .env("AULOS_RESOLVE_FALLTHROUGH", "false")
        .build()
        .await;
    let id = off.add("https://fake.test/watch/x").await;
    off.until_status(id, Status::Error).await;
    assert!(
        !off.events
            .changes(id)
            .iter()
            .any(|v| v.msg.as_deref().is_some_and(|m| m.starts_with("Retrying"))),
        "AULOS_RESOLVE_FALLTHROUGH=false disables the retry entirely"
    );

    // On, but the runner-up also answers `Unsupported`: exactly one retry, then terminal.
    let first = Arc::new(unsupported("sc", 200, true));
    let second = Arc::new(unsupported("ytdlp", 1, false));
    let twice = Harness::builder()
        .provider(Arc::clone(&first) as Arc<dyn Provider>)
        .provider(Arc::clone(&second) as Arc<dyn Provider>)
        .build()
        .await;
    let id = twice.add("https://fake.test/watch/y").await;
    let row = twice.until_status(id, Status::Error).await;
    assert_eq!(row.error.unwrap().provider.as_deref(), Some("ytdlp"));
    twice.settle().await;
    assert_eq!(first.resolve_count(), 1, "the winner ran once");
    assert_eq!(
        second.resolve_count(),
        1,
        "the runner-up ran once and there was no third attempt"
    );
}

#[tokio::test]
async fn cancel_resolve_all_aborts_every_in_flight_resolution() {
    let slow = FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "wait", ms = 600000 }]
    "#,
    )
    .unwrap();
    let h = Harness::builder().provider(Arc::new(slow)).build().await;
    let a = h.add("https://fake.test/watch/a").await;
    let b = h.add("https://fake.test/watch/b").await;
    h.until(a, "resolving", |i| i.status == Status::Resolving)
        .await;

    let result = h.handle.cancel_resolve(CancelScope::All).await;
    assert_eq!(result.applied.len(), 2);
    for id in [a, b] {
        let row = h.until_status(id, Status::Canceled).await;
        assert_eq!(row.error.unwrap().code, ErrorCode::Canceled);
    }
}

#[tokio::test]
async fn cancel_resolve_by_generation_leaves_a_concurrent_add_running() {
    let slow = FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        url_regex = "slow"
        resolve = [{ kind = "wait", ms = 600000 }]

        [[timeline]]
        resolve = []
    "#,
    )
    .unwrap();
    let h = Harness::builder().provider(Arc::new(slow)).build().await;
    let doomed = h
        .handle
        .add(
            vec![request("https://fake.test/slow/a")],
            aulos_core::SourceRef::bare(aulos_core::SourceKind::ApiV2),
        )
        .await
        .unwrap();
    let survivor = h
        .handle
        .add(
            vec![request("https://fake.test/slow/b")],
            aulos_core::SourceRef::bare(aulos_core::SourceKind::ApiV2),
        )
        .await
        .unwrap();
    assert_ne!(
        doomed.generation, survivor.generation,
        "one generation per add"
    );

    h.until(doomed.ids[0], "resolving", |i| {
        i.status == Status::Resolving
    })
    .await;
    h.until(survivor.ids[0], "resolving", |i| {
        i.status == Status::Resolving
    })
    .await;
    let result = h
        .handle
        .cancel_resolve(CancelScope::Generation(doomed.generation))
        .await;
    assert_eq!(
        result.applied, doomed.ids,
        "only the add that owns that generation"
    );
    h.until_status(doomed.ids[0], Status::Canceled).await;
    assert_eq!(
        h.item(survivor.ids[0]).await.unwrap().status,
        Status::Resolving,
        "the concurrent add is still resolving"
    );

    // A generation that nothing belongs to touches nothing, and does not bump the counter.
    let after = h.add("https://fake.test/watch/fresh").await;
    let none = h
        .handle
        .cancel_resolve(CancelScope::Generation(9_999))
        .await;
    assert!(none.applied.is_empty());
    h.until_resolved(after).await;

    // And `All` still condemns everything left in flight, including the survivor.
    h.handle.cancel_resolve(CancelScope::All).await;
    h.until_status(survivor.ids[0], Status::Canceled).await;
}

#[tokio::test]
async fn cancel_resolve_all_stops_an_expansion_mid_flight() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(500)))
        .env("MAX_CONCURRENT_DOWNLOADS", "1")
        .build()
        .await;
    let id = h.add("https://fake.test/playlist/big").await;
    h.until_all("the first batch", |rows| rows.len() > 1).await;
    h.handle.cancel_resolve(CancelScope::All).await;
    h.settle().await;

    let rows = h.rows().await;
    let children: Vec<_> = rows.iter().filter(|i| i.group_id == Some(id)).collect();
    assert!(
        children.len() < 500,
        "the not-yet-created children were never created: {}",
        children.len()
    );
    // PROTOCOL §4.7: "items already created keep their state, so follow it with a `delete` if you
    // want them gone". Cancelling them here would kill running downloads and delete their partial
    // bytes — and leave the client's documented follow-up with nothing to delete.
    assert!(
        children.iter().all(|i| i.status != Status::Canceled),
        "the children that did exist keep their state: {:?}",
        children.iter().map(|i| i.status).collect::<Vec<_>>()
    );
    let group = h.item(id).await.unwrap();
    assert_ne!(
        group.status,
        Status::Canceled,
        "the header follows its surviving children"
    );
}

#[tokio::test]
async fn wait_resolved_answers_immediately_for_a_settled_id() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/quick").await;
    h.until_resolved(id).await;
    let reports = h.handle.wait_resolved(vec![id]).await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].id, id);
    assert_eq!(reports[0].kind, Kind::Item);
    assert!(reports[0].outcome.is_ok());
}

#[tokio::test]
async fn wait_resolved_answers_on_the_transition_and_serves_two_callers() {
    let slow = FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "wait", ms = 30 }]
    "#,
    )
    .unwrap();
    let h = Harness::builder().provider(Arc::new(slow)).build().await;
    let id = h.add("https://fake.test/watch/wait").await;

    let one = h.handle.clone();
    let two = h.handle.clone();
    let (a, b) = tokio::join!(one.wait_resolved(vec![id]), two.wait_resolved(vec![id]));
    assert_eq!(a.len(), 1, "both callers get an answer");
    assert_eq!(b.len(), 1);
    assert!(a[0].outcome.is_ok());
    assert_eq!(a[0], b[0]);
}

#[tokio::test]
async fn wait_resolved_reports_a_failure_as_the_wire_error() {
    let failing = FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "wait", ms = 20 }, { kind = "fail", code = "geo_restricted" }]
    "#,
    )
    .unwrap();
    let h = Harness::builder().provider(Arc::new(failing)).build().await;
    let id = h.add("https://fake.test/watch/geo").await;
    let reports = h.handle.wait_resolved(vec![id]).await;
    assert_eq!(reports.len(), 1);
    let err = reports[0].outcome.clone().expect_err("a failure");
    assert_eq!(err.code, ErrorCode::GeoRestricted);
}

#[tokio::test]
async fn wait_resolved_reports_a_group_as_a_group() {
    let h = Harness::builder()
        .provider(Arc::new(expanding(2)))
        .build()
        .await;
    let id = h.add("https://fake.test/playlist/two").await;
    let reports = h.handle.wait_resolved(vec![id]).await;
    assert_eq!(reports[0].kind, Kind::Group);
    assert!(reports[0].outcome.is_ok());
}

#[tokio::test]
async fn a_dropped_wait_resolved_receiver_does_not_leak_a_waiter() {
    let slow = FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        hosts = ["fake.test"]

        [[timeline]]
        resolve = [{ kind = "wait", ms = 40 }]
    "#,
    )
    .unwrap();
    let h = Harness::builder().provider(Arc::new(slow)).build().await;
    let id = h.add("https://fake.test/watch/gone").await;

    // Abandon the wait, then let the resolution complete. The engine must prune the waiter rather
    // than keep it forever, which the following successful wait proves: a leaked waiter would
    // still be holding a `oneshot` for an id that never transitions again.
    let waiting = tokio::spawn({
        let handle = h.handle.clone();
        async move { handle.wait_resolved(vec![id]).await }
    });
    waiting.abort();
    h.until_resolved(id).await;

    let again = h.handle.wait_resolved(vec![id]).await;
    assert_eq!(again.len(), 1);
    assert!(again[0].outcome.is_ok());
}

#[tokio::test]
async fn an_unknown_id_is_reported_rather_than_waited_on() {
    let h = Harness::new().await;
    let reports = h
        .handle
        .wait_resolved(vec![aulos_core::ItemId::new()])
        .await;
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].outcome.clone().expect_err("gone").code,
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn the_default_fake_provider_only_claims_its_own_host() {
    // A guard on the fixture: several tests rely on a second provider being the runner-up.
    let p = fake();
    assert_eq!(
        p.matches(&url::Url::parse("https://fake.test/x").unwrap()),
        Match::Strong(200)
    );
    assert_eq!(
        p.matches(&url::Url::parse("https://other.invalid/x").unwrap()),
        Match::No
    );
}

/// A provider whose single entry carries the legacy upcoming-livestream `pre_error`.
fn upcoming_provider() -> UpcomingProvider {
    UpcomingProvider(fake())
}

/// Wraps the fake provider and decorates its entry with a `pre_error` (DESIGN §8.4).
struct UpcomingProvider(FakeProvider);

#[async_trait::async_trait]
impl Provider for UpcomingProvider {
    fn id(&self) -> aulos_provider::ProviderId {
        self.0.id()
    }

    fn matches(&self, url: &url::Url) -> Match {
        self.0.matches(url)
    }

    fn catalog(&self) -> Arc<aulos_core::FormatCatalog> {
        self.0.catalog()
    }

    async fn resolve(
        &self,
        url: &url::Url,
        ctx: aulos_provider::ResolveCtx<'_>,
    ) -> Result<Vec<aulos_provider::MediaEntry>, aulos_provider::ProviderError> {
        let mut entries = self.0.resolve(url, ctx).await?;
        if let Some(entry) = entries.first_mut() {
            entry.live = aulos_provider::entry::LiveStatus::IsUpcoming { at: Some(17) };
            entry.pre_error = Some(aulos_core::WireError::new(
                ErrorCode::Unavailable,
                "Live stream is scheduled to start at 2026-09-04 18:00:00 +0000",
            ));
        }
        Ok(entries)
    }

    async fn download(
        &self,
        ctx: aulos_provider::DownloadCtx<'_>,
        sink: aulos_provider::ProgressSink,
    ) -> Result<aulos_provider::Outcome, aulos_provider::ProviderError> {
        self.0.download(ctx, sink).await
    }
}

/// Keeps the unused-import warning honest for the timeline builders used above.
#[allow(dead_code)]
fn _timeline_types(_: Timeline, _: Step) {}
