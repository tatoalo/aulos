//! The single writer thread (DESIGN §7.1).
//!
//! One dedicated OS thread — `std::thread::spawn`, deliberately **not** a tokio worker — owns the
//! one read-write connection for the life of the process. Nothing else in the workspace holds a
//! `rusqlite::Connection` that can write, so "who is writing to SQLite right now" has exactly one
//! answer and no `Connection` ever crosses an `await`.
//!
//! The loop drains up to [`MAX_BATCH`] jobs into **one** transaction, extending the batch until
//! `AULOS_DB_FLUSH_MS` expires. That is what turns a 500-item playlist into ~2 transactions
//! instead of legacy's 500 whole-file JSON rewrites with 1 000 `fsync`s. A
//! [`Durability::Sync`] job short-circuits the extension and commits immediately, with
//! `PRAGMA synchronous = FULL` for the duration.
//!
//! **Poison isolation.** If any op in a batch fails, the batch is rolled back and then replayed
//! one job at a time, each in its own transaction. Without that, one bad write — a `NotFound`
//! against a row a concurrent delete removed, say — would fail the 255 unrelated writes that
//! happened to share its batch.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aulos_core::{HiLoAllocator, UnixMs};
use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::error::StoreError;
use crate::ops::{Durability, WriteOp};
use crate::options::StoreOptions;
use crate::{alloc, items, kv, meta, schema, subscriptions, telegram};

/// The most jobs one transaction may cover (DESIGN §7.1).
pub(crate) const MAX_BATCH: usize = 256;

/// Counters the handle exposes for `healthz` and for the batching tests.
#[derive(Debug, Default)]
pub(crate) struct StoreMetrics {
    /// How many transactions the writer has committed. Incremented from SQLite's own
    /// `commit_hook`, so it counts what the database did rather than what the code intended.
    pub commits: AtomicU64,
    /// How many jobs the writer has applied.
    pub jobs: AtomicU64,
    /// How many batches were replayed one job at a time after a failure.
    pub poison_replays: AtomicU64,
}

/// One `write()` call in flight.
pub(crate) struct WriteJob {
    pub ops: Vec<WriteOp>,
    pub durability: Durability,
    pub done: oneshot::Sender<Result<(), StoreError>>,
    /// Held for the job's life so the handle's in-flight budget is released on completion, not on
    /// submission.
    pub _permit: tokio::sync::OwnedSemaphorePermit,
}

/// What the writer thread receives.
pub(crate) enum WriteMsg {
    /// Apply these ops.
    Job(WriteJob),
    /// Record the shutdown witness, checkpoint, optimize and exit.
    Close(oneshot::Sender<Result<(), StoreError>>),
    /// `PRAGMA optimize` + `wal_checkpoint(TRUNCATE)` and **keep running** (DESIGN §7.1's
    /// six-hourly half). It goes through the writer because the writer owns the only connection
    /// that may hold the write lock; a read-pool connection cannot truncate the WAL.
    Checkpoint(oneshot::Sender<Result<(), StoreError>>),
}

/// Unix milliseconds. The writer stamps `updated_at` for the ops that carry no `at` of their own.
pub(crate) fn now_ms() -> UnixMs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Applies one op by asking each table's module in turn.
fn apply_one(
    conn: &Connection,
    op: &WriteOp,
    entry_max_bytes: u64,
    now: UnixMs,
) -> Result<(), StoreError> {
    if items::apply(conn, op, entry_max_bytes, now)?
        || subscriptions::apply(conn, op, now)?
        || telegram::apply(conn, op, now)?
        || kv::apply(conn, op, now)?
        || meta::apply(conn, op, now)?
    {
        Ok(())
    } else {
        // Unreachable while every variant is covered; a new variant with no handler must not be
        // silently dropped.
        Err(StoreError::Sqlite(
            format!("no handler for WriteOp::{}", op.name()).into_boxed_str(),
        ))
    }
}

