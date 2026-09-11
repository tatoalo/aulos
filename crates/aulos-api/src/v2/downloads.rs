//! `POST api/v2/downloads` and `POST api/v2/downloads/cancel-resolve` (PROTOCOL §4.1, §4.7).
//!
//! The add returns **before** any metadata extraction: the handler translates the wire body into
//! [`DownloadRequest`]s and hands them to [`aulos_queue::EngineHandle::add`], which validates,
//! inserts as `resolving`, acks, and only then starts resolving (DESIGN §8.3). By the time the
//! `202` reaches the client the item is in the queue and an `added` frame is on its way.
//!
//! Validation is deliberately **not** duplicated here. The engine owns the catalog check, the
//! folder resolve and containment, the preset and overrides gates and the dedupe lookup, and
//! reports each failure as a [`aulos_core::WireError`] with its `field` already set, so this
//! module's job is to parse types, fill defaults, collect `warnings` — and map
//! [`AddError`] onto a status.

use std::sync::Arc;

use aulos_core::{
    Codec, Config, DownloadRequest, DownloadType, ErrorCode, FormatId, ItemId, ProviderId,
    QualityId, RelDir, Selection, SourceKind, SourceRef, SubtitleLang, SubtitleMode,
};
use aulos_provider::Registry;
use aulos_queue::{AddError, CancelScope};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Map, Value, json};
use url::Url;

use crate::ApiState;
use crate::error::{ApiError, Json};
use crate::v2::{json_body, parse_bool, parse_str, parse_u32, unknown_fields};

/// Every key one add request may carry (PROTOCOL §4.1). Anything else is a `warnings` entry.
pub const REQUEST_FIELDS: [&str; 16] = [
    "url",
    "download_type",
    "codec",
    "format",
    "quality",
    "folder",
    "custom_name_prefix",
    "playlist_item_limit",
    "auto_start",
    "split_by_chapters",
    "chapter_template",
    "subtitle_language",
    "subtitle_mode",
    "ytdl_options_presets",
    "ytdl_options_overrides",
    "provider",
];

/// The two keys the batch envelope carries.
pub const BATCH_FIELDS: [&str; 2] = ["items", "defaults"];

/// `X-Aulos-Client` — a client naming itself so its adds are attributed to it (PROTOCOL §1.3).
///
/// The value is `<client>/<version>`; the iOS app sends `ios/<version>`. Only the token before the
/// first `/` is interpreted, so a client is free to put a build number or a platform string after
/// it without ever changing what the server does with the header.
pub const CLIENT_HEADER: HeaderName = HeaderName::from_static("x-aulos-client");

/// `X-Aulos-Install` — *which installation* of that client is calling (PROTOCOL §1.3).
///
/// `X-Aulos-Client` says "the iOS app"; this says "the iPhone, not the iPad". The app mints one
/// opaque id per install and sends it on every request, which is what lets a completion alert go
/// to the device the download was started from instead of to every phone in the household
/// (DESIGN §25.2, decision 42).
pub const INSTALL_HEADER: HeaderName = HeaderName::from_static("x-aulos-install");

/// The shortest install id accepted. Long enough that two households' ids colliding is not a thing
/// to reason about, short enough that a hyphen-less UUID and a ULID both fit the range.
const INSTALL_ID_MIN: usize = 8;

/// The longest. Above any identifier a client has reason to mint, and low enough that the column
/// it lands in stays sane.
const INSTALL_ID_MAX: usize = 64;

/// The `X-Aulos-Install` value, when it is one this server will key on (PROTOCOL §1.3).
///
/// 8 to 64 characters of `[A-Za-z0-9._-]`, surrounding whitespace trimmed. **Anything else is
/// treated as absent, never as an error** — exactly like [`source_for_client`], a header a client
/// got wrong must not turn a good add into a `400`. The caller then does what it would have done
/// without the header, which on the add path is the same bare `SourceRef` an older app build has
/// always produced.
///
/// The one place the same rule *is* a `400` is `install_id` in the `PUT api/v2/devices/{token}`
/// body (PROTOCOL §4.8), which calls this and rejects a `None`: a registration is a field the app
/// chose to send, not a header a proxy might have mangled, and a device stored under a wrong
/// install would silently never be alerted.
#[must_use]
pub fn install_id(header: Option<&str>) -> Option<&str> {
    let raw = header?.trim();
    if !(INSTALL_ID_MIN..=INSTALL_ID_MAX).contains(&raw.len()) {
        return None;
    }
    raw.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        .then_some(raw)
}

