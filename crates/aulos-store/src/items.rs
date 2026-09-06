//! The `items` table: the row codec and every item-shaped [`WriteOp`] (DESIGN §7.1, §7.2).

use std::str::FromStr;

use aulos_core::{
    EntryBlob, FieldUpdate, FileRef, FileSlot, Item, ItemId, ProviderId, RelPath, SourceRef,
    Status, UnixMs, WireError,
};
use rusqlite::{Connection, Row, params};

use crate::error::StoreError;
use crate::json::{
    entry_column, from_sql_json, from_sql_string, i64_to_u32, i64_to_u64, kind_from_str,
    status_from_str, to_sql_json, u64_to_i64,
};
use crate::ops::WriteOp;

/// The column list every item read shares, in the order [`row_to_item`] expects.
///
/// `updated_at` is deliberately absent: it is a storage concern (it drives nothing on the wire and
/// [`Item`] has no field for it), so reading it would only invite a caller to depend on it.
pub(crate) const COLUMNS: &str = "id, kind, group_id, group_index, ord, url, canonical_key, \
     provider, media_id, title, status, auto_start, msg, error_json, request_json, entry_json, \
     filename, size, chapter_files_json, subtitle_files_json, source_json, attempt, \
     children_total, created_at, started_at, finished_at, clear_after";

/// Decodes one `items` row selected with [`COLUMNS`].
pub(crate) fn row_to_item(row: &Row<'_>) -> Result<Item, StoreError> {
    let id_raw: String = row.get(0)?;
    let kind_raw: String = row.get(1)?;
    let group_raw: Option<String> = row.get(2)?;
    let group_index: Option<i64> = row.get(3)?;
    let url_raw: String = row.get(5)?;
    let provider_raw: Option<String> = row.get(7)?;
    let status_raw: String = row.get(10)?;
    let error_raw: Option<String> = row.get(13)?;
    let request_raw: String = row.get(14)?;
    let entry_raw: Option<String> = row.get(15)?;
    let filename_raw: Option<String> = row.get(16)?;
    let size: Option<i64> = row.get(17)?;
    let chapters_raw: String = row.get(18)?;
    let subtitles_raw: String = row.get(19)?;
    let source_raw: String = row.get(20)?;
    let attempt: i64 = row.get(21)?;
    let children_total: Option<i64> = row.get(22)?;

    Ok(Item {
        id: ItemId::from_str(&id_raw).map_err(|e| StoreError::decode("items.id", e))?,
        kind: kind_from_str(&kind_raw)?,
        group_id: group_raw
            .as_deref()
            .map(|g| ItemId::from_str(g).map_err(|e| StoreError::decode("items.group_id", e)))
            .transpose()?,
        group_index: group_index
            .map(|v| i64_to_u32(v, "items.group_index"))
            .transpose()?,
        ord: row.get(4)?,
        url: from_sql_string(&url_raw, "items.url")?,
        canonical_key: row.get::<_, String>(6)?.into_boxed_str(),
        provider: provider_raw
            .as_deref()
            .map(|p| ProviderId::parse(p).map_err(|e| StoreError::decode("items.provider", e)))
            .transpose()?,
        media_id: row.get::<_, Option<String>>(8)?.map(String::into_boxed_str),
        title: row.get::<_, String>(9)?.into_boxed_str(),
        status: status_from_str(&status_raw)?,
        auto_start: row.get::<_, i64>(11)? != 0,
        msg: row
            .get::<_, Option<String>>(12)?
            .map(String::into_boxed_str),
        error: error_raw
            .as_deref()
            .map(|e| from_sql_json::<WireError>(e, "items.error_json"))
            .transpose()?,
        request: from_sql_json(&request_raw, "items.request_json")?,
        entry: entry_raw
            .as_deref()
            .map(|e| from_sql_json::<EntryBlob>(e, "items.entry_json"))
            .transpose()?,
        filename: filename_raw
            .as_deref()
            .map(|f| RelPath::parse(f).map_err(|e| StoreError::decode("items.filename", e)))
            .transpose()?,
        size: size.map(|s| i64_to_u64(s, "items.size")).transpose()?,
        chapter_files: from_sql_json(&chapters_raw, "items.chapter_files_json")?,
        subtitle_files: from_sql_json(&subtitles_raw, "items.subtitle_files_json")?,
        created_at: row.get(23)?,
        started_at: row.get(24)?,
        finished_at: row.get(25)?,
        attempt: u16::try_from(attempt).map_err(|_| StoreError::OutOfRange {
            column: "items.attempt",
        })?,
        source: from_sql_json::<SourceRef>(&source_raw, "items.source_json")?,
        children_total: children_total
            .map(|v| i64_to_u32(v, "items.children_total"))
            .transpose()?,
        clear_after: row.get(26)?,
    })
}

