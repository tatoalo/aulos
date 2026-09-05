//! The v1 shim's behaviour, end to end (PLAN WP-15's acceptance list).
//!
//! What the golden corpus cannot cover lives here: the bounded pre-resolve (the corpus is
//! network-free by construction, so every captured `POST add` is a validation 400), the id
//! resolution ladder against real rows, the `where` semantics, `done[]` completeness at 4 211
//! rows, and the mechanical schema check the shipped Swift models imply.
//!
//! Every test runs under **both** `URL_PREFIX` values through [`support::for_each_prefix`], which
//! is what makes a route built from a raw string instead of the `Prefix` newtype fail in the same
//! test that covers its behaviour.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use aulos_api::v1::history::ITEM_KEYS;
use aulos_core::{
    Codec, DownloadRequest, DownloadType, FormatId, Item, ItemId, Kind, Ord0, QualityId, Selection,
    SourceKind, SourceRef, Status,
};
use aulos_provider::fake::FakeProvider;
use aulos_store::{Durability, Store, WriteOp};
use serde_json::{Value, json};
use support::{Rig, for_each_prefix, ytdlp_like};
use url::Url;

/// The five legacy status strings a v1 client may ever see.
const V1_STATUSES: [&str; 5] = ["pending", "preparing", "downloading", "finished", "error"];

// ---------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------

/// A `fake.test` provider whose resolution outcome is chosen by the URL.
///
/// `…/bad` and `…/geo` fail with two *different* codes, so their cleaned messages differ and the
/// join is observable; `…/slow` parks in `resolving` for ten minutes, which is how the pre-resolve
/// window's expiry is tested without waiting for it. A `done-take` URL finishes while the default
/// timeline hangs, so two rows can share a `media_id` and still be in different states.
fn scripted() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id     = "fake"
        score  = 200
        strong = true
        hosts  = ["fake.test"]

        [[timeline]]
        url_regex = "slow"
        resolve   = [{ kind = "wait", ms = 600000 }]

        [[timeline]]
        url_regex = "bad"
        resolve   = [{ kind = "fail", code = "unsupported_url" }]

        [[timeline]]
        url_regex = "geo"
        resolve   = [{ kind = "fail", code = "geo_restricted" }]

        [[timeline]]
        url_regex = "twice"
        download  = [{ kind = "finish", filename = "Twice.mp4", size = 1024 }]

        [[timeline]]
        url_regex = "done-take"
        download  = [{ kind = "finish", filename = "Take.mp4", size = 1024 }]

        [[timeline]]
        download = [
            { kind = "stage",  stage = "preparing" },
            { kind = "hang" },
        ]
    "#,
    )
    .expect("the scripted provider must parse")
}

/// A rig with the scripted provider and the real yt-dlp catalog behind it.
///
/// `AULOS_RESOLVE_FALLTHROUGH=false` matters: with it on (the default), a resolve failure is
/// retried against the runner-up provider (DESIGN §6.4), and the `ytdlp`-shaped catch-all would
/// happily resolve the URLs this file deliberately makes fail.
async fn rig(prefix: &'static str, env: &[(&str, &str)]) -> Rig {
    let mut builder = Rig::builder(prefix)
        .without_default_providers()
        .env("AULOS_RESOLVE_FALLTHROUGH", "false")
        .provider(Arc::new(ytdlp_like()))
        .provider(Arc::new(scripted()));
    for (key, value) in env {
        builder = builder.env(key, value);
    }
    builder.start().await
}

/// The legacy add body the shipped share extension sends.
fn add_body(url: &str) -> Value {
    json!({
        "url": url,
        "download_type": "video",
        "codec": "auto",
        "format": "mp4",
        "quality": "best",
    })
}

