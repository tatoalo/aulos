//! `GET <p>download/*` and `GET <p>audio_download/*` — serving what was downloaded
//! (PROTOCOL §4.7, DESIGN §16.6).
//!
//! Four properties, each one a legacy bug or a client need:
//!
//! | Property | Why |
//! |---|---|
//! | **component-wise containment** | legacy compared strings, so `/downloads-evil` passed as inside `/downloads` (DESIGN §16.6). [`aulos_core::contain`] compares `Path::components()`, and the resolved path is checked again after `canonicalize` so a **symlink** cannot escape either. |
//! | `Range` / `If-Range` | an iOS client streams and resumes; legacy sent whole files. |
//! | `ETag` / `Last-Modified` | a re-open of a finished download is a `304`. |
//! | a JSON directory listing behind `DOWNLOAD_DIRS_INDEXABLE` | legacy served an HTML index; a JSON one is something a client can actually use. |
//!
//! A traversal attempt, a symlink escape and a missing file are all **`404`** with the error
//! envelope: distinguishing them would tell a prober what exists outside the root.

use std::path::{Path, PathBuf};

use aulos_core::DownloadType;
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::{ApiError, Json};
use crate::{ApiState, view};

/// The two file routes.
pub fn router(state: ApiState) -> axum::Router {
    let p = state.cfg.url_prefix.clone();
    axum::Router::new()
        .route(&p.route("download/{*path}"), get(download))
        .route(&p.route("audio_download/{*path}"), get(audio_download))
        .with_state(state)
}

/// `GET <p>download/*`.
pub async fn download(
    State(state): State<ApiState>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    serve(&state, DownloadType::Video, &path, &headers).await
}

/// `GET <p>audio_download/*`.
pub async fn audio_download(
    State(state): State<ApiState>,
    UrlPath(path): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    serve(&state, DownloadType::Audio, &path, &headers).await
}

/// The one handler both routes use.
async fn serve(
    state: &ApiState,
    download_type: DownloadType,
    raw: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let configured = state.cfg.paths.root_for(download_type);
    // The **canonical** root, because every path this handler compares or strips has been through
    // `canonicalize`: on macOS `/var` is a symlink to `/private/var`, so a non-canonical root
    // would make `strip_prefix` fail and the listing report the wrong relative path.
    let root = configured
        .canonicalize()
        .unwrap_or_else(|_| configured.to_path_buf());
    let resolved = resolve(&root, raw).ok_or_else(not_found)?;

    let meta = tokio::fs::metadata(&resolved)
        .await
        .map_err(|_| not_found())?;
    if meta.is_dir() {
        if !state.cfg.download_dirs_indexable {
            return Err(not_found());
        }
        return listing(state, download_type, &root, &resolved).await;
    }
    file(&resolved, &meta, headers).await
}

/// Resolves a request path inside `root`, rejecting every escape.
///
/// Three gates, in order: percent-decoding, [`aulos_core::contain`]'s component-wise check (which
/// rejects `..`, absolute paths and Windows separators), and a `canonicalize` of the result that is
/// re-checked against the canonicalised root — the last one is what catches a symlink pointing
/// outside the download tree.
fn resolve(root: &Path, raw: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .ok()?;
    let relative = decoded.trim_start_matches('/');
    if relative.is_empty() {
        return Some(root.to_path_buf());
    }
    let candidate = aulos_core::contain(root, Path::new(relative)).ok()?;
    let real = candidate.canonicalize().ok()?;
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if !real.starts_with(&real_root) {
        tracing::warn!(path = %real.display(), "a symlink pointed outside the download root");
        return None;
    }
    Some(real)
}

/// The JSON directory listing, when `DOWNLOAD_DIRS_INDEXABLE` is on.
async fn listing(
    state: &ApiState,
    download_type: DownloadType,
    root: &Path,
    dir: &Path,
) -> Result<Response, ApiError> {
    let mut entries = tokio::fs::read_dir(dir).await.map_err(|_| not_found())?;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = entry
            .path()
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| name.clone());
        if meta.is_dir() {
            dirs.push(json!({ "name": name, "path": relative }));
        } else {
            files.push(json!({
                "name": name,
                "path": relative,
                "size": meta.len(),
                "modified_at": modified_ms(&meta),
                "download_url": view::public_url(&state.cfg, &relative, download_type),
            }));
        }
    }
    dirs.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let path = dir
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    Ok(Json(json!({ "path": path, "dirs": dirs, "files": files })).into_response())
}

