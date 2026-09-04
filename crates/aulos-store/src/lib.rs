//! Persistence: a single SQLite (WAL) connection pool behind one async-friendly [`Store`] handle,
//! forward-only embedded migrations, hi/lo allocators for `ord` and `seq`, typed reads, and the
//! one-shot importer for the legacy `schema_version 2` JSON state files.
//!
//! This is the only crate in the workspace permitted to depend on `rusqlite` (DESIGN §3 rule A2,
//! §7), and `tests/arch.rs` enforces that. Concretely it is what "`aulos-api` never sees SQL"
//! means: the API layer uses the typed reads below and cannot even *name* the `&Connection`
//! parameter of [`Store::read`] without acquiring the dependency the gate forbids.
//!
//! # Shape
//!
//! ```text
//!   callers (async)                    one OS thread                 N OS threads
//!   ───────────────                    ─────────────                 ────────────
//!   Store::write(ops, Durability) ──►  writer: batches up to 256      (read-only,
//!   Store::items/item/…          ──►   jobs into one transaction       WAL: never
//!   Store::next_ord()/next_seq() ──►   reserve-before-use hi/lo        blocked by
//!                                      block in `meta`                the writer)
//! ```
//!
//! Three properties are worth stating because each one is a legacy bug fixed:
//!
//! - **No whole-file rewrites.** Legacy rewrote `queue.json`, `pending.json` and `completed.json`
//!   in full on every state change, with two `fsync`s each. A 500-item playlist here is about two
//!   transactions.
//! - **No progress on disk.** There are no progress columns and no event log. Percent, speed and
//!   ETA live in memory only, which is the one genuinely good property legacy had.
//! - **Ids are reserved before they are used.** An unclean shutdown skips up to a block of `ord`
//!   values and can never re-issue one, so the `UNIQUE` index cannot be tripped by a restart
//!   (DESIGN §4.1).
//!
//! # BRIEF scope trims applied here
//!
//! `aulos-server print-schema` and `repair-ids` are CUT, so there is no JSON-Schema generation for
//! the wire types, and a failed boot consistency check **logs a WARN and continues** instead of
//! refusing to start. Continuing is safe because every counter is seeded with the maximum of all
//! its candidates, so an inconsistent `meta` row costs a gap in the sequence and nothing else;
//! [`Store::id_warnings`] carries what was found so `healthz` can report it. [`schema::dump_schema`]
//! survives because the checked-in `schema.sql` snapshot test needs it.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod actor;
mod alloc;
mod error;
pub mod import;
mod items;
mod json;
mod kv;
mod meta;
mod options;
mod readers;
mod reads;
mod schema;
mod subscriptions;
mod telegram;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

use aulos_core::{
    ChatConfig, EntryBlob, HiLoAllocator, Item, ItemId, Ord0, Seq, SubId, SubscriptionRecord,
    UnixMs,
};
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{Semaphore, oneshot};

use crate::actor::{StoreMetrics, WriteJob, WriteMsg};

pub use crate::alloc::{IdWarning, ORD_BLOCK, SEQ_BLOCK};
pub use crate::error::StoreError;
pub use crate::import::canonical_key;
pub use crate::meta::{IMPORT_KEYS, IMPORT_REPORT, IMPORTED_AT, IMPORTED_FROM};
pub use crate::ops::{Durability, WriteOp, retry_ops};
pub use crate::options::StoreOptions;
pub use crate::reads::{BootState, Cursor, GroupCounts, GroupScope, ItemFilter, Page};
pub use crate::schema::{SCHEMA_VERSION, dump_schema};

mod ops;

/// The `StoreCmd` channel budget (DESIGN §2.3): a full store queue means disk trouble and callers
/// *should* slow down, so the handle awaits a permit rather than growing the queue.
const WRITE_INFLIGHT: usize = 1_024;

