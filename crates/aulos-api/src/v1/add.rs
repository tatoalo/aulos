//! `POST <p>add` — validation, then the **bounded synchronous pre-resolve** (DESIGN §11.2).
//!
//! # Why this one route waits
//!
//! BRIEF §5 makes adding asynchronous: `POST api/v2/downloads` answers `202` before any metadata
//! extraction, because a v2 client watches the item's status. The shipped iOS build cannot. Its
//! `AddResultClassifier` decides success **purely by parsing this response body**, and the
//! "Couldn't add to Aulos" notification is the only failure signal the share extension can
//! produce. With an unconditional `{"status":"ok"}`, an unsupported URL, an
//! `Invalid/empty data was given.`, a geo-block or an `Unsupported resource "…"` would all read as
//! success and the item would fail silently in a queue the user may not open for hours — risk R24.
//!
//! So the shim submits the add and then waits, bounded by `AULOS_V1_ADD_RESOLVE_WAIT_MS`
//! (default 10 000):
//!
//! | Outcome within the window | Response |
//! |---|---|
//! | every id resolved | `200 {"status":"ok","ids":[…]}` |
//! | one or more ids ended in `error` | `200 {"status":"error","msg":"<messages joined with ", ">"}` |
//! | the window expired first | `200 {"status":"ok","ids":[…]}`, one WARN, the `timeout` counter |
//! | `AULOS_V1_ADD_RESOLVE_WAIT_MS=0` | `200 {"status":"ok","ids":[…]}` immediately, no wait at all |
//!
//! This is not a latency regression: legacy's `/add` blocked for the *whole* extraction with no
//! ceiling, so a bounded wait is strictly faster. And **validation still answers first** — every
//! DESIGN §11.7 400 is decided before a single message reaches the engine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aulos_core::{SourceKind, SourceRef};
use aulos_queue::{AddError, AddOutcome, CancelScope, ResolveReport};
use axum::extract::State;
use serde_json::{Value, json};

use super::{legacy, ok_body, request, status_error};
use crate::ApiState;
use crate::error::{ApiError, Json};

/// The three outcomes of the bounded pre-resolve, counted for the life of the process.
///
/// This is what DESIGN §11.2 calls `aulos_v1_add_resolve_total{outcome}`. The Prometheus endpoint
/// is CUT for v1.0 (BRIEF scope trims), so the counters live here as plain atomics: they cost
/// nothing, they are what a test asserts against, and re-exposing them as a metric later is a
/// three-line change in one place.
#[derive(Debug, Default)]
pub struct AddResolveCounters {
    /// Every id resolved inside the window.
    pub ok: AtomicU64,
    /// At least one id ended in `error`.
    pub error: AtomicU64,
    /// The window expired first.
    pub timeout: AtomicU64,
    /// `AULOS_V1_ADD_RESOLVE_WAIT_MS=0`, so no wait was attempted.
    pub skipped: AtomicU64,
}

impl AddResolveCounters {
    /// A snapshot as `(ok, error, timeout, skipped)`, for a test or a future exporter.
    #[must_use]
    pub fn read(&self) -> (u64, u64, u64, u64) {
        (
            self.ok.load(Ordering::Relaxed),
            self.error.load(Ordering::Relaxed),
            self.timeout.load(Ordering::Relaxed),
            self.skipped.load(Ordering::Relaxed),
        )
    }
}

/// The process-wide pre-resolve counters.
static ADD_RESOLVE: AddResolveCounters = AddResolveCounters {
    ok: AtomicU64::new(0),
    error: AtomicU64::new(0),
    timeout: AtomicU64::new(0),
    skipped: AtomicU64::new(0),
};

/// The `aulos_v1_add_resolve_total` counters (DESIGN §11.2).
#[must_use]
pub fn add_resolve_counters() -> &'static AddResolveCounters {
    &ADD_RESOLVE
}

