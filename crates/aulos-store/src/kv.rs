//! The `kv` table: runtime overrides (the uploaded cookiefile) and hook bookkeeping
//! (DESIGN §7.2, §13.1).

use std::collections::HashMap;

use aulos_core::UnixMs;
use rusqlite::{Connection, params};
use serde_json::Value;

use crate::error::StoreError;
use crate::json::{from_sql_json, to_sql_json};
use crate::ops::WriteOp;

/// Applies one kv-shaped op. `Ok(false)` when the op is not one.
pub(crate) fn apply(conn: &Connection, op: &WriteOp, now: UnixMs) -> Result<bool, StoreError> {
    match op {
        WriteOp::SetKv { key, value } => {
            match value {
                Some(v) => {
                    conn.prepare_cached(
                        "INSERT INTO kv (key, value_json, updated_at) VALUES (?1, ?2, ?3) \
                         ON CONFLICT(key) DO UPDATE SET \
                           value_json = excluded.value_json, updated_at = excluded.updated_at",
                    )?
                    .execute(params![
                        &**key,
                        to_sql_json(v, "kv.value_json")?,
                        now
                    ])?;
                }
                None => {
                    conn.prepare_cached("DELETE FROM kv WHERE key = ?1")?
                        .execute([&**key])?;
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// One key, or `None` when it is not set.
pub(crate) fn get(conn: &Connection, key: &str) -> Result<Option<Value>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT value_json FROM kv WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => {
            let raw: String = row.get(0)?;
            Ok(Some(from_sql_json(&raw, "kv.value_json")?))
        }
        None => Ok(None),
    }
}

/// Every key, for `aulos-server doctor` and the tests.
pub(crate) fn all(conn: &Connection) -> Result<HashMap<Box<str>, Value>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT key, value_json FROM kv")?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let raw: String = row.get(1)?;
        out.insert(
            key.into_boxed_str(),
            from_sql_json::<Value>(&raw, "kv.value_json")?,
        );
    }
    Ok(out)
}