/// The one handle to persisted state.
///
/// Cheap to clone (six `Arc`s and a channel sender); every clone talks to the same writer thread
/// and the same read pool. Cloning is the intended way to share it — there is no `Arc<Store>`
/// anywhere in the workspace.
#[derive(Clone)]
pub struct Store {
    w: Sender<WriteMsg>,
    r: Arc<readers::ReadPool>,
    ord: Arc<dyn HiLoAllocator>,
    seq: Arc<dyn HiLoAllocator>,
    metrics: Arc<StoreMetrics>,
    opts: Arc<StoreOptions>,
    write_permits: Arc<Semaphore>,
    id_warnings: Arc<[IdWarning]>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.opts.path)
            .field("readers", &self.opts.effective_readers())
            .field("next_ord", &self.ord.current())
            .field("next_seq", &self.seq.current())
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating if needed) the database, migrates it, seeds and checks both id counters and
    /// starts the writer thread and the read pool.
    ///
    /// Deliberately synchronous: it is called once, from the boot sequence, before the HTTP
    /// listener binds, and making it `async` would only hide the fact that it does blocking disk
    /// work. It is safe to call from inside a tokio runtime.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] when `PRAGMA quick_check` fails, [`StoreError::Migration`] when a
    /// migration cannot be applied, [`StoreError::Io`] when the parent directory cannot be created.
    pub fn open(opts: StoreOptions) -> Result<Self, StoreError> {
        let writer = schema::open_writer(&opts)?;
        seed_meta(&writer)?;
        let allocs = alloc::open(&writer, schema::open_aux(&opts)?, schema::open_aux(&opts)?)?;
        for w in &allocs.warnings {
            tracing::warn!(target: "aulos_store::alloc", "{w}");
        }
        let pool = readers::ReadPool::open(&opts)?;

        let metrics = Arc::new(StoreMetrics::default());
        let (tx, rx) = std::sync::mpsc::channel::<WriteMsg>();
        let thread_opts = opts.clone();
        let thread_seq = Arc::clone(&allocs.seq);
        let thread_metrics = Arc::clone(&metrics);
        std::thread::Builder::new()
            .name("aulos-store-write".to_owned())
            .spawn(move || actor::run(rx, writer, thread_opts, thread_seq, thread_metrics))
            .map_err(|e| StoreError::Io(format!("writer thread: {e}").into_boxed_str()))?;

        Ok(Self {
            w: tx,
            r: Arc::new(pool),
            ord: allocs.ord,
            seq: allocs.seq,
            metrics,
            opts: Arc::new(opts),
            write_permits: Arc::new(Semaphore::new(WRITE_INFLIGHT)),
            id_warnings: allocs.warnings.into(),
        })
    }

    /// The options this store was opened with.
    #[must_use]
    pub fn options(&self) -> &StoreOptions {
        &self.opts
    }

    /// What the DESIGN §4.1 boot consistency checks found. Empty on a healthy database.
    #[must_use]
    pub fn id_warnings(&self) -> &[IdWarning] {
        &self.id_warnings
    }

    // -----------------------------------------------------------------------
    // writes
    // -----------------------------------------------------------------------

    /// Applies `ops` atomically and resolves when the transaction covering them has committed.
    ///
    /// With [`Durability::Batched`] the ops join the writer's current batch; with
    /// [`Durability::Sync`] the batch is cut short and committed under
    /// `PRAGMA synchronous = FULL`. Either way the whole `ops` vector lands in **one** transaction,
    /// so a caller can rely on "the item row and its group's counter changed together".
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when an op targets a row that has been deleted,
    /// [`StoreError::Conflict`] on a constraint violation, [`StoreError::Busy`] when another
    /// process holds the write lock past `busy_timeout`, [`StoreError::Closed`] after
    /// [`Store::close`].
    pub async fn write(&self, ops: Vec<WriteOp>, d: Durability) -> Result<(), StoreError> {
        if ops.is_empty() {
            return Ok(());
        }
        let permit = Arc::clone(&self.write_permits)
            .acquire_owned()
            .await
            .map_err(|_| StoreError::Closed)?;
        let (done, reply) = oneshot::channel();
        self.w
            .send(WriteMsg::Job(WriteJob {
                ops,
                durability: d,
                done,
                _permit: permit,
            }))
            .map_err(|_| StoreError::Closed)?;
        reply.await.map_err(|_| StoreError::Closed)?
    }

    /// Records `meta.seq_hwm_witness`, checkpoints the WAL with `TRUNCATE`, runs
    /// `PRAGMA optimize` and stops both thread pools.
    ///
    /// Skipping this is survivable — the next boot seeds `seq` from `meta.seq_hwm` and every
    /// client resyncs on the new `boot_id` — but it is what keeps the WAL from growing across
    /// restarts.
    ///
    /// # Errors
    /// [`StoreError::Closed`] when the writer is already gone; otherwise whatever the checkpoint
    /// reported.
    pub async fn close(&self) -> Result<(), StoreError> {
        // The read pool goes first: a `wal_checkpoint(TRUNCATE)` cannot reclaim the file while
        // another connection may still be reading it, and a half-checkpointed WAL survives the
        // restart this call exists to prevent.
        self.r.close();
        let (tx, rx) = oneshot::channel();
        let sent = self.w.send(WriteMsg::Close(tx)).is_ok();
        let result = if sent {
            rx.await.map_err(|_| StoreError::Closed)?
        } else {
            Err(StoreError::Closed)
        };
        self.write_permits.close();
        result
    }

    // -----------------------------------------------------------------------
    // reads
    // -----------------------------------------------------------------------

    /// Runs an arbitrary query on a read-pool connection.
    ///
    /// The escape hatch for a caller inside this crate (the importer's verification pass, the
    /// tests). It is `pub` because DESIGN §7.1 declares it, and harmless outside the crate for the
    /// reason given in the module docs: naming `&Connection` requires a `rusqlite` dependency that
    /// `tests/arch.rs` forbids everywhere else.
    ///
    /// # Errors
    /// Whatever `f` returns, or [`StoreError::Closed`] when the pool has stopped.
    pub async fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        self.r.run(f).await
    }

    /// One page of items, `ORDER BY ord ASC, id ASC` unless the filter says otherwise.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn items(&self, f: ItemFilter) -> Result<Page<Item>, StoreError> {
        self.read(move |c| reads::items(c, &f)).await
    }

    /// One item by id, or `None`.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn item(&self, id: ItemId) -> Result<Option<Item>, StoreError> {
        self.read(move |c| reads::item(c, id)).await
    }

    /// Everything boot recovery needs, in one round trip (DESIGN §8.9).
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn boot_state(&self) -> Result<BootState, StoreError> {
        let window = self.opts.done_window;
        self.read(move |c| reads::boot_state(c, window)).await
    }

    /// The v1 shim's `delete`/`start` id resolution ladder (DESIGN §11.3).
    ///
    /// A ULID that exists wins outright; otherwise every exact `url` match; otherwise every exact
    /// `media_id` match; otherwise the empty vector, which the shim records in `skipped`.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn resolve_v1_token(&self, tok: &str) -> Result<Vec<ItemId>, StoreError> {
        let tok = tok.to_owned();
        self.read(move |c| reads::resolve_v1_token(c, &tok)).await
    }

    /// The v1 shim's `done[]` source (DESIGN §11.4).
    ///
    /// `finished` and `error` only — `canceled` is omitted, because the shipped iOS client maps an
    /// unknown status to `.pending` and a cancelled row would sit in "In Progress" forever.
    /// `limit` is tightened by `AULOS_V1_HISTORY_MAX`; `None` and `0` both mean unlimited, which is
    /// the default and full legacy fidelity. A cap keeps the **most recent** rows.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn v1_done(&self, limit: Option<u32>) -> Result<Vec<Item>, StoreError> {
        let effective = self.opts.v1_limit(limit);
        self.read(move |c| reads::v1_done(c, effective)).await
    }

    /// Every subscription, oldest first.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn subscriptions(&self) -> Result<Vec<SubscriptionRecord>, StoreError> {
        self.read(subscriptions::all).await
    }

    /// One subscription by id, or `None`.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn subscription(&self, id: &SubId) -> Result<Option<SubscriptionRecord>, StoreError> {
        let id = id.clone();
        self.read(move |c| subscriptions::one(c, &id)).await
    }

    /// The media ids one subscription has already produced items for (DESIGN §14.3).
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn seen(&self, sub: &SubId) -> Result<HashSet<Box<str>>, StoreError> {
        let sub = sub.clone();
        self.read(move |c| reads::seen(c, &sub)).await
    }

    /// Every chat's stored Telegram defaults.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn telegram_chats(&self) -> Result<HashMap<i64, ChatConfig>, StoreError> {
        self.read(telegram::all).await
    }

    /// The rows `CLEAR_COMPLETED_AFTER` has come due for (DESIGN §8.10).
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn due_clears(&self, now: UnixMs) -> Result<Vec<ItemId>, StoreError> {
        self.read(move |c| reads::due_clears(c, now)).await
    }

    /// The compacted provider entry, for the NFO hook (DESIGN §13.2).
    ///
    /// This is the read `aulos-queue::EngineHookStore` delegates here. The matching *writes*
    /// deliberately do not exist as a `HookStore` impl on this type: they go through the engine so
    /// its item cache and the aggregator's `last_sent` observe them and the change reaches clients
    /// as a `delta` (DESIGN §7.1, §13.3).
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the row is gone.
    pub async fn entry_blob(&self, id: ItemId) -> Result<Option<EntryBlob>, StoreError> {
        self.read(move |c| reads::entry_blob(c, id)).await
    }

    /// One `kv` key, or `None`.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn kv_get(&self, key: &str) -> Result<Option<Value>, StoreError> {
        let key = key.to_owned();
        self.read(move |c| kv::get(c, &key)).await
    }

    /// Every `kv` key.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn kv_all(&self) -> Result<HashMap<Box<str>, Value>, StoreError> {
        self.read(kv::all).await
    }

    /// The `meta` table: `schema_version`, `instance_id`, the id high-water marks and the
    /// importer's provenance keys.
    ///
    /// # Errors
    /// [`StoreError`] on a decode failure or an unreachable pool.
    pub async fn meta(&self) -> Result<HashMap<Box<str>, Box<str>>, StoreError> {
        self.read(reads::meta).await
    }

    // -----------------------------------------------------------------------
    // ids
    // -----------------------------------------------------------------------

    /// The next `ord` — the client's sort key (DESIGN §4.1).
    #[must_use]
    pub fn next_ord(&self) -> Ord0 {
        self.ord.next()
    }

    /// The next `seq` — the protocol cursor (DESIGN §4.1).
    ///
    /// Never negative in practice: the counter starts at 0 and only increases.
    #[must_use]
    pub fn next_seq(&self) -> Seq {
        Seq(self.seq.next().max(0).unsigned_abs())
    }

    /// The `ord` counter as a trait object.
    #[must_use]
    pub fn ord_allocator(&self) -> Arc<dyn HiLoAllocator> {
        Arc::clone(&self.ord)
    }

    /// The `seq` counter as a trait object, so `EventHub::new` (DESIGN §15.3) can be handed it
    /// directly instead of reaching into the store on every frame.
    #[must_use]
    pub fn seq_allocator(&self) -> Arc<dyn HiLoAllocator> {
        Arc::clone(&self.seq)
    }

    // -----------------------------------------------------------------------
    // health
    // -----------------------------------------------------------------------

    /// The size of the write-ahead log, in bytes.
    ///
    /// `healthz` reports it and flips the store component to `down` above 256 MB (DESIGN §16.3);
    /// `0` when the WAL has been checkpointed away or cannot be stat'ed.
    #[must_use]
    pub fn wal_bytes(&self) -> u64 {
        let mut wal = self.opts.path.clone().into_os_string();
        wal.push("-wal");
        std::fs::metadata(wal).map_or(0, |m| m.len())
    }

    /// The size of the database file itself, in bytes (DESIGN §16.3).
    #[must_use]
    pub fn db_bytes(&self) -> u64 {
        std::fs::metadata(&self.opts.path).map_or(0, |m| m.len())
    }

    /// How many transactions the writer has committed, counted by SQLite's own `commit_hook`.
    ///
    /// Exposed because "did the writer really batch that?" is otherwise unobservable, and the
    /// DESIGN §7.1 batching claim is the whole reason the writer is a single thread.
    #[must_use]
    pub fn commit_count(&self) -> u64 {
        self.metrics.commits.load(Ordering::Relaxed)
    }

    /// How many `write()` jobs have been applied.
    #[must_use]
    pub fn job_count(&self) -> u64 {
        self.metrics.jobs.load(Ordering::Relaxed)
    }
}

/// `instance_id` and `created_at`, written once and never again.
fn seed_meta(conn: &Connection) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES('instance_id', ?1)",
        [ulid::Ulid::new().to_string()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES('created_at', ?1)",
        [actor::now_ms().to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle is shared by cloning, from every task in the process, so both bounds matter.
    #[test]
    fn the_handle_is_send_and_sync() {
        const fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<Store>();
    }
}
