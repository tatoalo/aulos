//! Reading state: `GET api/v2/state`, `items`, `items/{id}`, `items/{id}/file`
//! (PROTOCOL §4.3, §4.4, §5.3).
//!
//! The snapshot builder lives here and is shared with the WebSocket, so the two surfaces are the
//! same object by construction rather than by review (PROTOCOL §0 rule 1). It reads
//! [`aulos_queue::StateView`] — one atomic load, no database round trip (DESIGN §15.2) — and adds
//! the three blocks the published generation does not carry: `subscriptions`, `ytdl_options` and
//! `health`. The last two are in the snapshot because their frames are transition-only: a client
//! connecting while the POT sidecar is down or the options file is broken would otherwise learn
//! nothing (PROTOCOL §5.3).

use std::sync::Arc;

use aulos_core::{
    Item, ItemId, ItemView, Kind, Ord0, Seq, Status, SubCmd, SubscriptionView, ViewExtras,
};
use aulos_queue::{Published, Resume};
use aulos_store::{Cursor, GroupScope, ItemFilter};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::{ApiError, Json};
use crate::v2::Q;
use crate::{ApiState, view};

/// The default `limit` on `GET api/v2/items` (PROTOCOL §4.4).
pub const DEFAULT_PAGE: u32 = 200;

/// The largest `limit` it accepts.
pub const MAX_PAGE: u32 = 1000;

/// How long the subscription list may take before the snapshot goes out without it.
const SUBS_TIMEOUT_MS: u64 = 1_000;

// ---------------------------------------------------------------------------
// GET api/v2/state
// ---------------------------------------------------------------------------

/// `GET api/v2/state` query parameters (PROTOCOL §4.3).
#[derive(Debug, Deserialize)]
pub struct StateQuery {
    /// Resume from this frame cursor.
    pub since: Option<u64>,
    /// The `boot_id` the cursor came from. A mismatch forces a snapshot.
    pub boot: Option<String>,
    /// Include the completed window. Default `true`.
    pub done: Option<bool>,
}

/// `GET api/v2/state` — a snapshot, a delta, or `up_to_date`.
pub async fn state(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Q(query): Q<StateQuery>,
) -> Result<Response, ApiError> {
    // What the response will carry is decided **before** the ETag is derived, because PROTOCOL
    // §4.3 defines the ETag as `W/"<boot_id>-<seq>"` where `seq` is *this body's own cursor*.
    //
    // Deriving it from `hub.head()` instead would be wrong for the length of every flush: the
    // aggregator publishes a flush's frames first and republishes the generation last, so
    // `published.seq < hub.head()` while the delta diff runs. A response that paired a
    // generation-500 body with `ETag: W/"…-501"` would answer `304` to every later poll and pin
    // the client to that stale snapshot for good.
    let plan = Plan::decide(&state, &query);
    let etag = state_etag(state.hub.boot_id(), plan.seq());
    if if_none_match(&headers, &etag) {
        return Ok(not_modified(&etag));
    }

    let body = plan.body(&state, query.done.unwrap_or(true)).await;

    let mut response = Json(body).into_response();
    response.headers_mut().insert(header::ETAG, etag);
    Ok(response)
}

/// What one `GET api/v2/state` will answer with, resolved before anything is serialised.
///
/// It exists so the cursor is read **once**: the ETag and the body are two views of the same
/// decision, never two independent reads of a moving hub.
enum Plan {
    /// The full state of this exact generation.
    Snapshot(Arc<Published>),
    /// `mode: "up_to_date"` at the cursor the client already holds.
    UpToDate(Seq),
    /// `mode: "delta"`, folded out of the replay window.
    Merged {
        /// The cursor the client sent.
        from: Seq,
        /// The cursor it holds afterwards.
        to: Seq,
        /// The folded frames.
        frames: Vec<Arc<aulos_queue::WireFrame>>,
    },
}

