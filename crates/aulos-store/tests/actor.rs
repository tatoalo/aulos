//! The writer actor: transaction batching, ordering, the `Sync` short-circuit, the `SQLITE_BUSY`
//! mapping and the WAL gauge (DESIGN §7.1, §16.3; PLAN WP-04).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::{Duration, Instant};

use aulos_core::{FieldUpdate, Status};
use aulos_store::{Durability, ItemFilter, Store, StoreError, WriteOp};
use rusqlite::Connection;

/// 500 concurrent inserts must land in at most eight transactions, and none may be lost.
///
/// The count comes from SQLite's own `commit_hook`, so it measures what the database did rather
/// than what the code meant to do — which is the only way this claim is worth making.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_inserts_commit_in_at_most_eight_transactions() {
    let dir = tempfile::tempdir().unwrap();
    // The production 200 ms flush window, deliberately: the claim is about the default.
    let store = Store::open(support::options(dir.path()).with_flush_ms(200)).unwrap();
    let before = store.commit_count();

    let mut tasks = Vec::with_capacity(500);
    for ord in 0..500 {
        let s = store.clone();
        tasks.push(tokio::spawn(async move {
            s.write(
                vec![WriteOp::InsertItems {
                    items: vec![support::item(ord)],
                }],
                Durability::Batched,
            )
            .await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }

    let commits = store.commit_count() - before;
    assert!(
        commits <= 8,
        "500 jobs should batch into at most 8 transactions, took {commits}"
    );
    assert_eq!(store.job_count(), 500, "no job may be dropped");
    let page = store.items(ItemFilter::default()).await.unwrap();
    assert_eq!(page.total, 500, "no row may be lost");
    assert_eq!(
        page.rows.iter().map(|i| i.ord).collect::<Vec<_>>(),
        (0..500).collect::<Vec<_>>(),
        "and none reordered"
    );
}

/// Ops inside one job are applied in submission order; jobs for one id are applied in the order
/// the writer received them, so nothing is lost when a hundred of them target the same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ops_for_one_id_are_neither_lost_nor_reordered() {
    let h = support::harness();
    let item = support::item(0);
    let id = item.id;
    h.store
        .write(
            vec![WriteOp::InsertItems { items: vec![item] }],
            Durability::Sync,
        )
        .await
        .unwrap();

    // Intra-job order: the last status in the vector is the one that survives.
    let mut ops = Vec::new();
    for (n, status) in [
        Status::Resolving,
        Status::Queued,
        Status::Preparing,
        Status::Downloading,
        Status::Postprocessing,
        Status::Finished,
    ]
    .into_iter()
    .enumerate()
    {
        ops.push(WriteOp::SetStatus {
            id,
            status,
            msg: FieldUpdate::Set(format!("step {n}").into()),
            error: FieldUpdate::Keep,
            auto_start: None,
            at: 100 + n as i64,
        });
    }
    h.store.write(ops, Durability::Sync).await.unwrap();
    let row = h.store.item(id).await.unwrap().unwrap();
    assert_eq!(row.status, Status::Finished);
    assert_eq!(row.msg.as_deref(), Some("step 5"));
    assert_eq!(
        row.started_at,
        Some(102),
        "the first Preparing in the batch"
    );
    assert_eq!(row.finished_at, Some(105));

    // Inter-job: a hundred increments, none lost.
    let mut tasks = Vec::new();
    for _ in 0..100 {
        let s = h.store.clone();
        tasks.push(tokio::spawn(async move {
            s.write(vec![WriteOp::BumpAttempt { id }], Durability::Batched)
                .await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(h.store.item(id).await.unwrap().unwrap().attempt, 100);
}

/// A `Sync` job cuts the batch window short instead of waiting it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_write_short_circuits_the_flush_window() {
    let dir = tempfile::tempdir().unwrap();
    // A window long enough that waiting it out would be unmistakable.
    let store = Store::open(support::options(dir.path()).with_flush_ms(2_000)).unwrap();

    let started = Instant::now();
    store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(0)],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(1_000),
        "a Sync write must not wait for the flush window, took {elapsed:?}"
    );
}

/// A batched write does wait for its window — the property the batching claim rests on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batched_write_waits_for_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path()).with_flush_ms(300)).unwrap();
    let started = Instant::now();
    store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(0)],
            }],
            Durability::Batched,
        )
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "the writer extends the batch until AULOS_DB_FLUSH_MS expires"
    );
}

/// A deliberately locked database must surface as [`StoreError::Busy`], which `aulos-api` maps to
/// `503 state_unavailable` with `Retry-After: 1` — never a `500`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_locked_database_reports_busy() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path())).unwrap();

    let blocker = Connection::open(support::db_path(dir.path())).unwrap();
    blocker.busy_timeout(Duration::from_millis(50)).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();

    let err = store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(0)],
            }],
            Durability::Sync,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Busy | StoreError::Locked),
        "expected a lock error, got {err}"
    );
    assert!(err.retryable());
    assert!(err.is_unavailable());

    blocker.execute_batch("ROLLBACK").unwrap();

    // And the writer is still alive once the lock is gone.
    store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(1)],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
}

/// The WAL gauge `healthz` reports (DESIGN §16.3), and the checkpoint `close()` performs.
#[tokio::test]
async fn the_wal_gauge_grows_with_writes_and_close_truncates_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path())).unwrap();
    for ord in 0..64 {
        store
            .write(
                vec![WriteOp::InsertItems {
                    items: vec![support::item(ord)],
                }],
                Durability::Sync,
            )
            .await
            .unwrap();
    }
    assert!(store.wal_bytes() > 0, "writes go through the WAL");
    assert!(store.db_bytes() > 0);

    store.close().await.unwrap();
    assert_eq!(
        store.wal_bytes(),
        0,
        "close() checkpoints with TRUNCATE so the WAL does not survive a restart"
    );
}

/// DESIGN §7.1's six-hourly half: the WAL is reclaimed **without** closing the store, and the
/// store keeps taking writes afterwards.
#[tokio::test]
async fn checkpoint_truncates_the_wal_and_leaves_the_store_usable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path())).unwrap();
    for ord in 0..64 {
        store
            .write(
                vec![WriteOp::InsertItems {
                    items: vec![support::item(ord)],
                }],
                Durability::Sync,
            )
            .await
            .unwrap();
    }
    assert!(store.wal_bytes() > 0, "writes go through the WAL");

    store.checkpoint().await.unwrap();
    assert_eq!(
        store.wal_bytes(),
        0,
        "checkpoint() truncates the WAL, which is the whole point of scheduling it"
    );

    // Still open: the write lands, and the row count includes everything written before.
    store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(64)],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    let items = store.items(ItemFilter::default()).await.unwrap();
    assert_eq!(items.rows.len(), 65);

    store.close().await.unwrap();
    assert!(
        matches!(store.checkpoint().await, Err(StoreError::Closed)),
        "a closed store has no writer to checkpoint through"
    );
}

/// Reads are served from the read pool while the writer is busy, and more concurrent reads than
/// there are pool threads simply queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reads_exceed_the_pool_size_without_deadlocking() {
    let h = support::harness(); // two reader threads
    h.store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![support::item(0)],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let s = h.store.clone();
        tasks.push(tokio::spawn(
            async move { s.items(ItemFilter::default()).await },
        ));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().unwrap().total, 1);
    }
}

#[tokio::test]
async fn reads_are_rejected_after_close() {
    let h = support::harness();
    h.store.close().await.unwrap();
    let err = h.store.items(ItemFilter::default()).await.unwrap_err();
    assert!(matches!(err, StoreError::Closed), "{err}");
}