/// Which origin an add is attributed to, given the `X-Aulos-Client` value (PROTOCOL §1.3).
///
/// This exists so notifications can route by origin (DESIGN §12.6, §25): a video added from the
/// iOS app should buzz the phone and a video added from Telegram should not, and the only thing
/// that distinguishes the two at add time is who is calling. An unknown client — or none — is
/// `api_v2`, so a client that never heard of the header keeps working exactly as before and a
/// typo in it can never turn a good add into a failure.
#[must_use]
pub fn source_for_client(header: Option<&str>) -> SourceKind {
    let client = header
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .trim();
    if client.eq_ignore_ascii_case("ios") {
        SourceKind::Ios
    } else {
        SourceKind::ApiV2
    }
}

/// `POST api/v2/downloads` — the async add.
pub async fn add(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::response::Response, ApiError> {
    let root = json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();

    let requests = parse_batch(&state, &root, &mut warnings)?;
    if requests.is_empty() {
        return Err(ApiError::invalid("items", "at least one url is required"));
    }

    // One add path serves both the single and the batch body (`parse_batch` flattens them), so the
    // attribution is read once, here, and covers both.
    //
    // `X-Aulos-Install` refines the iOS origin and only the iOS origin: `source.ref` is *the key*
    // for the kind (DESIGN §4.4) — a chat id for Telegram, a subscription id for a check — and for
    // `ios` it is the install the add came from. Every other kind keeps the bare source it had, so
    // a browser that sends the header changes nothing.
    let source = match (
        source_for_client(headers.get(CLIENT_HEADER).and_then(|v| v.to_str().ok())),
        install_id(headers.get(INSTALL_HEADER).and_then(|v| v.to_str().ok())),
    ) {
        (SourceKind::Ios, Some(install)) => SourceRef::with_ref(SourceKind::Ios, install),
        (kind, _) => SourceRef::bare(kind),
    };
    let outcome = state
        .engine
        .add(requests, source)
        .await
        .map_err(map_add_error)?;

    let duplicates: Vec<Value> = outcome
        .duplicates
        .iter()
        .map(|d| json!({ "url": d.url, "existing_id": d.existing_id }))
        .collect();

    // PROTOCOL §4.1 types `id` as a non-nullable string, and §0 rule 2 tells client authors to
    // declare it non-optional. With the default `AULOS_DEDUPE_MODE=active` an add whose every URL
    // matched a live item mints no ids at all, so `ids.first()` alone would serialise `null` and
    // make the documented "a duplicate is not an error" path a decode failure on the client. The
    // existing item's id is the useful answer there — it is what DESIGN §8.3 means by "202 with
    // the existing id", and it gives the caller something to poll.
    let id = outcome
        .ids
        .first()
        .copied()
        .or_else(|| outcome.duplicates.first().map(|d| d.existing_id));

    let payload = json!({
        "id": id,
        "ids": outcome.ids,
        "generation": outcome.generation,
        "seq": state.seq(),
        "duplicates": duplicates,
        "warnings": warnings,
    });
    Ok((StatusCode::ACCEPTED, Json(payload)).into_response())
}

/// `POST api/v2/downloads/cancel-resolve` — abort an add that is still resolving (PROTOCOL §4.7).
///
/// `{}` (or an absent/`null` `generation`) cancels **every** in-flight resolution, which is what
/// the legacy `cancel-add` did; `{"generation": n}` — the value from that add's `202` — cancels
/// only that add's resolution and its not-yet-created children.
pub async fn cancel_resolve(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let root = json_body(&headers, &body)?;
    let mut warnings: Vec<String> = Vec::new();
    unknown_fields(&root, &["generation"], &mut warnings);

    let generation = match root.get("generation") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            ApiError::invalid("generation", "generation must be a non-negative integer")
        })?),
    };
    let scope = generation.map_or(CancelScope::All, CancelScope::Generation);
    let result = state.engine.cancel_resolve(scope).await;

    Ok(Json(json!({
        "canceled": result.applied.len(),
        "generation": generation,
        "seq": state.seq(),
        "warnings": warnings,
    })))
}