/// `POST <p>add` (DESIGN §11.2).
///
/// # Errors
/// A `400` carrying a byte-identical legacy reason for any validation failure, `503` when the
/// engine is gone. Everything else — including a resolution failure and a duplicate — is a `200`.
pub async fn add(
    State(state): State<ApiState>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = super::read_json_object(&body)?;
    let presets = super::known_presets(&state);
    let request = request::parse_download_options(&state.cfg, &presets, root)?;

    let source = SourceRef::bare(SourceKind::ApiV1);
    let outcome = match state.engine.add(vec![request], source).await {
        Ok(outcome) => outcome,
        // Legacy skipped a duplicate silently and answered `{"status":"ok"}` with no new item, so
        // `AULOS_DEDUPE_MODE=strict` must not turn into a 409 on this route.
        Err(AddError::Duplicate { .. }) => return Ok(Json(ok_body(&[]))),
        Err(other) => return Err(map_add_error(other)),
    };

    Ok(Json(answer(&state, outcome).await))
}

/// The response body, after the bounded pre-resolve.
async fn answer(state: &ApiState, outcome: AddOutcome) -> Value {
    if outcome.ids.is_empty() {
        // Every request in the batch matched a live item (DESIGN §8.5's non-strict dedupe).
        return ok_body(&outcome.ids);
    }

    let wait_ms = state.cfg.v1_add_resolve_wait_ms;
    if wait_ms == 0 {
        // Pure async: error reporting for v1 is knowingly given up (DESIGN §11.2's fourth row).
        ADD_RESOLVE.skipped.fetch_add(1, Ordering::Relaxed);
        return ok_body(&outcome.ids);
    }

    let ids = outcome.ids.clone();
    let waited = tokio::time::timeout(
        Duration::from_millis(wait_ms),
        state.engine.wait_resolved(ids),
    )
    .await;

    match waited {
        Err(_elapsed) => {
            ADD_RESOLVE.timeout.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                wait_ms,
                ids = outcome.ids.len(),
                "the v1 add pre-resolve window expired; answering ok and letting the item's real \
                 outcome show up in GET history"
            );
            ok_body(&outcome.ids)
        }
        Ok(reports) => match failures(&reports) {
            None => {
                ADD_RESOLVE.ok.fetch_add(1, Ordering::Relaxed);
                ok_body(&outcome.ids)
            }
            Some(message) => {
                ADD_RESOLVE.error.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    ids = outcome.ids.len(),
                    "the v1 add resolved to an error; reporting it in the body as legacy did"
                );
                status_error(&message)
            }
        },
    }
}

/// The failing reports' messages, joined with a comma and a space — legacy's own `', '.join`.
///
/// `None` means every report succeeded. A duplicate message is dropped: a 500-item playlist whose
/// every child fails the same way produced one sentence in legacy, not five hundred.
fn failures(reports: &[ResolveReport]) -> Option<String> {
    let mut messages: Vec<String> = Vec::new();
    for report in reports {
        if let Err(err) = &report.outcome {
            let text = err.message.to_string();
            if !messages.contains(&text) {
                messages.push(text);
            }
        }
    }
    if messages.is_empty() {
        None
    } else {
        Some(messages.join(", "))
    }
}

/// `POST <p>cancel-add` (DESIGN §11.1).
///
/// The body is ignored, exactly as legacy's `cancel_add()` ignored it — it took no argument and
/// simply bumped a process-global counter, so there is no generation for a v1 client to send and
/// none for the shim to invent. [`CancelScope::All`] is the faithful translation, and unlike
/// legacy it actually aborts in-flight resolution instead of only checking between entries.
///
/// A missing body, a malformed body and a body with a `generation` all behave identically.
pub async fn cancel_add(State(state): State<ApiState>) -> Json<Value> {
    let result = state.engine.cancel_resolve(CancelScope::All).await;
    tracing::info!(
        canceled = result.applied.len(),
        "v1 cancel-add aborted every in-flight resolution"
    );
    Json(json!({ "status": "ok" }))
}

