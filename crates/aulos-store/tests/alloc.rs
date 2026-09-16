//! The reserve-before-use hi/lo allocators: crash safety, the two boot consistency checks, and a
//! property test over interleaved reserve/crash schedules (DESIGN §4.1; PLAN WP-04).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;
use std::path::Path;

use aulos_store::{Durability, IdWarning, ORD_BLOCK, SEQ_BLOCK, Store, WriteOp};
use rusqlite::Connection;

/// Reads a `meta` integer through a raw connection.
fn meta_int(path: &Path, key: &str) -> Option<i64> {
    let c = Connection::open(path).unwrap();
    c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|s| s.parse().ok())
}

/// Overwrites a `meta` value through a raw connection — the "someone restored an older file"
/// and "a reserved block was lost" scenarios.
fn set_meta(path: &Path, key: &str, value: &str) {
    let c = Connection::open(path).unwrap();
    c.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )
    .unwrap();
}

fn delete_meta(path: &Path, key: &str) {
    let c = Connection::open(path).unwrap();
    c.execute("DELETE FROM meta WHERE key = ?1", [key]).unwrap();
}

#[test]
fn a_fresh_database_starts_both_counters_at_zero() {
    let h = support::harness();
    assert_eq!(h.store.next_ord(), 0);
    assert_eq!(h.store.next_ord(), 1);
    assert_eq!(h.store.next_seq().0, 0);
    assert_eq!(h.store.next_seq().0, 1);
    assert!(h.store.id_warnings().is_empty());
}

/// A block is reserved **before** any value from it is handed out, so a crash mid-block skips the
/// rest of the block and can never re-issue a value.
#[test]
fn a_crash_mid_block_skips_values_and_never_re_issues_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = support::db_path(dir.path());

    let first = Store::open(support::options(dir.path())).unwrap();
    let taken: Vec<i64> = (0..3).map(|_| first.next_ord()).collect();
    assert_eq!(taken, [0, 1, 2]);
    assert_eq!(
        meta_int(&path, "ord_hwm"),
        Some(ORD_BLOCK),
        "the whole block is durable before the first value is used"
    );
    drop(first); // the crash: no close(), no witness, no checkpoint

    let second = Store::open(support::options(dir.path())).unwrap();
    let next = second.next_ord();
    assert_eq!(next, ORD_BLOCK, "the rest of the lost block is skipped");
    assert!(next > *taken.last().unwrap(), "still strictly increasing");
}

#[test]
fn the_seq_counter_survives_a_restart_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(support::options(dir.path())).unwrap();
    let last = (0..5).map(|_| first.next_seq().0).next_back().unwrap();
    drop(first);

    let second = Store::open(support::options(dir.path())).unwrap();
    assert!(
        second.next_seq().0 >= u64::try_from(SEQ_BLOCK).unwrap(),
        "a resumed cursor is never below the previous head"
    );
    assert!(second.next_seq().0 > last);
}

/// `close()` records `meta.seq_hwm_witness`, which is what the `seq` boot check compares against.
#[tokio::test]
async fn close_records_the_seq_witness() {
    let dir = tempfile::tempdir().unwrap();
    let path = support::db_path(dir.path());
    let store = Store::open(support::options(dir.path())).unwrap();
    for _ in 0..4 {
        let _ = store.next_seq();
    }
    store.close().await.unwrap();
    assert_eq!(
        meta_int(&path, "seq_hwm_witness"),
        Some(3),
        "the witness is the last value handed out"
    );
}

/// The `ord` boot check: `meta.ord_hwm` behind `MAX(items.ord) + 1`.
///
/// v1.0: `repair-ids` is CUT, so the open **succeeds** with a WARN and the counter is advanced
/// past the table — which is what actually prevents the `UNIQUE` index failure the check exists to
/// predict.
#[tokio::test]
async fn a_stale_ord_hwm_warns_and_still_never_re_issues_an_ord() {
    let dir = tempfile::tempdir().unwrap();
    let path = support::db_path(dir.path());

    let store = Store::open(support::options(dir.path())).unwrap();
    let rows: Vec<_> = (0..6)
        .map(|_| {
            let ord = store.next_ord();
            let mut item = support::item(ord);
            item.ord = ord;
            item
        })
        .collect();
    let max_ord = rows.iter().map(|i| i.ord).max().unwrap();
    store
        .write(vec![WriteOp::InsertItems { items: rows }], Durability::Sync)
        .await
        .unwrap();
    store.close().await.unwrap();

    // A reserved block was consumed and lost: the meta row is behind the table.
    set_meta(&path, "ord_hwm", "0");

    let reopened = Store::open(support::options(dir.path())).unwrap();
    let warnings = reopened.id_warnings();
    assert!(
        matches!(
            warnings.first(),
            Some(IdWarning::OrdHwmBehind { hwm: 0, needed }) if *needed == max_ord + 1
        ),
        "expected an OrdHwmBehind warning, got {warnings:?}"
    );
    assert!(
        reopened.id_warnings()[0].to_string().contains("ord_hwm"),
        "the warning names the counter"
    );
    let next = reopened.next_ord();
    assert!(
        next > max_ord,
        "the allocator must be advanced past the table: {next} vs {max_ord}"
    );

    // And the insert that the stale row would have killed now succeeds.
    let mut item = support::item(next);
    item.ord = next;
    reopened
        .write(
            vec![WriteOp::InsertItems { items: vec![item] }],
            Durability::Sync,
        )
        .await
        .unwrap();
}