/// Maps [`AddError`] onto the DESIGN §8.3 status table.
fn map_add_error(err: AddError) -> ApiError {
    match err {
        AddError::Invalid { errors, .. } => errors.into_iter().next().map_or_else(
            || ApiError::of(ErrorCode::ValidationFailed, "the request is invalid"),
            ApiError::from,
        ),
        AddError::Duplicate { existing_id, .. } => ApiError::of(
            ErrorCode::Conflict,
            format!("this url already has a live item ({existing_id})"),
        ),
        AddError::TooManyUrls { max, got } => ApiError::of(
            ErrorCode::PayloadTooLarge,
            format!("{got} urls exceeds the {max} per-batch limit"),
        ),
        AddError::Unavailable(message) => ApiError::unavailable(message),
    }
}

/// Parses either body shape into one request list.
///
/// A body with `items` is the batch form and `defaults` is merged **under** each entry, so a share
/// sheet can send three URLs with one selection. `defaults` is the *only* shared layer: a request
/// field at the top level of a batch body is an unknown field there, and is reported as one.
fn parse_batch(
    state: &ApiState,
    root: &Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<Vec<DownloadRequest>, ApiError> {
    let empty = Map::new();
    match root.get("items") {
        Some(Value::Array(items)) => {
            // PROTOCOL §4.1 gives the batch envelope exactly two keys, and `defaults` is the
            // only place a shared selection may go. A top-level `format`/`quality`/… is therefore
            // an unknown field and gets the §4.1 `warnings` entry: it is *not* applied, and the
            // one thing that rule exists to prevent is dropping the value silently, which is what
            // adding `REQUEST_FIELDS` to this list did.
            unknown_fields(root, &BATCH_FIELDS, warnings);
            let defaults = match root.get("defaults") {
                None | Some(Value::Null) => empty.clone(),
                Some(Value::Object(map)) => map.clone(),
                Some(_) => {
                    return Err(ApiError::invalid("defaults", "defaults must be an object"));
                }
            };
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let object = item.as_object().ok_or_else(|| {
                    ApiError::invalid("items", format!("items[{index}] must be an object"))
                })?;
                unknown_fields(object, &REQUEST_FIELDS, warnings);
                out.push(parse_request(state, object, &defaults)?);
            }
            Ok(out)
        }
        Some(_) => Err(ApiError::invalid("items", "items must be an array")),
        None => {
            unknown_fields(root, &REQUEST_FIELDS, warnings);
            Ok(vec![parse_request(state, root, &empty)?])
        }
    }
}

/// Parses one wire object as an add request, with no `defaults` layer.
///
/// This is what `POST api/v2/subscriptions` uses: PROTOCOL §4.7 says it "takes the same body as an
/// add plus `check_interval_minutes`", and sharing the parser is what keeps that true.
///
/// # Errors
/// Every failure `POST api/v2/downloads` can produce for a single request.
pub fn parse_one(
    state: &ApiState,
    entry: &Map<String, Value>,
) -> Result<DownloadRequest, ApiError> {
    parse_request(state, entry, &Map::new())
}

/// One field, from the entry or from `defaults`.
fn field<'a>(
    entry: &'a Map<String, Value>,
    defaults: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Value> {
    entry
        .get(key)
        .or_else(|| defaults.get(key))
        .filter(|v| !v.is_null())
}

