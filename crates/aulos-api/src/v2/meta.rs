//! Discovery, options and the small top-level routes: `capabilities`, `catalog`, `presets`,
//! `providers`, `plugins/reload`, `resolve-preview`, `custom-dirs`, `import-report`,
//! `ytdl-options[/reload]`, `debug/options`, plus `GET <p>`, `version`, `robots.txt`, the
//! `socket.io` 501 and the CUT `metrics` route (PROTOCOL §4.5–§4.7, §1.6).
//!
//! Everything here is a projection of state something else owns — the registry, the config, the
//! live `YTDL_OPTIONS` snapshot, the health registry — which is why the two ETag-able payloads can
//! be hashed rather than versioned: recomputing them is cheaper than tracking their invalidation.

use std::collections::BTreeSet;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use aulos_core::{
    Choice, ComponentStatus, Config, DownloadType, ErrorCode, FormatCatalog, NamingPolicy,
    OptionSpec, ProviderId, ReloadReport, YtdlOptions,
};
use aulos_provider::{Match, MatchReason, ProviderState, Registry};
use aulos_queue::{Action, FrameKind};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::error::{ApiError, Json};
use crate::v2::Q;
use crate::{ApiState, DirsCache};

/// The features `capabilities.features` advertises (PROTOCOL §4.5).
///
/// `cancel_resolve` is in the list because `POST api/v2/downloads/cancel-resolve` exists: without
/// the route a v2-only deployment would lose a capability legacy had — aborting a 500-item
/// playlist add (PLAN WP-14).
pub const FEATURES: [&str; 14] = [
    "async_add",
    "stable_ids",
    "deltas",
    "since_resume",
    "etag",
    "retry",
    "cancel",
    "cancel_resolve",
    "groups",
    "subscriptions",
    "file_serving",
    "batch_add",
    "postprocessing_status",
    "per_url_catalog",
];

/// The four download types, in PROTOCOL §4.5 order (which is not `DownloadType::ALL`'s
/// alphabetical order).
pub const DOWNLOAD_TYPES: [&str; 4] = ["video", "audio", "captions", "thumbnail"];

/// The five codecs, in PROTOCOL §4.5 order.
pub const CODECS: [&str; 5] = ["auto", "h264", "h265", "av1", "vp9"];

/// The four subtitle modes, in PROTOCOL §4.5 order.
pub const SUBTITLE_MODES: [&str; 4] = ["auto_only", "manual_only", "prefer_manual", "prefer_auto"];

/// How long a `custom-dirs` walk is reused.
const DIRS_TTL_MS: i64 = 10_000;

/// How long `healthz?probe=deep` waits before re-probing (DESIGN §16.3).
pub const DEEP_PROBE_INTERVAL_MS: i64 = 10_000;

/// The `provider` field of the merged catalog.
///
/// **PROTOCOL §4.6 gap**: it defines `provider` as "the provider whose catalog this is" and does
/// not say what a merged catalog reports. `"merged"` is not a provider id — no provider can be
/// named that, since a real id would then be ambiguous — and `match` is `null` on the same
/// payload, which is the documented signal that this is the union rather than one provider's view.
pub const MERGED_PROVIDER: &str = "merged";

// ---------------------------------------------------------------------------
// the small top-level routes
// ---------------------------------------------------------------------------

/// `GET <p>` — the identity document. No HTML, no theme cookie (DESIGN §11.1).
pub async fn identity(State(state): State<ApiState>) -> Json<Value> {
    Json(json!({
        "name": "aulos-server",
        "version": state.info.version,
        "url_prefix": state.cfg.url_prefix,
        "protocol": "v2",
    }))
}

/// `GET <p>version` — the legacy shape plus two additive keys (PROTOCOL §4.7, §10.1).
pub async fn version(State(state): State<ApiState>) -> Json<Value> {
    Json(json!({
        "version": state.info.version,
        "yt-dlp": state.info.yt_dlp,
        "url_prefix": state.cfg.url_prefix,
        "protocol": "v2",
    }))
}

