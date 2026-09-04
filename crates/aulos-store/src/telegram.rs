//! The `telegram_chats` table (DESIGN §7.2, §12.2).

use std::collections::HashMap;

use aulos_core::{ChatConfig, UnixMs};
use rusqlite::{Connection, params};

use crate::error::StoreError;
use crate::json::{from_sql_json, to_sql_json};
use crate::ops::WriteOp;

/// Applies one Telegram-shaped op. `Ok(false)` when the op is not one.
pub(crate) fn apply(conn: &Connection, op: &WriteOp, now: UnixMs) -> Result<bool, StoreError> {
    match op {
        WriteOp::UpsertTelegramChat { chat_id, config } => {
            conn.prepare_cached(
                "INSERT INTO telegram_chats (chat_id, config_json, updated_at) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(chat_id) DO UPDATE SET \
                   config_json = excluded.config_json, updated_at = excluded.updated_at",
            )?
            .execute(params![
                chat_id,
                to_sql_json(config, "telegram_chats.config_json")?,
                now
            ])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// [`crate::Store::telegram_chats`] — every chat's stored defaults.
pub(crate) fn all(conn: &Connection) -> Result<HashMap<i64, ChatConfig>, StoreError> {
    let mut stmt = conn.prepare_cached("SELECT chat_id, config_json FROM telegram_chats")?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        let chat_id: i64 = row.get(0)?;
        let raw: String = row.get(1)?;
        out.insert(
            chat_id,
            from_sql_json::<ChatConfig>(&raw, "telegram_chats.config_json")?,
        );
    }
    Ok(out)
}
