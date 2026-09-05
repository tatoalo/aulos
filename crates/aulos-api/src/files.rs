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
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
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
    let private = private_paths(state, &root);
    if is_private(&resolved, &private) {
        return Err(not_found());
    }

    let meta = tokio::fs::metadata(&resolved)
        .await
        .map_err(|_| not_found())?;
    if meta.is_dir() {
        if !state.cfg.download_dirs_indexable {
            return Err(not_found());
        }
        return listing(state, download_type, &root, &resolved, &private).await;
    }
    file(&resolved, &meta, headers).await
}

/// The state files this route must never serve, whatever an operator called them.
///
/// The database and its sidecars are derived from `AULOS_DB_PATH`, so only the fixed names belong
/// here: `cookies.txt` (the operator's live site sessions), the import marker, and the four legacy
/// JSON files a `STATE_DIR` carried before the import — the whole legacy queue, history and
/// subscription list. They mirror `aulos_store::import`'s layout; the constants there are not
/// public, and `aulos-api` may not reach into that module for them (DESIGN §3).
const STATE_FILES: [&str; 6] = [
    "cookies.txt",
    ".aulos-imported",
    "completed.json",
    "pending.json",
    "queue.json",
    "subscriptions.json",
];

/// The paths this route must never serve out of, canonicalised, for the root being served.
///
/// The state directory can sit anywhere relative to the download root, and the two arrangements
/// need opposite treatment:
///
/// | Layout | What is excluded |
/// |---|---|
/// | `STATE_DIR` strictly **inside** the root — the shipped image's `DOWNLOAD_DIR=/downloads STATE_DIR=/downloads/.metube` | the whole subtree, so a state file added later is covered too |
/// | `STATE_DIR` **is** the root, or contains it — `DOWNLOAD_DIR` and `STATE_DIR` both default to `.`, so every non-Docker run lands here, as does an operator who sets `STATE_DIR=/downloads` | the individual state files, by name |
///
/// The distinction is the whole point: excluding the subtree in the second layout would make every
/// path under the root "private" and answer `404` to the entire download tree (and hand back an
/// empty listing), which is what a plain `starts_with` test did.
///
/// Containment cannot help in either case — those paths really are inside — so the exclusion is by
/// path, with the same `404` every other refusal gets (DESIGN §16.6).
fn private_paths(state: &ApiState, root: &Path) -> Vec<PathBuf> {
    private_paths_for(&state.cfg.paths.state, &state.cfg.db_path, root)
}

/// [`private_paths`] over its three inputs, so the layout rule is testable without an `ApiState`.
fn private_paths_for(state_dir: &Path, db_path: &Path, root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::with_capacity(2);
    push(&mut dirs, canonical(state_dir));
    // `AULOS_DB_PATH` defaults inside STATE_DIR but can be pointed anywhere, including at a second
    // directory under the download root.
    push(&mut dirs, canonical(&db_dir(db_path)));

    let mut out = Vec::new();
    for dir in dirs {
        if dir != root && dir.starts_with(root) {
            push(&mut out, dir); // a strict descendant: the whole subtree
            continue;
        }
        for name in STATE_FILES {
            push(&mut out, canonical(&dir.join(name)));
        }
    }
    // The database is named by `AULOS_DB_PATH`, and SQLite writes two sidecars next to it (three
    // with a rollback journal). They are added unconditionally: when their directory was excluded
    // wholesale this is a no-op, and when it was not it is the only thing hiding them.
    if let Some(name) = db_path.file_name() {
        let parent = canonical(&db_dir(db_path));
        let name = name.to_string_lossy();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            push(&mut out, canonical(&parent.join(format!("{name}{suffix}"))));
        }
    }
    out
}

