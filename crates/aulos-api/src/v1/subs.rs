//! The five legacy subscription routes (DESIGN §11.1, §11.7, PROTOCOL §10.1).
//!
//! # The one shape that matters
//!
//! Legacy answered **HTTP 200 with a `status: "error"` body** for every *business* failure here —
//! a duplicate URL, a single-video URL, an unknown id — and a real `400` only for the four
//! request-shape failures (`check_interval_minutes` twice, `missing subscription id`,
//! `no valid fields to update`, `missing ids list`, `ids must be a list`). The shim keeps both
//! halves exactly, because a client that switches on the HTTP status would otherwise start
//! treating "already subscribed" as a hard error.
//!
//! The one promotion is the documented Δ C25: a bad `enabled` was a **leaked 500** with
//! `enabled must be a boolean` never leaving the log, and is now a `400` carrying that string.
//!
//! # The 13-key projection
//!
//! `SubscriptionView` is v2's 16 keys. v1 gets exactly the legacy 13 — no `next_due`, no
//! `consecutive_failures`, no `checking` — and `last_checked` as **float seconds**
//! (`time.time()`), where v2 uses integer milliseconds (PROTOCOL §10.5).

use aulos_core::{SubChanges, SubCmd, SubError, SubId, SubscriptionView};
use axum::extract::State;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::{legacy, request, status_error, status_ok};
use crate::ApiState;
use crate::error::{ApiError, Json};
use crate::v2::parse_bool;

/// The legacy `to_public_dict()` — exactly 13 keys, in legacy order.
///
/// `last_checked` is `time.time()`, i.e. float **seconds**; the record stores unix ms.
#[must_use]
pub fn project(view: &SubscriptionView) -> Value {
    json!({
        "id": view.id,
        "name": view.name,
        "url": view.url,
        "enabled": view.enabled,
        "check_interval_minutes": view.check_interval_minutes,
        "download_type": view.download_type.as_str(),
        "codec": view.codec.as_str(),
        "format": view.format,
        "quality": view.quality,
        "folder": view.folder,
        "last_checked": view.last_checked.map(|ms| ms as f64 / 1000.0),
        "seen_count": view.seen_count,
        "error": view.error,
    })
}

/// `POST <p>subscribe`.
///
/// Validation runs in the legacy order: `parse_download_options` first (so every DESIGN §11.7 add
/// string keeps its position), then `check_interval_minutes`, then the manager.
///
/// # Errors
/// A `400` for any request-shape failure; `503` when the manager is unreachable. A business
/// failure is a `200` with a `status: "error"` body.
pub async fn subscribe(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    let presets = super::known_presets(&state);
    let parsed = request::parse_download_options(&state.cfg, &presets, root.clone())?;
    let interval = request::parse_check_interval(&state.cfg, &root)?;

    // Legacy passed the whole download template plus the interval to `add_subscription`, and so
    // does `SubCmd::Add` — `custom_name_prefix`, `auto_start`, `split_by_chapters`, the two
    // subtitle fields and both `ytdl_options_*` reach the record rather than being dropped.
    let request = Box::new(parsed);
    let view = match send(&state, move |ack| SubCmd::Add {
        request,
        check_interval_minutes: Some(interval),
        ack,
    })
    .await
    {
        Ok(view) => *view,
        Err(Business(message)) => return Ok(Json(status_error(&message))),
        Err(Fatal(err)) => return Err(err),
    };

    Ok(Json(
        json!({ "status": "ok", "subscription": project(&view) }),
    ))
}

/// `GET <p>subscriptions` — a bare array of 13-key objects.
///
/// # Errors
/// `503` when the manager is unreachable.
pub async fn list(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let rows = match send(&state, |ack| SubCmd::List { ack }).await {
        Ok(rows) => rows,
        Err(Business(_)) => Vec::new(),
        Err(Fatal(err)) => return Err(err),
    };
    Ok(Json(Value::Array(rows.iter().map(project).collect())))
}

