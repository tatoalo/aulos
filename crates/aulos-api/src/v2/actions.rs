//! `POST api/v2/items/actions`, `DELETE api/v2/items/{id}` and the clear route
//! (PROTOCOL §4.2, DESIGN §8.7, §8.10).
//!
//! Every action is idempotent and every id is answered for: an id the engine could not act on
//! comes back in `skipped` with one of the six documented reasons, and so does a token that is not
//! a ULID at all — `not_found` is the honest answer for both, and it keeps a client's retry loop
//! from having to distinguish a 400 from a partial success.

use aulos_core::ItemId;
use aulos_queue::{Action, SkipReason};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ApiError, Json};
use crate::v2::Q;
use crate::v2::{json_body, optional_json_body, parse_bool, parse_ids, parse_str, unknown_fields};
use crate::{ApiState, v2};

/// The keys `POST items/actions` accepts.
const ACTION_FIELDS: [&str; 3] = ["action", "ids", "delete_file"];

/// `POST api/v2/items/actions`.
pub async fn actions(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();
    unknown_fields(&root, &ACTION_FIELDS, &mut warnings);

    let raw_action = root
        .get("action")
        .ok_or_else(|| ApiError::invalid("action", "action is required"))?;
    let action = parse_action(parse_str("action", raw_action)?)?;

    let delete_file = match root.get("delete_file") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_bool("delete_file", value)?),
    };

    let tokens = parse_ids(&root)?;
    let (ids, unknown) = split_ids(&tokens);

    let result = if ids.is_empty() {
        aulos_queue::ActionsResult::default()
    } else {
        state.engine.actions(action, ids, delete_file).await
    };

    let mut skipped: Vec<Value> = unknown
        .iter()
        .map(|token| json!({ "id": token, "reason": SkipReason::NotFound }))
        .collect();
    skipped.extend(
        result
            .skipped
            .iter()
            .map(|s| json!({ "id": s.id, "reason": s.reason })),
    );

    Ok(Json(json!({
        "applied": result.applied,
        "skipped": skipped,
        "seq": state.seq(),
        "warnings": warnings,
    })))
}

/// `?delete_file=` on the single-item shorthand.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteQuery {
    /// `None` follows `DELETE_FILE_ON_TRASHCAN`.
    pub delete_file: Option<bool>,
}

/// `DELETE api/v2/items/{id}` — the single-item shorthand (PROTOCOL §4.2). `204` on success.
pub async fn delete_one(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Q(query): Q<DeleteQuery>,
) -> Result<axum::response::Response, ApiError> {
    let item = v2::downloads::parse_item_id(&id)
        .ok_or_else(|| ApiError::not_found(format!("no such item: {id}")))?;
    let result = state
        .engine
        .actions(Action::Delete, vec![item], query.delete_file)
        .await;
    if result
        .skipped
        .iter()
        .any(|s| s.reason == SkipReason::NotFound)
    {
        return Err(ApiError::not_found(format!("no such item: {id}")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST api/v2/items/clear` — delete every terminal record (DESIGN §8.10).
///
/// **Addition to PROTOCOL §4.7**, which documents no v2 clear route while DESIGN §8.10 defines the
/// command and the v1 shim's `POST <p>delete` with `where: "done"` needs it. A v2-only deployment
/// would otherwise have no way to empty its history except one `delete` per id. Recorded in
/// `docs/INTEGRATION-NOTES.md` so PROTOCOL can adopt it.
pub async fn clear(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = optional_json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();
    unknown_fields(&root, &CLEAR_FIELDS, &mut warnings);
    // PROTOCOL §4.7 documents the body as `{"where":"done"}` or `{}`, and both mean the same
    // thing — every terminal row. Knowing the key is what stops the *documented* request from
    // coming back with `warnings: ["unknown field \"where\" ignored"]`, which a client that
    // surfaces warnings reports as a malformed request.
    match root.get("where") {
        None | Some(Value::Null) => {}
        Some(Value::String(scope)) if scope == "done" => {}
        Some(_) => {
            return Err(ApiError::invalid(
                "where",
                "where must be \"done\", the only scope this route clears",
            ));
        }
    }
    let delete_file = match root.get("delete_file") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_bool("delete_file", value)?),
    };
    let result = state.engine.clear(delete_file).await;
    Ok(Json(json!({
        "removed": result.applied,
        "seq": state.seq(),
        "warnings": warnings,
    })))
}

/// The two keys `POST api/v2/items/clear` accepts (PROTOCOL §4.7).
///
/// `where` is documented and `delete_file` is the shim's addition, recorded in
/// `docs/INTEGRATION-NOTES.md` along with the route itself.
pub const CLEAR_FIELDS: [&str; 2] = ["where", "delete_file"];

/// The five documented actions, and nothing else.
fn parse_action(raw: &str) -> Result<Action, ApiError> {
    Action::ALL
        .into_iter()
        .find(|a| a.as_str() == raw)
        .ok_or_else(|| {
            let allowed = Action::ALL.map(Action::as_str).join(", ");
            ApiError::invalid("action", format!("action must be one of {allowed}"))
        })
}

/// Splits wire tokens into parsed ids and the ones that are not ULIDs at all.
fn split_ids(tokens: &[String]) -> (Vec<ItemId>, Vec<String>) {
    let mut ids = Vec::with_capacity(tokens.len());
    let mut unknown = Vec::new();
    for token in tokens {
        match v2::downloads::parse_item_id(token) {
            Some(id) => ids.push(id),
            None => unknown.push(token.clone()),
        }
    }
    (ids, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_actions_parse_and_nothing_else_does() {
        for action in Action::ALL {
            assert_eq!(parse_action(action.as_str()).expect("known"), action);
        }
        let err = parse_action("explode").expect_err("unknown");
        assert_eq!(err.field.as_deref(), Some("action"));
        assert!(err.message.contains("start, pause, cancel, retry, delete"));
    }

    #[test]
    fn the_clear_body_knows_its_own_documented_key() {
        // PROTOCOL §4.7 documents `{"where":"done"}`; sending exactly that must not warn.
        let body: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"where":"done"}"#).expect("an object");
        let mut warnings = Vec::new();
        unknown_fields(&body, &CLEAR_FIELDS, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_non_ulid_token_is_skipped_not_rejected() {
        let (ids, unknown) = split_ids(&["nope".to_owned(), ItemId::new().to_string()]);
        assert_eq!(ids.len(), 1);
        assert_eq!(unknown, ["nope"]);
    }
}