/// Inserts one row. `updated_at` starts equal to `created_at`.
fn insert(conn: &Connection, item: &Item, entry_max_bytes: u64) -> Result<(), StoreError> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO items (id, kind, group_id, group_index, ord, url, canonical_key, provider, \
         media_id, title, status, auto_start, msg, error_json, request_json, entry_json, filename, \
         size, chapter_files_json, subtitle_files_json, source_json, attempt, children_total, \
         created_at, started_at, finished_at, updated_at, clear_after) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
         ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)",
    )?;
    stmt.execute(params![
        item.id.to_string(),
        item.kind.as_str(),
        item.group_id.map(|g| g.to_string()),
        item.group_index.map(i64::from),
        item.ord,
        item.url.as_str(),
        &*item.canonical_key,
        item.provider.as_ref().map(|p| p.as_str().to_owned()),
        item.media_id.as_deref(),
        &*item.title,
        item.status.as_str(),
        i64::from(item.auto_start),
        item.msg.as_deref(),
        item.error
            .as_ref()
            .map(|e| to_sql_json(e, "items.error_json"))
            .transpose()?,
        to_sql_json(&item.request, "items.request_json")?,
        entry_column(item, entry_max_bytes)?,
        item.filename.as_ref().map(|f| f.as_str().to_owned()),
        item.size.map(|s| u64_to_i64(s, "items.size")).transpose()?,
        to_sql_json(&item.chapter_files, "items.chapter_files_json")?,
        to_sql_json(&item.subtitle_files, "items.subtitle_files_json")?,
        to_sql_json(&item.source, "items.source_json")?,
        i64::from(item.attempt),
        item.children_total.map(i64::from),
        item.created_at,
        item.started_at,
        item.finished_at,
        item.created_at,
        item.clear_after,
    ])?;
    Ok(())
}

/// Applies the DESIGN §7.1 timestamp table in one statement.
///
/// | Condition | Columns written |
/// |---|---|
/// | always | `updated_at = at` |
/// | `Preparing` and `started_at IS NULL` | `started_at = at` |
/// | terminal | `finished_at = at` |
/// | terminal → non-terminal | `finished_at = NULL`, `started_at` kept |
/// | otherwise | neither timestamp is touched |
///
/// The `msg` and `error_json` patches use the three-way [`FieldUpdate`] encoding: mode `0` keeps
/// the column, `1` nulls it, `2` writes the bound value.
fn set_status(
    conn: &Connection,
    id: ItemId,
    status: Status,
    msg: &FieldUpdate<Box<str>>,
    error: &FieldUpdate<WireError>,
    auto_start: Option<bool>,
    at: UnixMs,
) -> Result<(), StoreError> {
    let (msg_mode, msg_val) = match msg {
        FieldUpdate::Keep => (0_i64, None),
        FieldUpdate::Clear => (1, None),
        FieldUpdate::Set(v) => (2, Some(v.to_string())),
    };
    let (err_mode, err_val) = match error {
        FieldUpdate::Keep => (0_i64, None),
        FieldUpdate::Clear => (1, None),
        FieldUpdate::Set(v) => (2, Some(to_sql_json(v, "items.error_json")?)),
    };
    let mut stmt = conn.prepare_cached(
        "UPDATE items SET \
           status      = ?2, \
           updated_at  = ?3, \
           started_at  = CASE WHEN ?4 = 1 AND started_at IS NULL THEN ?3 ELSE started_at END, \
           finished_at = CASE \
                           WHEN ?5 = 1 THEN ?3 \
                           WHEN status IN ('finished','error','canceled') THEN NULL \
                           ELSE finished_at END, \
           msg         = CASE ?6 WHEN 0 THEN msg WHEN 1 THEN NULL ELSE ?7 END, \
           error_json  = CASE ?8 WHEN 0 THEN error_json WHEN 1 THEN NULL ELSE ?9 END, \
           auto_start  = CASE WHEN ?10 THEN ?11 ELSE auto_start END \
         WHERE id = ?1",
    )?;
    let n = stmt.execute(params![
        id.to_string(),
        status.as_str(),
        at,
        i64::from(status == Status::Preparing),
        i64::from(status.is_terminal()),
        msg_mode,
        msg_val,
        err_mode,
        err_val,
        i64::from(auto_start.is_some()),
        i64::from(auto_start.unwrap_or(false)),
    ])?;
    missing(n, id)
}

