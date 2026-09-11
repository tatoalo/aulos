//! `POST api/v2/downloads`, `items/actions`, `downloads/cancel-resolve`, `GET api/v2/state` and
//! `GET api/v2/items` — the queue half of PROTOCOL §4, under both prefixes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use serde_json::{Value, json};
use support::{Rig, for_each_prefix, hanging, slow_resolve, ytdlp_like};

const YT: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

// ---------------------------------------------------------------------------
// POST api/v2/downloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_single_add_is_accepted_and_echoes_every_documented_key() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": YT, "format": "mp4", "quality": "1080" }),
            )
            .await;
        assert_eq!(status, 202, "{body}");
        for key in ["id", "ids", "generation", "seq", "duplicates", "warnings"] {
            assert!(body.get(key).is_some(), "{key} must be present: {body}");
        }
        assert_eq!(body["id"], body["ids"][0]);
        assert!(body["duplicates"].as_array().unwrap().is_empty());
        assert!(body["warnings"].as_array().unwrap().is_empty());
        assert!(body["seq"].as_u64().is_some());
    })
    .await;
}

/// The async-add guarantee: the item is in the state snapshot as `resolving`, and the `202` came
/// back before resolution finished.
///
/// The poll exists because the published snapshot is republished by the aggregator one urgent tick
/// (5 ms in the rig) after the ack — the ack itself is what happens "before any metadata
/// extraction", which is what the guarantee is about, and the assertion that the status is
/// `resolving` rather than `queued` is what proves the response did not wait for the resolve.
#[tokio::test]
async fn the_added_item_appears_in_state_as_resolving() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(slow_resolve()))
            .start()
            .await;
        let id = rig.add("https://fake.test/watch/x").await;
        let state = rig
            .until_state("the new item", |body| {
                body["items"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|i| i["id"] == id.as_str()))
            })
            .await;
        let item = state["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == id.as_str())
            .unwrap();
        assert_eq!(item["status"], "resolving");
        assert_eq!(
            item["title"], "https://fake.test/watch/x",
            "the URL until resolved"
        );
        assert_eq!(item["percent"], 0.0, "never null");
        assert!(item["download_url"].is_null());
    })
    .await;
}

/// PROTOCOL §1.3: `X-Aulos-Client: ios/<version>` attributes the add to the iOS app, and nothing
/// else does. The kind is what DESIGN §25 routes an APNs alert on, so it has to be on the item the
/// client can read back — both for a single add and for a batch, which share one add path.
#[tokio::test]
async fn the_ios_client_header_attributes_the_add_to_the_phone() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        let post = async |client: Option<&str>, body: Value| {
            let mut req = rig.http.post(rig.url("api/v2/downloads")).json(&body);
            if let Some(client) = client {
                req = req.header("X-Aulos-Client", client);
            }
            let response = req.send().await.unwrap();
            assert_eq!(response.status().as_u16(), 202);
            response.json::<Value>().await.unwrap()
        };

        // `auto_start: false` parks each row in `queued`, so the assertion is not a race with a
        // download that finishes before the item can be read back.
        let ios = post(
            Some("ios/1.4.0 (77)"),
            json!({ "url": YT, "auto_start": false }),
        )
        .await;
        let item = rig
            .until_status(ios["ids"][0].as_str().unwrap(), "queued")
            .await;
        assert_eq!(item["source"], json!({ "kind": "ios", "ref": null }));

        // A batch shares the add path, so every item in it is attributed the same way.
        let batch = post(
            Some("IOS"),
            json!({
                "items": [
                    { "url": "https://www.youtube.com/watch?v=batch-a" },
                    { "url": "https://www.youtube.com/watch?v=batch-b" },
                ],
                "defaults": { "auto_start": false },
            }),
        )
        .await;
        for id in batch["ids"].as_array().unwrap() {
            let item = rig.until_status(id.as_str().unwrap(), "queued").await;
            assert_eq!(item["source"]["kind"], "ios", "{item}");
        }

        // No header, and an unknown client, are both plain `api_v2`.
        for (client, url) in [
            (None, "https://www.youtube.com/watch?v=plain"),
            (Some("android/1"), "https://www.youtube.com/watch?v=droid"),
        ] {
            let body = post(client, json!({ "url": url, "auto_start": false })).await;
            let item = rig
                .until_status(body["ids"][0].as_str().unwrap(), "queued")
                .await;
            assert_eq!(item["source"]["kind"], "api_v2", "{client:?}: {item}");
        }
    })
    .await;
}