// ---------------------------------------------------------------------------
// GET history
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn history_has_three_arrays_and_the_five_legacy_statuses() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
        let (status, body) = rig.get("history").await;
        assert_eq!(status, 200);
        for key in ["queue", "pending", "done"] {
            assert!(body[key].is_array(), "{key} must always be present");
            assert_eq!(body[key].as_array().map(Vec::len), Some(0));
        }

        // One item that will park in `preparing`, and one the user has not started.
        let started = rig.add("https://fake.test/clip").await;
        let (_, parked) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": "https://fake.test/parked", "auto_start": false }),
            )
            .await;
        let parked = parked["ids"][0].as_str().expect("an id").to_owned();
        rig.until_status(&started, "preparing").await;
        rig.settle().await;

        let (_, body) = rig.get("history").await;
        let ids = |key: &str| -> Vec<String> {
            body[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(|i| i["url"].as_str().unwrap_or_default().to_owned())
                .collect()
        };
        assert!(ids("queue").iter().any(|u| u.contains("clip")));
        assert!(ids("pending").iter().any(|u| u.contains("parked")));
        assert!(body["done"].as_array().expect("done").is_empty());

        for key in ["queue", "pending", "done"] {
            for item in body[key].as_array().into_iter().flatten() {
                assert_item(item);
            }
        }
        assert_eq!(body["queue"][0]["status"], "preparing");
        assert_eq!(body["pending"][0]["status"], "pending");
        let _ = parked;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_group_and_a_cancelled_item_never_appear_but_a_child_does() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(support::expanding(3)))
            .env("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")
            .start()
            .await;

        let parent = rig.add("https://fake.test/playlist").await;
        rig.until_state("the group's children", |state| {
            state["items"]
                .as_array()
                .is_some_and(|items| items.iter().filter(|i| i["kind"] == "item").count() >= 3)
        })
        .await;

        let (_, v2) = rig.get("api/v2/state").await;
        let groups = v2["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|i| i["kind"] == "group")
            .count();
        assert_eq!(groups, 1, "v2 shows the group");

        let (_, body) = rig.get("history").await;
        let all: Vec<&Value> = ["queue", "pending", "done"]
            .into_iter()
            .flat_map(|k| body[k].as_array().into_iter().flatten())
            .collect();
        assert!(
            all.iter().all(|i| i["id"] != json!(parent.clone())),
            "the group row itself is omitted"
        );
        assert!(all.len() >= 3, "its children are not: {}", all.len());

        // Cancel one child and it disappears from every array, while v2 still reports it.
        let child = all[0]["url"].as_str().expect("a url").to_owned();
        let (status, _) = rig
            .post("delete", &json!({ "ids": [child], "where": "queue" }))
            .await;
        assert_eq!(status, 200);
        rig.settle().await;

        let (_, body) = rig.get("history").await;
        let remaining: Vec<&Value> = ["queue", "pending", "done"]
            .into_iter()
            .flat_map(|k| body[k].as_array().into_iter().flatten())
            .collect();
        for item in &remaining {
            assert_ne!(item["url"], json!(child.clone()), "the row is gone");
            assert!(V1_STATUSES.contains(&item["status"].as_str().unwrap_or_default()));
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn done_is_the_whole_completed_set_not_the_memory_window() {
    // 4 211 terminal rows, of which only `AULOS_MEM_DONE_ITEMS` (500 by default) are in the
    // published snapshot. v1 has no `truncated`, no `done_total` and no cursor, so anything less
    // than all of them would silently delete history out of the shipped client at cutover.
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
    seed_terminal(&rig.store, 4_211).await;

    let (status, body) = rig.get("history").await;
    assert_eq!(status, 200);
    let done = body["done"].as_array().expect("done");
    assert_eq!(done.len(), 4_211, "every completed row, exactly as legacy");

    // `ord` ascending, which is what the array order has to be.
    let stamps: Vec<i64> = done
        .iter()
        .map(|i| i["timestamp"].as_i64().unwrap_or_default())
        .collect();
    assert!(
        stamps.windows(2).all(|w| w[0] <= w[1]),
        "done[] must be ord ascending"
    );

    // The contrast that justifies the cost: v1 served eight times the window v2 would have.
    assert_eq!(
        rig.cfg.mem_done_items, 500,
        "the AULOS_MEM_DONE_ITEMS default"
    );
    assert!(
        done.len() > rig.cfg.mem_done_items as usize * 8,
        "v1 done[] is deliberately not the memory window"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_history_cap_keeps_the_most_recent_rows() {
    let rig = rig(
        "/",
        &[
            ("AULOS_V1_HISTORY_MAX", "1000"),
            ("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0"),
        ],
    )
    .await;
    seed_terminal(&rig.store, 4_211).await;

    let (status, body) = rig.get("history").await;
    assert_eq!(status, 200);
    let done = body["done"].as_array().expect("done");
    assert_eq!(done.len(), 1_000, "exactly the cap");
    // The most recent, not the oldest: the seeder titles rows `Row NNNN` in `ord` order.
    assert_eq!(done[0]["title"], "Row 3211");
    assert_eq!(done[999]["title"], "Row 4210");
}

// ---------------------------------------------------------------------------
// the id ladder and `where`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_id_ladder_resolves_a_ulid_a_url_and_a_legacy_media_id() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;

        // A ULID, which is what a v2-aware client sends.
        let id = rig.add("https://fake.test/by-ulid").await;
        rig.settle().await;
        let (status, _) = rig
            .post("delete", &json!({ "ids": [id.clone()], "where": "queue" }))
            .await;
        assert_eq!(status, 200);
        rig.settle().await;
        until_history_lacks(&rig, "by-ulid").await; // deleted by ULID

        // A url — what the shipped `clearCompleted` sends, and what `item.url ?? item.id`
        // resolves to for every row that has one.
        let _ = rig.add("https://fake.test/by-url").await;
        rig.settle().await;
        let (status, _) = rig
            .post(
                "delete",
                &json!({ "ids": ["https://fake.test/by-url"], "where": "queue" }),
            )
            .await;
        assert_eq!(status, 200);
        rig.settle().await;
        until_history_lacks(&rig, "by-url").await; // deleted by url

        // A legacy media id — the `fake:` prefixed id the fake provider mints.
        let id = rig.add("https://fake.test/by-media").await;
        rig.until_status(&id, "preparing").await;
        rig.settle().await;
        let (status, _) = rig
            .post(
                "delete",
                &json!({ "ids": ["fake:by-media"], "where": "queue" }),
            )
            .await;
        assert_eq!(status, 200);
        rig.settle().await;
        until_history_lacks(&rig, "by-media").await; // deleted by media id

        // An unknown token is silently skipped, as legacy did.
        let (status, body) = rig
            .post(
                "delete",
                &json!({ "ids": ["nope", "https://nowhere.test/x"], "where": "done" }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "status": "ok" }));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_url_matching_two_rows_affects_both() {
    // The same URL added twice is two rows, and a URL-keyed API is expected to affect both — what
    // a legacy user got for free, because legacy could not hold two rows for one url at all.
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
    let url = "https://fake.test/twice";
    let first = rig.add(url).await;
    rig.until_status(&first, "finished").await;
    let second = rig.add(url).await;
    rig.until_status(&second, "finished").await;
    rig.settle().await;

    let (_, body) = rig.get("history").await;
    assert_eq!(
        body["done"].as_array().map(Vec::len),
        Some(2),
        "two rows, one url"
    );

    let (status, _) = rig
        .post("delete", &json!({ "ids": [url], "where": "done" }))
        .await;
    assert_eq!(status, 200);
    rig.settle().await;

    let (_, body) = rig.get("history").await;
    assert_eq!(
        body["done"].as_array().map(Vec::len),
        Some(0),
        "both rows are gone"
    );
}

/// `where` names a collection, and the id ladder does not: the shipped client keys every delete on
/// the URL (`item.url ?? item.id`, and `clearCompletedItems` sends only urls), so one token can
/// resolve to a finished row *and* a running one. Legacy could not cross that line — `clear()`
/// looked only in `self.done` and `cancel()` only in `self.pending`/`self.queue`
/// (`app/ytdl.py:1697-1731`) — and neither may this: "clear completed" must not kill a re-added
/// download, and deleting a running item must not destroy the older finished row (and, with
/// `DELETE_FILE_ON_TRASHCAN`, its file).
///
/// The three rows share a `media_id` (`fake:` plus the last path segment) and differ only in the
/// query, which is how one token names rows in two different states through the real pipeline.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_only_touches_the_collection_where_names() {
    let rig = rig(
        "/",
        &[
            ("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0"),
            ("AULOS_DEDUPE_MODE", "off"),
        ],
    )
    .await;
    let token = "fake:re-added";

    // A finished row, and a live one hanging in `preparing`.
    let first = rig.add("https://fake.test/re-added?take=done-take-a").await;
    rig.until_status(&first, "finished").await;
    let live = rig.add("https://fake.test/re-added?take=running").await;
    rig.until_status(&live, "preparing").await;
    rig.settle().await;
    let (_, body) = rig.get("history").await;
    assert_eq!(body["done"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["queue"].as_array().map(Vec::len), Some(1), "{body}");

    // "Clear completed": the finished row goes, the running download is untouched.
    let (status, _) = rig
        .post("delete", &json!({ "ids": [token], "where": "done" }))
        .await;
    assert_eq!(status, 200);
    let body = until_history(&rig, "the finished row to go", |b| {
        b["done"].as_array().map(Vec::len) == Some(0)
    })
    .await;
    assert_eq!(
        body["queue"].as_array().map(Vec::len),
        Some(1),
        "the running row must survive a clear-completed: {body}"
    );
    assert_eq!(rig.status_of(&live).await, "preparing");

    // The other direction: a second finished row, then a `where: "queue"` delete of the same token.
    let kept = rig.add("https://fake.test/re-added?take=done-take-c").await;
    rig.until_status(&kept, "finished").await;
    rig.settle().await;

    let (status, _) = rig
        .post("delete", &json!({ "ids": [token], "where": "queue" }))
        .await;
    assert_eq!(status, 200);
    let body = until_history(&rig, "the running row to go", |b| {
        b["queue"].as_array().map(Vec::len) == Some(0)
    })
    .await;
    assert_eq!(
        body["done"].as_array().map(Vec::len),
        Some(1),
        "the finished row must survive a queue delete: {body}"
    );
    let (code, item) = rig.get(&format!("api/v2/items/{kept}")).await;
    assert_eq!(code, 200, "the finished row is still there: {item}");
    assert_eq!(item["status"], "finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn where_must_be_queue_or_done() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
        for body in [
            json!({ "ids": ["x"] }),
            json!({ "ids": ["x"], "where": "nope" }),
            json!({ "ids": ["x"], "where": null }),
            json!({ "ids": [], "where": "done" }),
            json!({ "ids": null, "where": "done" }),
            json!({ "where": "done" }),
        ] {
            let (status, answer) = rig.post("delete", &body).await;
            assert_eq!(status, 400, "{body}");
            assert!(
                !answer["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty(),
                "the envelope must say something legacy never did"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queue_delete_cancels_a_running_item_before_dropping_the_row() {
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
    let id = rig.add("https://fake.test/running").await;
    rig.until_status(&id, "preparing").await;

    let (status, _) = rig
        .post("delete", &json!({ "ids": [id.clone()], "where": "queue" }))
        .await;
    assert_eq!(status, 200);
    rig.settle().await;

    let (code, _) = rig.get(&format!("api/v2/items/{id}")).await;
    assert_eq!(code, 404, "the row is gone, not merely cancelled");
    until_history_lacks(&rig, "running").await;
}

// ---------------------------------------------------------------------------
// POST start
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn start_starts_a_pending_item_and_retries_a_failed_one() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;

        // The legacy case: a `pending` row moves into the queue.
        let (_, added) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": "https://fake.test/parked", "auto_start": false }),
            )
            .await;
        let id = added["ids"][0].as_str().expect("an id").to_owned();
        rig.settle().await;
        let (_, body) = rig.get("history").await;
        assert_eq!(body["pending"].as_array().map(Vec::len), Some(1));

        let (status, answer) = rig.post("start", &json!({ "ids": [id.clone()] })).await;
        assert_eq!(status, 200);
        assert_eq!(answer, json!({ "status": "ok" }));
        rig.until_status(&id, "preparing").await;

        // The additive case legacy could not do: a failed row is retried, which closes iOS pain
        // point #24 with no client change.
        let failed = rig.add("https://fake.test/bad-one").await;
        rig.until_status(&failed, "error").await;
        let (status, _) = rig.post("start", &json!({ "ids": [failed.clone()] })).await;
        assert_eq!(status, 200);
        // The URL fails every time, so the observable proof of a retry is the attempt counter —
        // which legacy had no route to increment at all (iOS pain point #24).
        let item = rig
            .until(
                "the retry to be attempted",
                |item| item["attempt"].as_u64().unwrap_or(0) >= 1,
                &failed,
            )
            .await;
        assert_eq!(item["attempt"], 1, "a real retry, not a no-op");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn start_needs_an_ids_key_where_legacy_crashed() {
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
    for body in [json!({}), json!({ "ids": null })] {
        let (status, answer) = rig.post("start", &body).await;
        assert_eq!(status, 400, "legacy leaked a 500 here");
        assert_eq!(
            answer["error"]["message"], "ids is required and must be a list",
            "{body}"
        );
    }
    // A bare string was iterated character by character in Python, matching nothing.
    let (status, answer) = rig.post("start", &json!({ "ids": "ab" })).await;
    assert_eq!(status, 200);
    assert_eq!(answer, json!({ "status": "ok" }));
}

// ---------------------------------------------------------------------------
// the bounded pre-resolve (DESIGN §11.2 step 6)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_url_that_resolves_answers_ok_with_the_additive_ids() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "10000")]).await;
        let (status, body) = rig.post("add", &add_body("https://fake.test/good")).await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        let ids = body["ids"].as_array().expect("the additive ids key");
        assert_eq!(ids.len(), 1);
        assert!(ids[0].is_string());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resolution_failure_is_reported_in_the_body_at_200() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "10000")]).await;
        let (status, body) = rig.post("add", &add_body("https://fake.test/bad")).await;

        // HTTP 200 with a `status: "error"` body is the only shape the shipped
        // `AddResultClassifier` can turn into "Couldn't add to Aulos" (risk R24).
        assert_eq!(
            status, 200,
            "not a 4xx: the shipped classifier parses the body"
        );
        assert_eq!(body["status"], "error");
        let msg = body["msg"].as_str().expect("a msg");
        assert!(
            msg.contains("unsupported_url"),
            "the provider's cleaned message, got {msg:?}"
        );
        assert!(!msg.starts_with("ERROR: "), "already cleaned");

        // Δ from legacy: the item **remains** in the queue as `error`, so it is visible to a v2
        // client and appears once in v1 `done[]`.
        rig.settle().await;
        let (_, history) = rig.get("history").await;
        let done = history["done"].as_array().expect("done");
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["status"], "error");
        assert_eq!(done[0]["error"], json!(msg));
        assert_eq!(done[0]["msg"], json!(msg), "legacy overloaded msg");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_window_that_expires_answers_ok_and_counts_a_timeout() {
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "60")]).await;
    let before = aulos_api::v1::add::add_resolve_counters().read();

    let (status, body) = rig.post("add", &add_body("https://fake.test/slow")).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok", "the real outcome shows up in history");
    assert!(body["ids"].as_array().is_some_and(|i| i.len() == 1));

    let after = aulos_api::v1::add::add_resolve_counters().read();
    assert_eq!(
        after.2,
        before.2 + 1,
        "aulos_v1_add_resolve_total{{outcome=\"timeout\"}}"
    );

    // The item is still resolving, which v1 projects as `pending` in `queue[]`.
    let (_, history) = rig.get("history").await;
    assert_eq!(history["queue"][0]["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_window_answers_immediately_and_never_waits() {
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
    let before = aulos_api::v1::add::add_resolve_counters().read();

    // `slow` parks in `resolving` for ten minutes; with the window disabled this must not block.
    let started = std::time::Instant::now();
    let (status, body) = rig.post("add", &add_body("https://fake.test/slow")).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "a disabled window must not wait at all"
    );

    let after = aulos_api::v1::add::add_resolve_counters().read();
    assert_eq!(after.3, before.3 + 1, "the skipped counter");
    assert_eq!(after.2, before.2, "and no timeout was recorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_validation_failure_answers_400_before_any_waiting() {
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "600000")]).await;
    let started = std::time::Instant::now();
    let (status, body) = rig
        .post(
            "add",
            &json!({
                "url": "https://fake.test/slow", "download_type": "video",
                "format": "mkv", "quality": "best"
            }),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "format must be one of ['any', 'ios', 'mp4'] for video"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "validation runs before the engine is even asked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_url_answers_ok_with_no_new_item() {
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "10000")]).await;
    let first = rig.post("add", &add_body("https://fake.test/dupe")).await;
    assert_eq!(first.1["status"], "ok");
    let one = first.1["ids"].as_array().expect("ids").len();
    assert_eq!(one, 1);

    let (status, body) = rig.post("add", &add_body("https://fake.test/dupe")).await;
    assert_eq!(status, 200, "legacy skipped a duplicate silently");
    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["ids"].as_array().map(Vec::len),
        Some(0),
        "no new item was minted"
    );
}