/// Translates one wire object into a [`DownloadRequest`], filling every default.
fn parse_request(
    state: &ApiState,
    entry: &Map<String, Value>,
    defaults: &Map<String, Value>,
) -> Result<DownloadRequest, ApiError> {
    let cfg = &state.cfg;
    let raw_url =
        field(entry, defaults, "url").ok_or_else(|| ApiError::invalid("url", "url is required"))?;
    let url = parse_url(cfg, parse_str("url", raw_url)?)?;

    let download_type = match field(entry, defaults, "download_type") {
        Some(value) => DownloadType::from_str_exact(parse_str("download_type", value)?)
            .ok_or_else(|| {
                ApiError::invalid(
                    "download_type",
                    aulos_core::request::legacy::download_type_legacy(),
                )
            })?,
        None => DownloadType::Video,
    };

    let mut codec = match field(entry, defaults, "codec") {
        Some(value) => Codec::from_str_exact(parse_str("codec", value)?).ok_or_else(|| {
            ApiError::invalid("codec", aulos_core::request::legacy::codec_legacy())
        })?,
        None => Codec::Auto,
    };
    // PROTOCOL §4.1: `codec` applies to video only and is forced to `auto` otherwise. The other
    // legacy coercion (`quality → best` for captions and thumbnails) is deliberately **not**
    // applied: a v2 client that asks for a quality the catalog does not have gets an honest 400,
    // and only the v1 shim keeps the silent fix-up.
    if download_type != DownloadType::Video {
        codec = Codec::Auto;
    }

    let (default_format, default_quality) = catalog_defaults(state, &url, download_type);
    let format = match field(entry, defaults, "format") {
        Some(value) => parse_id("format", parse_str("format", value)?)?,
        None => default_format,
    };
    let quality = match field(entry, defaults, "quality") {
        Some(value) => parse_quality("quality", parse_str("quality", value)?)?,
        None => default_quality,
    };

    let folder = match field(entry, defaults, "folder") {
        Some(value) => {
            let raw = parse_str("folder", value)?.trim();
            if raw.is_empty() {
                None
            } else {
                Some(RelDir::parse(raw).map_err(|e| {
                    ApiError::new(ErrorCode::FolderInvalid, e.to_string(), Some("folder"))
                })?)
            }
        }
        None => None,
    };

    let custom_name_prefix = match field(entry, defaults, "custom_name_prefix") {
        Some(value) => Box::from(parse_str("custom_name_prefix", value)?),
        None => Box::from(""),
    };

    let playlist_item_limit = match field(entry, defaults, "playlist_item_limit") {
        Some(value) => parse_u32("playlist_item_limit", value)?,
        None => cfg.default_option_playlist_item_limit,
    };

    let auto_start = match field(entry, defaults, "auto_start") {
        Some(value) => parse_bool("auto_start", value)?,
        None => true,
    };

    let split_by_chapters = match field(entry, defaults, "split_by_chapters") {
        Some(value) => parse_bool("split_by_chapters", value)?,
        None => false,
    };

    let chapter_template = match field(entry, defaults, "chapter_template") {
        Some(value) => Box::from(parse_str("chapter_template", value)?),
        None => Box::from(cfg.default_chapter_template()),
    };

    let subtitle_language = match field(entry, defaults, "subtitle_language") {
        Some(value) => {
            SubtitleLang::parse(parse_str("subtitle_language", value)?).map_err(|e| {
                ApiError::new(
                    ErrorCode::ValidationFailed,
                    e.to_string(),
                    Some("subtitle_language"),
                )
            })?
        }
        None => SubtitleLang::english(),
    };

    let subtitle_mode = match field(entry, defaults, "subtitle_mode") {
        Some(value) => SubtitleMode::from_str_exact(parse_str("subtitle_mode", value)?)
            .ok_or_else(|| {
                ApiError::invalid(
                    "subtitle_mode",
                    aulos_core::request::legacy::subtitle_mode_legacy(),
                )
            })?,
        None => SubtitleMode::PreferManual,
    };

    let ytdl_options_presets = match field(entry, defaults, "ytdl_options_presets") {
        Some(Value::Array(list)) => {
            let mut out = Vec::with_capacity(list.len());
            for value in list {
                out.push(Box::from(parse_str("ytdl_options_presets", value)?));
            }
            out
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "ytdl_options_presets",
                "ytdl_options_presets must be an array of preset names",
            ));
        }
        None => Vec::new(),
    };

    let ytdl_options_overrides = match field(entry, defaults, "ytdl_options_overrides") {
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err(ApiError::invalid(
                "ytdl_options_overrides",
                "ytdl_options_overrides must be an object",
            ));
        }
        None => Map::new(),
    };

    let provider_hint = match field(entry, defaults, "provider") {
        Some(value) => Some(
            ProviderId::parse(parse_str("provider", value)?)
                .map_err(|e| ApiError::new(e.code(), e.to_string(), Some("provider")))?,
        ),
        None => None,
    };

    Ok(DownloadRequest {
        url,
        selection: Selection::new(download_type, codec, format, quality),
        folder,
        custom_name_prefix,
        playlist_item_limit,
        auto_start,
        split_by_chapters,
        chapter_template,
        subtitle_language,
        subtitle_mode,
        ytdl_options_presets,
        ytdl_options_overrides,
        provider_hint,
    })
}