/// `GET <p>robots.txt` — the configured file, or the default that disallows everything.
pub async fn robots(State(state): State<ApiState>) -> Response {
    let body = match &state.cfg.robots_txt {
        Some(path) => tokio::fs::read_to_string(path).await.unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "ROBOTS_TXT is unreadable");
            default_robots()
        }),
        None => default_robots(),
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// What `robots.txt` says with nothing configured.
///
/// DESIGN §11.7 pins this body to the byte — three `\n`-terminated lines, no trailing blank one —
/// and it is defined once, in [`crate::v1::legacy::ROBOTS_TXT`], because the shim's golden replay
/// compares against it (WP-15).
fn default_robots() -> String {
    crate::v1::legacy::ROBOTS_TXT.to_owned()
}

/// `GET <p>socket.io/*` — the one 501 in the taxonomy (DESIGN §11.1).
pub async fn socketio_removed(State(state): State<ApiState>) -> ApiError {
    let prefix = state.cfg.url_prefix.as_str();
    ApiError::of(
        ErrorCode::SocketioRemoved,
        format!(
            "Socket.IO is not supported; use {prefix}ws (protocol v2) or GET {prefix}api/v2/state"
        ),
    )
}

/// `GET <p>metrics` — **v1.0: not implemented, see BRIEF.**
///
/// The Prometheus endpoint and `AULOS_METRICS_ENABLED` are CUT. The route exists so an operator's
/// scrape config fails visibly, with the error envelope, instead of hanging on a 404 page.
pub async fn metrics_cut() -> ApiError {
    ApiError::not_found("the Prometheus endpoint is not implemented in this build")
}

// ---------------------------------------------------------------------------
// capabilities
// ---------------------------------------------------------------------------

/// `GET api/v2/capabilities` — everything a client needs to configure itself (PROTOCOL §4.5).
pub async fn capabilities(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let body = capabilities_body(&state);
    Ok(etagged(&headers, &body))
}