/// One file, with conditional and range handling.
async fn file(
    path: &Path,
    meta: &std::fs::Metadata,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let len = meta.len();
    let etag = etag_for(meta);
    let last_modified = modified_ms(meta).map(http_date);
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    let mut base = HeaderMap::new();
    base.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(value) = HeaderValue::from_str(&mime) {
        base.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&etag) {
        base.insert(header::ETAG, value);
    }
    if let Some(date) = &last_modified
        && let Ok(value) = HeaderValue::from_str(date)
    {
        base.insert(header::LAST_MODIFIED, value);
    }

    if matches(headers, header::IF_NONE_MATCH, &etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().extend(base);
        return Ok(response);
    }

    // `If-Range` that does not match the current representation means "send me the whole thing",
    // never "send a range of a file you have since replaced" (RFC 9110 §13.1.5).
    let range_allowed = headers.get(header::IF_RANGE).is_none_or(|value| {
        value == etag.as_str() || last_modified.as_deref() == value.to_str().ok()
    });

    let requested = if range_allowed {
        headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|raw| parse_range(raw, len))
    } else {
        None
    };

    match requested {
        Some(Err(())) => {
            let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            response.headers_mut().extend(base);
            if let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            Ok(response)
        }
        Some(Ok((start, end))) => {
            let mut handle = tokio::fs::File::open(path).await.map_err(|_| not_found())?;
            handle
                .seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| ApiError::internal(format!("seek failed: {e}")))?;
            let count = end - start + 1;
            let body = Body::from_stream(ReaderStream::new(handle.take(count)));
            let mut response = (StatusCode::PARTIAL_CONTENT, body).into_response();
            response.headers_mut().extend(base);
            if let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")) {
                response.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&count.to_string()).unwrap_or(HeaderValue::from_static("0")),
            );
            Ok(response)
        }
        None => {
            let handle = tokio::fs::File::open(path).await.map_err(|_| not_found())?;
            let body = Body::from_stream(ReaderStream::new(handle));
            let mut response = body.into_response();
            response.headers_mut().extend(base);
            response.headers_mut().insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap_or(HeaderValue::from_static("0")),
            );
            Ok(response)
        }
    }
}

/// `404` — the same answer for a traversal attempt, a symlink escape and a missing file.
fn not_found() -> ApiError {
    ApiError::not_found("no such file")
}

/// Whether a conditional header names this representation.
fn matches(headers: &HeaderMap, name: header::HeaderName, etag: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .any(|value| value == etag || value == "*")
}

/// `"<len>-<mtime_ms>"` — cheap, and it changes whenever the bytes do.
fn etag_for(meta: &std::fs::Metadata) -> String {
    format!("\"{}-{}\"", meta.len(), modified_ms(meta).unwrap_or(0))
}

/// A file's mtime in unix milliseconds.
fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// Parses a single-range `Range: bytes=…` header.
///
/// `Err(())` is "unsatisfiable" — a `416`. A multi-range request, or anything unparseable, is
/// `Ok(None)`-shaped by the caller (it serves the whole file), which is always a legal answer.
#[allow(clippy::result_unit_err)] // the error carries no information: it is exactly "416"
fn parse_range(raw: &str, len: u64) -> Result<(u64, u64), ()> {
    let spec = raw.trim().strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(()); // multi-range: answered as unsatisfiable rather than mis-served
    }
    let (from, to) = spec.split_once('-').ok_or(())?;
    let (start, end) = match (from.trim(), to.trim()) {
        ("", "") => return Err(()),
        ("", suffix) => {
            let n: u64 = suffix.parse().map_err(|_| ())?;
            if n == 0 || len == 0 {
                return Err(());
            }
            (len.saturating_sub(n), len - 1)
        }
        (start, "") => (start.parse().map_err(|_| ())?, len.saturating_sub(1)),
        (start, end) => (start.parse().map_err(|_| ())?, end.parse().map_err(|_| ())?),
    };
    if len == 0 || start >= len || end < start {
        return Err(());
    }
    Ok((start, end.min(len - 1)))
}

/// An IMF-fixdate, for `Last-Modified`.
///
/// Written out rather than pulled from a date library: `time` is not in `aulos-api`'s DESIGN §3
/// dependency row, and the civil-from-days algorithm is twenty lines with an exact test.
#[must_use]
pub fn http_date(unix_ms: i64) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = unix_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );
    // 1970-01-01 was a Thursday, which is index 3.
    let weekday = DAYS[usize::try_from((days + 3).rem_euclid(7)).unwrap_or(0)];
    let (year, month, day) = civil_from_days(days);
    let month_name = MONTHS[usize::from(month - 1).min(11)];
    format!("{weekday}, {day:02} {month_name} {year} {hour:02}:{minute:02}:{second:02} GMT")
}

/// Howard Hinnant's `civil_from_days`: days since the epoch → `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u8::try_from(m).unwrap_or(1),
        u8::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_the_three_documented_forms() {
        assert_eq!(parse_range("bytes=0-9", 100), Ok((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Ok((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Ok((90, 99)));
        assert_eq!(parse_range("bytes=0-1000", 100), Ok((0, 99)), "clamped");
    }

    #[test]
    fn an_unsatisfiable_range_is_an_error() {
        assert!(parse_range("bytes=100-200", 100).is_err());
        assert!(parse_range("bytes=-", 100).is_err());
        assert!(parse_range("items=0-1", 100).is_err());
        assert!(parse_range("bytes=0-9,20-29", 100).is_err(), "multi-range");
        assert!(parse_range("bytes=0-0", 0).is_err(), "an empty file");
        assert!(parse_range("bytes=5-1", 100).is_err(), "inverted");
    }

    #[test]
    fn http_dates_match_the_imf_fixdate_examples() {
        // The RFC 9110 example, plus the epoch and a leap day.
        assert_eq!(http_date(784_111_777_000), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            http_date(1_582_934_400_000),
            "Sat, 29 Feb 2020 00:00:00 GMT"
        );
    }

    #[test]
    fn containment_rejects_the_legacy_prefix_bug() {
        let root = Path::new("/downloads");
        // `..` never resolves, and a sibling directory whose name starts with the root's is not
        // inside it — the legacy `startswith` bug (DESIGN §16.6).
        assert!(resolve(root, "../etc/passwd").is_none());
        assert!(resolve(root, "%2e%2e%2fetc%2fpasswd").is_none());
        assert!(resolve(root, "/etc/passwd").is_none());
    }
}