/// A URL, trimmed, with a usable scheme, and — when the operator has locked the deployment down —
/// a routable target.
///
/// `unsupported_url` rather than `validation_failed` for a scheme no provider could ever take, so
/// a client can tell "you typed this wrong" from "this server cannot download that".
///
/// The SSRF guard is [`ssrf_guard`]: DESIGN §16.6 and §17.3 promise that the v1/v2 adds run the
/// same validator the Telegram bot does, with `allow_private = AULOS_ALLOW_PRIVATE_TARGETS`.
fn parse_url(cfg: &Config, raw: &str) -> Result<Url, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::invalid("url", "url is required"));
    }
    let url = Url::parse(trimmed)
        .map_err(|e| ApiError::invalid("url", format!("url is not a valid URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::new(
            ErrorCode::UnsupportedUrl,
            format!("Unsupported resource \"{trimmed}\""),
            Some("url"),
        ));
    }
    ssrf_guard(cfg, &url)?;
    Ok(url)
}

/// Runs [`aulos_core::urls::check`] unless `AULOS_ALLOW_PRIVATE_TARGETS` says the operator wants
/// private targets reachable (the default, because a home deployment legitimately downloads from
/// its own LAN).
///
/// The reason string is the bot's, byte-for-byte, so the two surfaces refuse the same URL with the
/// same sentence.
///
/// # Errors
/// `400 validation_failed` naming `url`.
pub fn ssrf_guard(cfg: &Config, url: &Url) -> Result<(), ApiError> {
    if cfg.allow_private_targets {
        return Ok(());
    }
    aulos_core::urls::check(url).map_err(|reject| ApiError::invalid("url", reject.to_string()))
}

fn parse_id(field_name: &str, raw: &str) -> Result<FormatId, ApiError> {
    FormatId::parse(raw).map_err(|e| ApiError::new(e.code(), e.to_string(), Some(field_name)))
}

fn parse_quality(field_name: &str, raw: &str) -> Result<QualityId, ApiError> {
    QualityId::parse(raw).map_err(|e| ApiError::new(e.code(), e.to_string(), Some(field_name)))
}

/// The `(format, quality)` a request that named neither gets.
///
/// They come from the catalog of the provider that **would** be selected for this URL, so a
/// StreamingCommunity link defaults to that provider's single rendition rather than to yt-dlp's
/// `mp4`/`best`, and a plugin with its own catalog needs no client release. When nothing matches
/// the URL the merged catalog decides, and if even that is empty the legacy pair is used —
/// the engine will reject the add with `unsupported_url` a moment later either way.
fn catalog_defaults(
    state: &ApiState,
    url: &Url,
    download_type: DownloadType,
) -> (FormatId, QualityId) {
    let picked = {
        let registry = match state.registry.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog_defaults_from(&registry, url, download_type)
    };
    picked.unwrap_or_else(|| (fallback_format(download_type), fallback_quality()))
}

/// The catalog lookup, split out so it needs only a `&Registry`.
fn catalog_defaults_from(
    registry: &Registry,
    url: &Url,
    download_type: DownloadType,
) -> Option<(FormatId, QualityId)> {
    let catalog = registry
        .pick(url, None)
        .and_then(|s| registry.by_id(&s.id).map(|p| p.catalog()));
    let spec = catalog.as_ref().and_then(|c| c.spec_for(download_type));
    let (format_id, quality_id) = match spec {
        Some(dt) => {
            let format = dt.format(&dt.default_format)?;
            (dt.default_format.clone(), format.default_quality.clone())
        }
        None => {
            let merged = registry.merged_catalog();
            let dt = merged
                .download_types
                .iter()
                .find(|d| &*d.id == download_type.as_str())?;
            let format = dt.format(&dt.default_format)?;
            (dt.default_format.clone(), format.default_quality.clone())
        }
    };
    Some((
        FormatId::parse(&format_id).ok()?,
        QualityId::parse(&quality_id).ok()?,
    ))
}

