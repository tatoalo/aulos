//! The typed reads (DESIGN §7.1), and the three query-shaped types they need.
//!
//! Every function here takes a `&Connection` and is called on a read-pool thread, so nothing in
//! this module can block the writer.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use aulos_core::{GroupId, Item, ItemId, Kind, Ord0, Status, SubId, UnixMs};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};

use crate::error::StoreError;
use crate::items::{COLUMNS, row_to_item};

/// A keyset position in the `(ord, id)` total order that every item query uses.
///
/// A keyset cursor rather than an `OFFSET`: `ord` is unique and indexed, so resuming is one index
/// seek regardless of how deep the caller has paged, and a row inserted while the client pages
/// cannot make it skip or repeat one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cursor {
    /// The sort key of the last row the caller saw.
    pub ord: Ord0,
    /// Its id, to break the (impossible, but cheap to handle) `ord` tie deterministically.
    pub id: ItemId,
}

/// Which part of the group tree a query covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GroupScope {
    /// Every row, groups and children alike.
    #[default]
    Any,
    /// Only rows with no parent: single items and group containers.
    TopLevel,
    /// Only the children of one group.
    Of(GroupId),
}

/// What [`crate::Store::items`] selects.
///
/// An empty `statuses`/`kinds` list means "any", so [`ItemFilter::default`] is "everything, oldest
/// first, unpaged".
#[derive(Clone, Debug, Default)]
pub struct ItemFilter {
    /// Restrict to these statuses. Empty = any.
    pub statuses: Vec<Status>,
    /// Restrict to these kinds. Empty = any.
    pub kinds: Vec<Kind>,
    /// Restrict to a part of the group tree.
    pub group: GroupScope,
    /// At most this many rows.
    pub limit: Option<u32>,
    /// Resume after this position.
    pub after: Option<Cursor>,
    /// Newest first instead of oldest first.
    pub newest_first: bool,
    /// Case-insensitive substring match on `title`, or `None` for no title predicate.
    ///
    /// This is what `GET api/v2/items?q=` needs to page honestly: filtering the page the keyset
    /// query returned would make `total` the *unfiltered* count and leave a page that is mostly
    /// empty (the WP-14 request in `docs/INTEGRATION-NOTES.md`). The needle is bound as a
    /// parameter and `%`/`_`/`\` in it are escaped, so a title containing a wildcard is matched
    /// literally.
    pub title_like: Option<Box<str>>,
}

/// Escapes the three characters SQLite's `LIKE ... ESCAPE '\'` treats specially.
fn like_needle(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('%');
    for ch in raw.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

impl ItemFilter {
    /// Every non-terminal row, oldest first — the engine's boot query and the WS snapshot's
    /// working set.
    #[must_use]
    pub fn non_terminal() -> Self {
        Self {
            statuses: Status::ALL
                .into_iter()
                .filter(|s| !s.is_terminal())
                .collect(),
            ..Self::default()
        }
    }

    /// Every terminal row, newest first — the `AULOS_MEM_DONE_ITEMS` window.
    #[must_use]
    pub fn terminal() -> Self {
        Self {
            statuses: Status::ALL
                .into_iter()
                .filter(|s| s.is_terminal())
                .collect(),
            newest_first: true,
            ..Self::default()
        }
    }

    /// Restrict to one status.
    #[must_use]
    pub fn with_status(mut self, s: Status) -> Self {
        self.statuses = vec![s];
        self
    }

    /// Restrict to one kind.
    #[must_use]
    pub fn with_kind(mut self, k: Kind) -> Self {
        self.kinds = vec![k];
        self
    }

    /// Restrict to a group scope.
    #[must_use]
    pub fn with_group(mut self, g: GroupScope) -> Self {
        self.group = g;
        self
    }

    /// Cap the page size.
    #[must_use]
    pub fn with_limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }

    /// Resume after a position.
    #[must_use]
    pub fn after(mut self, c: Cursor) -> Self {
        self.after = Some(c);
        self
    }
}

