//! `POST <p>upload-cookies`, `POST <p>delete-cookies`, `GET <p>cookie-status` (DESIGN §11.1, §11.7).
//!
//! The cap is the legacy one **to the byte**: 1 000 000 decimal, not 1 MiB, tested as
//! `size > max_size`, so 1 000 000 is accepted and 1 000 001 is not. Getting that wrong is a
//! silent regression — a cookie file an operator has been uploading for a year would start being
//! rejected — which is why [`crate::v2::cookies::MAX_COOKIE_BYTES`] is a single constant shared
//! with the v2 route and asserted on both sides.
//!
//! # Two shapes, deliberately different from v2
//!
//! These three routes are the only place in the shim where legacy answered a **JSON body on a
//! 400**: `{"status":"error","msg":"Cookie file too large (max 1MB)"}` rather than an aiohttp
//! reason phrase. The shipped clients read `msg`, so the shim keeps that body verbatim instead of
//! substituting the §1.5 error envelope. Everywhere legacy used a *reason* the shim does use the
//! envelope — see [`super`].
//!
//! Legacy also read only the **first** multipart part and required it to be named `cookies`, so a
//! body whose first part is `cookiefile` is `No cookies file provided` even if a `cookies` part
//! follows. That is reproduced: `upload_cookies_wrong_field_name` pins it.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::{legacy, status_error, status_ok};
use crate::ApiState;
use crate::error::{ApiError, Json};
use crate::v2::cookies::{FIELD, MAX_COOKIE_BYTES, path};

/// `POST <p>upload-cookies` — `multipart/form-data`, field `cookies`.
///
/// Answers `200 {"status":"ok","msg":"Cookies uploaded (N bytes)"}`, or `400` with the legacy
/// `status: "error"` body.
///
/// # Errors
/// `500` only when the file cannot be written, which legacy would have raised as an unhandled
/// `OSError`.
pub async fn upload(
    State(state): State<ApiState>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Response, ApiError> {
    // A body that is not multipart at all was a 500 in legacy (`request.multipart()` raised); the
    // legacy `No cookies file provided` body at 400 is the honest translation, and it is what a
    // client that lost its boundary header can actually act on.
    let Ok(mut multipart) = multipart else {
        return Ok(rejected(legacy::NO_FILE));
    };

    let first = match multipart.next_field().await {
        Ok(field) => field,
        Err(e) => {
            tracing::debug!(error = %e, "v1 upload-cookies: malformed multipart body");
            return Ok(rejected(legacy::NO_FILE));
        }
    };
    let Some(mut field) = first.filter(|f| f.name() == Some(FIELD)) else {
        return Ok(rejected(legacy::NO_FILE));
    };

    let mut content: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(None) => break,
            Ok(Some(chunk)) => {
                // Legacy accumulated `size` first and then tested `size > max_size`, so exactly
                // 1 000 000 bytes is accepted and 1 000 001 is the first rejected size.
                if content.len() + chunk.len() > MAX_COOKIE_BYTES {
                    return Ok(rejected(legacy::TOO_LARGE));
                }
                content.extend_from_slice(&chunk);
            }
            Err(e) => {
                tracing::debug!(error = %e, "v1 upload-cookies: the upload was interrupted");
                return Ok(rejected(legacy::NO_FILE));
            }
        }
    }

    let target = path(&state);
    let bytes = content.len();
    write_atomically(&target, content)
        .await
        .map_err(|e| ApiError::internal(format!("the cookie file could not be written: {e}")))?;
    set_cookiefile(&state, Some(&target));
    tracing::info!(bytes, path = %target.display(), "cookies file uploaded via the v1 shim");

    Ok(Json(json!({ "status": "ok", "msg": legacy::cookies_uploaded(bytes) })).into_response())
}