/// PROTOCOL §1.3/§4.1: `X-Aulos-Install` puts the calling install into `source.ref` — but only
/// alongside `X-Aulos-Client: ios/…`, and only when the value is one the server will key on.
///
/// This is what DESIGN §25.2 routes the alert and the push-to-start on, so it has to come back on
/// the wire the same way a Telegram chat id does, and a malformed value has to be *absent* rather
/// than a `400`: a proxy that mangles a header must not be able to break an add.
#[tokio::test]
async fn the_install_header_names_the_install_inside_the_ios_source() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        let post = async |client: Option<&str>, install: Option<&str>, body: Value| {
            let mut req = rig.http.post(rig.url("api/v2/downloads")).json(&body);
            if let Some(client) = client {
                req = req.header("X-Aulos-Client", client);
            }
            if let Some(install) = install {
                req = req.header("X-Aulos-Install", install);
            }
            let response = req.send().await.unwrap();
            assert_eq!(response.status().as_u16(), 202);
            response.json::<Value>().await.unwrap()
        };
        let source = async |body: &Value| {
            rig.until_status(body["ids"][0].as_str().unwrap(), "queued")
                .await["source"]
                .clone()
        };

        // The phone: kind *and* ref.
        let phone = post(
            Some("ios/1.4.0 (77)"),
            Some("3F2504E0-4F89-11D3-9A0C-0305E82C3301"),
            json!({ "url": YT, "auto_start": false }),
        )
        .await;
        assert_eq!(
            source(&phone).await,
            json!({ "kind": "ios", "ref": "3F2504E0-4F89-11D3-9A0C-0305E82C3301" })
        );

        // A batch shares the add path, so every item in it carries the same install.
        let batch = post(
            Some("ios/1.4.0 (77)"),
            Some("ipad.0001"),
            json!({
                "items": [
                    { "url": "https://www.youtube.com/watch?v=inst-a" },
                    { "url": "https://www.youtube.com/watch?v=inst-b" },
                ],
                "defaults": { "auto_start": false },
            }),
        )
        .await;
        for id in batch["ids"].as_array().unwrap() {
            let item = rig.until_status(id.as_str().unwrap(), "queued").await;
            assert_eq!(
                item["source"],
                json!({ "kind": "ios", "ref": "ipad.0001" }),
                "{item}"
            );
        }

        // A malformed or missing value is *absent*, never a 400 — the add still succeeds as the
        // bare iOS source an older app build produces.
        for (install, url) in [
            (None, "https://www.youtube.com/watch?v=inst-none"),
            (Some("short7"), "https://www.youtube.com/watch?v=inst-short"),
            (
                Some("has spaces!"),
                "https://www.youtube.com/watch?v=inst-sp",
            ),
        ] {
            let body = post(
                Some("ios/1.4.0 (77)"),
                install,
                json!({ "url": url, "auto_start": false }),
            )
            .await;
            assert_eq!(
                source(&body).await,
                json!({ "kind": "ios", "ref": null }),
                "{install:?}"
            );
        }

        // The header refines the iOS origin and nothing else: a web client sending it is `api_v2`
        // with a null ref, exactly as before.
        let web = post(
            None,
            Some("3F2504E0-4F89-11D3-9A0C-0305E82C3301"),
            json!({ "url": "https://www.youtube.com/watch?v=inst-web", "auto_start": false }),
        )
        .await;
        assert_eq!(source(&web).await, json!({ "kind": "api_v2", "ref": null }));
    })
    .await;
}

#[tokio::test]
async fn a_batch_merges_defaults_under_each_item() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig
            .post(
                "api/v2/downloads",
                &json!({
                    "items": [
                        { "url": "https://www.youtube.com/watch?v=a" },
                        { "url": "https://www.youtube.com/watch?v=b", "quality": "720" },
                    ],
                    "defaults": { "format": "mp4", "quality": "1080", "auto_start": false },
                }),
            )
            .await;
        assert_eq!(status, 202, "{body}");
        let ids = body["ids"].as_array().unwrap();
        assert_eq!(ids.len(), 2);

        let first = rig.until_status(ids[0].as_str().unwrap(), "queued").await;
        let second = rig.until_status(ids[1].as_str().unwrap(), "queued").await;
        assert_eq!(first["selection"]["quality"], "1080", "from defaults");
        assert_eq!(second["selection"]["quality"], "720", "the item wins");
        assert_eq!(first["auto_start"], false, "defaults reach auto_start");
        assert_eq!(first["request"]["auto_start"], false);
    })
    .await;
}

/// A batch body's shared selection goes in `defaults` (PROTOCOL §4.1) and nowhere else. A
/// top-level `format` is a natural mis-read of that, and the §4.1 rule for a field the server does
/// not know is that it is ignored **and** named in `warnings` — accepting it silently, applying
/// nothing, is the one combination that leaves the client believing its selection was honoured.
#[tokio::test]
async fn a_top_level_request_field_on_a_batch_body_is_a_warning() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig
            .post(
                "api/v2/downloads",
                &json!({
                    "items": [
                        { "url": "https://www.youtube.com/watch?v=a" },
                        { "url": "https://www.youtube.com/watch?v=b" },
                    ],
                    "format": "mp4",
                    "quality": "1080",
                }),
            )
            .await;
        assert_eq!(status, 202, "{body}");
        let warnings = body["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for field in ["format", "quality"] {
            assert!(
                warnings.iter().any(|w| w.as_str().unwrap().contains(field)),
                "{field} was dropped without a word: {warnings:?}"
            );
        }
        // And nothing was applied from it — `defaults` is the only shared layer, so both items
        // carry exactly what a plain add would.
        // The selection is set at add time and never moves, so neither item has to be caught in
        // a particular status — only fetched once the row exists.
        let ids = body["ids"].as_array().unwrap();
        let has_selection = |item: &Value| item["selection"]["format"].is_string();
        let batched = rig
            .until("the batched item", has_selection, ids[0].as_str().unwrap())
            .await;
        let control = rig.add("https://www.youtube.com/watch?v=c").await;
        let control = rig.until("the control item", has_selection, &control).await;
        assert_eq!(
            batched["selection"], control["selection"],
            "a top-level field must not be applied: {batched}"
        );
    })
    .await;
}