/// The legacy default format per download type, for a registry with no catalog at all.
fn fallback_format(download_type: DownloadType) -> FormatId {
    let id = match download_type {
        DownloadType::Video => "mp4",
        DownloadType::Audio => "m4a",
        DownloadType::Captions => "srt",
        DownloadType::Thumbnail => "jpg",
    };
    FormatId::parse(id).unwrap_or_else(|_| unreachable!("{id} is a valid format id"))
}

/// `best`, the one quality every catalog has.
fn fallback_quality() -> QualityId {
    QualityId::parse("best").unwrap_or_else(|_| unreachable!("\"best\" is a valid quality id"))
}

/// The `config` block of `capabilities`, which advertises the same defaults this module applies
/// (PROTOCOL §4.5).
#[must_use]
pub fn advertised_defaults(state: &ApiState) -> (String, String, String) {
    let url = Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        .unwrap_or_else(|_| unreachable!("a literal URL parses"));
    let (format, quality) = catalog_defaults(state, &url, DownloadType::Video);
    (
        DownloadType::Video.as_str().to_owned(),
        format.as_str().to_owned(),
        quality.as_str().to_owned(),
    )
}

/// Resolves one wire id token to an [`ItemId`].
///
/// v2 has exactly one identifier (PROTOCOL §0 rule 3), so an unparseable token is a `404` rather
/// than the v1 shim's url-or-media-id search.
pub fn parse_item_id(raw: &str) -> Option<ItemId> {
    raw.parse().ok()
}

