//! Migrations, the pragma set, the integrity check and the DDL dump (DESIGN §7.1–7.3).

use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use rusqlite_migration::{M, Migrations};

use crate::error::StoreError;
use crate::options::StoreOptions;

/// `meta.schema_version` — written for human inspection alongside `user_version`, which
/// `rusqlite_migration` owns (DESIGN §7.3).
pub const SCHEMA_VERSION: u32 = 2;

/// Migration `0001_init.sql`, embedded so a fresh container needs no data files.
const M0001: &str = include_str!("../migrations/0001_init.sql");

/// Migration `0002_clear_finished_msg.sql`: the backfill for the stale `"MoveFiles…"` line a
/// pre-fix build persisted onto every finished row (PROTOCOL §2.3).
const M0002: &str = include_str!("../migrations/0002_clear_finished_msg.sql");

/// The forward-only migration set. Additive, one up-only file per migration.
///
/// A migration may be **data-only**: `0002` changes no DDL, so it leaves the checked-in
/// `schema.sql` dump untouched and only rewrites rows an older build wrote wrong.
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(M0001), M::up(M0002)])
}

/// Applies the DESIGN §7.1 pragma set to a read-write connection.
///
/// `journal_mode = WAL` is set here rather than in `0001_init.sql` because SQLite refuses to
/// change the journal mode inside the transaction `rusqlite_migration` wraps every migration in.
fn apply_pragmas(conn: &Connection, opts: &StoreOptions) -> Result<(), StoreError> {
    // `journal_mode` answers with the mode it settled on, so it is set through the checking form.
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Corrupt(
            format!("could not enable WAL (journal_mode is {mode:?})").into_boxed_str(),
        ));
    }
    conn.pragma_update(None, "synchronous", opts.synchronous_pragma())?;
    conn.busy_timeout(Duration::from_millis(opts.busy_timeout_ms))?;
    conn.pragma_update(None, "wal_autocheckpoint", 512)?;
    conn.pragma_update(None, "journal_size_limit", 67_108_864)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", 67_108_864)?;
    conn.pragma_update(None, "cache_size", -16_000)?;
    Ok(())
}

/// The subset of the pragma set a `SQLITE_OPEN_READ_ONLY` connection can meaningfully set.
fn apply_reader_pragmas(conn: &Connection, opts: &StoreOptions) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_millis(opts.busy_timeout_ms))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", 67_108_864)?;
    conn.pragma_update(None, "cache_size", -16_000)?;
    Ok(())
}

/// `PRAGMA quick_check` (DESIGN §7.1): run at open, before any migration.
///
/// A corrupt file is a documented, valid recovery path — delete it and restart, and the legacy
/// JSON re-imports — so this fails loudly rather than trying to repair anything.
fn quick_check(conn: &Connection) -> Result<(), StoreError> {
    let verdict: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if verdict.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(StoreError::Corrupt(verdict.into_boxed_str()))
    }
}

/// Opens the writer connection, checks it and migrates it to the latest schema.
pub(crate) fn open_writer(opts: &StoreOptions) -> Result<Connection, StoreError> {
    if let Some(parent) = opts.path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| StoreError::Io(format!("{}: {e}", parent.display()).into_boxed_str()))?;
    }
    let mut conn = Connection::open_with_flags(
        &opts.path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    apply_pragmas(&conn, opts)?;
    quick_check(&conn)?;
    migrations()
        .to_latest(&mut conn)
        .map_err(|e| StoreError::Migration(e.to_string().into_boxed_str()))?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES('schema_version', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(conn)
}

/// Opens an auxiliary read-write connection (the hi/lo allocators' reservation channel).
pub(crate) fn open_aux(opts: &StoreOptions) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        &opts.path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    apply_pragmas(&conn, opts)?;
    Ok(conn)
}

/// Opens one of the read pool's `SQLITE_OPEN_READ_ONLY` connections.
pub(crate) fn open_reader(opts: &StoreOptions) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        &opts.path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    apply_reader_pragmas(&conn, opts)?;
    Ok(conn)
}

/// The DDL as SQLite itself reports it, for the checked-in `schema.sql` snapshot (DESIGN §7.3).
///
/// Tables first, then indexes, each group ordered by name, so the dump is stable across SQLite
/// versions and across the order the migrations happened to create things in. Implicit indexes
/// (`sqlite_autoindex_*`) carry no `sql` and are skipped.
///
/// # Errors
/// [`StoreError`] when `sqlite_master` cannot be read.
pub fn dump_schema(conn: &Connection) -> Result<String, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master \
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
         ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name",
    )?;
    let mut out = String::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let sql: String = row.get(0)?;
        out.push_str(sql.trim());
        out.push_str(";\n");
    }
    Ok(out)
}

/// `PRAGMA optimize` followed by `PRAGMA wal_checkpoint(TRUNCATE)` (DESIGN §7.1).
///
/// Run on graceful shutdown and every six hours, so a long-lived container does not accumulate a
/// multi-hundred-megabyte WAL that flips the `healthz` store component to `down`.
///
/// The order is the reverse of the one DESIGN §7.1 lists them in, deliberately: `optimize` may run
/// `ANALYZE`, which **writes** `sqlite_stat1`, so checkpointing first leaves those writes behind in
/// a fresh WAL and the file the next boot opens is not the empty one the checkpoint promised.
pub(crate) fn checkpoint_and_optimize(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch("PRAGMA optimize")?;
    // The first column is 1 when the checkpoint could not run to completion because another
    // connection was still reading. That is worth a line in the log — a WAL that never shrinks is
    // how the `healthz` store component ends up `down` after a few weeks.
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    if busy != 0 {
        tracing::warn!("wal_checkpoint(TRUNCATE) was blocked by a live reader; the WAL was kept");
    }
    Ok(())
}