#[tokio::test]
async fn an_unknown_field_is_a_warning_and_never_a_400() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": YT, "shiny_new_field": 42, "another": "x" }),
            )
            .await;
        assert_eq!(status, 202, "a mixed-version rollout must not 400: {body}");
        let warnings = body["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap().contains("shiny_new_field"))
        );
        assert_eq!(body["ids"].as_array().unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn every_validation_failure_names_its_field() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("ALLOW_YTDL_OPTIONS_OVERRIDES", "false")
            .start()
            .await;
        let cases: [(Value, &str, &str); 10] = [
            (json!({}), "url", "validation_failed"),
            (json!({ "url": "not a url" }), "url", "validation_failed"),
            (
                json!({ "url": "magnet:?xt=urn:btih:x" }),
                "url",
                "unsupported_url",
            ),
            (
                json!({ "url": YT, "download_type": "hologram" }),
                "download_type",
                "validation_failed",
            ),
            (
                json!({ "url": YT, "codec": "h999" }),
                "codec",
                "validation_failed",
            ),
            (
                json!({ "url": YT, "format": "flac" }),
                "format",
                "validation_failed",
            ),
            (
                json!({ "url": YT, "quality": "1081" }),
                "quality",
                "validation_failed",
            ),
            (
                json!({ "url": YT, "subtitle_language": "en_US" }),
                "subtitle_language",
                "validation_failed",
            ),
            (
                json!({ "url": YT, "subtitle_mode": "whatever" }),
                "subtitle_mode",
                "validation_failed",
            ),
            (
                json!({ "url": YT, "ytdl_options_overrides": { "quiet": true } }),
                "ytdl_options_overrides",
                "overrides_disabled",
            ),
        ];
        for (body, field, code) in cases {
            let (status, answer) = rig.post("api/v2/downloads", &body).await;
            assert_eq!(status, 400, "{body} answered {answer}");
            assert_eq!(answer["error"]["code"], code, "for {body}");
            assert_eq!(answer["error"]["field"], field, "for {body}");
            assert!(
                answer["error"]["request_id"].as_str().is_some(),
                "the envelope is stamped: {answer}"
            );
        }

        // `playlist_item_limit` keeps the legacy message, byte for byte.
        let (status, answer) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": YT, "playlist_item_limit": "3" }),
            )
            .await;
        assert_eq!(status, 400);
        assert_eq!(
            answer["error"]["message"],
            "playlist_item_limit must be an integer"
        );

        // A folder that escapes the base directory is `folder_invalid`, not a 500.
        let (status, answer) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": YT, "folder": "../etc" }),
            )
            .await;
        assert_eq!(status, 400);
        assert_eq!(answer["error"]["code"], "folder_invalid");
        assert_eq!(answer["error"]["field"], "folder");

        // An unknown preset is its own code.
        let (status, answer) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": YT, "ytdl_options_presets": ["nope"] }),
            )
            .await;
        assert_eq!(status, 400);
        assert_eq!(answer["error"]["code"], "unknown_preset");
    })
    .await;
}

#[tokio::test]
async fn a_duplicate_is_reported_not_rejected() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let body = json!({ "url": "https://fake.test/dup", "auto_start": false });
        let (_, first) = rig.post("api/v2/downloads", &body).await;
        let id = first["ids"][0].as_str().unwrap().to_owned();
        rig.until_status(&id, "queued").await;

        let (status, second) = rig.post("api/v2/downloads", &body).await;
        assert_eq!(status, 202, "a duplicate is not an error: {second}");
        assert!(second["ids"].as_array().unwrap().is_empty());
        // PROTOCOL §4.1 types `id` as a non-nullable string and §0 rule 2 tells a Swift client to
        // declare it non-optional, so the all-deduped case falls back to the existing item's id —
        // never `null`, which would make the documented "not an error" path a decode failure.
        assert_eq!(
            second["id"],
            id.as_str(),
            "id falls back to the first duplicate's existing_id: {second}"
        );
        let duplicates = second["duplicates"].as_array().unwrap();
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0]["existing_id"], id.as_str());
        assert_eq!(duplicates[0]["url"], "https://fake.test/dup");
    })
    .await;
}

#[tokio::test]
async fn a_batch_over_the_cap_is_413() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_MAX_BATCH_URLS", "2")
            .start()
            .await;
        let (status, body) = rig
            .post(
                "api/v2/downloads",
                &json!({ "items": [
                    { "url": "https://fake.test/a" },
                    { "url": "https://fake.test/b" },
                    { "url": "https://fake.test/c" },
                ]}),
            )
            .await;
        assert_eq!(status, 413, "{body}");
        assert_eq!(body["error"]["code"], "payload_too_large");
    })
    .await;
}

#[tokio::test]
async fn a_mutating_route_requires_the_json_content_type() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let response = rig
            .http
            .post(rig.url("api/v2/downloads"))
            .header("content-type", "text/plain")
            .body(format!("{{\"url\":\"{YT}\"}}"))
            .send()
            .await
            .unwrap();
        let (status, body) = support::status_and_body(response).await;
        assert_eq!(status, 400, "a cross-origin form POST cannot add: {body}");
        assert_eq!(body["error"]["code"], "bad_request");
    })
    .await;
}

