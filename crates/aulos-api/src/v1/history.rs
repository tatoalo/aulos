//! `GET <p>history` — the three-array projection (DESIGN §11.4, §11.5, PROTOCOL §10.2).
//!
//! # Where the three arrays come from
//!
//! | Array | Source | Bound |
//! |---|---|---|
//! | `queue`, `pending` | [`aulos_queue::StateView::load`] — the published snapshot, which holds **every** non-terminal record by construction | none needed |
//! | `done` | [`aulos_store::Store::v1_done`] — `finished` and `error`, `ord` ascending | `AULOS_V1_HISTORY_MAX`, default `0` = unlimited |
//!
//! `done[]` is deliberately **not** the `AULOS_MEM_DONE_ITEMS` window `api/v2/state` uses. v1 has
//! no `truncated`, no `done_total` and no cursor, and the shipped iOS `HistoryResponse` declares
//! all three arrays non-optional, so serving a 500-row window to an operator with 4 000 completed
//! rows would silently delete 3 500 of them from the client at cutover. The cost is real — a
//! 4 000-row answer is roughly 3 MB of JSON — and the three mitigations are: the query runs on the
//! read pool and never touches the writer, `entry` is omitted (the single biggest contributor), and
//! `AULOS_V1_HISTORY_MAX` lets an operator cap it, keeping the **most recent** rows and logging one
//! WARN per hour.
//!
//! # Three omissions, each because the shipped client would render them wrong
//!
//! - **Groups** (`kind == "group"`): legacy had no group concept, so a parent row would show as an
//!   "In Progress" item that never progresses. Its children appear normally.
//! - **`canceled`**: the shipped `DownloadStatus` has no such case and maps unknown → `.pending`,
//!   so a cancelled row would sit in "In Progress" forever. Legacy made cancels vanish.
//! - **`entry`**: the full yt-dlp info dict. Nothing reads it (C22).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use aulos_core::{Config, FileRef, Item, ItemId, ItemView, Kind, Status, ViewExtras};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::ApiState;
use crate::error::ApiError;

/// One hour, in milliseconds: how often the `AULOS_V1_HISTORY_MAX` WARN repeats (DESIGN §11.4).
const CAP_WARN_INTERVAL_MS: i64 = 3_600_000;

/// When the cap WARN was last emitted, unix ms. `0` means never.
static CAP_WARN_AT: AtomicI64 = AtomicI64::new(0);

/// `ItemId` → the provider's own media id, for the rows that have one.
///
/// Absent means "this row has no media id", which is the "else the ULID" half of DESIGN §11.4's
/// `id` rule.
pub type MediaIds = HashMap<ItemId, Box<str>>;

/// The `GET <p>history` body: three arrays, **always** present even when empty.
///
/// `HistoryResponse` on the client declares all three non-optional and a missing key fails the
/// whole decode (ios-client-reference §7 row 25), so this is a struct rather than a hand-built
/// object.
#[derive(Clone, Debug, Default, Serialize)]
pub struct V1History {
    /// `resolving`, `preparing`, `downloading`, `postprocessing`, and `queued(auto_start=true)`.
    pub queue: Vec<Value>,
    /// `queued(auto_start=false)`, including a pre-download-problem item (DESIGN §8.4).
    pub pending: Vec<Value>,
    /// `finished` and `error`, from the store.
    pub done: Vec<Value>,
}

/// The legacy `status` string, or `None` when the item is omitted entirely (DESIGN §11.5).
///
/// The mapping itself is [`Status::v1`] in `aulos-core`, so there is one definition of it; this
/// wrapper adds the *ninth* case the mapping cannot express. `Status::v1` has to return a
/// `&'static str` for all eight values and answers `"error"` for `Canceled` as a defensive
/// fallback, but the shim must **omit** a cancelled row rather than project it: the shipped
/// `DownloadStatus` has no `canceled` case, maps unknown → `.pending`, and would leave the row in
/// "In Progress" forever. `None` is what makes that structural rather than a comment.
#[must_use]
pub fn v1_status(status: Status) -> Option<&'static str> {
    match status {
        Status::Canceled => None,
        other => Some(other.v1()),
    }
}