/// `POST <p>subscriptions/update` — only the legacy three fields.
///
/// The legacy filter is reproduced literally: `changes` is built from the keys present in the
/// body, **including** ones whose value is `null`, because that is what
/// `{k: v for k, v in post.items() if k in (...)}` produced. So `{"id": …, "name": null}` is *not*
/// `no valid fields to update` — it is an accepted no-op, which is what
/// `subscriptions_update_blank_name_is_ignored` records.
///
/// # Errors
/// `400` for a missing id, an empty change set, a bad `enabled` (Δ C25) or a bad interval; `503`
/// when the manager is unreachable.
pub async fn update(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    let raw_id = root.get("id").cloned().unwrap_or(Value::Null);
    if !request::truthy(&raw_id) {
        return Err(ApiError::bad_request(legacy::MISSING_SUBSCRIPTION_ID));
    }
    let id_text = request::py_str(&raw_id);

    if !has_updatable(&root) {
        return Err(ApiError::bad_request(legacy::NO_VALID_FIELDS));
    }

    let mut changes = SubChanges::default();
    if let Some(value) = root.get("enabled").filter(|v| !v.is_null()) {
        changes.enabled = Some(
            parse_bool("enabled", value)
                .map_err(|_| ApiError::invalid("enabled", legacy::ENABLED_NOT_BOOL))?,
        );
    }
    if let Some(value) = root.get("check_interval_minutes").filter(|v| !v.is_null()) {
        let minutes = request::py_int(value).ok_or_else(|| {
            ApiError::invalid("check_interval_minutes", legacy::CHECK_INTERVAL_NOT_INT)
        })?;
        // `max(1, int(...))` — legacy floored rather than rejecting, on this route only.
        let clamped = u32::try_from(minutes.max(1)).map_err(|_| {
            ApiError::invalid("check_interval_minutes", legacy::CHECK_INTERVAL_NOT_INT)
        })?;
        changes.check_interval_minutes = Some(clamped);
    }
    // `if "name" in changes and changes["name"]` — a falsy name is silently ignored.
    if let Some(value) = root.get("name").filter(|v| request::truthy(v)) {
        changes.name = Some(Box::from(request::py_str(value).as_str()));
    }

    let Ok(id) = SubId::parse(&id_text) else {
        // An id shaped like nothing the store could hold is "not found", not a 400: legacy did
        // `self._subs.get(sub_id)` and answered the same body for both.
        return Ok(Json(status_error(legacy::SUBSCRIPTION_NOT_FOUND)));
    };
    let changes = Box::new(changes);
    match send(&state, move |ack| SubCmd::Update { id, changes, ack }).await {
        Ok(view) => Ok(Json(
            json!({ "status": "ok", "subscription": project(&view) }),
        )),
        Err(Business(_)) => Ok(Json(status_error(legacy::SUBSCRIPTION_NOT_FOUND))),
        Err(Fatal(err)) => Err(err),
    }
}

/// The three keys `subscriptions/update` accepts, in legacy's tuple order.
pub const UPDATABLE: [&str; 3] = ["enabled", "check_interval_minutes", "name"];

/// `POST <p>subscriptions/delete` — `{"status":"ok"}`, and `[]` is still a `400`.
///
/// # Errors
/// `400` when `ids` is missing, empty or not a list; `503` when the manager is unreachable.
pub async fn delete(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    let ids = match root.get("ids") {
        Some(Value::Array(list)) if !list.is_empty() => list.clone(),
        _ => return Err(ApiError::bad_request(legacy::MISSING_IDS_LIST)),
    };
    let parsed: Vec<SubId> = ids
        .iter()
        .filter_map(|v| SubId::parse(&request::py_str(v)).ok())
        .collect();
    if !parsed.is_empty() {
        match send(&state, move |ack| SubCmd::Delete { ids: parsed, ack }).await {
            Ok(_) | Err(Business(_)) => {}
            Err(Fatal(err)) => return Err(err),
        }
    }
    Ok(Json(status_ok()))
}

/// `POST <p>subscriptions/check` — answers **immediately** with a job handle.
///
/// Legacy blocked for as long as every check took, which for a dozen feeds was minutes; the
/// `job_id` is the additive key that lets a v2-aware client follow along.
///
/// # Errors
/// `400` when `ids` is present and not a list; `503` when the manager is unreachable.
pub async fn check(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    let ids: Vec<SubId> = match root.get("ids") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(|v| SubId::parse(&request::py_str(v)).ok())
            .collect(),
        Some(_) => return Err(ApiError::bad_request(legacy::IDS_MUST_BE_LIST)),
    };
    match send(&state, move |ack| SubCmd::Check { ids, ack }).await {
        Ok(job) => Ok(Json(json!({ "status": "ok", "job_id": job.job_id }))),
        Err(Business(_)) => Ok(Json(status_ok())),
        Err(Fatal(err)) => Err(err),
    }
}

/// A manager failure, split into "report it in the body at 200" and "report it as HTTP".
enum Failure {
    /// A legacy business error: 200 with `{"status":"error","msg":…}`.
    Business(String),
    /// Not something legacy could produce: an unreachable manager.
    Fatal(ApiError),
}
use Failure::{Business, Fatal};

