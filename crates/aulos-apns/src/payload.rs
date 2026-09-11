//! The wire payloads, as pure functions of an [`ItemView`] (DESIGN §25.4).
//!
//! Everything in this module is total, synchronous and side-effect free, which is deliberate: the
//! payload shapes are a contract with an iOS app being written in parallel, so they are the part
//! that has to be pinned by tests that compare whole `serde_json::Value`s rather than by
//! inspection of the code that sends them.
//!
//! # The content state
//!
//! [`content_state`] emits **exactly seven keys, always, in camelCase**. A Live Activity's
//! `ContentState` is a Swift `Codable` struct: a missing key is a decode failure and a silently
//! frozen activity on the device, so "omit when null" is not an option here even though it is the
//! house style everywhere else in this workspace.

use aulos_core::item::{ItemView, Kind};
use aulos_core::status::Status;
use serde_json::{Value, json};

/// The `attributes-type` the iOS app registers its `ActivityAttributes` under.
pub const ATTRIBUTES_TYPE: &str = "AulosDownloadAttributes";

/// The `thread-id` every alert carries, so iOS groups them into one stack.
pub const THREAD_ID: &str = "aulos";

/// The alert title for a successful download.
pub const TITLE_FINISHED: &str = "Download finished";

/// The alert title for a failed one.
pub const TITLE_FAILED: &str = "Download failed";

/// The Live Activity start alert's title.
pub const TITLE_DOWNLOADING: &str = "Downloading";

/// How long after an `end` push iOS dismisses the Live Activity, in seconds.
pub const DISMISSAL_AFTER_SECS: i64 = 900;

/// How long an alert stays worth delivering, in seconds.
pub const ALERT_TTL_SECS: i64 = 3600;

/// How long after an update the widget should start calling its numbers stale, in seconds.
///
/// The server pushes a progress update every [`crate::notifier::PROGRESS_INTERVAL`] while an item
/// is moving, so 45 s is nine missed pushes: long enough that a throttled or reordered delivery
/// does not flicker, short enough that a VPN blip or a killed download stops the island from
/// showing a confident number nobody is maintaining any more (DESIGN §25.4).
pub const STALE_AFTER_SECS: i64 = 45;

/// The seven-key `content-state` (DESIGN §25.4).
///
/// `totalBytes` prefers the exact total and falls back to the estimate — `ItemView::total_bytes`
/// is null for most HLS and fragmented downloads, and an activity whose progress ring has no
/// denominator is the case this fallback exists for (`ItemView::total_bytes_estimate`'s own doc
/// gives clients exactly this rule).
#[must_use]
pub fn content_state(view: &ItemView) -> Value {
    json!({
        "status": view.status.as_str(),
        "percent": view.percent,
        "speed": view.speed,
        "eta": view.eta,
        "downloadedBytes": view.downloaded_bytes,
        "totalBytes": view.total_bytes.or(view.total_bytes_estimate),
        "message": view.msg.as_deref(),
    })
}

/// The alert title for a terminal item: [`TITLE_FINISHED`] for `finished`, [`TITLE_FAILED`]
/// otherwise.
#[must_use]
pub const fn alert_title(status: Status) -> &'static str {
    match status {
        Status::Finished => TITLE_FINISHED,
        _ => TITLE_FAILED,
    }
}

/// The alert body: the item title, or `"<title> — N of M done"` for a group.
///
/// `N` is `children_done` and `M` is `children_total`; both default to `0` on a group whose
/// counters the engine has not filled in, which reads as `"… — 0 of 0 done"` rather than
/// disappearing.
#[must_use]
pub fn alert_body(view: &ItemView) -> String {
    if view.kind == Kind::Group {
        let done = view.children_done.unwrap_or(0);
        let total = view.children_total.unwrap_or(0);
        format!("{} — {done} of {total} done", view.title)
    } else {
        view.title.to_string()
    }
}

/// The complete alert push payload for a terminal item (DESIGN §25.4).
#[must_use]
pub fn alert(view: &ItemView) -> Value {
    json!({
        "aps": {
            "alert": {
                "title": alert_title(view.status),
                "body": alert_body(view),
            },
            "sound": "default",
            "thread-id": THREAD_ID,
            "interruption-level": "active",
        },
        "item_id": view.id.to_string(),
        "status": view.status.as_str(),
        "url": view.url.as_ref(),
        "download_url": view.download_url.as_deref(),
    })
}

/// The Live Activity **push-to-start** payload (`event: "start"`).
///
/// `timestamp` is unix seconds; iOS uses it to discard a start that lost a race with a later one.
#[must_use]
pub fn live_activity_start(view: &ItemView, now_secs: i64) -> Value {
    json!({
        "aps": {
            "timestamp": now_secs,
            "event": "start",
            "content-state": content_state(view),
            "attributes-type": ATTRIBUTES_TYPE,
            "attributes": {
                "itemId": view.id.to_string(),
                "url": view.url.as_ref(),
                "title": view.title.as_ref(),
            },
            "alert": {
                "title": TITLE_DOWNLOADING,
                "body": view.title.as_ref(),
            },
        }
    })
}

/// The Live Activity **update** payload (`event: "update"`).
///
/// Two keys beyond the state, both Apple's own and both only on an update:
///
/// - `stale-date` ([`STALE_AFTER_SECS`] from now, unix seconds) is what makes the widget's
///   `isStale` true on the device, so a stream that stopped renders as "waiting for the server"
///   rather than as a percentage that has silently frozen;
/// - `relevance-score` is the percent as a `0.0..=1.0` fraction, which is how iOS orders several
///   live activities on the lock screen — the download closest to finishing sorts first.
#[must_use]
pub fn live_activity_update(view: &ItemView, now_secs: i64) -> Value {
    json!({
        "aps": {
            "timestamp": now_secs,
            "event": "update",
            "content-state": content_state(view),
            "stale-date": now_secs + STALE_AFTER_SECS,
            "relevance-score": relevance_score(view),
        }
    })
}

/// The `0.0..=1.0` fraction iOS sorts concurrent live activities by.
#[must_use]
pub fn relevance_score(view: &ItemView) -> f64 {
    view.percent.clamp(0.0, 100.0) / 100.0
}

/// The Live Activity **end** payload (`event: "end"`), dismissed
/// [`DISMISSAL_AFTER_SECS`] later.
#[must_use]
pub fn live_activity_end(view: &ItemView, now_secs: i64) -> Value {
    json!({
        "aps": {
            "timestamp": now_secs,
            "event": "end",
            "content-state": content_state(view),
            "dismissal-date": now_secs + DISMISSAL_AFTER_SECS,
        }
    })
}
