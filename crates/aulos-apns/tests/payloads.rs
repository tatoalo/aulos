//! The payload contract (DESIGN §25.4).
//!
//! These are the tests the iOS app is really being written against: every one of them compares a
//! whole `serde_json::Value`, so a key that changes name, moves, appears or disappears fails here
//! rather than on a phone.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use aulos_apns::payload;
use aulos_core::status::Status;
use common::{ItemBuilder, epoch_secs};
use serde_json::json;

#[test]
fn the_content_state_is_the_seven_camel_case_keys() {
    let item = ItemBuilder::new("Big Buck Bunny")
        .progress(42.5, Some(2_100_000.0), Some(68), Some(123), Some(456))
        .msg("Merging formats");
    let state = payload::content_state(&item.view());

    assert_eq!(
        state,
        json!({
            "status": "downloading",
            "percent": 42.5,
            "speed": 2_100_000.0,
            "eta": 68,
            "downloadedBytes": 123,
            "totalBytes": 456,
            "message": null,
        })
    );
}

/// A running item's `msg` is yt-dlp's postprocessor line; it never reaches the island. A failure's
/// `error.message` is the one line that does.
#[test]
fn the_message_is_a_failure_reason_and_never_the_postprocessor_line() {
    let running = ItemBuilder::new("x")
        .status(Status::Postprocessing)
        .msg("MoveFiles…");
    assert_eq!(
        payload::content_state(&running.view())["message"],
        json!(null)
    );

    let failed = ItemBuilder::new("x")
        .failed("Connection reset")
        .msg("MoveFiles…");
    assert_eq!(
        payload::content_state(&failed.view())["message"],
        json!("Connection reset")
    );
}

#[test]
fn all_seven_keys_are_present_even_when_nothing_is_known() {
    // A Live Activity's ContentState is a Swift `Codable`: a missing key is a decode failure and a
    // frozen activity, so "omit when null" is not available here.
    let state = payload::content_state(&ItemBuilder::new("x").status(Status::Preparing).view());
    assert_eq!(
        state,
        json!({
            "status": "preparing",
            "percent": 0.0,
            "speed": null,
            "eta": null,
            "downloadedBytes": null,
            "totalBytes": null,
            "message": null,
        })
    );
    let keys: Vec<&String> = state.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 7, "exactly seven keys: {keys:?}");
}

#[test]
fn total_bytes_falls_back_to_the_estimate() {
    // `ItemView::total_bytes` is null for most HLS and fragmented downloads, and its own doc tells
    // clients to use the estimate then. The notifier IS the client here.
    let item = ItemBuilder::new("x")
        .progress(10.0, None, None, Some(100), None)
        .total_estimate(999);
    assert_eq!(
        payload::content_state(&item.view())["totalBytes"],
        json!(999)
    );
}

#[test]
fn a_finished_item_alerts_with_its_title_and_the_terminal_fields() {
    let item = ItemBuilder::new("Big Buck Bunny")
        .status(Status::Finished)
        .download_url("download/Big%20Buck%20Bunny.mp4");
    let view = item.view();

    assert_eq!(
        payload::alert(&view),
        json!({
            "aps": {
                "alert": { "title": "Download finished", "body": "Big Buck Bunny" },
                "sound": "default",
                "thread-id": "aulos",
                "interruption-level": "active",
            },
            "item_id": view.id.to_string(),
            "status": "finished",
            "url": "https://videos.test/watch/9",
            "download_url": "download/Big%20Buck%20Bunny.mp4",
        })
    );
}

#[test]
fn a_failed_item_alerts_with_the_failure_title_and_a_null_download_url() {
    let view = ItemBuilder::new("Broken").status(Status::Error).view();
    let alert = payload::alert(&view);
    assert_eq!(alert["aps"]["alert"]["title"], json!("Download failed"));
    assert_eq!(alert["status"], json!("error"));
    assert_eq!(alert["download_url"], json!(null));
}