/// One page of rows, plus how many the filter matches in total.
#[derive(Clone, Debug)]
pub struct Page<T> {
    /// The rows, in the filter's order.
    pub rows: Vec<T>,
    /// How many rows match the filter, ignoring `limit` and `after`. This is what feeds
    /// `done_total` and `truncated.done` on the wire (PROTOCOL §2).
    pub total: u64,
    /// The position to resume from, or `None` when this page is the last one.
    pub next: Option<Cursor>,
}

/// Group child counters, recomputed from the children in one `GROUP BY` (DESIGN §8.9).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GroupCounts {
    /// Children of any status.
    pub children: u32,
    /// Children with `status == finished`.
    pub done: u32,
    /// Children with `status == error`.
    pub error: u32,
    /// Children in `preparing`/`downloading`/`postprocessing`.
    pub active: u32,
}

/// Everything boot recovery needs, in one round trip (DESIGN §8.9).
///
/// The bucketing by status is deliberately left to the engine: it has the transition table and the
/// `AULOS_RESTART_POLICY` switch, and duplicating either here would give two places to change when
/// the recovery table does.
#[derive(Clone, Debug)]
pub struct BootState {
    /// Every non-terminal row, `ORDER BY ord ASC, id ASC`. By construction this is the whole
    /// working set: the WS snapshot carries all of it.
    pub non_terminal: Vec<Item>,
    /// The most recent `AULOS_MEM_DONE_ITEMS` terminal rows, also oldest-first.
    pub done_window: Vec<Item>,
    /// How many terminal rows exist, so the wire can be honest about the window.
    pub done_total: u64,
    /// How many rows exist at all.
    pub items_total: u64,
    /// Child counters per group.
    pub group_counts: HashMap<GroupId, GroupCounts>,
    /// The earliest armed `clear_after`, for the `ClearScheduler`'s `sleep_until` fast path.
    pub next_clear_at: Option<UnixMs>,
}

// ---------------------------------------------------------------------------
// items / item
// ---------------------------------------------------------------------------

/// Builds the `WHERE` clause and its bound parameters for a filter.
fn where_clause(f: &ItemFilter) -> (String, Vec<SqlValue>) {
    let mut sql = String::from(" WHERE 1 = 1");
    let mut args: Vec<SqlValue> = Vec::new();
    if !f.statuses.is_empty() {
        sql.push_str(" AND status IN (");
        for (i, s) in f.statuses.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            args.push(SqlValue::Text(s.as_str().to_owned()));
        }
        sql.push(')');
    }
    if !f.kinds.is_empty() {
        sql.push_str(" AND kind IN (");
        for (i, k) in f.kinds.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push('?');
            args.push(SqlValue::Text(k.as_str().to_owned()));
        }
        sql.push(')');
    }
    match f.group {
        GroupScope::Any => {}
        GroupScope::TopLevel => sql.push_str(" AND group_id IS NULL"),
        GroupScope::Of(g) => {
            sql.push_str(" AND group_id = ?");
            args.push(SqlValue::Text(g.to_string()));
        }
    }
    if let Some(needle) = f
        .title_like
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        // `title` is `TEXT`, so SQLite's `LIKE` is already ASCII-case-insensitive; `lower()` on
        // both sides would defeat any index and still not fold non-ASCII.
        sql.push_str(" AND title LIKE ? ESCAPE '\\'");
        args.push(SqlValue::Text(like_needle(needle)));
    }
    (sql, args)
}

/// [`crate::Store::items`].
pub(crate) fn items(conn: &Connection, f: &ItemFilter) -> Result<Page<Item>, StoreError> {
    let (predicate, mut args) = where_clause(f);
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM items{predicate}"),
        params_from_iter(args.iter()),
        |r| r.get(0),
    )?;

    let mut sql = format!("SELECT {COLUMNS} FROM items{predicate}");
    if let Some(c) = f.after {
        // `ord` is UNIQUE, so the `(ord, id)` tuple comparison only ever needs the first term;
        // the second is here so a hand-repaired database with a duplicated `ord` still pages.
        if f.newest_first {
            sql.push_str(" AND (ord < ? OR (ord = ? AND id < ?))");
        } else {
            sql.push_str(" AND (ord > ? OR (ord = ? AND id > ?))");
        }
        args.push(SqlValue::Integer(c.ord));
        args.push(SqlValue::Integer(c.ord));
        args.push(SqlValue::Text(c.id.to_string()));
    }
    sql.push_str(if f.newest_first {
        " ORDER BY ord DESC, id DESC"
    } else {
        " ORDER BY ord ASC, id ASC"
    });
    if let Some(n) = f.limit {
        sql.push_str(" LIMIT ?");
        args.push(SqlValue::Integer(i64::from(n)));
    }

    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params_from_iter(args.iter()))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_item(row)?);
    }
    let next = match (f.limit, out.last()) {
        (Some(n), Some(last)) if out.len() as u64 >= u64::from(n) => Some(Cursor {
            ord: last.ord,
            id: last.id,
        }),
        _ => None,
    };
    Ok(Page {
        rows: out,
        total: u64::try_from(total).unwrap_or(0),
        next,
    })
}