/// One of the three arrays, or `None` for an omitted row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Queue,
    Pending,
    Done,
}

/// Which of the three arrays an item belongs in, or `None` when it is omitted.
fn bucket(view: &ItemView) -> Option<Which> {
    if view.kind == Kind::Group {
        return None;
    }
    match view.status {
        Status::Queued if !view.auto_start => Some(Which::Pending),
        Status::Queued
        | Status::Resolving
        | Status::Preparing
        | Status::Downloading
        | Status::Postprocessing => Some(Which::Queue),
        Status::Finished | Status::Error => Some(Which::Done),
        Status::Canceled => None,
    }
}

/// The pure projection, so it is testable without a store (PLAN WP-15's interface block).
///
/// `active` is every non-terminal record — the published snapshot's `items` — and `done` is the
/// terminal page. Groups and `canceled` rows are filtered here, and each array keeps its input
/// order, which both sources already deliver as `ord` ascending.
///
/// Deviation from the PLAN's `project_history(active, done, cfg)`: `media` is an extra argument.
/// `ItemView` carries no `media_id` (PROTOCOL §0 rule 3) while DESIGN §11.4's `id` rule needs one,
/// so the caller supplies the lookup rather than this function reaching for a store it must stay
/// free of. See `docs/INTEGRATION-NOTES.md`, WP-15.
#[must_use]
pub fn project_history(
    active: &[Arc<ItemView>],
    done: &[Arc<ItemView>],
    media: &MediaIds,
    cfg: &Config,
) -> V1History {
    let mut out = V1History::default();
    let project = |view: &ItemView| project_item(view, media.get(&view.id).map(|m| &**m), cfg);
    for view in active {
        match bucket(view) {
            Some(Which::Queue) => out.queue.push(project(view)),
            Some(Which::Pending) => out.pending.push(project(view)),
            // A terminal record reaches `items` only as a group, which `bucket` already rejected,
            // so this arm exists for totality rather than for a real row.
            Some(Which::Done) => out.done.push(project(view)),
            None => {}
        }
    }
    for view in done {
        if bucket(view) == Some(Which::Done) {
            out.done.push(project(view));
        }
    }
    out
}

