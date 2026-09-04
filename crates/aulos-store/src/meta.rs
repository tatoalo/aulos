//! The `meta` table's write path (DESIGN §7.2, §7.6.6).
//!
//! `meta` already had two writers before this module: [`crate::schema`] seeds `schema_version` and
//! [`crate::alloc`] owns `ord_hwm` / `seq_hwm` / `seq_hwm_witness`, both through their own
//! connections at open time. The importer needs something neither of those can give it — a `meta`
//! write **inside the same transaction as the rows** (DESIGN §7.6.6: "on success, *in the same
//! transaction*: `meta.imported_from`, `meta.imported_at`, `meta.import_report`") — so it goes
//! through the writer actor like every other mutation, as [`WriteOp::SetMeta`].
//!
//! Reads go through [`crate::Store::meta`], which already returns the whole table.

use aulos_core::UnixMs;
use rusqlite::{Connection, params};

use crate::error::StoreError;
use crate::ops::WriteOp;

/// `meta.imported_from` — the `STATE_DIR` an import read.
pub const IMPORTED_FROM: &str = "imported_from";

/// `meta.imported_at` — when it ran, unix ms, as a decimal string.
pub const IMPORTED_AT: &str = "imported_at";

/// `meta.import_report` — the whole [`crate::import::ImportReport`] as JSON.
pub const IMPORT_REPORT: &str = "import_report";

/// The provenance keys, so a test can assert the set is complete.
pub const IMPORT_KEYS: [&str; 3] = [IMPORTED_FROM, IMPORTED_AT, IMPORT_REPORT];

/// Builds a `meta` write.
pub(crate) fn set(key: &str, value: impl Into<Box<str>>) -> WriteOp {
    WriteOp::SetMeta {
        key: key.into(),
        value: value.into(),
    }
}

/// Applies one `meta`-shaped op. `Ok(false)` when the op is not one.
///
/// `meta` has no `updated_at` column — it is a flat key/value table (DESIGN §7.2) — so `now` is
/// accepted only to keep the handler signature uniform with the other tables'.
pub(crate) fn apply(conn: &Connection, op: &WriteOp, _now: UnixMs) -> Result<bool, StoreError> {
    match op {
        WriteOp::SetMeta { key, value } => {
            conn.prepare_cached(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?
            .execute(params![&**key, &**value])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provenance_keys_are_the_documented_three() {
        assert_eq!(
            IMPORT_KEYS,
            ["imported_from", "imported_at", "import_report"]
        );
    }

    #[test]
    fn set_builds_the_op() {
        match set(IMPORTED_AT, "17") {
            WriteOp::SetMeta { key, value } => {
                assert_eq!(&*key, "imported_at");
                assert_eq!(&*value, "17");
            }
            other => panic!("expected SetMeta, got {}", other.name()),
        }
    }
}