// ---------------------------------------------------------------------------
// POST api/v2/items/actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn actions_are_idempotent_and_answer_for_every_id() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (_, added) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": "https://fake.test/a", "auto_start": false }),
            )
            .await;
        let id = added["ids"][0].as_str().unwrap().to_owned();
        rig.until_status(&id, "queued").await;

        // `cancel` from `queued` is terminal, and cancelling again succeeds with the id in
        // `applied` — every action is idempotent (PROTOCOL §4.2).
        for _ in 0..2 {
            let (status, body) = rig
                .post(
                    "api/v2/items/actions",
                    &json!({ "action": "cancel", "ids": [id] }),
                )
                .await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["applied"], json!([id]));
            assert!(body["skipped"].as_array().unwrap().is_empty());
        }
        rig.until_status(&id, "canceled").await;

        // `retry` takes a cancelled item back to `queued` with `attempt` incremented.
        let (_, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "retry", "ids": [id] }),
            )
            .await;
        assert_eq!(body["applied"], json!([id]));
        let retried = rig
            .until("attempt 1", |item| item["attempt"] == 1, &id)
            .await;
        assert_eq!(retried["attempt"], 1);

        // An unknown token is `not_found`, never a 400.
        let (status, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "start", "ids": ["nope", "01JBQ7Z5T9K3M2R8V4XW6Y0AAA"] }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let skipped = body["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 2);
        for entry in skipped {
            assert_eq!(entry["reason"], "not_found");
        }
    })
    .await;
}

#[tokio::test]
async fn the_reachable_skip_reasons_are_all_produced() {
    for_each_prefix(|prefix| async move {
        // `not_pausable` needs an item parked in `resolving`, which is what the slow provider
        // gives; `not_retryable` needs a non-terminal one; `already_terminal` and `not_startable`
        // need a finished and a resolving item respectively.
        //
        // `not_cancelable` is deliberately absent: the engine's cancel is idempotent from every
        // state (DESIGN §8.7), so no request can produce it. It stays in the closed wire enum
        // because PROTOCOL §4.2 documents it.
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(slow_resolve()))
            .start()
            .await;
        let resolving = rig.add("https://fake.test/slow").await;
        rig.until_status(&resolving, "resolving").await;

        for (action, reason) in [("pause", "not_pausable"), ("start", "not_startable")] {
            let (_, body) = rig
                .post(
                    "api/v2/items/actions",
                    &json!({ "action": action, "ids": [resolving] }),
                )
                .await;
            assert_eq!(body["skipped"][0]["reason"], reason, "{action}: {body}");
        }

        let (_, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "retry", "ids": [resolving] }),
            )
            .await;
        assert_eq!(body["skipped"][0]["reason"], "not_retryable");

        let (_, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "cancel", "ids": [resolving] }),
            )
            .await;
        assert_eq!(body["applied"], json!([resolving]));
        rig.until_status(&resolving, "canceled").await;

        // PROTOCOL §4.2: `start` on an error/canceled item *is* a retry, so it is applied, not
        // skipped (review round 2, aulos-queue). Only `finished` stays `already_terminal`.
        let (_, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "start", "ids": [resolving] }),
            )
            .await;
        assert_eq!(
            body["applied"],
            json!([resolving]),
            "start on canceled is a retry: {body}"
        );

        let finished = rig.add("https://example.test/a").await; // ytdlp_like is the catch-all; fake.test would hit slow_resolve
        rig.until_status(&finished, "finished").await;
        let (_, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "start", "ids": [finished] }),
            )
            .await;
        assert_eq!(body["skipped"][0]["reason"], "already_terminal", "{body}");
    })
    .await;
}

#[tokio::test]
async fn pausing_a_running_item_parks_it_and_start_resumes_without_a_retry() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(hanging()))
            .start()
            .await;
        let id = rig.add("https://fake.test/hang").await;
        rig.until_status(&id, "downloading").await;

        let (status, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "pause", "ids": [id] }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let paused = rig
            .until("the paused row", |i| i["status"] == "queued", &id)
            .await;
        assert_eq!(paused["auto_start"], false, "paused, not a new status");
        assert_eq!(paused["attempt"], 0, "pause is not a retry");

        let (_, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "start", "ids": [id] }),
            )
            .await;
        assert_eq!(body["applied"], json!([id]));
        let resumed = rig
            .until("the resumed row", |i| i["auto_start"] == true, &id)
            .await;
        assert_eq!(resumed["attempt"], 0, "resuming still is not a retry");
    })
    .await;
}

#[tokio::test]
async fn the_single_item_delete_is_a_204_and_a_404() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/a").await;
        rig.until_status(&id, "finished").await;

        let response = rig
            .http
            .delete(rig.url(&format!("api/v2/items/{id}?delete_file=true")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 204);
        assert!(response.text().await.unwrap().is_empty(), "no body");

        let (status, body) = rig.delete(&format!("api/v2/items/{id}")).await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
    })
    .await;
}