/// Maps the remaining [`AddError`]s onto the envelope.
///
/// `Invalid` cannot normally happen here — the shim has already run the legacy matrix — but the
/// engine also owns the folder resolve and containment checks (DESIGN §11.7's folder row), so its
/// [`aulos_core::WireError`]s must reach the client with their strings intact.
fn map_add_error(err: AddError) -> ApiError {
    match err {
        AddError::Invalid { errors, .. } => errors.into_iter().next().map_or_else(
            || {
                ApiError::of(
                    aulos_core::ErrorCode::ValidationFailed,
                    legacy::MISSING_REQUIRED,
                )
            },
            ApiError::from,
        ),
        AddError::Duplicate { existing_id, .. } => ApiError::of(
            aulos_core::ErrorCode::Conflict,
            format!("this url already has a live item ({existing_id})"),
        ),
        AddError::TooManyUrls { max, got } => ApiError::of(
            aulos_core::ErrorCode::PayloadTooLarge,
            format!("{got} urls exceeds the {max} per-batch limit"),
        ),
        AddError::Unavailable(message) => ApiError::unavailable(message),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::{ErrorCode, ItemId, Kind, WireError};

    fn report(message: Option<&str>) -> ResolveReport {
        ResolveReport {
            id: ItemId::new(),
            kind: Kind::Item,
            outcome: match message {
                None => Ok(()),
                Some(text) => Err(WireError::new(ErrorCode::UnsupportedUrl, text)),
            },
        }
    }

    #[test]
    fn every_success_joins_to_nothing() {
        assert_eq!(failures(&[]), None);
        assert_eq!(failures(&[report(None), report(None)]), None);
    }

    #[test]
    fn two_failures_join_with_a_comma_and_a_space() {
        let joined = failures(&[
            report(Some("Invalid/empty data was given.")),
            report(None),
            report(Some("Unsupported resource \"playlist\"")),
        ])
        .expect("a message");
        assert_eq!(
            joined,
            "Invalid/empty data was given., Unsupported resource \"playlist\""
        );
    }

    #[test]
    fn a_repeated_message_is_reported_once() {
        // A 500-item playlist whose children all fail the same way produced one sentence in
        // legacy, because the children shared an error object.
        let joined = failures(&[
            report(Some("Video unavailable")),
            report(Some("Video unavailable")),
        ])
        .expect("a message");
        assert_eq!(joined, "Video unavailable");
    }

    #[test]
    fn the_error_body_is_the_shape_the_shipped_classifier_parses() {
        let body = status_error("Video unavailable");
        assert_eq!(body["status"], "error");
        assert_eq!(body["msg"], "Video unavailable");
        assert!(body.get("ids").is_none(), "a failure minted nothing usable");
    }

    #[test]
    fn the_ok_body_carries_the_additive_ids_key() {
        let id = ItemId::new();
        let body = ok_body(&[id]);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["ids"][0], id.to_string());
        assert_eq!(
            ok_body(&[])["ids"].as_array().unwrap().len(),
            0,
            "a duplicate still answers ok, with an empty ids"
        );
    }

    #[test]
    fn the_add_errors_keep_their_documented_statuses() {
        assert_eq!(
            map_add_error(AddError::TooManyUrls { max: 500, got: 501 })
                .status()
                .as_u16(),
            413
        );
        assert_eq!(
            map_add_error(AddError::Unavailable("busy".into()))
                .status()
                .as_u16(),
            503
        );
        let err = map_add_error(AddError::field(
            0,
            ErrorCode::FolderInvalid,
            "folder",
            "A folder for the download was specified but CUSTOM_DIRS is not true in the \
             configuration.",
        ));
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(
            &*err.message,
            "A folder for the download was specified but CUSTOM_DIRS is not true in the \
             configuration."
        );
    }
}
