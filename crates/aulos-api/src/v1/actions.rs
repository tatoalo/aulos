//! `POST <p>delete` and `POST <p>start` — the id-resolution ladder and the `where` semantics
//! (DESIGN §11.3, PROTOCOL §10.3).
//!
//! # Why a ladder
//!
//! Legacy keyed everything by `url`. The shipped iOS build sends `item.url ?? item.id`, and its
//! `clearCompleted` sends **only** urls. So each token is resolved in this order, in one query
//! (`Store::resolve_v1_token`):
//!
//! ```text
//! 1. the token is a ULID and that item exists   -> [that id]
//! 2. exact match on items.url                    -> ALL matching ids
//! 3. exact match on items.media_id               -> ALL matching ids
//! 4. otherwise nothing                           -> counted, logged at DEBUG
//! ```
//!
//! Step 2 resolving to *all* matches is deliberate: the same URL added twice is two rows, and a
//! URL-keyed API is expected to affect both. Step 4 is silent, as legacy was — a stale token from
//! a client that has not refreshed must not fail the whole call.
//!
//! # `where`
//!
//! | `where` | v2 action |
//! |---|---|
//! | `"queue"` | `Cancel` on the non-terminal rows, then `Delete` — legacy dropped the row entirely, so the item disappears and the app's optimistic local removal stays correct |
//! | `"done"` | `Delete`, with `DELETE_FILE_ON_TRASHCAN` deciding the file |
//! | anything else, or absent | `400` (legacy sent a reasonless one) |

use aulos_core::ItemId;
use aulos_queue::{Action, SkipReason};
use axum::extract::State;
use serde_json::Value;

use super::{legacy, request, status_ok};
use crate::ApiState;
use crate::error::{ApiError, Json};

/// `POST <p>delete`.
///
/// # Errors
/// `400` when `ids` is falsy or `where` is neither `queue` nor `done`; `503` when the store is
/// busy.
pub async fn delete(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    let ids = root.get("ids").cloned().unwrap_or(Value::Null);
    let target = match root.get("where").and_then(Value::as_str) {
        Some("queue") => Where::Queue,
        Some("done") => Where::Done,
        _ => return Err(ApiError::bad_request(legacy::DELETE_BAD_REQUEST)),
    };
    if !request::truthy(&ids) {
        return Err(ApiError::bad_request(legacy::DELETE_BAD_REQUEST));
    }

    let resolved = resolve(&state, &tokens(&ids)).await?;
    match target {
        Where::Queue => {
            // Cancel first so a running process is actually killed, then delete the row. The
            // engine skips whatever the action does not apply to, which is exactly right: a
            // `finished` row in a `where: "queue"` call is a stale client, not an error.
            if !resolved.is_empty() {
                state
                    .engine
                    .actions(Action::Cancel, resolved.clone(), None)
                    .await;
                state.engine.actions(Action::Delete, resolved, None).await;
            }
        }
        Where::Done => {
            if !resolved.is_empty() {
                state.engine.actions(Action::Delete, resolved, None).await;
            }
        }
    }
    Ok(Json(status_ok()))
}

/// `POST <p>start`.
///
/// `queued(auto_start=false)` is started; a terminal row is **retried**, which legacy could not do
/// and which closes iOS pain point #24 with no client change. The two are told apart by the
/// engine, not by a status read here: every id is offered to `Start` first, and the ones it skips
/// as `not_startable` are offered to `Retry`. That is one extra message on a rare route and it
/// needs no second lookup, which matters because a terminal row outside the in-memory window has
/// no status the API layer can see.
///
/// # Errors
/// `400` when `ids` is missing or `null` — legacy crashed with a `TypeError` and answered `500`
/// (DESIGN §11.1). `503` when the store is busy.
pub async fn start(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    match root.get("ids") {
        None | Some(Value::Null) => return Err(ApiError::bad_request(legacy::START_IDS_REQUIRED)),
        Some(_) => {}
    }
    let ids = root.get("ids").cloned().unwrap_or(Value::Null);

    let resolved = resolve(&state, &tokens(&ids)).await?;
    if !resolved.is_empty() {
        let started = state.engine.actions(Action::Start, resolved, None).await;
        let retryable: Vec<ItemId> = started
            .skipped
            .iter()
            .filter(|s| {
                matches!(
                    s.reason,
                    SkipReason::NotStartable | SkipReason::AlreadyTerminal
                )
            })
            .map(|s| s.id)
            .collect();
        if !retryable.is_empty() {
            let retried = state.engine.actions(Action::Retry, retryable, None).await;
            tracing::info!(
                retried = retried.applied.len(),
                "v1 start retried terminal items, which legacy could not do"
            );
        }
    }
    Ok(Json(status_ok()))
}

/// Which collection a `POST <p>delete` addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Where {
    Queue,
    Done,
}

/// The tokens in an `ids` value.
///
/// A list is the documented form. A **bare string** is accepted because Python iterated it
/// character by character, so legacy answered `200 {"status":"ok"}` (having matched nothing)
/// rather than failing — WP-00's `start_ids_is_a_string` pins exactly that. Reproducing the
/// character split rather than treating the string as one token keeps a one-character `ids` from
/// accidentally matching a real row.
#[must_use]
pub fn tokens(ids: &Value) -> Vec<String> {
    match ids {
        Value::Array(list) => list.iter().map(request::py_str).collect(),
        Value::String(text) => text.chars().map(String::from).collect(),
        _ => Vec::new(),
    }
}

/// Every token through the DESIGN §11.3 ladder, de-duplicated, order preserved.
async fn resolve(state: &ApiState, tokens: &[String]) -> Result<Vec<ItemId>, ApiError> {
    let mut out: Vec<ItemId> = Vec::with_capacity(tokens.len());
    let mut skipped = 0_usize;
    for token in tokens {
        let matched = state.store.resolve_v1_token(token).await?;
        if matched.is_empty() {
            skipped += 1;
            tracing::debug!(token = %token, "v1 id token matched nothing; skipped as legacy did");
            continue;
        }
        for id in matched {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    if skipped > 0 {
        tracing::debug!(skipped, "v1 id tokens matched nothing");
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_list_of_tokens_passes_through() {
        assert_eq!(
            tokens(&json!(["https://a.test/x", "01JBQ7Z5T9K3M2R8V4XW6Y0AAA"])),
            ["https://a.test/x", "01JBQ7Z5T9K3M2R8V4XW6Y0AAA"]
        );
        // Python `str()`ed each element, so a number in the list is a token, not a type error.
        assert_eq!(tokens(&json!([7])), ["7"]);
    }

    #[test]
    fn a_bare_string_is_iterated_character_by_character() {
        assert_eq!(tokens(&json!("abc")), ["a", "b", "c"]);
        assert!(tokens(&json!(7)).is_empty());
        assert!(tokens(&json!(null)).is_empty());
        assert!(tokens(&json!({})).is_empty());
    }
}