impl Plan {
    /// Reads the hub once and decides.
    fn decide(state: &ApiState, query: &StateQuery) -> Self {
        let Some(since) = query.since else {
            return Self::Snapshot(state.state.snapshot());
        };
        let Some(boot) = resume_boot(query.boot.as_deref()) else {
            return Self::Snapshot(state.state.snapshot());
        };
        match state.hub.resume(Seq(since), Some(boot)) {
            Resume::Snapshot => Self::Snapshot(state.state.snapshot()),
            // `resume` returned `UpToDate` because `since == head` at that moment, so the cursor
            // the body reports is `since` itself — re-reading `head()` here could name a frame
            // published in between, which is exactly the drift this type exists to prevent.
            Resume::UpToDate => Self::UpToDate(Seq(since)),
            Resume::Merged {
                from, to, frames, ..
            } => Self::Merged { from, to, frames },
        }
    }

    /// The cursor the body will report as `seq`.
    fn seq(&self) -> Seq {
        match self {
            Self::Snapshot(published) => published.seq,
            Self::UpToDate(seq) => *seq,
            Self::Merged { to, .. } => *to,
        }
    }

    /// The body itself.
    async fn body(self, state: &ApiState, done: bool) -> Value {
        match self {
            Self::Snapshot(published) => snapshot_of(state, &published, done, None).await,
            Self::UpToDate(seq) => json!({
                "mode": "up_to_date",
                "seq": seq.0,
                "boot_id": state.hub.boot_id(),
            }),
            Self::Merged { from, to, frames } => delta_body(state, from, to, &frames),
        }
    }
}

/// The `boot` that goes with a `since` cursor, or `None` when the cursor must be discarded
/// (PROTOCOL §5.1, §6.2).
///
/// §5.1 says `boot` is sent with **every** `since`, and §6.2 makes the server the one that
/// decides: a cursor whose origin the server cannot confirm is one it must not fold against.
/// Three inputs therefore mean the same thing — "take a snapshot":
///
/// | `boot` | Why |
/// |---|---|
/// | absent | the client did not say which boot the cursor came from |
/// | empty | same, spelled with a `&boot=` that carried nothing |
/// | unparseable | it is not a ULID, so it is certainly not *this* server's `boot_id` |
///
/// The old `and_then(|raw| raw.parse().ok())` collapsed the third case into the first and then
/// handed `None` to [`aulos_queue::EventHub::resume`], whose `None` means "no boot to check" — so
/// `?since=1197&boot=NOT-A-ULID` resumed exactly as if the boot had matched.
///
/// Shared with the WebSocket so `<p>ws` and `GET api/v2/state` cannot drift (PROTOCOL §0 rule 1).
#[must_use]
pub fn resume_boot(raw: Option<&str>) -> Option<aulos_core::BootId> {
    raw?.parse().ok()
}

/// `W/"<boot_id>-<seq>"`, where `seq` is the cursor the body carries (PROTOCOL §4.3).
fn state_etag(boot_id: aulos_core::BootId, seq: Seq) -> HeaderValue {
    let raw = format!("W/\"{boot_id}-{}\"", seq.0);
    HeaderValue::from_str(&raw).unwrap_or(HeaderValue::from_static("W/\"0-0\""))
}

/// Whether the client already holds this exact version.
fn if_none_match(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .any(|value| value == etag || value == "*")
}

/// `304` with an empty body — what makes pull-to-refresh nearly free.
fn not_modified(etag: &HeaderValue) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    response.headers_mut().insert(header::ETAG, etag.clone());
    response
}

