//! `GET`/`POST`/`DELETE api/v2/cookies` (PROTOCOL §4.7, DESIGN §16.6).
//!
//! The upload cap is the legacy one, preserved to the byte: **1 000 000 bytes, decimal**, not
//! 1 MiB, with the message `Cookie file too large (max 1MB)`. A 1 020 000-byte file the Python
//! server rejected must still be rejected, and a client that shows the message must see the same
//! string — which is why the three legacy strings are `pub const`s here rather than literals: the
//! v1 shim (WP-15) emits the same three from `<p>upload-cookies` / `<p>delete-cookies`.
//!
//! The file is written atomically (`cookies.txt.tmp`, then a rename) with mode `0600`, and then
//! registered as the `cookiefile` runtime override so every job spawned afterwards picks it up
//! without a restart — exactly what `config.set_runtime_override('cookiefile', …)` did.

use std::path::PathBuf;
use std::sync::Arc;

use aulos_core::ErrorCode;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::ApiState;
use crate::error::{ApiError, Json};

/// The legacy cookie-upload cap: 1 000 000 bytes, **decimal** (legacy spec §2.1, DESIGN §16.6).
pub const MAX_COOKIE_BYTES: usize = 1_000_000;

/// The request-body ceiling for the whole v2 surface.
///
/// It has to exceed [`MAX_COOKIE_BYTES`] by enough for the multipart part headers, otherwise a
/// just-over-cap upload would be rejected by the body limit and the client would get axum's bare
/// `413` instead of the legacy message.
pub const BODY_LIMIT: usize = MAX_COOKIE_BYTES + 64 * 1024;

/// The multipart field name the upload must use.
pub const FIELD: &str = "cookies";

/// Legacy: no part named `cookies`.
pub const NO_FILE: &str = "No cookies file provided";

/// Legacy: the over-cap message, byte-identical.
pub const TOO_LARGE: &str = "Cookie file too large (max 1MB)";

/// Legacy: nothing to delete.
pub const NOTHING_TO_DELETE: &str = "No uploaded cookies to delete";

/// Legacy: `YTDL_OPTIONS` names a `cookiefile` the UI did not upload.
pub const MANUAL_COOKIEFILE: &str = "Cookies are configured manually via YTDL_OPTIONS (cookiefile). Remove or change that setting manually; UI delete only removes uploaded cookies.";

/// `<STATE_DIR>/cookies.txt` — the one path legacy used, so a cutover keeps the operator's file.
#[must_use]
pub fn path(state: &ApiState) -> PathBuf {
    state.cfg.paths.state.join("cookies.txt")
}

/// `GET api/v2/cookies` — `{has_cookies, bytes, updated_at}`.
///
/// `has_cookies` is true for an uploaded file **or** for a `cookiefile` the operator configured in
/// `YTDL_OPTIONS`, which is what the legacy `cookie-status` route reported; `bytes` and
/// `updated_at` describe the uploaded file only, and are `null` when there is none.
pub async fn status(State(state): State<ApiState>) -> Json<Value> {
    let uploaded = tokio::fs::metadata(path(&state)).await.ok();
    let configured = configured_cookiefile(&state);
    let has_configured = match &configured {
        Some(p) => tokio::fs::metadata(p).await.is_ok(),
        None => false,
    };
    Json(json!({
        "has_cookies": uploaded.is_some() || has_configured,
        "bytes": uploaded.as_ref().map(std::fs::Metadata::len),
        "updated_at": uploaded.as_ref().and_then(modified_ms),
    }))
}

/// `POST api/v2/cookies` — `multipart/form-data`, field `cookies` (PROTOCOL §4.7).
pub async fn upload(
    State(state): State<ApiState>,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Json<Value>, ApiError> {
    // The rejection is taken by value so that a body which is not multipart at all answers with
    // the error envelope rather than axum's plain-text `400` — PROTOCOL §1.5 has no exceptions.
    let mut multipart = multipart.map_err(|e| {
        ApiError::bad_request(format!("a multipart/form-data body is required: {e}"))
    })?;
    let mut content: Vec<u8> = Vec::new();
    let mut seen = false;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("the multipart body is malformed: {e}")))?
    {
        if field.name() != Some(FIELD) {
            continue;
        }
        seen = true;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| ApiError::bad_request(format!("the upload was interrupted: {e}")))?
        {
            if content.len() + chunk.len() > MAX_COOKIE_BYTES {
                return Err(ApiError::of(ErrorCode::PayloadTooLarge, TOO_LARGE));
            }
            content.extend_from_slice(&chunk);
        }
        break;
    }

    if !seen {
        return Err(ApiError::new(ErrorCode::BadRequest, NO_FILE, Some(FIELD)));
    }

    let target = path(&state);
    let bytes = content.len();
    write_atomically(&target, content)
        .await
        .map_err(|e| ApiError::internal(format!("the cookie file could not be written: {e}")))?;
    set_cookiefile(&state, Some(&target));
    tracing::info!(bytes, path = %target.display(), "cookies file uploaded");

    Ok(Json(json!({ "has_cookies": true, "bytes": bytes })))
}

/// `DELETE api/v2/cookies` — `204`, or `400` with the legacy message when there is nothing to
/// delete.
pub async fn remove(State(state): State<ApiState>) -> Result<Response, ApiError> {
    let target = path(&state);
    if tokio::fs::metadata(&target).await.is_err() {
        let manual = configured_cookiefile(&state).is_some_and(|p| p != target);
        let message = if manual {
            MANUAL_COOKIEFILE
        } else {
            NOTHING_TO_DELETE
        };
        return Err(ApiError::bad_request(message));
    }
    tokio::fs::remove_file(&target)
        .await
        .map_err(|e| ApiError::internal(format!("the cookie file could not be removed: {e}")))?;
    set_cookiefile(&state, None);
    tracing::info!(path = %target.display(), "cookies file deleted");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The `cookiefile` the *operator* configured, if any.
fn configured_cookiefile(state: &ApiState) -> Option<PathBuf> {
    let options = state.ytdl.load();
    options
        .base
        .get("cookiefile")
        .or_else(|| options.overrides.get("cookiefile"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Installs or clears the `cookiefile` runtime override on the live options snapshot.
fn set_cookiefile(state: &ApiState, file: Option<&std::path::Path>) {
    let mut next = (*state.ytdl.load_full()).clone();
    match file {
        Some(path) => {
            next.set_runtime_override("cookiefile", json!(path.display().to_string()));
        }
        None => next.remove_runtime_override("cookiefile"),
    }
    state.ytdl.store(Arc::new(next));
}

/// Writes `content` to `target` through a temporary file, mode `0600`.
async fn write_atomically(target: &std::path::Path, content: Vec<u8>) -> std::io::Result<()> {
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

/// A file's mtime in unix milliseconds.
fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_is_decimal_and_the_body_limit_leaves_room_for_it() {
        assert_eq!(MAX_COOKIE_BYTES, 1_000_000, "decimal, not 1 MiB");
        const {
            // A just-over-cap upload must reach the handler, so that the answer is the legacy
            // message rather than axum's bare body-limit rejection.
            assert!(BODY_LIMIT > MAX_COOKIE_BYTES);
        }
    }

    #[test]
    fn the_legacy_strings_are_byte_identical() {
        assert_eq!(TOO_LARGE, "Cookie file too large (max 1MB)");
        assert_eq!(NO_FILE, "No cookies file provided");
        assert_eq!(NOTHING_TO_DELETE, "No uploaded cookies to delete");
    }
}