/// One row was expected; zero means the item was deleted under us.
fn missing(n: usize, id: ItemId) -> Result<(), StoreError> {
    if n == 0 {
        Err(StoreError::NotFound(id))
    } else {
        Ok(())
    }
}

/// Appends one [`FileRef`] to `chapter_files_json` or `subtitle_files_json`.
///
/// Read-modify-write inside the caller's transaction: the lists are short (a handful of chapters
/// or subtitle tracks) and `json_insert` would not validate the shape on the way in.
fn push_file(
    conn: &Connection,
    id: ItemId,
    slot: FileSlot,
    file: &FileRef,
    at: UnixMs,
) -> Result<(), StoreError> {
    let (column, sql_column) = match slot {
        FileSlot::Chapter => ("items.chapter_files_json", "chapter_files_json"),
        FileSlot::Subtitle => ("items.subtitle_files_json", "subtitle_files_json"),
    };
    let sql = format!("SELECT {sql_column} FROM items WHERE id = ?1");
    let current: Option<String> = conn
        .prepare_cached(&sql)?
        .query_row([id.to_string()], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(StoreError::from(other)),
        })?;
    let Some(current) = current else {
        return Err(StoreError::NotFound(id));
    };
    let mut list: Vec<FileRef> = from_sql_json(&current, column)?;
    list.push(file.clone());
    let sql = format!("UPDATE items SET {sql_column} = ?2, updated_at = ?3 WHERE id = ?1");
    let n = conn.prepare_cached(&sql)?.execute(params![
        id.to_string(),
        to_sql_json(&list, column)?,
        at
    ])?;
    missing(n, id)
}

