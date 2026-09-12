//! `api/v2/subscriptions*` (PROTOCOL §4.7, §9, DESIGN §14.1).
//!
//! Every route is one [`SubCmd`] on an mpsc sender with no logic, which is the whole reason this
//! crate does not depend on `aulos-subscriptions` (DESIGN §3, §14.1) and why these handlers can be
//! tested against a fake receiver.
//!
//! `POST api/v2/subscriptions` takes **the same body as an add** plus `check_interval_minutes` and
//! an optional `name`, so it reuses the add parser rather than re-deriving a selection: the two
//! bodies cannot drift, and a new add field is available to a subscription the day it lands.

use aulos_core::{CheckJob, SubChanges, SubCmd, SubError, SubId, SubscriptionView};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::ApiState;
use crate::error::{ApiError, Json};
use crate::v2::{json_body, optional_json_body, parse_bool, parse_str, parse_u32, unknown_fields};

/// `GET api/v2/subscriptions`.
pub async fn list(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let list = send(&state, |ack| SubCmd::List { ack }).await?;
    Ok(Json(json!({ "subscriptions": list })))
}

/// `POST api/v2/subscriptions` — `201` with the created subscription (PROTOCOL §4.7).
///
/// The manager resolves the feed URL synchronously enough to reject a single-video URL with
/// `400 validation_failed` and a duplicate with `409 conflict`, both carrying the legacy strings
/// (DESIGN §14.3).
pub async fn create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let root = json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();
    let mut known: Vec<&str> = super::downloads::REQUEST_FIELDS.to_vec();
    known.push("check_interval_minutes");
    known.push("name");
    unknown_fields(&root, &known, &mut warnings);

    let request = super::downloads::parse_one(&state, &root)?;
    let interval = match root.get("check_interval_minutes") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_u32("check_interval_minutes", value)?.max(1)),
    };
    // Both shipped clients send `name`, so it has to reach the manager: the web page and the iOS
    // app let you name a subscription as you add it, and until it travelled the record was always
    // named after the feed. A blank one is left to the manager, which names it after the channel.
    let name = match root.get("name") {
        None | Some(Value::Null) => None,
        Some(value) => Some(Box::from(parse_str("name", value)?)),
    };

    // The whole download template, the interval and the name travel in the one `SubCmd::Add`, so
    // every documented body field reaches the record instead of being silently dropped.
    let request = Box::new(request);
    let view = send(&state, move |ack| SubCmd::Add {
        request,
        check_interval_minutes: interval,
        name,
        ack,
    })
    .await?;

    Ok((StatusCode::CREATED, Json(*view)).into_response())
}

/// `PATCH api/v2/subscriptions/{id}` — `{name?, enabled?, check_interval_minutes?}`.
pub async fn update(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<SubscriptionView>, ApiError> {
    let root = json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();
    unknown_fields(
        &root,
        &["name", "enabled", "check_interval_minutes"],
        &mut warnings,
    );

    let mut changes = SubChanges::default();
    if let Some(value) = root.get("enabled").filter(|v| !v.is_null()) {
        // The legacy `_coerce_bool` port: `true|1|on` / `false|0|off`, and
        // `enabled must be a boolean` on anything else (DESIGN §14.3 step 10).
        changes.enabled = Some(parse_bool("enabled", value)?);
    }
    if let Some(value) = root.get("check_interval_minutes").filter(|v| !v.is_null()) {
        changes.check_interval_minutes = Some(parse_u32("check_interval_minutes", value)?.max(1));
    }
    if let Some(value) = root.get("name").filter(|v| !v.is_null()) {
        changes.name = Some(Box::from(parse_str("name", value)?));
    }
    if changes.is_empty() {
        return Err(ApiError::bad_request(
            "at least one of name, enabled or check_interval_minutes is required",
        ));
    }

    let sub = parse_id(&id)?;
    let changes = Box::new(changes);
    let view = send(&state, move |ack| SubCmd::Update {
        id: sub,
        changes,
        ack,
    })
    .await?;
    Ok(Json(*view))
}

/// `DELETE api/v2/subscriptions/{id}` — `204`, or `404`.
pub async fn remove(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let sub = parse_id(&id)?;
    let removed = send(&state, move |ack| SubCmd::Delete {
        ids: vec![sub],
        ack,
    })
    .await?;
    if removed.is_empty() {
        return Err(ApiError::not_found(format!("no such subscription: {id}")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST api/v2/subscriptions/check` — `{"ids":[…]}` or `{}`, answered `202` immediately.
pub async fn check_many(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let root = optional_json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();
    unknown_fields(&root, &["ids"], &mut warnings);

    let ids = match root.get("ids") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => {
            let mut out = Vec::with_capacity(list.len());
            for value in list {
                out.push(parse_id(parse_str("ids", value)?)?);
            }
            out
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "ids",
                "ids must be an array of subscription ids",
            ));
        }
    };
    accepted(&state, ids).await
}

/// `POST api/v2/subscriptions/{id}/check` — `202`, or `404`.
pub async fn check_one(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    // Same gate as every other mutating v2 route (DESIGN §16.6): a present-but-wrong
    // `Content-Type` is a `400`, an absent one with an empty body is the bodyless `curl` case.
    optional_json_body(&headers, &body)?;
    let sub = parse_id(&id)?;
    accepted(&state, vec![sub]).await
}

/// Runs a check and answers `202 {"job_id", "count"}`.
async fn accepted(state: &ApiState, ids: Vec<SubId>) -> Result<Response, ApiError> {
    let job: CheckJob = send(state, move |ack| SubCmd::Check { ids, ack }).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job.job_id, "count": job.subscriptions.len() })),
    )
        .into_response())
}

/// A subscription id: any opaque token the store could hold (a ULID, or an imported UUID).
fn parse_id(raw: &str) -> Result<SubId, ApiError> {
    SubId::parse(raw).map_err(|_| ApiError::not_found(format!("no such subscription: {raw}")))
}

/// Sends one [`SubCmd`] and awaits its answer, mapping [`SubError`] onto the envelope.
///
/// A manager that is gone answers `503 state_unavailable` rather than hanging: `aulos-api` starts
/// before `aulos-subscriptions` in some wiring orders, and a client deserves an honest retryable
/// error in that window.
async fn send<T, F>(state: &ApiState, build: F) -> Result<T, ApiError>
where
    F: FnOnce(oneshot::Sender<Result<T, SubError>>) -> SubCmd,
{
    let (ack, reply) = oneshot::channel();
    state
        .subs
        .send(build(ack))
        .await
        .map_err(|e| ApiError::from(e.to_wire()))?;
    reply
        .await
        .map_err(|_| ApiError::from(SubError::Unavailable.to_wire()))?
        .map_err(|e| ApiError::from(e.to_wire()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unparseable_subscription_id_is_a_404() {
        let err = parse_id("").expect_err("empty");
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_legacy_subscription_strings_reach_the_envelope() {
        let err = ApiError::from(SubError::VideoOnly.to_wire());
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            &*err.message,
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );
        let err = ApiError::from(SubError::AlreadySubscribed.to_wire());
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(&*err.message, "This URL is already subscribed");
    }
}