/// Sends one [`SubCmd`] and classifies its answer.
///
/// Everything legacy reported in the body stays in the body; only [`SubError::Unavailable`] — a
/// manager that has not started yet or has stopped — becomes an HTTP failure, because a client
/// deserves a retryable `503` rather than a cheerful `status: "error"` for that.
async fn send<T, F>(state: &ApiState, build: F) -> Result<T, Failure>
where
    F: FnOnce(oneshot::Sender<Result<T, SubError>>) -> SubCmd,
{
    let (ack, reply) = oneshot::channel();
    if let Err(e) = state.subs.send(build(ack)).await {
        return Err(Fatal(ApiError::from(e.to_wire())));
    }
    match reply.await {
        Err(_) => Err(Fatal(ApiError::from(SubError::Unavailable.to_wire()))),
        Ok(Ok(value)) => Ok(value),
        Ok(Err(SubError::Unavailable)) => {
            Err(Fatal(ApiError::from(SubError::Unavailable.to_wire())))
        }
        Ok(Err(SubError::NotFound(_))) => Err(Business(legacy::SUBSCRIPTION_NOT_FOUND.to_owned())),
        Ok(Err(other)) => Err(Business(other.to_string())),
    }
}

/// Whether a body names at least one updatable key.
///
/// Presence, not truthiness: legacy built `changes` with
/// `{k: v for k, v in post.items() if k in (...)}`, so `{"id": …, "name": null}` produced a
/// non-empty dict and was accepted as a no-op rather than rejected with
/// `no valid fields to update`.
#[must_use]
pub fn has_updatable(root: &Map<String, Value>) -> bool {
    UPDATABLE.iter().any(|key| root.contains_key(*key))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::{Codec, DownloadType};
    use std::sync::Arc;

    fn view() -> SubscriptionView {
        SubscriptionView {
            id: SubId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
            name: Arc::from("Blender Open Movies"),
            url: Arc::from("https://www.youtube.com/@BlenderOfficial/videos"),
            enabled: true,
            check_interval_minutes: 1440,
            download_type: DownloadType::Video,
            codec: Codec::Auto,
            format: Arc::from("any"),
            quality: Arc::from("best"),
            folder: Arc::from(""),
            last_checked: Some(1_893_456_000_000),
            seen_count: 7,
            error: None,
            next_due: Some(1_893_542_400_000),
            consecutive_failures: 0,
            checking: false,
        }
    }

    #[test]
    fn the_projection_is_exactly_the_legacy_thirteen() {
        let body = project(&view());
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), 13);
        for key in SubscriptionView::V1_KEYS {
            assert!(object.contains_key(key), "{key} missing");
        }
        for key in SubscriptionView::V2_ADDITIONAL_KEYS {
            assert!(!object.contains_key(key), "{key} is v2-only");
        }
    }

    #[test]
    fn last_checked_is_float_seconds() {
        let body = project(&view());
        assert_eq!(body["last_checked"], 1_893_456_000.0);
        assert!(body["last_checked"].is_f64(), "float, not integer ms");

        let mut never = view();
        never.last_checked = None;
        assert!(project(&never)["last_checked"].is_null());
    }

    #[test]
    fn the_projection_matches_the_captured_values() {
        // The exact row WP-00 captured from `GET /subscriptions`.
        let body = project(&view());
        assert_eq!(body["id"], "11111111-1111-4111-8111-111111111111");
        assert_eq!(body["name"], "Blender Open Movies");
        assert_eq!(
            body["url"],
            "https://www.youtube.com/@BlenderOfficial/videos"
        );
        assert_eq!(body["enabled"], true);
        assert_eq!(body["check_interval_minutes"], 1440);
        assert_eq!(body["download_type"], "video");
        assert_eq!(body["codec"], "auto");
        assert_eq!(body["format"], "any");
        assert_eq!(body["quality"], "best");
        assert_eq!(body["folder"], "");
        assert_eq!(body["seen_count"], 7);
        assert!(body["error"].is_null());
    }

    #[test]
    fn the_updatable_set_is_the_legacy_three() {
        assert_eq!(UPDATABLE, ["enabled", "check_interval_minutes", "name"]);
        let object = json!({ "id": "x", "name": null });
        assert!(
            has_updatable(object.as_object().unwrap()),
            "a null value still counts as a present key, as legacy's dict comprehension did"
        );
        let object = json!({ "id": "x", "nope": 1 });
        assert!(!has_updatable(object.as_object().unwrap()));
    }
}
