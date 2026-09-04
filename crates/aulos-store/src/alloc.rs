//! The durable, reserve-before-use hi/lo allocators for `ord` and `seq` (DESIGN §4.1, §7.1).
//!
//! One implementation, two instances, seeded differently — because only one of the two counters
//! has a column to compare against:
//!
//! | Counter | `meta` key | Block | Seeded with | Boot check |
//! |---|---|---|---|---|
//! | `ord` | `ord_hwm` | 256 | `max(meta.ord_hwm, COALESCE(MAX(items.ord), -1) + 1)` | `meta.ord_hwm` must not be *behind* the table |
//! | `seq` | `seq_hwm` | 1024 | `meta.seq_hwm` | must be present, parseable, and not below `meta.seq_hwm_witness` |
//!
//! There is deliberately no `seq` column: a frame sequence orders *frames*, and frames are not
//! persisted. Comparing the frame counter against the record-order column is meaningless, and an
//! earlier draft of the boot check that did exactly that would have refused every healthy database
//! with more than 1024 frames of history.
//!
//! **A block is reserved before any value from it is handed out**, so a crash skips up to
//! `block - 1` values and can never re-issue one. That is the fix for "an unclean shutdown
//! re-issues used `ord` values and every subsequent insert dies on the UNIQUE index" and for
//! "a client resuming with `since=10251` after a restart presents a cursor above the new head".

use std::sync::{Arc, Mutex};

use aulos_core::HiLoAllocator;
use rusqlite::Connection;

use crate::error::StoreError;

/// The `ord` block size (DESIGN §4.1).
pub const ORD_BLOCK: i64 = 256;
/// The `seq` block size (DESIGN §4.1).
pub const SEQ_BLOCK: i64 = 1_024;

/// The `meta` key holding the `ord` high-water mark.
pub const ORD_HWM: &str = "ord_hwm";
/// The `meta` key holding the `seq` high-water mark.
pub const SEQ_HWM: &str = "seq_hwm";
/// The `meta` key written on every graceful shutdown; the `seq` boot check compares against it.
pub const SEQ_HWM_WITNESS: &str = "seq_hwm_witness";

/// An inconsistency the boot consistency checks found in the durable counters (DESIGN §4.1).
///
/// v1.0: `aulos-server repair-ids` is CUT by the BRIEF, so a failed check **logs a WARN and
/// continues** rather than refusing to start. Continuing is safe because the seed is always the
/// maximum of every candidate, so no value that could already be in use is ever re-issued — the
/// only cost of an inconsistent `meta` row is a gap in the sequence. The warnings are kept on the
/// handle so `healthz` and the tests can see what was found.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum IdWarning {
    /// `meta.ord_hwm` was behind `COALESCE(MAX(items.ord), -1) + 1`: a reserved block was consumed
    /// and lost. Without the repair the next insert would re-issue an `ord` and die on the UNIQUE
    /// index, so the counter is advanced past the table instead.
    OrdHwmBehind {
        /// What `meta.ord_hwm` said.
        hwm: i64,
        /// What the table needs, i.e. `MAX(ord) + 1`.
        needed: i64,
    },
    /// `meta.seq_hwm` was absent or unparseable. Seeded from `meta.seq_hwm_witness`, or from 0.
    SeqHwmMissing {
        /// The raw column value, when there was one.
        found: Option<Box<str>>,
    },
    /// `meta.seq_hwm` was below the value the previous process recorded on shutdown.
    SeqHwmBehindWitness {
        /// What `meta.seq_hwm` said.
        hwm: i64,
        /// What `meta.seq_hwm_witness` said.
        witness: i64,
    },
}