/// `POST <p>delete-cookies`.
///
/// Two 400 bodies, both byte-identical: `No uploaded cookies to delete` when there is nothing, and
/// the long `Cookies are configured manually via YTDL_OPTIONS (cookiefile)…` sentence when the
/// operator set one in `YTDL_OPTIONS` that the UI did not upload.
///
/// Legacy's third answer — a `500` because reloading `YTDL_OPTIONS` failed after the delete — has
/// no counterpart: removing the runtime override *is* the reload here (DESIGN §17.2), so there is
/// no second step that can fail.
///
/// # Errors
/// `500` when the file exists but cannot be removed.
pub async fn remove(State(state): State<ApiState>) -> Result<Response, ApiError> {
    let target = path(&state);
    if tokio::fs::metadata(&target).await.is_err() {
        let manual = configured_cookiefile(&state).is_some_and(|p| p != target);
        return Ok(rejected(if manual {
            legacy::MANUAL_COOKIEFILE
        } else {
            legacy::NOTHING_TO_DELETE
        }));
    }
    tokio::fs::remove_file(&target)
        .await
        .map_err(|e| ApiError::internal(format!("the cookie file could not be removed: {e}")))?;
    set_cookiefile(&state, None);
    tracing::info!(path = %target.display(), "cookies file deleted via the v1 shim");
    Ok(Json(status_ok()).into_response())
}

/// `GET <p>cookie-status` — `{"status":"ok","has_cookies":bool}`.
///
/// `has_cookies` is true for an uploaded file **or** for a `cookiefile` the operator configured in
/// `YTDL_OPTIONS` that exists on disk, which is exactly what legacy reported.
pub async fn status(State(state): State<ApiState>) -> Json<Value> {
    let uploaded = tokio::fs::metadata(path(&state)).await.is_ok();
    let configured = match configured_cookiefile(&state) {
        Some(p) => tokio::fs::metadata(&p).await.is_ok(),
        None => false,
    };
    Json(json!({ "status": "ok", "has_cookies": uploaded || configured }))
}

/// A `400` carrying the legacy `status: "error"` body.
fn rejected(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(status_error(message))).into_response()
}

/// The `cookiefile` the *operator* configured, if any.
fn configured_cookiefile(state: &ApiState) -> Option<std::path::PathBuf> {
    let options = state.ytdl.load();
    options
        .base
        .get("cookiefile")
        .or_else(|| options.overrides.get("cookiefile"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

/// Installs or clears the `cookiefile` runtime override on the live options snapshot, so every job
/// spawned afterwards picks it up with no restart (legacy's `config.set_runtime_override`).
fn set_cookiefile(state: &ApiState, file: Option<&Path>) {
    let mut next = (*state.ytdl.load_full()).clone();
    match file {
        Some(path) => next.set_runtime_override("cookiefile", json!(path.display().to_string())),
        None => next.remove_runtime_override("cookiefile"),
    }
    state.ytdl.store(Arc::new(next));
}

/// Writes `content` to `target` through a temporary file, mode `0600`.
async fn write_atomically(target: &Path, content: Vec<u8>) -> std::io::Result<()> {
    let tmp = target.with_extension("txt.tmp");
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&tmp, &content).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(&tmp, target).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_is_the_decimal_million_shared_with_v2() {
        assert_eq!(MAX_COOKIE_BYTES, 1_000_000, "decimal, not 1 MiB");
        assert_ne!(MAX_COOKIE_BYTES, 1_048_576);
        // The boundary the corpus captures on both sides: `size > max_size` is the legacy test,
        // so 1 000 000 is the last accepted size and 1 000 001 the first rejected one.
        const {
            assert!(1_000_000 <= MAX_COOKIE_BYTES);
            assert!(1_000_001 > MAX_COOKIE_BYTES);
        }
    }

    #[test]
    fn a_rejection_is_the_legacy_body_at_400() {
        let response = rejected(legacy::TOO_LARGE);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = status_error(legacy::TOO_LARGE);
        assert_eq!(body["status"], "error");
        assert_eq!(body["msg"], "Cookie file too large (max 1MB)");
    }

    #[test]
    fn the_multipart_field_name_is_exactly_cookies() {
        assert_eq!(FIELD, "cookies");
    }
}