/// One item, as the legacy `DownloadInfo.__dict__` minus `entry` (DESIGN §11.4's field table).
///
/// Three legacy quirks are reproduced deliberately:
///
/// - **the `custom_name_prefix` dotting.** `DownloadInfo.__init__` wrote `f'{prefix}.{id}'` and
///   `f'{prefix}.{title}'` when a prefix was set, so a client that shows `title` shows the prefix
///   too. The engine keeps `media_id` and `title` clean (the prefix belongs to the output
///   template), so the shim re-applies it here.
/// - **`msg` overloading.** Legacy used one field for the live stage line *and* for the terminal
///   error, so a failed row's `msg` and `error` were the same string.
/// - **`error` as a plain string.** v2's `error` is `{code, message, …}`; v1's was the message.
///
/// Two documented deltas: `filename` and `size` are **always** present (legacy omitted the keys
/// until the download produced a file, and an absent key is worse for a decoder than a `null`),
/// and `percent` is always a number (legacy sometimes sent `null`; the shipped client clamps it
/// anyway, and PROTOCOL §0 rule 4 is that a numeric field is always a number).
#[must_use]
pub fn project_item(view: &ItemView, media_id: Option<&str>, cfg: &Config) -> Value {
    let prefix = &*view.request.custom_name_prefix;
    let dotted = |value: &str| {
        if prefix.is_empty() {
            value.to_owned()
        } else {
            format!("{prefix}.{value}")
        }
    };

    // A caller that hands over a cancelled row gets `Status::v1`'s defensive `"error"`; the
    // bucketing above never does.
    let status = view.status.v1();
    let error = view.error.as_ref().map(|e| e.message.to_string());
    let msg = match (view.status, &view.msg, &error) {
        (Status::Error, _, Some(text)) => Some(text.clone()),
        (_, Some(text), _) => Some(text.to_string()),
        _ => None,
    };

    let legacy_id = media_id.map_or_else(|| view.id.to_string(), &dotted);

    let mut item = Map::new();
    item.insert("id".to_owned(), Value::String(legacy_id));
    item.insert("title".to_owned(), Value::String(dotted(&view.title)));
    item.insert("url".to_owned(), Value::String(view.url.to_string()));
    item.insert("status".to_owned(), Value::String(status.to_owned()));

    item.insert("percent".to_owned(), json_f64(view.percent));
    item.insert("speed".to_owned(), view.speed.map_or(Value::Null, json_f64));
    item.insert("eta".to_owned(), view.eta.map_or(Value::Null, Value::from));
    item.insert(
        "downloaded_bytes".to_owned(),
        opt_u64(view.downloaded_bytes),
    );
    item.insert("total_bytes".to_owned(), opt_u64(view.total_bytes));
    item.insert(
        "total_bytes_estimate".to_owned(),
        opt_u64(view.total_bytes_estimate),
    );
    item.insert("fragment_index".to_owned(), opt_u32(view.fragment_index));
    item.insert("fragment_count".to_owned(), opt_u32(view.fragment_count));

    item.insert("msg".to_owned(), msg.map_or(Value::Null, Value::String));
    item.insert("error".to_owned(), error.map_or(Value::Null, Value::String));

    item.insert(
        "filename".to_owned(),
        view.filename
            .as_ref()
            .map_or(Value::Null, |f| Value::String(f.to_string())),
    );
    item.insert("size".to_owned(), opt_u64(view.size));

    item.insert(
        "download_type".to_owned(),
        Value::String(view.selection.download_type.as_str().to_owned()),
    );
    item.insert(
        "codec".to_owned(),
        Value::String(view.selection.codec.as_str().to_owned()),
    );
    item.insert(
        "format".to_owned(),
        Value::String(view.selection.format.to_string()),
    );
    item.insert(
        "quality".to_owned(),
        Value::String(view.selection.quality.to_string()),
    );

    // Legacy emitted `""` for "the base directory" on every path that went through a
    // subscription, and the shipped client does not decode the key at all, so the empty string is
    // the safer of the two possible answers for a `None` folder.
    item.insert(
        "folder".to_owned(),
        Value::String(
            view.folder
                .as_ref()
                .map_or_else(String::new, |f| (**f).to_owned()),
        ),
    );
    item.insert(
        "custom_name_prefix".to_owned(),
        Value::String(prefix.to_owned()),
    );
    item.insert(
        "playlist_item_limit".to_owned(),
        Value::from(view.request.playlist_item_limit),
    );
    item.insert(
        "split_by_chapters".to_owned(),
        Value::Bool(view.request.split_by_chapters),
    );
    item.insert(
        "chapter_template".to_owned(),
        Value::String(view.request.chapter_template.to_string()),
    );
    item.insert(
        "subtitle_language".to_owned(),
        Value::String(view.request.subtitle_language.to_string()),
    );
    item.insert(
        "subtitle_mode".to_owned(),
        Value::String(view.request.subtitle_mode.as_str().to_owned()),
    );
    item.insert(
        "ytdl_options_presets".to_owned(),
        Value::Array(
            view.request
                .ytdl_options_presets
                .iter()
                .map(|p| Value::String(p.to_string()))
                .collect(),
        ),
    );
    item.insert(
        "ytdl_options_overrides".to_owned(),
        Value::Object((*view.request.ytdl_options_overrides).clone()),
    );

    // `time.time_ns()`: legacy's `timestamp` was nanoseconds, and a client sorts on it.
    item.insert(
        "timestamp".to_owned(),
        Value::from(view.created_at.saturating_mul(1_000_000)),
    );
    item.insert("chapter_files".to_owned(), files(&view.chapter_files));
    item.insert("subtitle_files".to_owned(), files(&view.subtitle_files));
    let _ = cfg; // the projection needs no configuration today; the argument is the PLAN's.

    Value::Object(item)
}