#[tokio::test]
async fn an_unknown_action_is_a_validation_failure() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig
            .post(
                "api/v2/items/actions",
                &json!({ "action": "explode", "ids": [] }),
            )
            .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "action");
    })
    .await;
}

// ---------------------------------------------------------------------------
// POST api/v2/downloads/cancel-resolve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_resolve_scopes_by_generation_and_falls_back_to_everything() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(slow_resolve()))
            .start()
            .await;

        let (_, first) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": "https://fake.test/one" }),
            )
            .await;
        let (_, second) = rig
            .post(
                "api/v2/downloads",
                &json!({ "url": "https://fake.test/two" }),
            )
            .await;
        let gen_one = first["generation"].as_u64().unwrap();
        let id_one = first["ids"][0].as_str().unwrap().to_owned();
        let id_two = second["ids"][0].as_str().unwrap().to_owned();
        rig.until_status(&id_one, "resolving").await;
        rig.until_status(&id_two, "resolving").await;

        // An unknown generation cancels nothing and still answers 200.
        let (status, body) = rig
            .post(
                "api/v2/downloads/cancel-resolve",
                &json!({ "generation": 999_999 }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["canceled"], 0);
        assert_eq!(body["generation"], 999_999);

        // The generation from a `202` cancels that add's in-flight resolution and leaves a
        // concurrent add running (PLAN WP-14). The engine mints one generation per `Add` since
        // the wave-2 integration pass.
        assert_ne!(
            gen_one,
            second["generation"].as_u64().unwrap(),
            "one generation per add"
        );
        let (status, body) = rig
            .post(
                "api/v2/downloads/cancel-resolve",
                &json!({ "generation": gen_one }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["canceled"], 1, "{body}");
        rig.until_status(&id_one, "canceled").await;
        assert_eq!(
            rig.status_of(&id_two).await,
            "resolving",
            "the concurrent add is untouched"
        );

        // `{}` cancels everything still in flight — what the legacy `cancel-add` did.
        let (status, body) = rig
            .post("api/v2/downloads/cancel-resolve", &json!({}))
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body["generation"].is_null());
        rig.until_status(&id_two, "canceled").await;

        // A malformed body is a 400.
        let (status, body) = rig
            .post(
                "api/v2/downloads/cancel-resolve",
                &json!({ "generation": "soon" }),
            )
            .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "generation");
    })
    .await;
}

// ---------------------------------------------------------------------------
// GET api/v2/state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_snapshot_carries_every_documented_block() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig.get("api/v2/state").await;
        assert_eq!(status, 200);
        for key in [
            "mode",
            "seq",
            "boot_id",
            "server_time",
            "protocol",
            "counts",
            "done_total",
            "truncated",
            "items",
            "done",
            "subscriptions",
            "ytdl_options",
            "health",
        ] {
            assert!(body.get(key).is_some(), "{key} must be present: {body}");
        }
        assert_eq!(body["mode"], "snapshot");
        assert_eq!(
            body["protocol"]["delta_semantics"],
            "absent-key-means-unchanged"
        );
        assert_eq!(body["protocol"]["batch_ms"], 50);
        assert_eq!(body["truncated"]["groups"], json!([]));
        assert_eq!(body["ytdl_options"]["ok"], true);
        assert_eq!(body["health"]["status"], "ok");
        for status_name in [
            "queued",
            "resolving",
            "preparing",
            "downloading",
            "postprocessing",
            "finished",
            "error",
            "canceled",
        ] {
            assert!(body["counts"].get(status_name).is_some(), "{status_name}");
        }
    })
    .await;
}

#[tokio::test]
async fn done_false_omits_the_completed_window() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/a").await;
        rig.until_status(&id, "finished").await;
        rig.settle().await;

        let (_, with) = rig.get("api/v2/state").await;
        assert_eq!(with["done"].as_array().unwrap().len(), 1);
        let (_, without) = rig.get("api/v2/state?done=false").await;
        assert!(without["done"].as_array().unwrap().is_empty());
        assert_eq!(
            without["done_total"], with["done_total"],
            "the count stays honest"
        );
    })
    .await;
}

#[tokio::test]
async fn the_state_etag_makes_a_refresh_free() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let response = rig.get_raw("api/v2/state").await;
        let etag = response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(etag.starts_with("W/\""), "{etag}");
        assert_eq!(etag_seq(&etag), body_of(response).await["seq"].as_u64());

        let response = rig
            .http
            .get(rig.url("api/v2/state"))
            .header("if-none-match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 304);
        assert!(response.text().await.unwrap().is_empty(), "an empty body");
    })
    .await;
}