impl std::fmt::Display for IdWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OrdHwmBehind { hwm, needed } => write!(
                f,
                "meta.ord_hwm ({hwm}) is behind MAX(items.ord) + 1 ({needed}): a reserved block \
                 was lost to an unclean shutdown. The allocator has been advanced to {needed} so \
                 no `ord` is re-issued; run `aulos-server import`/restore from backup if the \
                 database looks otherwise wrong."
            ),
            Self::SeqHwmMissing { found } => write!(
                f,
                "meta.seq_hwm is absent or unparseable (found {found:?}); the frame cursor has \
                 been reseeded. Connected clients will be handed a full snapshot, which the new \
                 boot_id already forces."
            ),
            Self::SeqHwmBehindWitness { hwm, witness } => write!(
                f,
                "meta.seq_hwm ({hwm}) is below meta.seq_hwm_witness ({witness}) recorded on the \
                 last graceful shutdown; the frame cursor has been advanced to {witness}."
            ),
        }
    }
}

/// A reserved half-open range plus the last value handed out.
#[derive(Debug)]
struct Cursor {
    /// The next value to hand out.
    next: i64,
    /// One past the end of the reserved block.
    limit: i64,
    /// The last value handed out; `seed - 1` before the first call.
    last: i64,
}

/// One durable counter.
pub(crate) struct HiLo {
    key: &'static str,
    block: i64,
    /// A dedicated read-write connection, so a reservation never has to wait behind the writer's
    /// batch window. WAL plus `busy_timeout` serialises the two.
    conn: Mutex<Connection>,
    cursor: Mutex<Cursor>,
}

impl HiLo {
    /// Wraps a seeded counter. `seed` is the first value that may be handed out.
    fn new(key: &'static str, block: i64, conn: Connection, seed: i64) -> Self {
        Self {
            key,
            block,
            conn: Mutex::new(conn),
            cursor: Mutex::new(Cursor {
                next: seed,
                limit: seed,
                last: seed - 1,
            }),
        }
    }

    /// `BEGIN IMMEDIATE; UPDATE meta …; SELECT value; COMMIT` — the in-memory cursor is only
    /// advanced **after** the commit returns.
    fn reserve(&self, from: i64) -> Result<i64, StoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| StoreError::Sqlite("allocator connection poisoned".into()))?;
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<i64, StoreError> {
            // `from` guards against a `meta` row that somehow moved backwards while we ran.
            conn.execute(
                "UPDATE meta SET value = CAST(MAX(CAST(value AS INTEGER), ?2) + ?3 AS TEXT) \
                 WHERE key = ?1",
                rusqlite::params![self.key, from, self.block],
            )?;
            let end: i64 = conn.query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?1",
                [self.key],
                |r| r.get(0),
            )?;
            Ok(end)
        })();
        match result {
            Ok(end) => {
                conn.execute_batch("COMMIT")?;
                Ok(end)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

impl HiLoAllocator for HiLo {
    fn next(&self) -> i64 {
        let Ok(mut cur) = self.cursor.lock() else {
            // A poisoned mutex means a panic while a block was being handed out. Returning a
            // value we cannot prove is unused would be worse than a visible, monotonic guess.
            tracing::error!(key = self.key, "id allocator mutex poisoned");
            return i64::MAX;
        };
        if cur.next >= cur.limit {
            match self.reserve(cur.next) {
                Ok(end) => {
                    cur.next = end - self.block;
                    cur.limit = end;
                }
                Err(e) => {
                    // The counter must keep increasing even when the disk is unhappy: the caller
                    // has an item to insert either way, and the write will fail with its own,
                    // more informative error. Extending in memory can only skip values.
                    tracing::error!(key = self.key, error = %e, "could not reserve an id block");
                    cur.limit = cur.next + self.block;
                }
            }
        }
        let v = cur.next;
        cur.next += 1;
        cur.last = v;
        v
    }

    fn current(&self) -> i64 {
        self.cursor.lock().map_or(i64::MAX, |c| c.last)
    }
}

/// Both counters, seeded and checked (DESIGN §4.1).
pub(crate) struct Allocators {
    /// The `ord` counter — the client sort key.
    pub ord: Arc<dyn HiLoAllocator>,
    /// The `seq` counter — the protocol cursor.
    pub seq: Arc<dyn HiLoAllocator>,
    /// What the boot consistency checks found.
    pub warnings: Vec<IdWarning>,
}

/// Reads a `meta` integer, distinguishing "absent" from "unparseable".
fn meta_int(conn: &Connection, key: &str) -> Result<Option<Result<i64, Box<str>>>, StoreError> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(StoreError::from(other)),
        })?;
    Ok(raw.map(|s| s.trim().parse::<i64>().map_err(|_| s.into_boxed_str())))
}