/// The key list `project_item` always emits: legacy's 33 minus `entry`, plus the two keys legacy
/// omitted until a file existed.
pub const ITEM_KEYS: [&str; 32] = [
    "id",
    "title",
    "url",
    "status",
    "percent",
    "speed",
    "eta",
    "downloaded_bytes",
    "total_bytes",
    "total_bytes_estimate",
    "fragment_index",
    "fragment_count",
    "msg",
    "error",
    "filename",
    "size",
    "download_type",
    "codec",
    "format",
    "quality",
    "folder",
    "custom_name_prefix",
    "playlist_item_limit",
    "split_by_chapters",
    "chapter_template",
    "subtitle_language",
    "subtitle_mode",
    "ytdl_options_presets",
    "ytdl_options_overrides",
    "timestamp",
    "chapter_files",
    "subtitle_files",
];

/// The `chapter_files` / `subtitle_files` arrays, as legacy's `{filename, size}` dicts.
///
/// `lang` is appended for a subtitle track, which legacy did not have and no consumer rejects.
fn files(list: &[FileRef]) -> Value {
    Value::Array(
        list.iter()
            .map(|f| {
                let mut entry = Map::new();
                entry.insert("filename".to_owned(), Value::String(f.filename.to_string()));
                entry.insert("size".to_owned(), opt_u64(f.size));
                if let Some(lang) = &f.lang {
                    entry.insert("lang".to_owned(), Value::String(lang.to_string()));
                }
                Value::Object(entry)
            })
            .collect(),
    )
}