/// PROTOCOL §4.3 defines the ETag as `W/"<boot_id>-<seq>"` where `seq` is the response's **own**
/// cursor. During a flush the hub's head runs ahead of the published generation the body is built
/// from (the aggregator republishes last), and an ETag taken from the head would name a cursor the
/// body does not carry — so the next poll would `304` forever against a snapshot that never
/// contained the finished item.
///
/// The window is reproduced exactly by publishing a frame onto the hub without republishing.
#[tokio::test]
async fn the_state_etag_never_names_a_cursor_the_body_does_not_carry() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/etag").await;
        rig.until_status(&id, "finished").await;
        rig.settle().await;

        let (_, before) = rig.get("api/v2/state").await;
        let published = before["seq"].as_u64().unwrap();

        // Head moves; the published generation does not. This is the shape of a flush's interior.
        rig.hub.publish(
            aulos_queue::FrameKind::Notice,
            json!({ "level": "info", "code": "plugin_note", "id": null, "message": "gap" }),
        );
        assert!(rig.hub.head().0 > published, "the window is open");

        let response = rig.get_raw("api/v2/state").await;
        let etag = response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = body_of(response).await;
        assert_eq!(
            etag_seq(&etag),
            body["seq"].as_u64(),
            "the ETag names the body's cursor, not the hub head: {etag} vs {body}"
        );

        // And the conditional refresh that follows must not hide the generation that lands next.
        let (_, item) = rig.get(&format!("api/v2/items/{id}")).await;
        assert_eq!(item["status"], "finished");
    })
    .await;
}

/// The `seq` inside a `W/"<boot_id>-<seq>"` ETag.
fn etag_seq(etag: &str) -> Option<u64> {
    etag.trim_start_matches("W/")
        .trim_matches('"')
        .rsplit_once('-')
        .and_then(|(_, seq)| seq.parse().ok())
}

/// The JSON body of a response a test already holds for its headers.
async fn body_of(response: reqwest::Response) -> Value {
    let text = response.text().await.unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn since_answers_up_to_date_a_delta_or_a_snapshot() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/a").await;
        rig.until_status(&id, "finished").await;
        rig.settle().await;

        let (_, snapshot) = rig.get("api/v2/state").await;
        let seq = snapshot["seq"].as_u64().unwrap();
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();

        // `since == seq` is `up_to_date`, with no arrays at all.
        let (_, body) = rig
            .get(&format!("api/v2/state?since={seq}&boot={boot}"))
            .await;
        assert_eq!(body["mode"], "up_to_date", "{body}");
        assert!(body.get("items").is_none());

        // A window with changes in it is a delta.
        let second = rig.add("https://fake.test/b").await;
        rig.until_status(&second, "finished").await;
        rig.settle().await;
        let (_, body) = rig
            .get(&format!("api/v2/state?since={seq}&boot={boot}"))
            .await;
        assert_eq!(body["mode"], "delta", "{body}");
        assert_eq!(body["from"], seq);
        assert!(body["seq"].as_u64().unwrap() > seq);
        assert!(body["removed"].is_array(), "an array, never null");
        assert!(
            body["added"]
                .as_array()
                .unwrap()
                .iter()
                .chain(body["completed"].as_array().unwrap())
                .any(|i| i["id"] == second.as_str()),
            "the new item is in the window: {body}"
        );

        // A boot mismatch discards.
        let (_, body) = rig
            .get(&format!(
                "api/v2/state?since={seq}&boot=01JBQ8YQ2E0000000000000000"
            ))
            .await;
        assert_eq!(body["mode"], "snapshot");

        // A cursor above the head is a snapshot, not "you are up to date".
        let (_, body) = rig
            .get(&format!("api/v2/state?since={}&boot={boot}", seq + 10_000))
            .await;
        assert_eq!(body["mode"], "snapshot");
    })
    .await;
}

#[tokio::test]
async fn counts_are_the_done_window_and_the_docs_say_so() {
    // PROTOCOL §4.3/§5.3: `counts` is a histogram over the snapshot's own `items` + `done`, and
    // `done` is the most recent `AULOS_MEM_DONE_ITEMS` terminal records. So the terminal counters
    // are bounded by that window while `done_total` and `GET api/v2/items` are not. This pins the
    // documented shape, because the number is cheap and the honest total lives next to it.
    let rig = Rig::builder("/")
        .env("AULOS_MEM_DONE_ITEMS", "2")
        .start()
        .await;
    for n in 0..4 {
        let id = rig.add(&format!("https://fake.test/done{n}")).await;
        rig.until_status(&id, "finished").await;
    }
    rig.settle().await;

    let (_, state) = rig.get("api/v2/state").await;
    assert_eq!(state["done"].as_array().unwrap().len(), 2, "{state}");
    assert_eq!(state["counts"]["finished"], 2, "the window, not the store");
    assert_eq!(state["done_total"], 4, "the honest total sits next to it");
    assert_eq!(state["truncated"]["done"], true);

    // The exact per-status number is a store query, and it disagrees with `counts` on purpose.
    let (_, page) = rig.get("api/v2/items?status=finished").await;
    assert_eq!(page["total"], 4, "{page}");

    // `healthz.items` is the same object, so it is windowed the same way (§4.7).
    let (_, health) = rig.get("healthz").await;
    assert_eq!(health["items"]["finished"], 2, "{health}");
}