/// The directory `AULOS_DB_PATH` names, with a bare file name meaning the working directory.
///
/// `PathBuf::from("aulos.db").parent()` is `Some("")`, and an empty path canonicalises to nothing,
/// so the database would be excluded under a name that matches no resolved path. `.` canonicalises
/// to the working directory, which is where SQLite actually opens it.
fn db_dir(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// A path with every symlink resolved, or the path itself when it does not exist yet.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Appends unless the list already holds it.
fn push(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// Whether a resolved path *is*, or is inside, one of [`private_paths`].
fn is_private(path: &Path, private: &[PathBuf]) -> bool {
    private.iter().any(|root| path.starts_with(root))
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
    private: &[PathBuf],
) -> Result<Response, ApiError> {
    let mut entries = tokio::fs::read_dir(dir).await.map_err(|_| not_found())?;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        // An excluded directory must not even be advertised: a listing naming `.metube` tells a
        // prober exactly where to look next.
        let path = entry.path();
        let canonical = path.canonicalize().unwrap_or(path);
        if is_private(&canonical, private) {
            continue;
        }
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
    let guessed = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    let inline = is_inlineable(&guessed);
    let mime = if inline {
        guessed
    } else {
        "application/octet-stream".to_owned()
    };

    let mut base = HeaderMap::new();
    base.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // The download tree is served from the same origin as the API and the WebSocket, and in the
    // intended deployment that origin carries the reverse proxy's session cookie. A file named
    // `*.html` or `*.svg` — planted through the Samba/Jellyfin share the volume is exported over,
    // or produced by a `command` plugin — would otherwise execute script on the API origin and be
    // able to drive every authenticated route with the viewer's session. Media keeps its real type
    // (so `<video>` and `Range` are unaffected); everything else is an opaque attachment, and
    // `nosniff` stops the browser from second-guessing either.
    base.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(value) = HeaderValue::from_str(&mime) {
        base.insert(header::CONTENT_TYPE, value);
    }
    if !inline && let Some(value) = content_disposition(path) {
        base.insert(header::CONTENT_DISPOSITION, value);
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

    // `Ok(None)` from `parse_range` and "no `Range` at all" are the same answer — serve the whole
    // file — so they collapse into one `None` here; only `Err(())` reaches the `416` arm.
    let requested = if range_allowed {
        match headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|raw| parse_range(raw, len))
        {
            None | Some(Ok(None)) => None,
            Some(Ok(Some(range))) => Some(Ok(range)),
            Some(Err(())) => Some(Err(())),
        }
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

/// Whether a guessed media type may be served with its real `Content-Type` and rendered inline.
///
/// The allow-list is the set an iOS client actually plays or shows: audio, video, raster images,
/// and the two subtitle types. `image/svg+xml` is deliberately **not** on it — an SVG is a script
/// container, not a picture, as far as a browser is concerned.
fn is_inlineable(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or_default().trim();
    if base.eq_ignore_ascii_case("image/svg+xml") {
        return false;
    }
    let lower = base.to_ascii_lowercase();
    lower.starts_with("audio/")
        || lower.starts_with("video/")
        || lower.starts_with("image/")
        || matches!(
            lower.as_str(),
            "text/vtt" | "application/x-subrip" | "application/mp4"
        )
}

/// `Content-Disposition: attachment; filename*=UTF-8''<name>` for a file served opaquely.
fn content_disposition(path: &Path) -> Option<HeaderValue> {
    let name = path.file_name()?.to_string_lossy();
    // RFC 5987: percent-encode everything outside the attr-char set, so a quote or a newline in a
    // downloaded title cannot break out of the header.
    let encoded: String =
        percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    HeaderValue::from_str(&format!("attachment; filename*=UTF-8''{encoded}")).ok()
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
/// The two failures are **not** the same answer, and RFC 9110 §14.2 is explicit about it:
///
/// | Outcome | Meaning | Answer |
/// |---|---|---|
/// | `Ok(Some((start, end)))` | a satisfiable byte range | `206` |
/// | `Ok(None)` | a header this server does not understand — an unknown unit, an unparseable spec, or a multi-range set | **ignore it** and serve `200` |
/// | `Err(())` | syntactically valid but unsatisfiable — `start >= len`, `end < start`, a suffix of an empty file | `416` |
///
/// The old shape answered `416` to `bytes=0-1023,2048-3071`, a perfectly legal multi-range that
/// every other server serves whole, and a downloader that sends one treats the `416` as "this
/// file cannot be fetched".
#[allow(clippy::result_unit_err)] // the error carries no information: it is exactly "416"
fn parse_range(raw: &str, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return Ok(None); // a unit this server does not implement
    };
    if spec.contains(',') {
        return Ok(None); // multi-range: legal, and serving the whole file is a legal answer
    }
    let Some((from, to)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (start, end) = match (from.trim(), to.trim()) {
        ("", "") => return Ok(None), // `bytes=-` is not a byte-range-spec at all
        ("", suffix) => {
            let Ok(n) = suffix.parse::<u64>() else {
                return Ok(None);
            };
            if n == 0 || len == 0 {
                return Err(()); // a well-formed suffix that cannot be satisfied
            }
            (len.saturating_sub(n), len - 1)
        }
        (start, "") => {
            let Ok(start) = start.parse::<u64>() else {
                return Ok(None);
            };
            (start, len.saturating_sub(1))
        }
        (start, end) => {
            let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                return Ok(None);
            };
            (start, end)
        }
    };
    if len == 0 || start >= len || end < start {
        return Err(());
    }
    Ok(Some((start, end.min(len - 1))))
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
        assert_eq!(parse_range("bytes=0-9", 100), Ok(Some((0, 9))));
        assert_eq!(parse_range("bytes=90-", 100), Ok(Some((90, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Ok(Some((90, 99))));
        assert_eq!(
            parse_range("bytes=0-1000", 100),
            Ok(Some((0, 99))),
            "clamped"
        );
    }

    #[test]
    fn an_unsatisfiable_range_is_an_error() {
        // Syntactically valid, cannot be met: RFC 9110 §14.2's `416`.
        assert_eq!(parse_range("bytes=100-200", 100), Err(()));
        assert_eq!(parse_range("bytes=0-0", 0), Err(()), "an empty file");
        assert_eq!(parse_range("bytes=-5", 0), Err(()), "a suffix of nothing");
        assert_eq!(parse_range("bytes=5-1", 100), Err(()), "inverted");
    }

    /// RFC 9110 §14.2: "An origin server MUST ignore a Range header field that contains a range
    /// unit it does not understand" — and a syntactically invalid set is likewise ignored, never
    /// a `416`. A multi-range set is legal, and serving the whole file answers it.
    #[test]
    fn an_unparseable_or_multi_range_header_is_ignored_not_416() {
        for raw in [
            "bytes=-",
            "items=0-1",
            "bytes=abc",
            "bytes=0-9,20-29",
            "bytes=x-9",
            "bytes=0-y",
            "bytes=-z",
            "bytes",
        ] {
            assert_eq!(parse_range(raw, 100), Ok(None), "{raw}");
        }
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

    /// The shipped image's layout: the state directory is a strict descendant of the served root,
    /// so the whole subtree goes — a state file added in a later version is covered too.
    #[test]
    fn a_nested_state_directory_is_excluded_whole() {
        let root = Path::new("/downloads");
        let private = private_paths_for(
            Path::new("/downloads/.metube"),
            Path::new("/downloads/.metube/aulos.db"),
            root,
        );
        assert!(private.contains(&PathBuf::from("/downloads/.metube")));
        assert!(
            !private.contains(&PathBuf::from("/downloads")),
            "{private:?}"
        );
        assert!(is_private(
            Path::new("/downloads/.metube/cookies.txt"),
            &private
        ));
        assert!(is_private(Path::new("/downloads/.metube"), &private));
        assert!(!is_private(Path::new("/downloads/A video.mp4"), &private));
        assert!(!is_private(Path::new("/downloads/Media/ok.mp4"), &private));
    }

    /// `DOWNLOAD_DIR` and `STATE_DIR` both default to `.`, so a bare `cargo run` or a
    /// bare-binary deployment has the state directory **equal** to the served root. Excluding the
    /// subtree there would `404` the entire download tree, so the state files go by name.
    #[test]
    fn a_state_directory_equal_to_the_root_excludes_files_not_the_tree() {
        let root = Path::new("/downloads");
        let private = private_paths_for(
            Path::new("/downloads"),
            Path::new("/downloads/aulos.db"),
            root,
        );
        assert!(
            !private.contains(&PathBuf::from("/downloads")),
            "{private:?}"
        );
        for secret in [
            "/downloads/cookies.txt",
            "/downloads/aulos.db",
            "/downloads/aulos.db-wal",
            "/downloads/aulos.db-shm",
            "/downloads/.aulos-imported",
            "/downloads/queue.json",
        ] {
            assert!(is_private(Path::new(secret), &private), "{secret}");
        }
        for served in [
            "/downloads/A video.mp4",
            "/downloads/Media/ok.mp4",
            "/downloads/aulos.db.mp4",
        ] {
            assert!(!is_private(Path::new(served), &private), "{served}");
        }
    }

    /// The mirror image — `STATE_DIR` *contains* the download root (`STATE_DIR=/data`,
    /// `DOWNLOAD_DIR=/data/downloads`) — is the same trap and gets the same answer.
    #[test]
    fn a_state_directory_above_the_root_does_not_hide_the_tree() {
        let root = Path::new("/data/downloads");
        let private = private_paths_for(Path::new("/data"), Path::new("/data/aulos.db"), root);
        assert!(!is_private(Path::new("/data/downloads/clip.mp4"), &private));
        assert!(is_private(Path::new("/data/aulos.db"), &private));
    }

    /// `AULOS_DB_PATH` can point at a second directory under the root, on its own.
    #[test]
    fn the_database_is_excluded_wherever_it_is_pointed() {
        let root = Path::new("/downloads");
        let private = private_paths_for(
            Path::new("/state"),
            Path::new("/downloads/db/aulos.db"),
            root,
        );
        assert!(is_private(Path::new("/downloads/db/aulos.db"), &private));
        assert!(
            is_private(Path::new("/downloads/db"), &private),
            "a strict descendant"
        );
        assert!(!is_private(Path::new("/downloads/clip.mp4"), &private));
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
