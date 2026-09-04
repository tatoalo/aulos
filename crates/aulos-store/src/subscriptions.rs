//! The `subscriptions` and `subscription_seen` tables (DESIGN §7.1, §7.2, §14.1).

use aulos_core::paths::RelDir;
use aulos_core::{Selection, SubId, SubscriptionRecord, SubtitleLang, SubtitleMode, UnixMs};
use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::StoreError;
use crate::json::{from_sql_json, from_sql_string, i64_to_u32, to_sql_json};
use crate::ops::WriteOp;

/// `subscriptions.request_json`: the [`aulos_core::DownloadRequest`] template, minus the url.
///
/// The url is the subscription's own column (it is `UNIQUE` there, which is the duplicate-feed
/// guard), so carrying it twice would give two places for it to disagree. Everything else a check
/// needs to build a request for a newly-seen entry lives in here.
#[derive(Serialize, Deserialize)]
struct Template {
    selection: Selection,
    folder: Option<RelDir>,
    custom_name_prefix: Box<str>,
    auto_start: bool,
    playlist_item_limit: u32,
    split_by_chapters: bool,
    chapter_template: Box<str>,
    subtitle_language: SubtitleLang,
    subtitle_mode: SubtitleMode,
    ytdl_options_presets: Vec<Box<str>>,
    ytdl_options_overrides: Map<String, Value>,
}

impl Template {
    fn of(r: &SubscriptionRecord) -> Self {
        Self {
            selection: r.selection.clone(),
            folder: r.folder.clone(),
            custom_name_prefix: r.custom_name_prefix.clone(),
            auto_start: r.auto_start,
            playlist_item_limit: r.playlist_item_limit,
            split_by_chapters: r.split_by_chapters,
            chapter_template: r.chapter_template.clone(),
            subtitle_language: r.subtitle_language.clone(),
            subtitle_mode: r.subtitle_mode,
            ytdl_options_presets: r.ytdl_options_presets.clone(),
            ytdl_options_overrides: r.ytdl_options_overrides.clone(),
        }
    }
}

/// The column list every subscription read shares, in the order [`row_to_record`] expects.
///
/// `s.created_at`, `s.updated_at` and `s.last_success` are storage bookkeeping with no field on
/// [`SubscriptionRecord`] and nothing on the wire; `seen_count` is denormalised from
/// `subscription_seen` here rather than kept in a column that could go stale.
pub(crate) const COLUMNS: &str = "s.id, s.name, s.url, s.enabled, s.check_interval_minutes, \
     s.request_json, s.last_checked, s.next_due, s.consecutive_failures, s.error, \
     (SELECT COUNT(*) FROM subscription_seen v WHERE v.subscription_id = s.id)";

/// Decodes one row selected with [`COLUMNS`].
pub(crate) fn row_to_record(row: &Row<'_>) -> Result<SubscriptionRecord, StoreError> {
    let id_raw: String = row.get(0)?;
    let url_raw: String = row.get(2)?;
    let template_raw: String = row.get(5)?;
    let t: Template = from_sql_json(&template_raw, "subscriptions.request_json")?;
    Ok(SubscriptionRecord {
        id: SubId::parse(&id_raw).map_err(|e| StoreError::decode("subscriptions.id", e))?,
        name: row.get::<_, String>(1)?.into_boxed_str(),
        url: from_sql_string(&url_raw, "subscriptions.url")?,
        enabled: row.get::<_, i64>(3)? != 0,
        check_interval_minutes: i64_to_u32(row.get(4)?, "subscriptions.check_interval_minutes")?,
        selection: t.selection,
        folder: t.folder,
        custom_name_prefix: t.custom_name_prefix,
        auto_start: t.auto_start,
        playlist_item_limit: t.playlist_item_limit,
        split_by_chapters: t.split_by_chapters,
        chapter_template: t.chapter_template,
        subtitle_language: t.subtitle_language,
        subtitle_mode: t.subtitle_mode,
        ytdl_options_presets: t.ytdl_options_presets,
        ytdl_options_overrides: t.ytdl_options_overrides,
        last_checked: row.get(6)?,
        next_due: row.get(7)?,
        consecutive_failures: i64_to_u32(row.get(8)?, "subscriptions.consecutive_failures")?,
        error: row.get::<_, Option<String>>(9)?.map(String::into_boxed_str),
        seen_count: i64_to_u32(row.get(10)?, "subscription_seen.count")?,
    })
}