#[tokio::test]
async fn state_discards_a_cursor_whose_boot_it_cannot_confirm() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let first = rig.add("https://fake.test/one").await;
        rig.until_status(&first, "finished").await;
        rig.settle().await;
        let (_, snapshot) = rig.get("api/v2/state").await;
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();
        let seq = snapshot["seq"].as_u64().unwrap();
        let second = rig.add("https://fake.test/two").await;
        rig.until_status(&second, "finished").await;
        rig.settle().await;

        // PROTOCOL §6.2: a boot the server cannot confirm forces a snapshot. A `boot` that is not
        // a ULID used to parse to `None` and be treated as "not sent", which answered a delta.
        for query in [
            format!("api/v2/state?since={seq}&boot=NOT-A-ULID"),
            format!("api/v2/state?since={seq}&boot="),
            format!("api/v2/state?since={seq}"),
        ] {
            let (status, body) = rig.get(&query).await;
            assert_eq!(status, 200, "{query}: {body}");
            assert_eq!(body["mode"], "snapshot", "{query} answered {body}");
        }

        // The matching boot still folds a delta, so it is the boot that decides.
        let (_, body) = rig
            .get(&format!("api/v2/state?since={seq}&boot={boot}"))
            .await;
        assert_eq!(body["mode"], "delta", "{body}");
    })
    .await;
}

#[tokio::test]
async fn a_cursor_older_than_the_replay_window_is_a_snapshot() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_WS_REPLAY_FRAMES", "1")
            .start()
            .await;
        for n in 0..4 {
            let id = rig.add(&format!("https://fake.test/{n}")).await;
            rig.until_status(&id, "finished").await;
        }
        rig.settle().await;
        let (_, snapshot) = rig.get("api/v2/state").await;
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();
        let (_, body) = rig.get(&format!("api/v2/state?since=1&boot={boot}")).await;
        assert_eq!(body["mode"], "snapshot", "{body}");
    })
    .await;
}

#[tokio::test]
async fn removed_is_an_array_of_reason_groups() {
    for_each_prefix(|prefix| async move {
        // Two reasons in one window: a user delete (`deleted`) and a clear (`cleared`).
        let rig = Rig::start(prefix).await;
        let keep = rig.add("https://fake.test/keep").await;
        let drop_me = rig.add("https://fake.test/drop").await;
        rig.until_status(&keep, "finished").await;
        rig.until_status(&drop_me, "finished").await;
        rig.settle().await;

        let (_, snapshot) = rig.get("api/v2/state").await;
        let seq = snapshot["seq"].as_u64().unwrap();
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();

        rig.post(
            "api/v2/items/actions",
            &json!({ "action": "delete", "ids": [drop_me] }),
        )
        .await;
        rig.post("api/v2/items/clear", &json!({})).await;
        rig.settle().await;

        let (_, body) = rig
            .get(&format!("api/v2/state?since={seq}&boot={boot}"))
            .await;
        let removed = body["removed"].as_array().unwrap();
        assert!(!removed.is_empty(), "{body}");
        let reasons: Vec<&str> = removed
            .iter()
            .map(|group| group["reason"].as_str().unwrap())
            .collect();
        assert!(reasons.contains(&"deleted"), "{reasons:?}");
        assert!(reasons.contains(&"cleared"), "{reasons:?}");
        for group in removed {
            assert!(
                group["ids"].is_array(),
                "one homogeneous ids array per reason"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn an_empty_window_still_reports_removed_as_an_array() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/a").await;
        rig.until_status(&id, "finished").await;
        rig.settle().await;
        let (_, snapshot) = rig.get("api/v2/state").await;
        let seq = snapshot["seq"].as_u64().unwrap() - 1;
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();
        let (_, body) = rig
            .get(&format!("api/v2/state?since={seq}&boot={boot}"))
            .await;
        if body["mode"] == "delta" {
            assert_eq!(body["removed"], json!([]), "[] and never null");
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// GET api/v2/items
// ---------------------------------------------------------------------------

#[tokio::test]
async fn items_pages_by_cursor_and_filters_by_status() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        for n in 0..3 {
            let id = rig.add(&format!("https://fake.test/{n}")).await;
            rig.until_status(&id, "finished").await;
        }
        rig.settle().await;

        let (status, body) = rig.get("api/v2/items?limit=2").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["total"], 3);
        let cursor = body["next_cursor"].as_str().unwrap().to_owned();

        let (_, page) = rig
            .get(&format!("api/v2/items?limit=2&cursor={cursor}"))
            .await;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert!(page["next_cursor"].is_null(), "the last page");

        let (_, filtered) = rig.get("api/v2/items?status=finished").await;
        assert_eq!(filtered["items"].as_array().unwrap().len(), 3);
        let (_, none) = rig.get("api/v2/items?status=queued").await;
        assert!(none["items"].as_array().unwrap().is_empty());

        let (status, body) = rig.get("api/v2/items?status=nonsense").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "status");
        let (status, body) = rig.get("api/v2/items?order=title").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "order");
        let (status, body) = rig.get("api/v2/items?cursor=forged").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "cursor");
    })
    .await;
}