/// [`crate::Store::item`].
pub(crate) fn item(conn: &Connection, id: ItemId) -> Result<Option<Item>, StoreError> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {COLUMNS} FROM items WHERE id = ?1"))?;
    let mut rows = stmt.query([id.to_string()])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_item(row)?)),
        None => Ok(None),
    }
}

/// [`crate::Store::entry_blob`] — the read `EngineHookStore` delegates here (DESIGN §7.1, §13.2).
pub(crate) fn entry_blob(
    conn: &Connection,
    id: ItemId,
) -> Result<Option<aulos_core::EntryBlob>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT entry_json FROM items WHERE id = ?1")?;
    let mut rows = stmt.query([id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Err(StoreError::NotFound(id));
    };
    let raw: Option<String> = row.get(0)?;
    raw.as_deref()
        .map(|s| crate::json::from_sql_json(s, "items.entry_json"))
        .transpose()
}

// ---------------------------------------------------------------------------
// v1_done / resolve_v1_token / due_clears
// ---------------------------------------------------------------------------

/// [`crate::Store::v1_done`] — the v1 shim's `done[]` source (DESIGN §11.4).
///
/// `status IN ('finished','error')`, `ORDER BY ord ASC, id ASC`, served off the `(status, ord)`
/// index. `canceled` is excluded even though it is terminal: the shipped iOS `DownloadStatus` has
/// no `canceled` case and maps unknown to `.pending`, so a cancelled row would sit in the client's
/// "In Progress" section forever. Legacy made cancels vanish and this is faithful to that.
///
/// A `limit` keeps the **most recent** rows — the oldest are the ones an operator with a
/// 4 000-row history can afford to lose — but the returned order is still oldest-first.
pub(crate) fn v1_done(conn: &Connection, limit: Option<u32>) -> Result<Vec<Item>, StoreError> {
    let mut out = match limit {
        None => {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM items WHERE status IN ('finished','error') \
                 ORDER BY ord ASC, id ASC"
            ))?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row_to_item(row)?);
            }
            out
        }
        Some(n) => {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM items WHERE status IN ('finished','error') \
                 ORDER BY ord DESC, id DESC LIMIT ?1"
            ))?;
            let mut rows = stmt.query([i64::from(n)])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row_to_item(row)?);
            }
            out.reverse();
            out
        }
    };
    out.shrink_to_fit();
    Ok(out)
}

/// [`crate::Store::resolve_v1_token`] — the DESIGN §11.3 resolution ladder.
///
/// One query, then the ladder is applied to its rows: a ULID that exists wins outright; otherwise
/// every exact `url` match; otherwise every exact `media_id` match; otherwise nothing. Ties resolve
/// to **all** matches, which is what a legacy user expects from a URL-keyed API — the shipped iOS
/// client's `clearCompleted` sends only urls.
pub(crate) fn resolve_v1_token(conn: &Connection, token: &str) -> Result<Vec<ItemId>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, id = ?1 AS by_id, url = ?1 AS by_url, \
                COALESCE(media_id = ?1, 0) AS by_media \
         FROM items WHERE id = ?1 OR url = ?1 OR media_id = ?1 ORDER BY ord ASC, id ASC",
    )?;
    let mut rows = stmt.query([token])?;
    let mut by_id = Vec::new();
    let mut by_url = Vec::new();
    let mut by_media = Vec::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let id = ItemId::from_str(&raw).map_err(|e| StoreError::decode("items.id", e))?;
        if row.get::<_, i64>(1)? != 0 {
            by_id.push(id);
        } else if row.get::<_, i64>(2)? != 0 {
            by_url.push(id);
        } else if row.get::<_, i64>(3)? != 0 {
            by_media.push(id);
        }
    }
    if !by_id.is_empty() {
        return Ok(by_id);
    }
    if !by_url.is_empty() {
        return Ok(by_url);
    }
    Ok(by_media)
}