fn write_meta(conn: &Connection, key: &str, value: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value.to_string()],
    )?;
    Ok(())
}

/// Seeds both counters, runs both boot consistency checks and returns the handles.
///
/// `writer` is used for the seeding writes (it is the only connection alive at this point);
/// `ord_conn` and `seq_conn` become the two allocators' private reservation channels.
pub(crate) fn open(
    writer: &Connection,
    ord_conn: Connection,
    seq_conn: Connection,
) -> Result<Allocators, StoreError> {
    let mut warnings = Vec::new();

    // --- ord: seeded from max(meta.ord_hwm, MAX(items.ord) + 1) --------------------------------
    let needed: i64 =
        writer.query_row("SELECT COALESCE(MAX(ord), -1) + 1 FROM items", [], |r| {
            r.get(0)
        })?;
    let ord_hwm = match meta_int(writer, ORD_HWM)? {
        Some(Ok(v)) => v,
        Some(Err(found)) => {
            tracing::warn!(key = ORD_HWM, %found, "meta.ord_hwm is unparseable; reseeding");
            0
        }
        None => 0,
    };
    // A fresh database has `ord_hwm` absent (0) and `needed` 0, which is not an inconsistency.
    if ord_hwm < needed && needed > 0 {
        let w = IdWarning::OrdHwmBehind {
            hwm: ord_hwm,
            needed,
        };
        tracing::warn!("{w}");
        warnings.push(w);
    }
    let ord_seed = ord_hwm.max(needed);
    write_meta(writer, ORD_HWM, ord_seed)?;

    // --- seq: seeded from meta.seq_hwm, checked against meta.seq_hwm_witness -------------------
    let witness = match meta_int(writer, SEQ_HWM_WITNESS)? {
        Some(Ok(v)) => Some(v),
        _ => None,
    };
    let seq_seed = match meta_int(writer, SEQ_HWM)? {
        Some(Ok(hwm)) => match witness {
            Some(w) if hwm < w => {
                let warn = IdWarning::SeqHwmBehindWitness { hwm, witness: w };
                tracing::warn!("{warn}");
                warnings.push(warn);
                w
            }
            _ => hwm,
        },
        found => {
            let raw = match found {
                Some(Err(s)) => Some(s),
                _ => None,
            };
            // An absent `seq_hwm` on a database that has never been opened is normal; on one that
            // recorded a witness it is not.
            if raw.is_some() || witness.is_some() {
                let warn = IdWarning::SeqHwmMissing { found: raw };
                tracing::warn!("{warn}");
                warnings.push(warn);
            }
            witness.unwrap_or(0)
        }
    };
    write_meta(writer, SEQ_HWM, seq_seed)?;

    Ok(Allocators {
        ord: Arc::new(HiLo::new(ORD_HWM, ORD_BLOCK, ord_conn, ord_seed)),
        seq: Arc::new(HiLo::new(SEQ_HWM, SEQ_BLOCK, seq_conn, seq_seed)),
        warnings,
    })
}

/// Records `meta.seq_hwm_witness` on graceful shutdown (DESIGN §7.2).
pub(crate) fn write_witness(conn: &Connection, seq_current: i64) -> Result<(), StoreError> {
    write_meta(conn, SEQ_HWM_WITNESS, seq_current)
}