/// `?q=` is part of the query, not a filter over the page, so `total` counts the matching set and
/// a page of it is full (the WP-14 request in `docs/INTEGRATION-NOTES.md`).
#[tokio::test]
async fn q_filters_the_query_so_total_and_the_cursor_describe_the_matching_set() {
    let rig = Rig::start("/").await;
    // The `fake` provider titles an item after its URL until it resolves, and resolution names it
    // after the media id, so the URL path is what `q` has to match on.
    for n in 0..3 {
        let id = rig.add(&format!("https://fake.test/lofi-{n}")).await;
        rig.until_status(&id, "finished").await;
    }
    for n in 0..2 {
        let id = rig.add(&format!("https://fake.test/other-{n}")).await;
        rig.until_status(&id, "finished").await;
    }
    rig.settle().await;

    let (status, all) = rig.get("api/v2/items").await;
    assert_eq!(status, 200, "{all}");
    assert_eq!(all["total"], 5);

    let (status, hits) = rig.get("api/v2/items?q=lofi").await;
    assert_eq!(status, 200, "{hits}");
    assert_eq!(hits["total"], 3, "the count is of the matching set: {hits}");
    assert_eq!(hits["items"].as_array().unwrap().len(), 3);

    // A page of two is full, not "two rows minus the ones that did not match".
    let (_, first) = rig.get("api/v2/items?q=lofi&limit=2").await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2, "{first}");
    assert_eq!(first["total"], 3);
    let cursor = first["next_cursor"].as_str().expect("a cursor").to_owned();
    let (_, second) = rig
        .get(&format!("api/v2/items?q=lofi&limit=2&cursor={cursor}"))
        .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1, "{second}");
    assert!(second["next_cursor"].is_null());

    // Case-insensitive, composes with `status`, and an empty `q` is no filter.
    let (_, upper) = rig.get("api/v2/items?q=LOFI").await;
    assert_eq!(upper["total"], 3);
    let (_, both) = rig.get("api/v2/items?q=lofi&status=queued").await;
    assert_eq!(both["total"], 0, "{both}");
    let (_, empty) = rig.get("api/v2/items?q=").await;
    assert_eq!(empty["total"], 5, "an empty q is not a filter");
    let (_, miss) = rig.get("api/v2/items?q=nothing-matches-this").await;
    assert_eq!(miss["total"], 0);
    assert_eq!(miss["items"], json!([]));
}

#[tokio::test]
async fn a_group_and_its_children_are_in_the_same_array() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(support::expanding(3)))
            .start()
            .await;
        let id = rig.add("https://fake.test/playlist/one").await;
        let group = rig
            .until("the group promotion", |i| i["kind"] == "group", &id)
            .await;
        assert_eq!(group["id"], id.as_str(), "the id survives the promotion");
        assert_eq!(group["children_total"], 3);
        assert_eq!(
            group["children_inline"], true,
            "the snapshot is never truncated"
        );

        let (_, children) = rig.get(&format!("api/v2/items?group_id={id}")).await;
        assert_eq!(children["items"].as_array().unwrap().len(), 3);
        for child in children["items"].as_array().unwrap() {
            assert_eq!(child["group_id"], id.as_str());
            assert!(child["group_index"].as_u64().is_some());
        }
    })
    .await;
}

#[tokio::test]
async fn one_item_and_its_file_redirect() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/a").await;
        let item = rig.until_status(&id, "finished").await;
        assert_eq!(item["percent"], 100.0);
        let download_url = item["download_url"].as_str().unwrap();
        assert!(download_url.starts_with("download/"), "{download_url}");
        assert_eq!(item["filename"].as_str().unwrap(), "a.mp4");

        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = no_redirect
            .get(rig.url(&format!("api/v2/items/{id}/file")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 302);
        let location = response
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(location, format!("{}download/a.mp4", rig.prefix));

        let (status, body) = rig.get("api/v2/items/01JBQ7Z5T9K3M2R8V4XW6Y0AAA").await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
    })
    .await;
}

/// DESIGN §16.6 (SSRF row) and §17.3: "the v1/v2 API adds run the same validator with
/// `allow_private = AULOS_ALLOW_PRIVATE_TARGETS`". The knob defaults to `true`, so the LAN adds a
/// home deployment relies on keep working; `false` is the operator locking the deployment down,
/// and it must actually reach both add paths.
#[tokio::test]
async fn allow_private_targets_gates_the_ssrf_guard_on_both_add_paths() {
    for_each_prefix(|prefix| async move {
        const METADATA: &str = "http://169.254.169.254/latest/meta-data/iam/security-credentials/";

        // The default posture: private targets are reachable, exactly as before.
        let open = Rig::start(prefix).await;
        let (status, body) = open
            .post("api/v2/downloads", &json!({ "url": METADATA }))
            .await;
        assert_eq!(status, 202, "the default is permissive: {body}");

        let locked = Rig::builder(prefix)
            .env("AULOS_ALLOW_PRIVATE_TARGETS", "false")
            .start()
            .await;
        for url in [
            METADATA,
            "http://127.0.0.1:8081/x",
            "http://10.0.0.5/x",
            "http://localhost/x",
        ] {
            let (status, body) = locked
                .post("api/v2/downloads", &json!({ "url": url }))
                .await;
            assert_eq!(status, 400, "{url} was accepted: {body}");
            assert_eq!(body["error"]["code"], "validation_failed", "{url}");
            assert_eq!(body["error"]["field"], "url", "{url}");
        }

        // The v1 shim is a surface too.
        let response = locked
            .http
            .post(locked.url("add"))
            .json(&json!({ "url": METADATA, "quality": "best", "format": "any" }))
            .send()
            .await
            .unwrap();
        let (status, body) = support::status_and_body(response).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["code"], "validation_failed", "{body}");

        // A public URL is untouched by the guard.
        let (status, body) = locked.post("api/v2/downloads", &json!({ "url": YT })).await;
        assert_eq!(status, 202, "{body}");
    })
    .await;
}