/// The effective `chapter_template` a request with none of its own gets.
#[must_use]
pub fn default_chapter_template(cfg: &Config) -> Arc<str> {
    Arc::from(cfg.default_chapter_template())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        aulos_core::config::load(&aulos_core::config::RawEnv::from_pairs(pairs.to_vec()))
            .expect("the test config must load")
    }

    /// PROTOCOL §1.3: only the token before the first `/` is interpreted, ASCII-case-insensitively,
    /// and everything the server does not recognise is `api_v2`.
    #[test]
    fn only_a_leading_ios_token_claims_the_ios_origin() {
        assert_eq!(source_for_client(None), SourceKind::ApiV2);
        assert_eq!(source_for_client(Some("")), SourceKind::ApiV2);
        assert_eq!(source_for_client(Some("ios/1.0.0 (5)")), SourceKind::Ios);
        assert_eq!(source_for_client(Some("ios")), SourceKind::Ios);
        assert_eq!(source_for_client(Some("IOS")), SourceKind::Ios);
        assert_eq!(source_for_client(Some(" ios /2")), SourceKind::Ios);
        // A prefix is not a match: `iosx` is some other client.
        assert_eq!(source_for_client(Some("iosx")), SourceKind::ApiV2);
        assert_eq!(source_for_client(Some("android/1")), SourceKind::ApiV2);
    }

    /// PROTOCOL §1.3: 8–64 of `[A-Za-z0-9._-]`, trimmed, and **anything else is absent** — the
    /// header can never be the reason an add fails.
    #[test]
    fn an_install_id_is_eight_to_sixty_four_safe_characters_or_nothing() {
        assert_eq!(install_id(Some("7F3A1B2C")), Some("7F3A1B2C"));
        assert_eq!(install_id(Some("  7F3A1B2C  ")), Some("7F3A1B2C"));
        assert_eq!(
            install_id(Some("3F2504E0-4F89-11D3-9A0C-0305E82C3301")),
            Some("3F2504E0-4F89-11D3-9A0C-0305E82C3301")
        );
        assert_eq!(install_id(Some("a.b_c-d1")), Some("a.b_c-d1"));
        assert_eq!(install_id(Some(&"a".repeat(8))).map(str::len), Some(8));
        assert_eq!(install_id(Some(&"a".repeat(64))).map(str::len), Some(64));

        // Absent, too short, too long, or carrying a character the column has no business holding.
        assert_eq!(install_id(None), None);
        assert_eq!(install_id(Some("")), None);
        assert_eq!(install_id(Some("short7")), None);
        assert_eq!(install_id(Some(&"a".repeat(65))), None);
        assert_eq!(install_id(Some("has spaces here")), None);
        assert_eq!(install_id(Some("semi;colon;here")), None);
        assert_eq!(install_id(Some("éléphant-one")), None);
    }

    #[test]
    fn a_scheme_no_provider_can_take_is_unsupported_not_invalid() {
        let c = cfg(&[]);
        let err = parse_url(&c, "magnet:?xt=urn:btih:deadbeef").expect_err("magnet");
        assert_eq!(err.code, ErrorCode::UnsupportedUrl);
        assert_eq!(
            &*err.message,
            "Unsupported resource \"magnet:?xt=urn:btih:deadbeef\""
        );
        let err = parse_url(&c, "  ").expect_err("blank");
        assert_eq!(err.code, ErrorCode::ValidationFailed);
        assert_eq!(err.field.as_deref(), Some("url"));
        assert!(parse_url(&c, " https://a.test/x ").is_ok(), "trimmed");
    }

    /// DESIGN §16.6 (SSRF) and §17.3: the API adds run the bot's validator, gated on
    /// `AULOS_ALLOW_PRIVATE_TARGETS` — which defaults to `true`, because a home deployment
    /// legitimately downloads from its own LAN.
    #[test]
    fn the_ssrf_guard_follows_allow_private_targets() {
        let permissive = cfg(&[]);
        assert!(
            permissive.allow_private_targets,
            "the API default is permissive"
        );
        for raw in [
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            "http://127.0.0.1:8081/x",
            "http://[::1]/x",
            "http://localhost/x",
        ] {
            assert!(parse_url(&permissive, raw).is_ok(), "{raw}");
        }

        let locked = cfg(&[("AULOS_ALLOW_PRIVATE_TARGETS", "false")]);
        for (raw, reason) in [
            (
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
                "private/local IP targets are not allowed",
            ),
            (
                "http://10.0.0.5/x",
                "private/local IP targets are not allowed",
            ),
            (
                "http://[::ffff:127.0.0.1]/x",
                "private/local IP targets are not allowed",
            ),
            ("http://localhost/x", "local network hosts are not allowed"),
            ("http://nas.local/x", "local network hosts are not allowed"),
        ] {
            let err = parse_url(&locked, raw).expect_err(raw);
            assert_eq!(err.code, ErrorCode::ValidationFailed, "{raw}");
            assert_eq!(err.field.as_deref(), Some("url"), "{raw}");
            assert_eq!(&*err.message, reason, "{raw}");
        }
        // A public target is still fine, and an unusable scheme is still `unsupported_url` rather
        // than the guard's `validation_failed`.
        assert!(parse_url(&locked, "https://www.youtube.com/watch?v=x").is_ok());
        assert_eq!(
            parse_url(&locked, "magnet:?xt=urn:btih:deadbeef")
                .expect_err("magnet")
                .code,
            ErrorCode::UnsupportedUrl
        );
    }

    #[test]
    fn the_request_field_list_is_the_protocol_list() {
        // PROTOCOL §4.1's table has sixteen rows; any drift here silently turns a documented
        // field into a `warnings` entry.
        assert_eq!(REQUEST_FIELDS.len(), 16);
        for key in ["url", "provider", "ytdl_options_overrides", "subtitle_mode"] {
            assert!(REQUEST_FIELDS.contains(&key), "{key}");
        }
    }

    #[test]
    fn a_batch_error_maps_to_its_status() {
        assert_eq!(
            map_add_error(AddError::TooManyUrls { max: 500, got: 501 }).status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            map_add_error(AddError::Unavailable("busy".into())).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_add_error(AddError::field(
                0,
                ErrorCode::FolderInvalid,
                "folder",
                "nope"
            ))
            .field
            .as_deref(),
            Some("folder")
        );
    }
}