/// Applies one item-shaped op. Returns `Ok(false)` when the op is not an item op at all.
pub(crate) fn apply(
    conn: &Connection,
    op: &WriteOp,
    entry_max_bytes: u64,
    now: UnixMs,
) -> Result<bool, StoreError> {
    match op {
        WriteOp::InsertItems { items } => {
            for item in items {
                insert(conn, item, entry_max_bytes)?;
            }
        }
        WriteOp::SetStatus {
            id,
            status,
            msg,
            error,
            auto_start,
            at,
        } => set_status(conn, *id, *status, msg, error, *auto_start, *at)?,
        WriteOp::SetAutoStart { id, auto_start, at } => {
            let n = conn
                .prepare_cached("UPDATE items SET auto_start = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![id.to_string(), i64::from(*auto_start), at])?;
            missing(n, *id)?;
        }
        WriteOp::SetSource { id, source } => {
            let n = conn
                .prepare_cached("UPDATE items SET source_json = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![
                    id.to_string(),
                    to_sql_json(source, "items.source_json")?,
                    now
                ])?;
            missing(n, *id)?;
        }
        WriteOp::SetResolved {
            id,
            provider,
            media_id,
            title,
            entry,
            canonical_key,
        } => {
            let entry_json = match entry {
                None => None,
                Some(blob) => {
                    let encoded = to_sql_json(blob, "items.entry_json")?;
                    if entry_max_bytes > 0 && encoded.len() as u64 > entry_max_bytes {
                        tracing::warn!(
                            item = %id,
                            bytes = encoded.len(),
                            max_bytes = entry_max_bytes,
                            "provider entry exceeds AULOS_ENTRY_MAX_BYTES; storing the marker"
                        );
                        Some(to_sql_json(&EntryBlob::truncated(), "items.entry_json")?)
                    } else {
                        Some(encoded)
                    }
                }
            };
            let n = conn
                .prepare_cached(
                    "UPDATE items SET provider = ?2, media_id = ?3, title = ?4, entry_json = ?5, \
                     canonical_key = ?6, updated_at = ?7 WHERE id = ?1",
                )?
                .execute(params![
                    id.to_string(),
                    provider.as_str(),
                    media_id.as_deref(),
                    &**title,
                    entry_json,
                    &**canonical_key,
                    now
                ])?;
            missing(n, *id)?;
        }
        WriteOp::PromoteToGroup {
            id,
            children_total,
            title,
        } => {
            let n = conn
                .prepare_cached(
                    "UPDATE items SET kind = 'group', children_total = ?2, title = ?3, \
                     updated_at = ?4 WHERE id = ?1",
                )?
                .execute(params![
                    id.to_string(),
                    i64::from(*children_total),
                    &**title,
                    now
                ])?;
            missing(n, *id)?;
        }
        WriteOp::SetOutput { id, filename, size } => {
            let n = conn
                .prepare_cached(
                    "UPDATE items SET filename = ?2, size = ?3, updated_at = ?4 WHERE id = ?1",
                )?
                .execute(params![
                    id.to_string(),
                    filename.as_ref().map(|f| f.as_str().to_owned()),
                    size.map(|s| u64_to_i64(s, "items.size")).transpose()?,
                    now
                ])?;
            missing(n, *id)?;
        }
        WriteOp::SetSize { id, size } => {
            let n = conn
                .prepare_cached("UPDATE items SET size = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![
                    id.to_string(),
                    u64_to_i64(*size, "items.size")?,
                    now
                ])?;
            missing(n, *id)?;
        }
        WriteOp::PushFile { id, slot, file } => push_file(conn, *id, *slot, file, now)?,
        WriteOp::DropEntryBlob { id } => {
            let n = conn
                .prepare_cached(
                    "UPDATE items SET entry_json = NULL, updated_at = ?2 WHERE id = ?1",
                )?
                .execute(params![id.to_string(), now])?;
            missing(n, *id)?;
        }
        WriteOp::BumpAttempt { id } => {
            let n = conn
                .prepare_cached(
                    "UPDATE items SET attempt = attempt + 1, updated_at = ?2 WHERE id = ?1",
                )?
                .execute(params![id.to_string(), now])?;
            missing(n, *id)?;
        }
        WriteOp::SetClearAfter { id, at } => {
            let n = conn
                .prepare_cached("UPDATE items SET clear_after = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![id.to_string(), at, now])?;
            missing(n, *id)?;
        }
        WriteOp::DeleteItems(ids) => {
            // The Live Activity rows go **first**: `live_activities.item_id` is not a foreign key
            // (PROTOCOL §4.8 lets the app register an activity before the item row exists), and a
            // group's children are only reachable through `items.group_id` until the `DELETE`
            // cascades them away. Doing this after the item delete would strand every child's
            // registration for ever, and the notifier would keep pushing to an activity whose
            // download no longer exists (DESIGN §25).
            for id in ids {
                crate::devices::remove_for_item(conn, *id)?;
            }
            let mut stmt = conn.prepare_cached("DELETE FROM items WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id.to_string()])?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}