#[test]
fn a_group_alert_body_counts_its_children() {
    let view = ItemBuilder::new("Season 1")
        .status(Status::Finished)
        .group(9, 12)
        .view();
    assert_eq!(
        payload::alert(&view)["aps"]["alert"]["body"],
        json!("Season 1 — 9 of 12 done")
    );
}

#[test]
fn the_start_payload_carries_the_attributes_the_app_registers() {
    let item = ItemBuilder::new("Big Buck Bunny").status(Status::Preparing);
    let view = item.view();
    let now = epoch_secs();

    assert_eq!(
        payload::live_activity_start(&view, now),
        json!({
            "aps": {
                "timestamp": now,
                "event": "start",
                "content-state": payload::content_state(&view),
                "attributes-type": "AulosDownloadAttributes",
                "attributes": {
                    "itemId": view.id.to_string(),
                    "url": "https://videos.test/watch/9",
                    "title": "Big Buck Bunny",
                },
                "alert": { "title": "Downloading", "body": "Big Buck Bunny" },
            }
        })
    );
}

#[test]
fn the_update_payload_carries_the_state_a_stale_date_and_a_relevance_score() {
    let view = ItemBuilder::new("x")
        .progress(50.0, Some(1.0), Some(2), Some(3), Some(4))
        .view();
    let now = epoch_secs();
    let update = payload::live_activity_update(&view, now);

    assert_eq!(
        update,
        json!({
            "aps": {
                "timestamp": now,
                "event": "update",
                "content-state": payload::content_state(&view),
                // Unix seconds, as Apple documents `stale-date` — not milliseconds, and not a
                // duration: the widget compares it against `Date.now`.
                "stale-date": now + 45,
                "relevance-score": 0.5,
            }
        })
    );
    let keys: Vec<&String> = update["aps"].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 5, "nothing else may ride along: {keys:?}");
}

#[test]
fn the_relevance_score_is_the_percent_as_a_fraction_and_never_leaves_the_unit_range() {
    let at = |percent: f64| {
        payload::relevance_score(
            &ItemBuilder::new("x")
                .progress(percent, None, None, None, None)
                .view(),
        )
    };
    assert!((at(0.0) - 0.0).abs() < f64::EPSILON);
    assert!((at(37.5) - 0.375).abs() < f64::EPSILON);
    assert!((at(100.0) - 1.0).abs() < f64::EPSILON);
    // A provider that over-reports must not hand iOS a score it rejects.
    assert!((at(140.0) - 1.0).abs() < f64::EPSILON);
    assert!((at(-5.0) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn neither_the_start_nor_the_end_carries_a_stale_date() {
    // A start is immediately followed by updates, and an end is the last word on the activity:
    // marking either stale would put "waiting for the server" under a ring that is done.
    let view = ItemBuilder::new("x").view();
    for payload in [
        payload::live_activity_start(&view, epoch_secs()),
        payload::live_activity_end(&view, epoch_secs()),
    ] {
        assert_eq!(payload["aps"]["stale-date"], json!(null));
        assert_eq!(payload["aps"]["relevance-score"], json!(null));
    }
}

#[test]
fn the_end_payload_dismisses_the_activity_fifteen_minutes_later() {
    let view = ItemBuilder::new("x").status(Status::Finished).view();
    let now = epoch_secs();
    let end = payload::live_activity_end(&view, now);

    assert_eq!(end["aps"]["event"], json!("end"));
    assert_eq!(end["aps"]["timestamp"], json!(now));
    assert_eq!(end["aps"]["dismissal-date"], json!(now + 900));
    // A finished item reads 100 %, which is what the ring should freeze at.
    assert_eq!(end["aps"]["content-state"]["percent"], json!(100.0));
    assert_eq!(end["aps"]["content-state"]["status"], json!("finished"));
}

#[test]
fn a_cancelled_item_still_ends_its_activity_with_the_v2_status_word() {
    let view = ItemBuilder::new("x").status(Status::Canceled).view();
    let end = payload::live_activity_end(&view, epoch_secs());
    assert_eq!(end["aps"]["content-state"]["status"], json!("canceled"));
}