/// The `capabilities` payload.
#[must_use]
pub fn capabilities_body(state: &ApiState) -> Value {
    let presets = preset_names(state);
    let (formats, providers) = {
        let registry = read_registry(state);
        let merged = registry.merged_catalog();
        let formats: Vec<Value> = merged
            .download_types
            .iter()
            .flat_map(|dt| {
                dt.formats.iter().map(move |f| {
                    json!({
                        "id": f.id,
                        "text": f.label,
                        "download_type": dt.id,
                        "qualities": f.qualities.iter()
                            .map(|q| json!({ "id": q.id, "text": q.label }))
                            .collect::<Vec<_>>(),
                    })
                })
            })
            .collect();
        (formats, provider_summaries(&registry))
    };
    let (default_download_type, default_format, default_quality) =
        super::downloads::advertised_defaults(state);

    json!({
        "version": state.info.version,
        "yt_dlp": state.info.yt_dlp,
        "url_prefix": state.cfg.url_prefix,
        "boot_id": state.hub.boot_id(),
        "protocol": {
            "v2": true,
            "v1_shim": state.cfg.v1_enabled,
            "socketio": false,
            "ws_path": "ws",
            "ws_subprotocol": crate::WS_SUBPROTOCOL,
            "batch_ms": state.cfg.ws_batch_ms,
            "urgent_ms": state.cfg.ws_urgent_ms,
            "delta_semantics": "absent-key-means-unchanged",
        },
        "features": FEATURES,
        "actions": Action::ALL.map(Action::as_str),
        "formats": formats,
        "download_types": DOWNLOAD_TYPES,
        "codecs": CODECS,
        "subtitle_modes": SUBTITLE_MODES,
        "presets": presets,
        "providers": providers,
        "config": {
            "custom_dirs": state.cfg.custom_dirs,
            "create_custom_dirs": state.cfg.create_custom_dirs,
            "allow_ytdl_options_overrides": state.cfg.allow_ytdl_options_overrides,
            "default_option_playlist_item_limit": state.cfg.default_option_playlist_item_limit,
            "subscription_default_check_interval": state.cfg.subscription_default_check_interval,
            "output_template_chapter": state.cfg.output_template_chapter,
            "public_host_url": state.cfg.public_host_url,
            "public_host_audio_url": state.cfg.public_host_audio_url,
            "default_theme": state.cfg.default_theme.as_str(),
            "max_concurrent_downloads": state.cfg.max_concurrent_downloads,
            "delete_file_on_trashcan": state.cfg.delete_file_on_trashcan,
            "clear_completed_after": state.cfg.clear_completed_after,
            "default_download_type": default_download_type,
            "default_format": default_format,
            "default_quality": default_quality,
        },
    })
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

/// `?url=` on `catalog` and `resolve-preview`.
#[derive(Debug, Deserialize)]
pub struct UrlQuery {
    /// The URL to answer for. Absent on `catalog` means "the merged catalog".
    pub url: Option<String>,
}

/// `GET api/v2/catalog[?url=]` — the honest, per-URL picker (PROTOCOL §4.6).
pub async fn catalog(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Q(query): Q<UrlQuery>,
) -> Result<Response, ApiError> {
    let presets = preset_names(&state);
    let body = match query.url.as_deref() {
        None => {
            let registry = read_registry(&state);
            let merged = registry.merged_catalog();
            let types = download_types_json(&merged.download_types, &presets);
            catalog_body(MERGED_PROVIDER, merged.naming, None, None, &types)
        }
        Some(raw) => {
            let url = url::Url::parse(raw.trim())
                .map_err(|e| ApiError::invalid("url", format!("url is not a valid URL: {e}")))?;
            let registry = read_registry(&state);
            match registry.pick(&url, None) {
                Some(selected) => {
                    let runner_up = selected
                        .runner_up
                        .as_ref()
                        .map(|(id, score)| json!({ "provider": id, "score": score }));
                    let catalog = registry
                        .by_id(&selected.id)
                        .map(|p| p.catalog())
                        .ok_or_else(|| ApiError::internal("the selected provider vanished"))?;
                    let types = download_types_json(&catalog.download_types, &presets);
                    catalog_body(
                        selected.id.as_str(),
                        catalog.naming,
                        Some((selected.score, selected.reason)),
                        runner_up.as_ref(),
                        &types,
                    )
                }
                // Nothing matched: the merged catalog with `match: null`, so a client still has a
                // picker to render and can tell that the URL is not claimed by anyone
                // (docs/INTEGRATION-NOTES.md, WP-03).
                None => {
                    let merged = registry.merged_catalog();
                    let types = download_types_json(&merged.download_types, &presets);
                    catalog_body(MERGED_PROVIDER, merged.naming, None, None, &types)
                }
            }
        }
    };
    // PROTOCOL §4.6: the body's `etag` *is* the header, so the header is read back off the body
    // rather than recomputed over a payload that now contains it.
    let hash = body
        .get("etag")
        .and_then(Value::as_str)
        .map_or_else(|| hash_of(&body), ToOwned::to_owned);
    Ok(etagged_with(&headers, &body, &hash))
}

/// Assembles one catalog response, `etag` included in the body as PROTOCOL §4.6 shows.
fn catalog_body(
    provider: &str,
    naming: NamingPolicy,
    matched: Option<(u8, MatchReason)>,
    runner_up: Option<&Value>,
    download_types: &[Value],
) -> Value {
    let mut body = json!({
        "provider": provider,
        "match": matched.map(|(score, reason)| json!({ "score": score, "reason": reason })),
        "runner_up": runner_up,
        "naming": naming,
        "download_types": download_types,
    });
    // Hashed **before** `etag` is inserted, and that same hash is what the header carries — see
    // `etagged_with`. A client may therefore cache on the body's `etag` and send it back as
    // `If-None-Match`, which is what PROTOCOL §4.6 tells it to do.
    let etag = hash_of(&body);
    if let Some(object) = body.as_object_mut() {
        object.insert("etag".to_owned(), json!(etag));
    }
    body
}

/// The `download_types` array, with `ytdl_options_presets.choices` filled in from the operator's
/// configuration (`docs/INTEGRATION-NOTES.md`, WP-02: the catalog deliberately leaves it empty).
fn download_types_json(types: &[aulos_core::DownloadTypeSpec], presets: &[String]) -> Vec<Value> {
    types
        .iter()
        .map(|dt| {
            let options: Vec<Value> = dt
                .options
                .iter()
                .map(|option| option_json(option, presets))
                .collect();
            json!({
                "id": dt.id,
                "label": dt.label,
                "default_format": dt.default_format,
                "formats": dt.formats,
                "options": options,
            })
        })
        .collect()
}

/// One [`OptionSpec`], with the preset picker's choices resolved.
fn option_json(option: &OptionSpec, presets: &[String]) -> Value {
    if &*option.id != "ytdl_options_presets" {
        return json!(option);
    }
    let choices: Vec<Choice> = presets
        .iter()
        .map(|name| Choice {
            id: name.clone().into_boxed_str(),
            label: name.clone().into_boxed_str(),
        })
        .collect();
    json!(OptionSpec {
        choices,
        ..option.clone()
    })
}

// ---------------------------------------------------------------------------
// presets, providers, plugins, resolve-preview
// ---------------------------------------------------------------------------

/// `GET api/v2/presets`.
pub async fn presets(State(state): State<ApiState>) -> Json<Value> {
    Json(json!({ "presets": preset_names(&state) }))
}

/// `GET api/v2/providers` (PROTOCOL §4.7).
pub async fn providers(State(state): State<ApiState>) -> Json<Value> {
    let registry = read_registry(&state);
    let warnings: Vec<&str> = registry.command_warnings().iter().map(|w| &**w).collect();
    let list: Vec<Value> = registry
        .iter()
        .map(|(id, provider, provider_state)| {
            json!({
                "id": id,
                "state": state_label(provider_state),
                "reason": provider_state.reason(),
                "fallback": is_fallback(provider.as_ref()),
                // The `Provider` trait exposes no version, no declared capability list and no
                // argv (DESIGN §6.1), so these three are `null`/`[]` rather than invented. An
                // additive `Provider::describe()` would fill them; recorded in
                // docs/INTEGRATION-NOTES.md.
                "version": Value::Null,
                "capabilities": Vec::<String>::new(),
                "limits": { "slots": provider.own_slots() },
                "argv": Value::Null,
            })
        })
        .collect();
    // The non-fatal manifest problems of the last plugin scan, next to the fatal ones a
    // `plugins/reload` reports as `failed` — clamps and auto-anchors a plugin author needs to see
    // (the WP-14 request in `docs/INTEGRATION-NOTES.md`). Keyed by directory, because a
    // hook-only manifest can warn without registering a provider at all.
    Json(json!({ "providers": list, "warnings": warnings }))
}

/// `POST api/v2/plugins/reload` — rescan `AULOS_PLUGINS_DIR` (PROTOCOL §4.7, §5.9).
///
/// The report is broadcast as a `providers` frame as well as returned, because a client that is
/// holding a cached `capabilities`/`catalog` needs to know to refetch (PROTOCOL §5.9).
pub async fn plugins_reload(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<ReloadReport>, ApiError> {
    // Nothing in the body is read, but the content-type gate is what DESIGN §16.6 relies on to
    // keep a cross-origin form POST off every mutating v2 route — a reload is cheap, not free.
    super::optional_json_body(&headers, &body)?;
    let dir = state.cfg.plugins_dir.clone();
    let report = {
        let mut registry = match state.registry.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        registry.reload_commands(&dir)
    };
    if !report.is_empty() {
        state.hub.publish(FrameKind::Providers, &report);
    }
    Ok(Json(report))
}

/// `GET api/v2/resolve-preview?url=` (PROTOCOL §4.7).
pub async fn resolve_preview(
    State(state): State<ApiState>,
    Q(query): Q<UrlQuery>,
) -> Result<Json<Value>, ApiError> {
    let raw = query
        .url
        .as_deref()
        .ok_or_else(|| ApiError::invalid("url", "url is required"))?;
    let url = url::Url::parse(raw.trim())
        .map_err(|e| ApiError::invalid("url", format!("url is not a valid URL: {e}")))?;
    let registry = read_registry(&state);
    let selected = registry.pick(&url, None).ok_or_else(|| {
        ApiError::new(
            ErrorCode::UnsupportedUrl,
            format!("Unsupported resource \"{}\"", raw.trim()),
            Some("url"),
        )
    })?;
    Ok(Json(json!({
        "provider": selected.id,
        "score": selected.score,
        "reason": selected.reason,
        "state": state_label(&selected.state),
        "runner_up": selected.runner_up.as_ref()
            .map(|(id, score)| json!({ "provider": id, "score": score })),
    })))
}

// ---------------------------------------------------------------------------
// custom dirs
// ---------------------------------------------------------------------------

/// `GET api/v2/custom-dirs` — `404` when `CUSTOM_DIRS` is off (PROTOCOL §4.7).
///
/// The walk is bounded by `AULOS_CUSTOM_DIRS_MAX_DEPTH`, filtered by `CUSTOM_DIRS_EXCLUDE_REGEX`,
/// run off the runtime with [`tokio::task::spawn_blocking`], and cached for ten seconds — a deep
/// download tree on a spinning disk must not be able to stall the event loop or be re-walked once
/// per app foregrounding.
pub async fn custom_dirs(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    if !state.cfg.custom_dirs {
        return Err(ApiError::new(
            ErrorCode::NotFound,
            "CUSTOM_DIRS is not enabled",
            None,
        ));
    }
    let now = state.now_ms();
    if let Some(cached) = state
        .live
        .dirs
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .filter(|c| now.saturating_sub(c.at) < DIRS_TTL_MS)
    {
        return Ok(Json((*cached.value).clone()));
    }

    let cfg = Arc::clone(&state.cfg);
    let value = tokio::task::spawn_blocking(move || {
        json!({
            "download_dir": walk_dirs(&cfg, &cfg.paths.download),
            "audio_download_dir": walk_dirs(&cfg, &cfg.paths.audio_download),
        })
    })
    .await
    .map_err(|e| ApiError::internal(format!("the directory walk failed: {e}")))?;

    if let Ok(mut guard) = state.live.dirs.lock() {
        *guard = Some(DirsCache {
            at: now,
            value: Arc::new(value.clone()),
        });
    }
    Ok(Json(value))
}

/// Every directory under `root`, relative, `""` first — the legacy projection.
fn walk_dirs(cfg: &Config, root: &FsPath) -> Vec<String> {
    let mut out = vec![String::new()];
    let max_depth = cfg.custom_dirs_max_depth as usize;
    let mut queue: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        if depth >= max_depth {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `is_dir` follows symlinks, which is what an operator expects of a curated download
            // tree; the containment check on `folder` is what keeps a *request* inside the root.
            if !path.is_dir() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let name = relative.to_string_lossy().replace('\\', "/");
            if let Some(exclude) = &cfg.custom_dirs_exclude_regex
                && exclude.is_match(&name)
            {
                continue;
            }
            out.push(name);
            queue.push((path, depth + 1));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// the importer's report
// ---------------------------------------------------------------------------

/// `GET api/v2/import-report` — `404` when nothing was imported (PROTOCOL §4.7, DESIGN §7.6.6).
pub async fn import_report(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let report = aulos_store::import::stored_report(&state.store)
        .await?
        .ok_or_else(|| ApiError::not_found("this database was not imported from legacy state"))?;
    Ok(Json(serde_json::to_value(report).map_err(|e| {
        ApiError::internal(format!("the import report is not serialisable: {e}"))
    })?))
}

// ---------------------------------------------------------------------------
// yt-dlp options
// ---------------------------------------------------------------------------

/// `GET api/v2/ytdl-options` — the reload status, the key set and the preset names, values
/// redacted (PROTOCOL §4.7).
pub async fn ytdl_options(State(state): State<ApiState>) -> Json<Value> {
    let options = state.ytdl.load();
    let mut body = ytdl_options_block(&state);
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "keys".to_owned(),
            json!(options.base.keys().collect::<Vec<_>>()),
        );
        object.insert(
            "presets".to_owned(),
            json!(options.presets.keys().collect::<Vec<_>>()),
        );
    }
    Json(body)
}

/// `POST api/v2/ytdl-options/reload` — re-read `YTDL_OPTIONS_FILE` now (PROTOCOL §4.7, §5.9).
///
/// Always `200`: a broken options file is reported as `{"ok": false, "msg": …}` with the legacy
/// error string, exactly as the `ytdl_options` frame does, and the last-good options stay in
/// force. Runtime overrides (today only `cookiefile`) are re-applied, as in legacy.
pub async fn ytdl_options_reload(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    super::optional_json_body(&headers, &body)?;
    let cfg = Arc::clone(&state.cfg);
    let loaded = tokio::task::spawn_blocking(move || {
        YtdlOptions::load(
            &cfg.ytdl_options,
            cfg.ytdl_options_file.as_deref(),
            &cfg.ytdl_options_presets,
            cfg.ytdl_options_presets_file.as_deref(),
        )
    })
    .await;

    let (ok, msg, update_time) = match loaded {
        Ok(Ok(mut fresh)) => {
            let previous = state.ytdl.load_full();
            fresh.inherit_overrides(&previous);
            let update_time = fresh.file_mtime;
            state.ytdl.store(Arc::new(fresh));
            (true, String::new(), update_time)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "the YTDL_OPTIONS reload failed; keeping the last good set");
            (false, e.to_string(), state.ytdl.load().file_mtime)
        }
        Err(e) => (
            false,
            format!("the reload task failed: {e}"),
            state.ytdl.load().file_mtime,
        ),
    };

    if let Ok(mut guard) = state.live.options_status.lock() {
        *guard = Some((ok, msg.clone().into_boxed_str()));
    }
    let body = json!({ "ok": ok, "msg": msg, "update_time": update_time });
    state.hub.publish(FrameKind::YtdlOptions, &body);
    Ok(Json(body))
}

/// `?item_id=` on `debug/options`.
#[derive(Debug, Deserialize)]
pub struct DebugQuery {
    /// Layer this item's request over the effective options.
    pub item_id: Option<String>,
}

/// `GET api/v2/debug/options` — the merged yt-dlp dict, per-key source labels included
/// (PROTOCOL §4.7).
///
/// This is the route that answers "why did my `YTDL_OPTIONS` not take effect?". The labels are
/// derived by replaying the layering: the environment value alone, then the file, then each named
/// preset in order, then the request's own overrides, then the runtime overrides the server itself
/// installed (`cookiefile` after a cookie upload).
pub async fn debug_options(
    State(state): State<ApiState>,
    Q(query): Q<DebugQuery>,
) -> Result<Json<Value>, ApiError> {
    let options = state.ytdl.load_full();
    let (presets, overrides, item) = match query.item_id.as_deref() {
        None => (Vec::new(), Map::new(), Value::Null),
        Some(raw) => {
            let view = super::query::lookup(&state, raw).await?;
            let presets: Vec<Box<str>> = view
                .request
                .ytdl_options_presets
                .iter()
                .map(|p| Box::from(&**p))
                .collect();
            (
                presets,
                (*view.request.ytdl_options_overrides).clone(),
                json!(view.id),
            )
        }
    };

    let env_only =
        aulos_core::ytdl_options::load_options(&state.cfg.ytdl_options, None).unwrap_or_default();
    let mut sources: Map<String, Value> = Map::new();
    for key in options.base.keys() {
        let label = if env_only.get(key) == options.base.get(key) {
            "env"
        } else {
            "file"
        };
        sources.insert(key.clone(), json!(label));
    }
    for name in &presets {
        if let Some(preset) = options.presets.get(&**name) {
            for key in preset.keys() {
                sources.insert(key.clone(), json!(format!("preset:{name}")));
            }
        }
    }
    for key in overrides.keys() {
        sources.insert(key.clone(), json!("request"));
    }
    for key in options.overrides.keys() {
        sources.insert(key.clone(), json!("aulos"));
    }

    let merged = options.layer(&presets, &overrides);
    let annotated: Map<String, Value> = merged
        .iter()
        .map(|(key, value)| {
            let source = sources.get(key).cloned().unwrap_or(json!("env"));
            let shown = if aulos_core::is_secret_key(key) {
                json!(aulos_core::REDACTED)
            } else {
                value.clone()
            };
            (key.clone(), json!({ "value": shown, "source": source }))
        })
        .collect();

    Ok(Json(json!({
        "item_id": item,
        "presets": presets,
        "options": annotated,
    })))
}

// ---------------------------------------------------------------------------
// blocks shared with the snapshot and with healthz
// ---------------------------------------------------------------------------

/// The `ytdl_options` block of a snapshot: `{ok, msg, update_time}` (PROTOCOL §5.3, §5.9).
///
/// `ok`/`msg` come from the `ytdl_options` health component when the binary maintains one
/// (DESIGN §16.3), else from this process's own last reload, else the honest default of a server
/// whose options loaded at boot and have not been touched since.
#[must_use]
pub fn ytdl_options_block(state: &ApiState) -> Value {
    let update_time = state.ytdl.load().file_mtime;
    let component = state
        .health
        .snapshot()
        .components
        .get("ytdl_options")
        .cloned();
    if let Some(component) = component {
        let ok = matches!(
            component.status,
            ComponentStatus::Ok | ComponentStatus::Disabled
        );
        let msg = component
            .detail
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        return json!({ "ok": ok, "msg": msg, "update_time": update_time });
    }
    let (ok, msg) = state
        .live
        .options_status
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .map_or_else(|| (true, String::new()), |(ok, msg)| (ok, msg.to_string()));
    json!({ "ok": ok, "msg": msg, "update_time": update_time })
}

/// The abridged `health` block of a snapshot: a roll-up plus a flat component map
/// (PROTOCOL §5.3).
#[must_use]
pub fn health_block(state: &ApiState) -> Value {
    let view = state.health.snapshot();
    let components: Map<String, Value> = view
        .components
        .iter()
        .map(|(name, component)| (name.clone(), json!(component.status.as_str())))
        .collect();
    json!({ "status": view.status.as_str(), "components": components })
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The registry, never poisoned: a panic while holding the read lock must not take the API down.
pub(crate) fn read_registry(state: &ApiState) -> std::sync::RwLockReadGuard<'_, Registry> {
    match state.registry.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Every configured preset name, sorted.
#[must_use]
pub fn preset_names(state: &ApiState) -> Vec<String> {
    let options = state.ytdl.load();
    let names: BTreeSet<String> = options.presets.keys().cloned().collect();
    names.into_iter().collect()
}

/// `"ready"` or `"degraded"` — the two states PROTOCOL shows.
fn state_label(state: &ProviderState) -> &'static str {
    match state {
        ProviderState::Ready => "ready",
        ProviderState::Degraded { .. } => "degraded",
    }
}

/// Whether this provider is the catch-all.
///
/// Derived rather than hard-coded on the id: a provider that answers [`Match::Weak`] to a host it
/// has never heard of *is* the fallback, which is exactly what DESIGN §6.3 defines. The probe URL
/// is deliberately in the reserved `.invalid` TLD, so no real provider can claim it.
fn is_fallback(provider: &dyn aulos_provider::Provider) -> bool {
    let Ok(probe) = url::Url::parse("https://probe.invalid/watch?v=x") else {
        return false;
    };
    matches!(provider.matches(&probe), Match::Weak(_))
}

/// A short, stable hash of a payload, for `ETag`.
fn hash_of(body: &Value) -> String {
    let text = serde_json::to_string(body).unwrap_or_default();
    let digest = Sha256::digest(text.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// Serves a payload with a content hash as its `ETag`, answering `304` when the client has it.
fn etagged(headers: &HeaderMap, body: &Value) -> Response {
    etagged_with(headers, body, &hash_of(body))
}

/// [`etagged`] with the hash supplied.
///
/// The catalog needs this: PROTOCOL §4.6 says its body's `etag` field is "also sent as the `ETag`
/// header", and the field is part of the payload, so hashing the payload a second time here would
/// hash the field too and the two could never be equal. One hash, taken over the etag-less body,
/// serves both.
fn etagged_with(headers: &HeaderMap, body: &Value, hash: &str) -> Response {
    let tag = format!("\"{hash}\"");
    let value = HeaderValue::from_str(&tag).unwrap_or(HeaderValue::from_static("\"0\""));
    let matched = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .any(|candidate| candidate == value || candidate == "*");
    if matched {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, value);
        return response;
    }
    let mut response = Json(body).into_response();
    response.headers_mut().insert(header::ETAG, value);
    response
}

/// One line per registered provider, for `capabilities.providers` and for `healthz.providers`.
#[must_use]
pub fn capabilities_providers(state: &ApiState) -> Vec<Value> {
    let registry = read_registry(state);
    provider_summaries(&registry)
}

/// One line per registered provider, for `capabilities.providers`.
fn provider_summaries(registry: &Registry) -> Vec<Value> {
    registry
        .iter()
        .map(|(id, provider, provider_state)| {
            json!({
                "id": id,
                "state": state_label(provider_state),
                "reason": provider_state.reason(),
                "fallback": is_fallback(provider.as_ref()),
                "slots": provider.own_slots(),
            })
        })
        .collect()
}

/// The `ProviderId` of the catch-all provider, when there is one.
#[must_use]
pub fn fallback_provider(registry: &Registry) -> Option<ProviderId> {
    registry
        .iter()
        .find(|(_, provider, _)| is_fallback(provider.as_ref()))
        .map(|(id, _, _)| id.clone())
}

/// The deep-probe rate limit of DESIGN §16.3: at most one live probe per ten seconds.
#[must_use]
pub fn claim_deep_probe(state: &ApiState) -> bool {
    let now = state.now_ms();
    let last = state.live.deep_probe_at.load(Ordering::Relaxed);
    if now.saturating_sub(last) < DEEP_PROBE_INTERVAL_MS {
        return false;
    }
    state.live.deep_probe_at.store(now, Ordering::Relaxed);
    true
}

/// Every registered provider's live [`aulos_provider::ProviderHealth`], for `?probe=deep`.
pub async fn probe_providers(
    state: &ApiState,
) -> Vec<(ProviderId, aulos_provider::ProviderHealth)> {
    let providers: Vec<(ProviderId, Arc<dyn aulos_provider::Provider>)> = {
        let registry = read_registry(state);
        registry
            .iter()
            .map(|(id, provider, _)| (id.clone(), Arc::clone(provider)))
            .collect()
    };
    let mut out = Vec::with_capacity(providers.len());
    for (id, provider) in providers {
        out.push((id, provider.probe().await));
    }
    out
}

/// The catalog of the provider that would be selected for `url`, for the v1 shim and the bot.
#[must_use]
pub fn catalog_for(state: &ApiState, url: &url::Url) -> Option<Arc<FormatCatalog>> {
    let registry = read_registry(state);
    registry.catalog_for(url).map(|(_, catalog, _)| catalog)
}

/// The download type ids a catalog advertises, for a caller that wants to validate one.
#[must_use]
pub fn download_type_ids(catalog: &FormatCatalog) -> Vec<&str> {
    catalog.download_types.iter().map(|d| &*d.id).collect()
}

/// `DownloadType` from a wire string, for the v1 shim.
#[must_use]
pub fn download_type(raw: &str) -> Option<DownloadType> {
    DownloadType::from_str_exact(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertised_vocabularies_are_the_protocol_order() {
        assert_eq!(DOWNLOAD_TYPES, ["video", "audio", "captions", "thumbnail"]);
        assert_eq!(CODECS, ["auto", "h264", "h265", "av1", "vp9"]);
        assert_eq!(
            SUBTITLE_MODES,
            ["auto_only", "manual_only", "prefer_manual", "prefer_auto"]
        );
        assert_eq!(FEATURES.len(), 14);
        assert!(FEATURES.contains(&"cancel_resolve"));
    }

    #[test]
    fn the_hash_is_stable_and_short() {
        let a = hash_of(&json!({ "a": 1 }));
        let b = hash_of(&json!({ "a": 1 }));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert_ne!(a, hash_of(&json!({ "a": 2 })));
    }

    #[test]
    fn the_preset_option_is_the_only_one_whose_choices_are_injected() {
        let presets = vec!["sponsorblock".to_owned(), "archive".to_owned()];
        let catalog = aulos_core::ytdlp_catalog();
        let video = catalog
            .spec_for(DownloadType::Video)
            .expect("the ytdlp catalog has a video type");
        let rendered = download_types_json(&catalog.download_types, &presets);
        let options = rendered[0]["options"]
            .as_array()
            .expect("an options array")
            .clone();
        assert_eq!(options.len(), video.options.len());
        for option in options {
            let id = option["id"].as_str().unwrap_or_default();
            let choices = option["choices"].as_array().expect("always an array");
            if id == "ytdl_options_presets" {
                assert_eq!(choices.len(), 2, "filled from the operator's config");
                assert_eq!(choices[0]["id"], "sponsorblock");
            }
        }
    }
}