/// Inserts or replaces one subscription.
///
/// `created_at` is absent from the `DO UPDATE` list, so it survives every later write;
/// `last_success` is advanced to `last_checked` only on a check that reported no error and is
/// otherwise left alone. Neither column has a field on [`SubscriptionRecord`] — they exist for the
/// operator reading the file with `sqlite3` — so neither may be reset by a scheduler tick.
fn upsert(conn: &Connection, r: &SubscriptionRecord, now: UnixMs) -> Result<(), StoreError> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO subscriptions (id, name, url, enabled, check_interval_minutes, request_json, \
         last_checked, last_success, next_due, consecutive_failures, error, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12) \
         ON CONFLICT(id) DO UPDATE SET \
           name = excluded.name, \
           url = excluded.url, \
           enabled = excluded.enabled, \
           check_interval_minutes = excluded.check_interval_minutes, \
           request_json = excluded.request_json, \
           last_checked = excluded.last_checked, \
           last_success = COALESCE(excluded.last_success, subscriptions.last_success), \
           next_due = excluded.next_due, \
           consecutive_failures = excluded.consecutive_failures, \
           error = excluded.error, \
           updated_at = excluded.updated_at",
    )?;
    let last_success = (r.error.is_none() && r.consecutive_failures == 0)
        .then_some(r.last_checked)
        .flatten();
    stmt.execute(params![
        r.id.as_str(),
        &*r.name,
        r.url.as_str(),
        i64::from(r.enabled),
        i64::from(r.check_interval_minutes.max(1)),
        to_sql_json(&Template::of(r), "subscriptions.request_json")?,
        r.last_checked,
        last_success,
        r.next_due,
        i64::from(r.consecutive_failures),
        r.error.as_deref(),
        now,
    ])?;
    Ok(())
}

/// Applies one subscription-shaped op. `Ok(false)` when the op is not one.
pub(crate) fn apply(conn: &Connection, op: &WriteOp, now: UnixMs) -> Result<bool, StoreError> {
    match op {
        WriteOp::UpsertSubscription(record) => upsert(conn, record, now)?,
        WriteOp::MarkSeen { sub, ids, at } => {
            // First sighting wins, so `seen_at` keeps telling the truth about when an id first
            // appeared — which is what `PruneSeen`'s ordering depends on.
            let mut stmt = conn.prepare_cached(
                "INSERT INTO subscription_seen (subscription_id, media_id, seen_at) \
                 VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            )?;
            for id in ids {
                stmt.execute(params![sub.as_str(), &**id, at])?;
            }
        }
        WriteOp::PruneSeen { sub, keep } => {
            // `NOT IN (… ORDER BY seen_at DESC LIMIT :keep)` rather than DESIGN §7.2's
            // `seen_at < (… OFFSET :keep)`: the two agree whenever `seen_at` is distinct, and this
            // form also breaks ties deterministically instead of keeping an arbitrary subset of a
            // batch that was all marked in the same millisecond.
            conn.prepare_cached(
                "DELETE FROM subscription_seen WHERE subscription_id = ?1 AND media_id NOT IN (\
                   SELECT media_id FROM subscription_seen WHERE subscription_id = ?1 \
                   ORDER BY seen_at DESC, media_id DESC LIMIT ?2)",
            )?
            .execute(params![sub.as_str(), i64::from(*keep)])?;
        }
        WriteOp::DeleteSubscriptions(ids) => {
            let mut stmt = conn.prepare_cached("DELETE FROM subscriptions WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id.as_str()])?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// [`crate::Store::subscriptions`] — every subscription, oldest first.
pub(crate) fn all(conn: &Connection) -> Result<Vec<SubscriptionRecord>, StoreError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM subscriptions s ORDER BY s.created_at ASC, s.id ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_record(row)?);
    }
    Ok(out)
}

/// One subscription by id.
pub(crate) fn one(conn: &Connection, id: &SubId) -> Result<Option<SubscriptionRecord>, StoreError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM subscriptions s WHERE s.id = ?1"
    ))?;
    let mut rows = stmt.query([id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_record(row)?)),
        None => Ok(None),
    }
}