/// [`crate::Store::due_clears`] — the rows `CLEAR_COMPLETED_AFTER` has come due for
/// (DESIGN §8.10).
pub(crate) fn due_clears(conn: &Connection, now: UnixMs) -> Result<Vec<ItemId>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM items WHERE clear_after IS NOT NULL AND clear_after <= ?1 \
         ORDER BY clear_after ASC, ord ASC",
    )?;
    let mut rows = stmt.query([now])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        out.push(ItemId::from_str(&raw).map_err(|e| StoreError::decode("items.id", e))?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// boot_state
// ---------------------------------------------------------------------------

/// The child counters for every group, in one `GROUP BY` (DESIGN §8.9).
pub(crate) fn group_counts(conn: &Connection) -> Result<HashMap<GroupId, GroupCounts>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT group_id, \
                COUNT(*), \
                SUM(status = 'finished'), \
                SUM(status = 'error'), \
                SUM(status IN ('preparing','downloading','postprocessing')) \
         FROM items WHERE group_id IS NOT NULL GROUP BY group_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let id = ItemId::from_str(&raw).map_err(|e| StoreError::decode("items.group_id", e))?;
        let count = |i: usize| -> Result<u32, StoreError> {
            Ok(u32::try_from(row.get::<_, i64>(i)?).unwrap_or(u32::MAX))
        };
        out.insert(
            id,
            GroupCounts {
                children: count(1)?,
                done: count(2)?,
                error: count(3)?,
                active: count(4)?,
            },
        );
    }
    Ok(out)
}

/// [`crate::Store::boot_state`].
pub(crate) fn boot_state(conn: &Connection, done_window: u32) -> Result<BootState, StoreError> {
    let non_terminal = items(conn, &ItemFilter::non_terminal())?.rows;

    let mut done = items(conn, &{
        let mut f = ItemFilter::terminal();
        if done_window > 0 {
            f.limit = Some(done_window);
        }
        f
    })?;
    // The window is selected newest-first so `LIMIT` keeps the right end; callers want oldest-first.
    done.rows.reverse();

    let items_total: i64 = conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
    let next_clear_at: Option<i64> =
        conn.query_row("SELECT MIN(clear_after) FROM items", [], |r| r.get(0))?;

    Ok(BootState {
        non_terminal,
        done_window: done.rows,
        done_total: done.total,
        items_total: u64::try_from(items_total).unwrap_or(0),
        group_counts: group_counts(conn)?,
        next_clear_at,
    })
}

// ---------------------------------------------------------------------------
// subscription_seen
// ---------------------------------------------------------------------------

/// [`crate::Store::seen`] — the media ids one subscription has already produced items for.
pub(crate) fn seen(conn: &Connection, sub: &SubId) -> Result<HashSet<Box<str>>, StoreError> {
    let mut stmt =
        conn.prepare_cached("SELECT media_id FROM subscription_seen WHERE subscription_id = ?1")?;
    let mut rows = stmt.query([sub.as_str()])?;
    let mut out = HashSet::new();
    while let Some(row) = rows.next()? {
        out.insert(row.get::<_, String>(0)?.into_boxed_str());
    }
    Ok(out)
}

/// The `meta` table as a map, for `healthz` and for the importer's provenance keys.
pub(crate) fn meta(conn: &Connection) -> Result<HashMap<Box<str>, Box<str>>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT key, value FROM meta")?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        out.insert(
            row.get::<_, String>(0)?.into_boxed_str(),
            row.get::<_, String>(1)?.into_boxed_str(),
        );
    }
    Ok(out)
}