/// A finite `f64` as a JSON number; a NaN or an infinity as `null`, which is the only thing
/// `serde_json` can represent.
fn json_f64(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn opt_u64(value: Option<u64>) -> Value {
    value.map_or(Value::Null, Value::from)
}

fn opt_u32(value: Option<u32>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// The handler: `active` from the published snapshot, `done` from the store (PLAN WP-15).
///
/// # Errors
/// `503 state_unavailable` when the read pool is busy — the only failure this route has.
pub async fn history(state: &ApiState) -> Result<V1History, ApiError> {
    let cap = (state.cfg.v1_history_max > 0).then_some(state.cfg.v1_history_max);
    let rows = state.store.v1_done(cap).await?;
    if let Some(limit) = cap
        && rows.len() as u64 >= u64::from(limit)
    {
        warn_capped(state, limit);
    }

    let mut media = aulos_store::v1::live_media_ids(&state.store).await?;
    let done: Vec<Arc<ItemView>> = rows
        .iter()
        .map(|item| {
            if let Some(id) = &item.media_id {
                media.insert(item.id, id.clone());
            }
            Arc::new(view_of(item))
        })
        .collect();

    let published = state.state.load();
    Ok(project_history(&published.items, &done, &media, &state.cfg))
}

/// A terminal store row as an [`ItemView`].
///
/// No progress cell and no [`ViewExtras`]: a terminal row's `percent` is decided by
/// [`ItemView::from_item`] (`100.0` for `finished`, `0.0` for `error`, since the in-memory cell is
/// long gone), and `download_url` is a v2 field the v1 projection does not emit.
fn view_of(item: &Item) -> ItemView {
    ItemView::from_item(item, None, &ViewExtras::default())
}

/// One WARN per hour naming the cap, so an operator who set it knows history is being trimmed.
fn warn_capped(state: &ApiState, limit: u32) {
    let now = state.now_ms();
    let last = CAP_WARN_AT.load(Ordering::Relaxed);
    if !due(now, last) {
        return;
    }
    if CAP_WARN_AT
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        tracing::warn!(
            limit,
            "AULOS_V1_HISTORY_MAX is capping GET history done[]; the oldest completed rows are \
             not being served to v1 clients"
        );
    }
}

/// Whether the once-per-hour WARN is due.
///
/// `last == 0` means "never emitted", so the first capped request always logs. Split out from
/// [`warn_capped`] so the rate limit is testable without a clock, a store and a rig — and because
/// a rate limit that is only exercised through a 4 000-row HTTP response is a rate limit nobody
/// checks.
const fn due(now: i64, last: i64) -> bool {
    last == 0 || now.saturating_sub(last) >= CAP_WARN_INTERVAL_MS
}

/// The legacy `frontend_safe()` block, with `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and
/// `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` as **strings** (DESIGN §11.4's last paragraph,
/// PROTOCOL §10.5).
///
/// Legacy never int-coerced those two, because its config values were the raw environment strings;
/// v2 emits them as numbers in `api/v2/capabilities.config`. **No v1 route carries this block**:
/// its only legacy carrier was the Socket.IO `configuration` event, which BRIEF §8 does not
/// provide (DESIGN §11.6). It lives here so the string-typed contract is written down and asserted
/// in exactly one place, ready for a `configuration` route if one is ever wanted.
#[must_use]
pub fn legacy_configuration(cfg: &Config) -> Value {
    serde_json::json!({
        "CUSTOM_DIRS": cfg.custom_dirs,
        "CREATE_CUSTOM_DIRS": cfg.create_custom_dirs,
        "OUTPUT_TEMPLATE_CHAPTER": cfg.output_template_chapter,
        "PUBLIC_HOST_URL": cfg.public_host_url,
        "PUBLIC_HOST_AUDIO_URL": cfg.public_host_audio_url,
        "DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT": cfg.default_option_playlist_item_limit_raw,
        "SUBSCRIPTION_DEFAULT_CHECK_INTERVAL": cfg.subscription_default_check_interval_raw,
        "ALLOW_YTDL_OPTIONS_OVERRIDES": cfg.allow_ytdl_options_overrides,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::config::{RawEnv, load};
    use aulos_core::{
        Codec, DownloadRequest, DownloadType, ErrorCode, FormatId, Ord0, QualityId, Selection,
        SourceKind, SourceRef, Status, WireError,
    };
    use url::Url;

    fn cfg() -> Config {
        load(&RawEnv::from_pairs(Vec::<(String, String)>::new())).unwrap()
    }

    fn item(status: Status) -> Item {
        let selection = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("best").unwrap(),
        );
        let request = DownloadRequest::new(
            Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap(),
            selection,
        );
        Item {
            id: ItemId::new(),
            kind: Kind::Item,
            group_id: None,
            group_index: None,
            ord: Ord0::from(1),
            url: request.url.clone(),
            canonical_key: "k".into(),
            provider: None,
            media_id: Some("dQw4w9WgXcQ".into()),
            title: "Never Gonna Give You Up".into(),
            status,
            auto_start: true,
            msg: None,
            error: None,
            request,
            entry: None,
            filename: None,
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: 1_757_000_000_000,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: SourceRef::bare(SourceKind::ApiV1),
            children_total: None,
            clear_after: None,
        }
    }

    fn view(status: Status) -> Arc<ItemView> {
        Arc::new(view_of(&item(status)))
    }

    fn media(views: &[Arc<ItemView>]) -> MediaIds {
        views
            .iter()
            .map(|v| (v.id, Box::from("dQw4w9WgXcQ")))
            .collect()
    }

    #[test]
    fn the_status_map_is_design_11_5() {
        assert_eq!(v1_status(Status::Queued), Some("pending"));
        assert_eq!(v1_status(Status::Resolving), Some("pending"));
        assert_eq!(v1_status(Status::Preparing), Some("preparing"));
        assert_eq!(v1_status(Status::Downloading), Some("downloading"));
        assert_eq!(v1_status(Status::Postprocessing), Some("downloading"));
        assert_eq!(v1_status(Status::Finished), Some("finished"));
        assert_eq!(v1_status(Status::Error), Some("error"));
        assert_eq!(v1_status(Status::Canceled), None, "omitted entirely");

        // Only the five legacy strings can ever appear — the shipped `DownloadStatus` has exactly
        // these cases and maps anything else to `.pending`.
        let mut seen: Vec<&str> = Status::ALL.iter().filter_map(|s| v1_status(*s)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen,
            ["downloading", "error", "finished", "pending", "preparing"]
        );
    }

    #[test]
    fn an_empty_projection_still_has_all_three_keys() {
        let body =
            serde_json::to_value(project_history(&[], &[], &MediaIds::new(), &cfg())).unwrap();
        for key in ["queue", "pending", "done"] {
            assert!(body[key].is_array(), "{key} must be an array");
            assert_eq!(body[key].as_array().unwrap().len(), 0);
        }
    }

    #[test]
    fn every_projected_item_carries_the_same_keys() {
        let v = view(Status::Downloading);
        let body = project_item(&v, Some("dQw4w9WgXcQ"), &cfg());
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), ITEM_KEYS.len());
        for key in ITEM_KEYS {
            assert!(object.contains_key(key), "{key} missing");
        }
        assert!(!object.contains_key("entry"), "entry is omitted (C22)");
        assert!(object["filename"].is_null(), "the key is always present");
        assert!(object["size"].is_null());
        assert!(object["percent"].is_number(), "never null");
    }

    #[test]
    fn the_three_buckets_follow_the_status_and_auto_start() {
        let mut queued = ItemView::clone(&view(Status::Queued));
        queued.auto_start = false;
        let queued = Arc::new(queued);
        let active = vec![
            view(Status::Resolving),
            view(Status::Preparing),
            view(Status::Downloading),
            view(Status::Postprocessing),
            view(Status::Queued),
            Arc::clone(&queued),
        ];
        let done = vec![view(Status::Finished), view(Status::Error)];
        let ids = media(&active);
        let out = project_history(&active, &done, &ids, &cfg());
        assert_eq!(out.queue.len(), 5, "four active plus queued(auto_start)");
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.done.len(), 2);
        assert_eq!(out.pending[0]["status"], "pending");
        assert_eq!(out.queue[0]["status"], "pending", "resolving reads pending");
    }

    #[test]
    fn a_group_and_a_cancel_are_omitted_but_a_child_is_not() {
        let mut group = ItemView::clone(&view(Status::Downloading));
        group.kind = Kind::Group;
        let mut child = ItemView::clone(&view(Status::Downloading));
        child.group_id = Some(group.id);
        child.group_index = Some(1);
        let active = vec![Arc::new(group), Arc::new(child), view(Status::Canceled)];
        let out = project_history(&active, &[view(Status::Canceled)], &MediaIds::new(), &cfg());
        assert_eq!(out.queue.len(), 1, "only the child");
        assert_eq!(out.pending.len(), 0);
        assert_eq!(out.done.len(), 0, "a cancelled row never reaches done[]");
        assert_eq!(out.queue[0]["status"], "downloading");
    }

    #[test]
    fn the_legacy_id_is_the_media_id_with_the_prefix_dotting() {
        let mut base = ItemView::clone(&view(Status::Finished));
        base.request.custom_name_prefix = Arc::from("Lecture 3");
        base.title = Arc::from("Introduction to Rust");
        let projected = project_item(&base, Some("abcdefghijk"), &cfg());
        assert_eq!(projected["id"], "Lecture 3.abcdefghijk");
        assert_eq!(projected["title"], "Lecture 3.Introduction to Rust");
        assert_eq!(projected["custom_name_prefix"], "Lecture 3");

        // With no prefix, neither field is dotted.
        let plain = project_item(&view(Status::Finished), Some("abcdefghijk"), &cfg());
        assert_eq!(plain["id"], "abcdefghijk");
        assert_eq!(plain["title"], "Never Gonna Give You Up");
    }

    #[test]
    fn an_unresolved_item_falls_back_to_its_ulid() {
        let v = view(Status::Resolving);
        let projected = project_item(&v, None, &cfg());
        assert_eq!(projected["id"], v.id.to_string());
    }

    #[test]
    fn a_terminal_error_populates_both_msg_and_error_as_plain_strings() {
        let mut row = item(Status::Error);
        row.error = Some(WireError::new(
            ErrorCode::Unavailable,
            "[youtube] x: Video unavailable",
        ));
        let v = view_of(&row);
        let projected = project_item(&v, Some("x"), &cfg());
        assert_eq!(projected["error"], "[youtube] x: Video unavailable");
        assert_eq!(
            projected["msg"], "[youtube] x: Video unavailable",
            "legacy overloaded msg"
        );
        assert!(projected["error"].is_string(), "a string, not an object");
    }

    #[test]
    fn a_pre_download_problem_item_is_pending_with_a_populated_error() {
        // DESIGN §8.4: an upcoming livestream is `queued(auto_start=false)` carrying the legacy
        // string, so it lands in `pending[]` and never in the client's Failed section.
        let mut row = item(Status::Queued);
        row.auto_start = false;
        row.error = Some(WireError::new(
            ErrorCode::NotYetLive,
            "Live stream is scheduled to start at 2026-12-31 20:00:00 +0000",
        ));
        let v = Arc::new(view_of(&row));
        let out = project_history(&[Arc::clone(&v)], &[], &MediaIds::new(), &cfg());
        assert_eq!(out.pending.len(), 1);
        assert_eq!(out.done.len(), 0);
        assert_eq!(out.pending[0]["status"], "pending");
        assert_eq!(
            out.pending[0]["error"],
            "Live stream is scheduled to start at 2026-12-31 20:00:00 +0000"
        );
    }

    #[test]
    fn the_timestamp_is_nanoseconds() {
        let v = view(Status::Finished);
        let projected = project_item(&v, None, &cfg());
        assert_eq!(projected["timestamp"], 1_757_000_000_000_i64 * 1_000_000);
    }

    #[test]
    fn a_finished_row_reports_a_hundred_percent_and_its_file() {
        let mut row = item(Status::Finished);
        row.filename = Some(aulos_core::RelPath::parse("Big Buck Bunny.mp4").unwrap());
        row.size = Some(158_008_374);
        row.chapter_files = vec![FileRef {
            filename: Arc::from("Talk - 01 - Intro.mkv"),
            size: Some(11),
            download_url: None,
            lang: None,
        }];
        row.subtitle_files = vec![FileRef {
            filename: Arc::from("Talk.en.srt"),
            size: Some(3),
            download_url: None,
            lang: Some(Arc::from("en")),
        }];
        let v = view_of(&row);
        let projected = project_item(&v, Some("BigBuckBunny_124"), &cfg());
        assert_eq!(projected["percent"], 100.0);
        assert_eq!(projected["filename"], "Big Buck Bunny.mp4");
        assert_eq!(projected["size"], 158_008_374_u64);
        assert_eq!(
            projected["chapter_files"][0]["filename"],
            "Talk - 01 - Intro.mkv"
        );
        assert_eq!(projected["chapter_files"][0]["size"], 11);
        assert_eq!(projected["subtitle_files"][0]["lang"], "en");
    }

    #[test]
    fn the_cap_warn_fires_once_an_hour_and_always_on_the_first_capped_request() {
        assert!(due(1_000, 0), "never emitted before");
        assert!(!due(1_000, 999), "same second");
        assert!(!due(1_000 + CAP_WARN_INTERVAL_MS - 1, 1_000), "59m59s");
        assert!(due(1_000 + CAP_WARN_INTERVAL_MS, 1_000), "exactly an hour");
        assert!(due(1_000 + CAP_WARN_INTERVAL_MS * 3, 1_000));
        // A clock that went backwards must not spam: `saturating_sub` floors at zero.
        assert!(!due(500, 1_000));
    }

    #[test]
    fn the_two_string_typed_config_keys_stay_strings() {
        let block = legacy_configuration(&cfg());
        assert!(block["DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT"].is_string());
        assert!(block["SUBSCRIPTION_DEFAULT_CHECK_INTERVAL"].is_string());
        assert_eq!(block["DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT"], "0");
        assert_eq!(block["SUBSCRIPTION_DEFAULT_CHECK_INTERVAL"], "60");
    }
}