/// A healthy database with a large `MAX(items.ord)` and a correctly-ahead `meta.ord_hwm` must open
/// quietly — the negative test the earlier draft of this check would have failed.
#[tokio::test]
async fn a_healthy_database_with_a_long_history_opens_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let path = support::db_path(dir.path());
    let store = Store::open(support::options(dir.path())).unwrap();

    let ord = store.next_ord();
    let mut item = support::item(ord);
    item.ord = ord;
    store
        .write(
            vec![WriteOp::InsertItems { items: vec![item] }],
            Durability::Sync,
        )
        .await
        .unwrap();
    // Far more frames than rows: `seq_hwm` is thousands ahead of MAX(items.ord), which is normal.
    for _ in 0..2_000 {
        let _ = store.next_seq();
    }
    store.close().await.unwrap();
    assert!(meta_int(&path, "seq_hwm").unwrap() > 1_000);

    let reopened = Store::open(support::options(dir.path())).unwrap();
    assert!(
        reopened.id_warnings().is_empty(),
        "a healthy database must not warn: {:?}",
        reopened.id_warnings()
    );
}

/// The `seq` boot check: absent, and behind the witness.
#[tokio::test]
async fn a_missing_or_stale_seq_hwm_warns_and_is_reseeded() {
    // (a) deleted
    let dir = tempfile::tempdir().unwrap();
    let path = support::db_path(dir.path());
    let store = Store::open(support::options(dir.path())).unwrap();
    for _ in 0..3 {
        let _ = store.next_seq();
    }
    store.close().await.unwrap();
    delete_meta(&path, "seq_hwm");

    let reopened = Store::open(support::options(dir.path())).unwrap();
    assert!(
        matches!(
            reopened.id_warnings().first(),
            Some(IdWarning::SeqHwmMissing { .. })
        ),
        "{:?}",
        reopened.id_warnings()
    );
    assert!(
        reopened.next_seq().0 >= 2,
        "reseeded from the witness, not from zero"
    );
    reopened.close().await.unwrap();

    // (b) behind the witness — the "an older file was restored" shape
    set_meta(&path, "seq_hwm", "1");
    set_meta(&path, "seq_hwm_witness", "9999");
    let reopened = Store::open(support::options(dir.path())).unwrap();
    assert!(
        matches!(
            reopened.id_warnings().first(),
            Some(IdWarning::SeqHwmBehindWitness {
                hwm: 1,
                witness: 9_999
            })
        ),
        "{:?}",
        reopened.id_warnings()
    );
    assert!(reopened.next_seq().0 >= 9_999);
    reopened.close().await.unwrap();

    // (c) unparseable
    set_meta(&path, "seq_hwm", "not a number");
    let reopened = Store::open(support::options(dir.path())).unwrap();
    assert!(matches!(
        reopened.id_warnings().first(),
        Some(IdWarning::SeqHwmMissing { found: Some(_) })
    ));
    reopened.close().await.unwrap();
}

/// A property test over interleaved reserve/crash schedules: however many values each session
/// takes before dying, the global sequence is strictly increasing and no value is ever repeated.
#[test]
fn no_schedule_of_crashes_can_re_issue_a_value() {
    use proptest::prelude::*;

    let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
        cases: 12,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    });

    runner
        .run(
            &proptest::collection::vec(0_usize..=9_usize, 1..6),
            |sessions| {
                let dir = tempfile::tempdir().unwrap();
                let mut all_ord: Vec<i64> = Vec::new();
                let mut all_seq: Vec<u64> = Vec::new();
                for takes in sessions {
                    let store = Store::open(support::options(dir.path())).unwrap();
                    for _ in 0..takes {
                        all_ord.push(store.next_ord());
                        all_seq.push(store.next_seq().0);
                    }
                    // Half the sessions "crash", half shut down cleanly.
                    if takes % 2 == 0 {
                        drop(store);
                    } else {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        rt.block_on(store.close()).unwrap();
                    }
                }
                prop_assert!(
                    all_ord.windows(2).all(|w| w[1] > w[0]),
                    "ord must be strictly increasing: {all_ord:?}"
                );
                prop_assert!(
                    all_seq.windows(2).all(|w| w[1] > w[0]),
                    "seq must be strictly increasing: {all_seq:?}"
                );
                prop_assert_eq!(
                    all_ord.iter().collect::<BTreeSet<_>>().len(),
                    all_ord.len(),
                    "no ord may be re-issued"
                );
                prop_assert_eq!(
                    all_seq.iter().collect::<BTreeSet<_>>().len(),
                    all_seq.len(),
                    "no seq may be re-issued"
                );
                Ok(())
            },
        )
        .unwrap();
}

/// The two counters are independent: taking `ord` values must not move `seq`.
#[test]
fn the_two_counters_are_independent() {
    let h = support::harness();
    for _ in 0..300 {
        let _ = h.store.next_ord();
    }
    assert_eq!(h.store.next_seq().0, 0, "seq is untouched by ord");
    assert_eq!(
        h.store.ord_allocator().current(),
        299,
        "current() is the last value handed out"
    );
    assert_eq!(h.store.seq_allocator().current(), 0);
}