// ---------------------------------------------------------------------------
// cancel-add
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cancel_add_ignores_its_body_and_aborts_in_flight_resolution() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")]).await;
        let id = rig.add("https://fake.test/slow").await;
        rig.until_status(&id, "resolving").await;

        // A generation, an arbitrary object and no body at all all behave identically: legacy's
        // `cancel_add()` took no argument, so there is nothing for a v1 client to send.
        for body in [
            json!({ "generation": 7 }),
            json!({ "anything": [1, 2] }),
            json!({}),
        ] {
            let (status, answer) = rig.post("cancel-add", &body).await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(answer, json!({ "status": "ok" }));
        }
        let response = rig
            .http
            .post(rig.url("cancel-add"))
            .send()
            .await
            .expect("a bodyless POST must be accepted");
        assert_eq!(response.status(), 200);

        // Δ: it now actually aborts, where legacy only checked between entries.
        let item = rig
            .until(
                "the resolve to be aborted",
                |item| item["status"] == "canceled",
                &id,
            )
            .await;
        assert_eq!(item["status"], "canceled");
        // A cancelled row vanishes from v1, as legacy made cancels vanish.
        until_history_lacks(&rig, "slow").await;
    })
    .await;
}

// ---------------------------------------------------------------------------
// the small routes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn robots_txt_is_the_three_line_body() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[]).await;
        let response = rig.get_raw("robots.txt").await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
        let text = response.text().await.expect("a body");
        assert_eq!(
            text, "User-agent: *\nDisallow: /download/\nDisallow: /audio_download/\n",
            "DESIGN §11.7, byte for byte"
        );
        assert_eq!(text.lines().count(), 3);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn socket_io_fails_loudly_and_immediately() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[]).await;
        for suffix in [
            "socket.io/",
            "socket.io/?EIO=4&transport=polling",
            "socket.io/anything/else",
        ] {
            let started = std::time::Instant::now();
            let (status, body) = rig.get(suffix).await;
            assert_eq!(status, 501, "{suffix}");
            assert_eq!(body["error"]["code"], "socketio_removed");
            let message = body["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains("Socket.IO is not supported"), "{message}");
            assert!(message.contains("ws (protocol v2)"), "{message}");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "it must not hang on a handshake"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shim_can_be_switched_off_without_touching_v2() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("AULOS_V1_ENABLED", "false")]).await;
        for (method, suffix) in [
            ("GET", "history"),
            ("GET", "presets"),
            ("GET", "subscriptions"),
            ("GET", "cookie-status"),
            ("POST", "add"),
            ("POST", "delete"),
            ("POST", "start"),
            ("POST", "cancel-add"),
            ("POST", "subscribe"),
            ("POST", "subscriptions/update"),
            ("POST", "subscriptions/delete"),
            ("POST", "subscriptions/check"),
            ("POST", "upload-cookies"),
            ("POST", "delete-cookies"),
        ] {
            let status = if method == "GET" {
                rig.get(suffix).await.0
            } else {
                rig.post(suffix, &json!({})).await.0
            };
            assert_eq!(status, 404, "{method} {suffix} must be gone");
        }

        // v2 is untouched, and `capabilities` says so.
        let (status, body) = rig.get("api/v2/state").await;
        assert_eq!(status, 200);
        assert!(body["items"].is_array());
        let (_, caps) = rig.get("api/v2/capabilities").await;
        assert_eq!(caps["protocol"]["v1_shim"], false);
        let (status, _) = rig.get("version").await;
        assert_eq!(status, 200, "version is served for both protocol versions");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_root_prefix_redirects_the_bare_root() {
    let rig = rig("/metube/", &[]).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a client");
    for path in ["/", "/metube"] {
        let response = client
            .get(format!("http://{}{path}", rig.addr))
            .send()
            .await
            .expect("an answer");
        assert_eq!(response.status(), 302, "{path}: legacy used web.HTTPFound");
        assert_eq!(
            response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok()),
            Some("/metube/"),
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_nine_options_routes_answer_ok_with_legacy_cors() {
    for_each_prefix(|prefix| async move {
        let rig = rig(
            prefix,
            &[("CORS_ALLOWED_ORIGINS", "https://ui.example.com")],
        )
        .await;
        for suffix in aulos_api::v1::OPTIONS_ROUTES {
            let response = rig
                .http
                .request(reqwest::Method::OPTIONS, rig.url(suffix))
                .header("Origin", "https://ui.example.com")
                .send()
                .await
                .expect("an answer");
            assert_eq!(response.status(), 200, "{suffix}");
            let headers = response.headers().clone();
            assert_eq!(
                headers
                    .get("access-control-allow-origin")
                    .and_then(|v| v.to_str().ok()),
                Some("https://ui.example.com"),
                "{suffix}"
            );
            assert_eq!(
                headers
                    .get("access-control-allow-headers")
                    .and_then(|v| v.to_str().ok()),
                Some("Content-Type"),
                "{suffix}: legacy sent Content-Type and nothing else"
            );
            assert!(
                headers.get("access-control-allow-methods").is_none(),
                "{suffix}: legacy sent no methods header on a v1 route"
            );
            let body: Value = response.json().await.expect("a JSON body");
            assert_eq!(body, json!({ "status": "ok" }), "{suffix}");
        }

        // A disallowed origin is reflected nowhere.
        let response = rig
            .http
            .get(rig.url("history"))
            .header("Origin", "https://evil.example.com")
            .send()
            .await
            .expect("an answer");
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shim_sits_behind_the_same_auth_layer_as_v2_and_never_redirects() {
    let rig = rig("/", &[("AULOS_API_TOKEN", "s3cret")]).await;
    for (method, suffix) in [
        ("GET", "history"),
        ("POST", "add"),
        ("GET", "subscriptions"),
    ] {
        let response = rig
            .http
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).expect("a method"),
                rig.url(suffix),
            )
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("an answer");
        assert_eq!(response.status(), 401, "{method} {suffix}");
        assert!(
            response.headers().get("location").is_none(),
            "{method} {suffix}: never a redirect (PROTOCOL §1.4)"
        );
        let body: Value = response.json().await.expect("the error envelope");
        assert_eq!(body["error"]["code"], "unauthorized");
        assert_eq!(body["error"]["message"], "authentication required");
    }

    // With the token, the shim answers normally.
    let response = rig
        .http
        .get(rig.url("history"))
        .bearer_auth("s3cret")
        .send()
        .await
        .expect("an answer");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("a body");
    assert!(body["queue"].is_array());
}

// ---------------------------------------------------------------------------
// the subscription routes
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_subscription_routes_answer_the_legacy_thirteen_keys() {
    for_each_prefix(|prefix| async move {
        let rig = rig(prefix, &[("SUBSCRIPTION_DEFAULT_CHECK_INTERVAL", "60")]).await;

        // `GET subscriptions` is a bare **array**, not an envelope.
        let (status, body) = rig.get("subscriptions").await;
        assert_eq!(status, 200);
        assert_eq!(body, json!([]));

        // A numeric-string interval is accepted (§11.2.1 row 5) and reaches the record.
        let (status, body) = rig
            .post(
                "subscribe",
                &json!({
                    "url": "https://www.youtube.com/@veritasium/videos",
                    "download_type": "video",
                    "format": "any",
                    "quality": "best",
                    "check_interval_minutes": "30"
                }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        let created = &body["subscription"];
        assert_subscription(created);
        assert_eq!(created["check_interval_minutes"], 30);
        let id = created["id"].as_str().expect("an id").to_owned();

        // The same 13 keys come back from the list.
        let (_, body) = rig.get("subscriptions").await;
        let rows = body.as_array().expect("an array");
        assert_eq!(rows.len(), 1);
        assert_subscription(&rows[0]);

        // A single-video URL is a legacy *business* error: HTTP 200 with the exact sentence.
        let (status, body) = rig
            .post(
                "subscribe",
                &json!({
                    "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                    "download_type": "video", "format": "any", "quality": "best"
                }),
            )
            .await;
        assert_eq!(status, 200, "legacy reported this in the body");
        assert_eq!(body["status"], "error");
        assert_eq!(
            body["msg"],
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );

        // `update` accepts only the legacy three, floors the interval at 1, and ignores a blank
        // name — every one a legacy behaviour rather than a validation error.
        let (status, body) = rig
            .post(
                "subscriptions/update",
                &json!({ "id": id.clone(), "name": "", "check_interval_minutes": 0,
                         "enabled": "false" }),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["subscription"]["check_interval_minutes"], 1);
        assert_eq!(body["subscription"]["enabled"], false);
        assert_eq!(
            body["subscription"]["name"], "Veritasium",
            "a falsy name is silently ignored"
        );

        // `check` answers immediately with a job handle instead of blocking for minutes.
        let (status, body) = rig
            .post("subscriptions/check", &json!({ "ids": [id.clone()] }))
            .await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        assert!(body["job_id"].is_string(), "the additive job handle");

        // `delete` is `{"status":"ok"}`, and `[]` is still a 400.
        let (status, body) = rig
            .post("subscriptions/delete", &json!({ "ids": [id] }))
            .await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "status": "ok" }));
        let (status, body) = rig
            .post("subscriptions/delete", &json!({ "ids": [] }))
            .await;
        assert_eq!(status, 400);
        assert_eq!(body["error"]["message"], "missing ids list");
        let (_, body) = rig.get("subscriptions").await;
        assert_eq!(body, json!([]));
    })
    .await;
}

// ---------------------------------------------------------------------------
// the mechanical schema check (PLAN WP-15)
// ---------------------------------------------------------------------------

/// The shipped Swift models' expectations, asserted mechanically rather than by inspection.
///
/// PLAN WP-15 asks for this as "a JSON-Schema check generated by `aulos-server print-schema`";
/// `print-schema` is **CUT** by the BRIEF scope trims, so the same three claims are asserted
/// directly against live responses instead: `queue`/`pending`/`done` are always present, `status`
/// is only ever one of the five legacy strings, and `percent` decodes as a number.
#[tokio::test(flavor = "multi_thread")]
async fn the_shipped_client_models_decode_every_route() {
    // A real pre-resolve window, so the `status: "error"` half of the add contract is exercised.
    let rig = rig("/", &[("AULOS_V1_ADD_RESOLVE_WAIT_MS", "10000")]).await;

    // A row per bucket, plus a terminal one from the store.
    let running = rig.add("https://fake.test/running").await;
    let (_, parked) = rig
        .post(
            "api/v2/downloads",
            &json!({ "url": "https://fake.test/parked", "auto_start": false }),
        )
        .await;
    let _ = parked;
    rig.until_status(&running, "preparing").await;
    seed_terminal(&rig.store, 3).await;
    rig.settle().await;

    let (status, body) = rig.get("history").await;
    assert_eq!(status, 200);
    // `HistoryResponse` declares all three non-optional and a missing key empties the queue.
    for key in ["queue", "pending", "done"] {
        assert!(
            body[key].as_array().is_some_and(|a| !a.is_empty()),
            "{key} must be present and populated"
        );
    }
    for key in ["queue", "pending", "done"] {
        for item in body[key].as_array().into_iter().flatten() {
            assert_item(item);
        }
    }

    // `GET version`: the two legacy keys plus the two additive ones.
    let (status, body) = rig.get("version").await;
    assert_eq!(status, 200);
    assert!(body["version"].is_string());
    assert!(body["yt-dlp"].is_string() || body["yt-dlp"].is_null());
    assert!(body["url_prefix"].is_string());
    assert_eq!(body["protocol"], "v2");

    // `POST add`: always `status`, and `msg` exactly when `status == "error"`.
    let (status, body) = rig.post("add", &add_body("https://fake.test/schema")).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "ok");
    assert!(body.get("msg").is_none());
    let (status, body) = rig
        .post("add", &add_body("https://fake.test/bad-schema"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "error");
    assert!(body["msg"].is_string());
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Every projected item must carry the 32 keys, one of the five statuses, and numeric numbers.
fn assert_item(item: &Value) {
    let object = item.as_object().expect("an item object");
    assert_eq!(
        object.len(),
        ITEM_KEYS.len(),
        "the key set is fixed: {item}"
    );
    for key in ITEM_KEYS {
        assert!(object.contains_key(key), "missing {key} in {item}");
    }
    assert!(!object.contains_key("entry"), "entry is omitted");
    let status = object["status"].as_str().unwrap_or_default();
    assert!(V1_STATUSES.contains(&status), "{status:?} is not legacy");
    assert!(object["percent"].is_number(), "percent is always a number");
    assert!(object["timestamp"].is_number());
    assert!(object["ytdl_options_presets"].is_array());
    assert!(object["ytdl_options_overrides"].is_object());
    assert!(object["chapter_files"].is_array());
    assert!(object["subtitle_files"].is_array());
}

/// One subscription: exactly the legacy 13 keys, `last_checked` as float seconds or null.
fn assert_subscription(row: &Value) {
    let object = row.as_object().expect("a subscription object");
    assert_eq!(
        object.len(),
        13,
        "the legacy projection, not v2's 16: {row}"
    );
    for key in [
        "id",
        "name",
        "url",
        "enabled",
        "check_interval_minutes",
        "download_type",
        "codec",
        "format",
        "quality",
        "folder",
        "last_checked",
        "seen_count",
        "error",
    ] {
        assert!(object.contains_key(key), "missing {key} in {row}");
    }
    for key in ["next_due", "consecutive_failures", "checking"] {
        assert!(!object.contains_key(key), "{key} is v2-only");
    }
    assert!(
        object["last_checked"].is_null() || object["last_checked"].is_f64(),
        "last_checked is float seconds in v1"
    );
    assert!(
        object["folder"].is_string(),
        "legacy emitted \"\", not null"
    );
}

/// Whether any of the three arrays holds an item whose URL contains `needle`.
fn history_body_contains(body: &Value, needle: &str) -> bool {
    ["queue", "pending", "done"].into_iter().any(|key| {
        body[key]
            .as_array()
            .into_iter()
            .flatten()
            .any(|i| i["url"].as_str().unwrap_or_default().contains(needle))
    })
}

/// Waits until `GET history` no longer mentions `needle`, or panics.
///
/// `GET history` sources `queue`/`pending` from the **published snapshot** (DESIGN §11.4), which
/// the aggregator refreshes on its own tick — so a row that `GET api/v2/items/{id}` already
/// reports as terminal can still be in the last published generation for up to one tick. That is
/// the documented ordering ("a REST reader's cursor is never newer than the socket"), not a bug,
/// so a test must wait for the condition it means rather than for a fixed number of sleeps.
async fn until_history_lacks(rig: &Rig, needle: &str) {
    for _ in 0..600 {
        let (_, body) = rig.get("history").await;
        if !history_body_contains(&body, needle) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, body) = rig.get("history").await;
    panic!("timed out waiting for {needle} to leave v1 history; it is {body}");
}

/// Waits until `GET history` satisfies `pred`, and returns that body.
///
/// `queue`/`pending` come from the **published snapshot**, which the aggregator refreshes on its
/// own tick, so a row `GET api/v2/items/{id}` already reports as gone can linger there for up to
/// one tick — the documented ordering, not a bug.
async fn until_history(rig: &Rig, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..600 {
        let (_, body) = rig.get("history").await;
        if pred(&body) {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (_, body) = rig.get("history").await;
    panic!("timed out waiting for {what}; v1 history is {body}");
}

/// Inserts `count` `finished` rows straight into the store, titled `Row NNNN` in `ord` order.
///
/// Direct insertion rather than `count` real downloads: the point of the test is the *read* path's
/// bound, and 4 211 scripted downloads would take minutes for no extra coverage.
async fn seed_terminal(store: &Store, count: usize) {
    let mut items = Vec::with_capacity(count);
    for i in 0..count {
        let ord = store.next_ord();
        items.push(terminal_item(
            ord,
            &format!("Row {i:04}"),
            &format!("https://archive.test/row/{i}"),
        ));
    }
    // One transaction per 1 000 rows keeps the batch under any statement-parameter ceiling.
    for chunk in items.chunks(1_000) {
        store
            .write(
                vec![WriteOp::InsertItems {
                    items: chunk.to_vec(),
                }],
                Durability::Sync,
            )
            .await
            .expect("the seed must land");
    }
}

/// One `finished` row with the legacy-shaped fields the projection reads.
fn terminal_item(ord: Ord0, title: &str, url: &str) -> Item {
    let parsed = Url::parse(url).expect("a literal url");
    let selection = Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").expect("mp4"),
        QualityId::parse("best").expect("best"),
    );
    let request = DownloadRequest::new(parsed.clone(), selection);
    Item {
        id: ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord,
        url: parsed,
        canonical_key: url.into(),
        provider: None,
        media_id: Some(Box::from(title)),
        title: Box::from(title),
        status: Status::Finished,
        auto_start: true,
        msg: None,
        error: None,
        request,
        entry: None,
        filename: None,
        size: Some(1_024),
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 1_757_000_000_000,
        started_at: Some(1_757_000_000_000),
        finished_at: Some(1_757_000_001_000),
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV1),
        children_total: None,
        clear_after: None,
    }
}

/// A legacy `POST add` for a provider whose catalog is **advisory** must be accepted, because
/// legacy had no per-provider catalog and accepted every matrix-legal combination for every URL.
/// The WP-15 request in `docs/INTEGRATION-NOTES.md`.
#[tokio::test]
async fn a_legacy_add_for_an_advisory_catalog_is_accepted() {
    let rig = Rig::start("/").await;
    // Only a download type the advisory catalog declares is snapped. `audio` is *not*: SC serves
    // no audio-only rendition, and a `400` is more honest than silently handing back a video
    // file. See `v1::request::snap_to_advisory_catalog`.
    // Only a download type the advisory catalog declares is snapped. `audio` is *not*: SC serves
    // no audio-only rendition, and a `400` is more honest than silently handing back a video
    // file. See `v1::request::snap_to_advisory_catalog`.
    for (i, (quality, format, codec)) in [
        ("1080", "any", "h264"),
        ("best", "mp4", "auto"),
        ("720", "any", "auto"),
        ("2160", "any", "vp9"),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = rig
            .post(
                "add",
                &json!({
                    "url": format!("https://streamingcommunity.test/watch/{i}"),
                    "download_type": "video",
                    "quality": quality,
                    "format": format,
                    "codec": codec,
                }),
            )
            .await;
        assert_eq!(status, 200, "{quality}/{format}/{codec}: {body}");
        assert_eq!(body["status"], "ok", "{quality}/{format}/{codec}: {body}");

        // And it was snapped to the catalog's one offering, not merely waved through.
        let id = body["ids"][0].as_str().expect("an id");
        let (_, item) = rig.get(&format!("api/v2/items/{id}")).await;
        assert_eq!(item["selection"]["format"], "mp4", "{item}");
        assert_eq!(item["selection"]["quality"], "best", "{item}");
        assert_eq!(
            item["selection"]["codec"], "auto",
            "the codec control is hidden for this catalog: {item}"
        );
    }

    // `audio` is not declared by that catalog and still answers the legacy validation error.
    let (status, body) = rig
        .post(
            "add",
            &json!({
                "url": "https://streamingcommunity.test/watch/audio",
                "download_type": "audio",
                "quality": "best",
                "format": "m4a",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
}

/// The snap is confined to the advisory case: a `ytdlp` URL still gets the full catalog check,
/// so a selection the catalog does not offer is still a `400` with the legacy string.
#[tokio::test]
async fn the_advisory_snap_does_not_loosen_a_real_catalog() {
    let rig = Rig::start("/").await;
    let (status, body) = rig
        .post(
            "add",
            &json!({
                "url": "https://youtube.test/watch?v=1",
                "download_type": "video",
                "quality": "1080",
                "format": "nonsense",
            }),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["field"], "format");
}

/// The same bug through the legacy surface the shipped clients read: `GET history`'s `done[]`.
///
/// v1 overloaded `msg` — the live stage line for a running item, the failure text for a terminal
/// error (`project_item`) — so a stale `"MoveFiles…"` on a finished row is exactly as visible
/// there as it is on v2. It is `null` on `done[]`, and the `error` projection is untouched.
#[tokio::test]
async fn a_finished_history_entry_carries_no_postprocessor_line() {
    /// `preparing → downloading → a window this test can act in → finished`.
    fn slow_finish() -> FakeProvider {
        FakeProvider::from_toml(
            r#"
            id     = "fake"
            score  = 200
            strong = true
            hosts  = ["fake.test"]

            [[timeline]]
            download = [
                { kind = "stage", stage = "preparing" },
                { kind = "stage", stage = "downloading" },
                { kind = "wait",  ms = 900 },
                { kind = "finish", filename = "A video.mp4", size = 1024 },
            ]
        "#,
        )
        .expect("the slow provider must parse")
    }

    let rig = Rig::builder("/")
        .without_default_providers()
        .env("AULOS_RESOLVE_FALLTHROUGH", "false")
        .provider(Arc::new(ytdlp_like()))
        .provider(Arc::new(slow_finish()))
        .start()
        .await;
    let id = rig.add("https://fake.test/watch/pp").await;
    rig.until_status(&id, "downloading").await;

    // The shim's last `pp` frame (DESIGN §9.5), on an item that is still running.
    rig.state
        .engine
        .stage(
            id.parse::<ItemId>().expect("a ULID"),
            aulos_provider::Stage::Postprocessing,
            Some("MoveFiles…".into()),
        )
        .await;
    let running = rig
        .until(
            "the postprocessor line",
            |item| item["msg"] == "MoveFiles…",
            &id,
        )
        .await;
    assert_eq!(running["status"], "postprocessing", "{running}");

    rig.until_status(&id, "finished").await;
    rig.settle().await;

    let (status, body) = rig.get("history").await;
    assert_eq!(status, 200, "{body}");
    let done = body["done"].as_array().expect("done");
    assert_eq!(done.len(), 1, "{body}");
    assert_eq!(done[0]["status"], "finished");
    assert!(
        done[0].get("msg").is_some(),
        "the key is always present: {body}"
    );
    assert_eq!(
        done[0]["msg"],
        Value::Null,
        "a finished row's status line is null, not the last postprocessor: {body}"
    );
    assert_eq!(done[0]["error"], Value::Null);
}