/// The REST delta of PROTOCOL §4.3, folded out of the replay window.
///
/// The merged frames come from the hub already serialised (one encode for every reader), so this
/// re-parses them into the four buckets rather than asking the ring for a second representation.
/// The passthrough kinds a window may also contain — `subscription`, `notice`, `providers`,
/// `ytdl_options`, `health` — have no place in the §4.3 shape and are dropped: a polling client
/// re-reads them from the next snapshot, which is what the four documented keys imply.
fn delta_body(
    state: &ApiState,
    from: Seq,
    to: Seq,
    frames: &[Arc<aulos_queue::WireFrame>],
) -> Value {
    let mut added: Vec<Value> = Vec::new();
    let mut completed: Vec<Value> = Vec::new();
    let mut removed: Vec<Value> = Vec::new();
    let mut delta: Vec<Value> = Vec::new();
    let published = state.state.load();

    for frame in frames {
        let Ok(mut value) = serde_json::from_str::<Value>(frame.as_str()) else {
            continue;
        };
        match frame.kind {
            aulos_queue::FrameKind::Added | aulos_queue::FrameKind::Completed => {
                let terminal = frame.kind == aulos_queue::FrameKind::Completed;
                if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
                    for item in items.iter_mut() {
                        view::patch_item_json(&state.cfg, item);
                        if terminal {
                            completed.push(item.take());
                        } else {
                            added.push(item.take());
                        }
                    }
                }
            }
            aulos_queue::FrameKind::Removed => {
                let ids = value.get("ids").cloned().unwrap_or(Value::Array(vec![]));
                let reason = value.get("reason").cloned().unwrap_or(Value::Null);
                removed.push(json!({ "ids": ids, "reason": reason }));
            }
            aulos_queue::FrameKind::Delta => {
                if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
                    for item in items.iter_mut() {
                        if let Some(patched) = view::patch_frame(
                            &state.cfg,
                            &published,
                            aulos_queue::FrameKind::Delta,
                            &json!({ "items": [item.clone()] }).to_string(),
                        )
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|mut v| v.get_mut("items").map(|i| i[0].take()))
                        {
                            delta.push(patched);
                        } else {
                            delta.push(item.take());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    json!({
        "mode": "delta",
        "seq": to.0,
        "boot_id": state.hub.boot_id(),
        "from": from.0,
        "added": added,
        "completed": completed,
        "removed": removed,
        "delta": { "items": delta },
    })
}

// ---------------------------------------------------------------------------
// the snapshot
// ---------------------------------------------------------------------------

/// The complete state, in the one shape REST and the WebSocket share (PROTOCOL §4.3, §5.3).
///
/// `extra` is how the WebSocket adds its two frame-only keys (`t` and `server`) without a second
/// builder. `mode: "snapshot"` is always present, because a REST caller branches on it and a
/// WebSocket caller branches on `t`; carrying both costs 20 bytes and removes a fork.
pub async fn snapshot(state: &ApiState, done: bool, extra: Option<Map<String, Value>>) -> Value {
    let published = state.state.snapshot();
    snapshot_of(state, &published, done, extra).await
}

/// The snapshot of one **already-loaded** generation.
///
/// The WebSocket needs this: the cursor it forwards frames against and the body it sends have to
/// come from the same generation, or a frame could be filtered as "already reflected" against a
/// snapshot that does not contain it (DESIGN §15.4 step 2).
pub async fn snapshot_of(
    state: &ApiState,
    published: &Published,
    done: bool,
    extra: Option<Map<String, Value>>,
) -> Value {
    let subscriptions = subscriptions(state).await;

    let items = view::project_all(&state.cfg, published.items.iter());
    let done_window: Vec<Arc<ItemView>> = if done {
        view::project_all(&state.cfg, published.done.iter())
    } else {
        Vec::new()
    };

    let mut body = Map::new();
    if let Some(extra) = extra {
        for (k, v) in extra {
            body.insert(k, v);
        }
    }
    body.insert("mode".to_owned(), json!("snapshot"));
    body.insert("seq".to_owned(), json!(published.seq.0));
    body.insert("boot_id".to_owned(), json!(published.boot_id));
    body.insert("server_time".to_owned(), json!(state.now_ms()));
    body.insert("protocol".to_owned(), state.protocol_block());
    body.insert("counts".to_owned(), json!(published.counts));
    body.insert("done_total".to_owned(), json!(published.done_total));
    body.insert(
        "truncated".to_owned(),
        json!({
            "done": published.truncated.done && done,
            // v1.0: not implemented, see BRIEF — group collapsing is CUT, so every non-terminal
            // child ships inline and this list is always empty (PROTOCOL §5.3).
            "groups": published.truncated.groups,
        }),
    );
    body.insert("items".to_owned(), json!(items));
    body.insert("done".to_owned(), json!(done_window));
    body.insert("subscriptions".to_owned(), json!(subscriptions));
    body.insert(
        "ytdl_options".to_owned(),
        super::meta::ytdl_options_block(state),
    );
    body.insert("health".to_owned(), super::meta::health_block(state));
    Value::Object(body)
}

/// The current subscription projections.
///
/// The manager answers from memory, so the normal path is one message round trip; if it is gone or
/// slow the store's rows are used instead, with `checking: false` — a snapshot that is missing its
/// `subscriptions` array entirely would make a client believe every subscription was deleted.
pub async fn subscriptions(state: &ApiState) -> Vec<SubscriptionView> {
    let (ack, reply) = tokio::sync::oneshot::channel();
    if state.subs.send(SubCmd::List { ack }).await.is_ok() {
        let deadline = std::time::Duration::from_millis(SUBS_TIMEOUT_MS);
        if let Ok(Ok(Ok(list))) = tokio::time::timeout(deadline, reply).await {
            return list;
        }
    }
    match state.store.subscriptions().await {
        Ok(rows) => rows.iter().map(|r| r.to_view(false)).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "the subscription list is unavailable for this snapshot");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// GET api/v2/items
// ---------------------------------------------------------------------------

/// `GET api/v2/items` query parameters (PROTOCOL §4.4).
#[derive(Debug, Deserialize)]
pub struct ItemsQuery {
    /// Comma list of statuses.
    pub status: Option<String>,
    /// `item` or `group`.
    pub kind: Option<String>,
    /// Only the children of this group.
    pub group_id: Option<String>,
    /// Case-insensitive title substring, applied in the `WHERE` clause so `total` and the
    /// cursor describe the matching set.
    pub q: Option<String>,
    /// `ord` — the only accepted value.
    pub order: Option<String>,
    /// Page size, default 200, max 1000.
    pub limit: Option<u32>,
    /// The opaque position from a previous page's `next_cursor`.
    pub cursor: Option<String>,
}

/// `GET api/v2/items` — the paged list, `ord` ascending.
pub async fn items(
    State(state): State<ApiState>,
    Q(query): Q<ItemsQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(order) = &query.order
        && order != "ord"
    {
        return Err(ApiError::invalid("order", "order must be \"ord\""));
    }

    let mut filter = ItemFilter::default();
    if let Some(raw) = &query.status {
        for token in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let status = Status::ALL
                .into_iter()
                .find(|s| s.as_str() == token)
                .ok_or_else(|| {
                    ApiError::invalid("status", format!("unknown status \"{token}\""))
                })?;
            filter.statuses.push(status);
        }
    }
    if let Some(raw) = &query.kind {
        let kind = match raw.as_str() {
            "item" => Kind::Item,
            "group" => Kind::Group,
            other => {
                return Err(ApiError::invalid(
                    "kind",
                    format!("unknown kind \"{other}\""),
                ));
            }
        };
        filter.kinds.push(kind);
    }
    if let Some(raw) = &query.group_id {
        let id: ItemId = raw
            .parse()
            .map_err(|_| ApiError::invalid("group_id", format!("no such group: {raw}")))?;
        filter.group = GroupScope::Of(id);
    }
    // `q` is part of the query, not a filter over the page: `ItemFilter::title_like` puts it in
    // the `WHERE` clause, so `total` counts the matching set and every page is full. Filtering
    // the returned page instead made `total` the unfiltered count and left mostly-empty pages.
    if let Some(needle) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        filter.title_like = Some(Box::from(needle));
    }
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    filter.limit = Some(limit);
    if let Some(raw) = &query.cursor {
        filter.after = Some(parse_cursor(raw)?);
    }

    let page = state.store.items(filter).await?;
    let published = state.state.load();
    let rows: Vec<Arc<ItemView>> = page
        .rows
        .iter()
        .map(|row| view_of_row(&state, &published, row))
        .collect();

    Ok(Json(json!({
        "items": rows,
        "next_cursor": page.next.map(format_cursor),
        "total": page.total,
        "seq": state.seq(),
    })))
}

/// `GET api/v2/items/{id}` — one item, or `404`.
pub async fn item(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Arc<ItemView>>, ApiError> {
    Ok(Json(lookup(&state, &id).await?))
}

/// `GET api/v2/items/{id}/file` — `302` to the file route, `404` when there is no file yet.
pub async fn item_file(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let view = lookup(&state, &id).await?;
    let target = view
        .download_url
        .as_deref()
        .ok_or_else(|| ApiError::not_found(format!("item {id} has produced no file yet")))?;
    // `download_url` is relative to `<p>` unless the operator pointed `PUBLIC_HOST_URL` at a CDN,
    // in which case it is already absolute (PROTOCOL §2.3).
    let location = if url::Url::parse(target).is_ok() {
        target.to_owned()
    } else {
        state.cfg.url_prefix.route(target)
    };
    let mut response = StatusCode::FOUND.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&location)
            .map_err(|_| ApiError::internal("the file location is not a valid header"))?,
    );
    Ok(response)
}

/// One item by wire id: the live view when the engine still holds it, else the persisted row.
pub async fn lookup(state: &ApiState, raw: &str) -> Result<Arc<ItemView>, ApiError> {
    let id: ItemId = raw
        .parse()
        .map_err(|_| ApiError::not_found(format!("no such item: {raw}")))?;
    {
        let published = state.state.load();
        if let Some(live) = published.get(id) {
            return Ok(view::project(&state.cfg, live));
        }
    }
    let row = state
        .store
        .item(id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no such item: {raw}")))?;
    let published = state.state.load();
    Ok(view_of_row(state, &published, &row))
}

/// The wire view of a persisted row.
///
/// The published generation wins when it has the record, because it carries the live progress cell
/// and the group counters; a row outside it is terminal or historical, so `percent` follows the
/// DESIGN §4.6 rule (`100.0` when finished, the last value otherwise) and the group counters are
/// `null`.
fn view_of_row(state: &ApiState, published: &Published, row: &Item) -> Arc<ItemView> {
    if let Some(live) = published.get(row.id) {
        return view::project(&state.cfg, live);
    }
    let extras = ViewExtras {
        children_inline: (row.kind == Kind::Group).then_some(true),
        ..ViewExtras::default()
    };
    let built = Arc::new(ItemView::from_item(row, None, &extras));
    view::project(&state.cfg, &built)
}

/// `"<ord>.<id>"` — opaque to a client, which PROTOCOL §4.4 requires, and stable across restarts.
fn format_cursor(cursor: Cursor) -> String {
    format!("{}.{}", cursor.ord, cursor.id)
}

/// Parses a cursor a previous page handed out.
fn parse_cursor(raw: &str) -> Result<Cursor, ApiError> {
    let bad = || ApiError::invalid("cursor", "cursor is not one this server issued");
    let (ord, id) = raw.split_once('.').ok_or_else(bad)?;
    Ok(Cursor {
        ord: ord.parse::<Ord0>().map_err(|_| bad())?,
        id: id.parse().map_err(|_| bad())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips() {
        let cursor = Cursor {
            ord: 981,
            id: ItemId::new(),
        };
        let raw = format_cursor(cursor);
        let back = parse_cursor(&raw).expect("round trip");
        assert_eq!(back.ord, cursor.ord);
        assert_eq!(back.id, cursor.id);
    }

    #[test]
    fn a_forged_cursor_is_a_validation_failure() {
        for raw in ["", "abc", "1.notaulid", ".", "x.y"] {
            let err = parse_cursor(raw).expect_err(raw);
            assert_eq!(err.field.as_deref(), Some("cursor"), "{raw}");
        }
    }
}