/// Applies every job in one transaction. On failure the transaction is rolled back and the caller
/// replays the jobs individually.
fn commit_batch(
    conn: &Connection,
    batch: &[WriteJob],
    entry_max_bytes: u64,
) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    let now = now_ms();
    for job in batch {
        for op in &job.ops {
            apply_one(&tx, op, entry_max_bytes, now)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Applies one job in its own transaction.
fn commit_one(conn: &Connection, job: &WriteJob, entry_max_bytes: u64) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    let now = now_ms();
    for op in &job.ops {
        apply_one(&tx, op, entry_max_bytes, now)?;
    }
    tx.commit()?;
    Ok(())
}

/// The writer thread body.
///
/// Everything is taken by value because the thread *is* the owner: the connection lives exactly as
/// long as this call, and handing it a reference would mean something else outlives it.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run(
    rx: Receiver<WriteMsg>,
    conn: Connection,
    opts: StoreOptions,
    seq: Arc<dyn HiLoAllocator>,
    metrics: Arc<StoreMetrics>,
) {
    // SQLite tells us when a transaction actually commits, which is what the batching test
    // asserts against — a counter the code bumps itself would only prove the code's intent.
    {
        let commits = Arc::clone(&metrics);
        conn.commit_hook(Some(move || {
            commits.commits.fetch_add(1, Ordering::Relaxed);
            false
        }));
    }

    let flush = Duration::from_millis(opts.flush_ms);
    loop {
        let Ok(first) = rx.recv() else { break };
        let mut batch: Vec<WriteJob> = Vec::new();
        let mut close: Option<oneshot::Sender<Result<(), StoreError>>> = None;
        let mut checkpoint: Option<oneshot::Sender<Result<(), StoreError>>> = None;
        let mut sync_now = false;
        match first {
            WriteMsg::Job(j) => {
                sync_now = j.durability == Durability::Sync;
                batch.push(j);
            }
            WriteMsg::Close(tx) => close = Some(tx),
            WriteMsg::Checkpoint(tx) => checkpoint = Some(tx),
        }

        // Extend the batch until the flush window closes, 256 jobs have accumulated, or a `Sync`
        // job demands the platter now.
        if close.is_none() && !flush.is_zero() {
            let deadline = Instant::now() + flush;
            while batch.len() < MAX_BATCH && !sync_now {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(WriteMsg::Job(j)) => {
                        sync_now |= j.durability == Durability::Sync;
                        batch.push(j);
                    }
                    Ok(WriteMsg::Close(tx)) => {
                        close = Some(tx);
                        break;
                    }
                    // The checkpoint runs after this batch commits, not instead of it.
                    Ok(WriteMsg::Checkpoint(tx)) => {
                        checkpoint = Some(tx);
                        break;
                    }
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        // Whatever is already queued costs nothing to include, `Sync` or not.
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(WriteMsg::Job(j)) => batch.push(j),
                Ok(WriteMsg::Close(tx)) => {
                    close = Some(tx);
                    break;
                }
                Ok(WriteMsg::Checkpoint(tx)) => {
                    checkpoint = Some(tx);
                    break;
                }
                Err(_) => break,
            }
        }

        if !batch.is_empty() {
            if sync_now {
                let _ = conn.pragma_update(None, "synchronous", "FULL");
            }
            match commit_batch(&conn, &batch, opts.entry_max_bytes) {
                Ok(()) => {
                    metrics
                        .jobs
                        .fetch_add(batch.len() as u64, Ordering::Relaxed);
                    for job in batch.drain(..) {
                        let _ = job.done.send(Ok(()));
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, jobs = batch.len(),
                        "write batch failed; replaying jobs individually");
                    metrics.poison_replays.fetch_add(1, Ordering::Relaxed);
                    for job in batch.drain(..) {
                        let result = commit_one(&conn, &job, opts.entry_max_bytes);
                        if result.is_ok() {
                            metrics.jobs.fetch_add(1, Ordering::Relaxed);
                        }
                        let _ = job.done.send(result);
                    }
                }
            }
            if sync_now {
                let _ = conn.pragma_update(None, "synchronous", opts.synchronous_pragma());
            }
        }

        if let Some(tx) = checkpoint {
            let _ = tx.send(schema::checkpoint_and_optimize(&conn));
        }

        if let Some(tx) = close {
            let _ = tx.send(shutdown(&conn, seq.as_ref()));
            return;
        }
    }
    // The handle was dropped without `close()`. Still record the witness: an unclean `seq_hwm`
    // costs every connected client a full resync on the next boot.
    let _ = shutdown(&conn, seq.as_ref());
}

/// `meta.seq_hwm_witness`, then `wal_checkpoint(TRUNCATE)` and `PRAGMA optimize` (DESIGN §7.1).
fn shutdown(conn: &Connection, seq: &dyn HiLoAllocator) -> Result<(), StoreError> {
    alloc::write_witness(conn, seq.current())?;
    schema::checkpoint_and_optimize(conn)
}
