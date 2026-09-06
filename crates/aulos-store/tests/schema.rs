//! Migrations, the checked-in DDL snapshot, the pragma set and the `STRICT` guard
//! (DESIGN §7.2, §7.3; PLAN WP-04).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use aulos_core::Kind;
use aulos_store::{Durability, SCHEMA_VERSION, Store, WriteOp, dump_schema};
use rusqlite::Connection;

/// Applying every migration from scratch must equal the checked-in `schema.sql`.
///
/// Both halves matter: the `insta` snapshot is what CI diffs when someone edits a migration, and
/// `schema.sql` is what a human reads — and what `print-schema` would have printed before the
/// BRIEF cut that subcommand.
#[tokio::test]
async fn migrations_from_empty_match_the_checked_in_schema() {
    let h = support::harness();
    let dump = h.store.read(dump_schema).await.unwrap();

    insta::assert_snapshot!("schema", dump);

    let checked_in = include_str!("../schema.sql");
    assert_eq!(
        dump.trim(),
        checked_in.trim(),
        "crates/aulos-store/schema.sql is stale — replace it with the dump above"
    );
}

#[tokio::test]
async fn schema_version_and_identity_are_recorded_in_meta() {
    let h = support::harness();
    let meta = h.store.meta().await.unwrap();
    assert_eq!(
        meta.get("schema_version").map(std::convert::AsRef::as_ref),
        Some(SCHEMA_VERSION.to_string().as_str())
    );
    assert!(meta.contains_key("instance_id"), "{meta:?}");
    assert!(meta.contains_key("created_at"), "{meta:?}");
}

#[tokio::test]
async fn reopening_an_existing_database_keeps_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(support::options(dir.path())).unwrap();
    let instance = first.meta().await.unwrap();
    first.close().await.unwrap();

    let second = Store::open(support::options(dir.path())).unwrap();
    assert_eq!(
        second.meta().await.unwrap().get("instance_id"),
        instance.get("instance_id"),
        "instance_id is minted once"
    );
    assert!(second.id_warnings().is_empty(), "a clean reopen is quiet");
}

#[tokio::test]
async fn the_pragma_set_is_applied() {
    let h = support::harness();
    let (journal, fk, busy) = h
        .store
        .read(|c| {
            Ok((
                c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?,
                c.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))?,
                c.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(journal.to_lowercase(), "wal");
    assert_eq!(fk, 1, "ON DELETE CASCADE needs foreign_keys = ON");
    assert_eq!(busy, 200, "the harness lowers busy_timeout");
}

/// A negative test proving the `STRICT` guard is live, not merely spelled in the DDL.
///
/// It writes through a raw connection because the typed API cannot express a wrong-typed value —
/// which is the point of the typed API, but leaves the database-level guard unproven.
#[test]
fn strict_tables_and_check_constraints_reject_bad_writes() {
    let h = support::harness();
    let raw = Connection::open(support::db_path(h.dir.path())).unwrap();

    let err = raw
        .execute(
            "INSERT INTO items (id, kind, ord, url, canonical_key, title, status, request_json, \
             source_json, created_at, updated_at) \
             VALUES ('01J000000000000000000000AA', 'item', 'not-an-integer', 'u', 'k', 't', \
             'queued', '{}', '{}', 0, 0)",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot store TEXT value in INTEGER column"),
        "STRICT should reject a TEXT ord, got: {err}"
    );

    let err = raw
        .execute(
            "INSERT INTO items (id, kind, ord, url, canonical_key, title, status, request_json, \
             source_json, created_at, updated_at) \
             VALUES ('01J000000000000000000000AB', 'item', 1, 'u', 'k', 't', 'pending', '{}', \
             '{}', 0, 0)",
            [],
        )
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("check"),
        "the status CHECK should reject the legacy vocabulary, got: {err}"
    );
}

#[tokio::test]
async fn deleting_a_group_cascades_to_its_children() {
    let h = support::harness();
    let mut group = support::item(0);
    group.kind = Kind::Group;
    group.children_total = Some(2);
    let group_id = group.id;
    let mut child = support::item(1);
    child.group_id = Some(group_id);
    child.group_index = Some(1);
    let child_id = child.id;

    h.store
        .write(
            vec![WriteOp::InsertItems {
                items: vec![group, child],
            }],
            Durability::Sync,
        )
        .await
        .unwrap();
    h.store
        .write(vec![WriteOp::DeleteItems(vec![group_id])], Durability::Sync)
        .await
        .unwrap();

    assert!(h.store.item(child_id).await.unwrap().is_none());
}

/// Migration `0002` rewrites the rows a **pre-fix build already settled** (PROTOCOL §2.3).
///
/// `set_status` is the only writer of `items.msg` and it is never re-run for a settled row, so
/// clearing the line at the terminal write is forward-only: without this migration the reporter's
/// own `finished` row keeps `"MoveFiles…"` across the upgrade and boot recovery reloads it into
/// the engine's cache, so the bug report's repro still prints the stale line after deploying.
///
/// The pre-fix database is reproduced the only honest way: a `finished` row really written with a
/// `msg`, then `user_version` rolled back so the store has not seen `0002` yet.
#[tokio::test]
async fn the_backfill_clears_a_stale_status_line_off_rows_an_older_build_finished() {
    use aulos_core::{FieldUpdate, Status};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path())).unwrap();
    let item = support::item(0);
    let id = item.id;
    store
        .write(
            vec![
                WriteOp::InsertItems { items: vec![item] },
                WriteOp::SetStatus {
                    id,
                    status: Status::Finished,
                    msg: FieldUpdate::Set("MoveFiles…".into()),
                    error: FieldUpdate::Clear,
                    auto_start: None,
                    at: 1_757_000_000_000,
                },
            ],
            Durability::Sync,
        )
        .await
        .unwrap();
    assert_eq!(
        store.item(id).await.unwrap().unwrap().msg.as_deref(),
        Some("MoveFiles…"),
        "the store itself is dumb: this is exactly what the buggy build persisted"
    );
    store.close().await.unwrap();

    // Rewind to the schema the buggy build shipped, so the reopen has a migration to run.
    let conn = Connection::open(support::db_path(dir.path())).unwrap();
    rewind_to_0001(&conn);
    drop(conn);

    let upgraded = Store::open(support::options(dir.path())).unwrap();
    assert_eq!(
        upgraded.item(id).await.unwrap().unwrap().msg,
        None,
        "the upgrade rewrites the line the older build left behind"
    );
    assert_eq!(
        upgraded
            .meta()
            .await
            .unwrap()
            .get("schema_version")
            .map(std::convert::AsRef::as_ref),
        Some(SCHEMA_VERSION.to_string().as_str()),
        "and records the schema it migrated to"
    );
}

/// The backfill is scoped to `finished`. `error` and `canceled` may carry a terminal *note* that
/// was never a live progress line — the importer's "Imported with unknown legacy status: …" — and
/// SQL cannot tell one from the other, so those rows are left alone.
#[tokio::test]
async fn the_backfill_leaves_a_failed_row_its_message() {
    use aulos_core::{ErrorCode, FieldUpdate, Status, WireError};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(dir.path())).unwrap();
    let item = support::item(0);
    let id = item.id;
    store
        .write(
            vec![
                WriteOp::InsertItems { items: vec![item] },
                WriteOp::SetStatus {
                    id,
                    status: Status::Error,
                    msg: FieldUpdate::Set("Imported with unknown legacy status: cancelled".into()),
                    error: FieldUpdate::Set(WireError::new(ErrorCode::Unavailable, "unavailable")),
                    auto_start: None,
                    at: 1_757_000_000_000,
                },
            ],
            Durability::Sync,
        )
        .await
        .unwrap();
    store.close().await.unwrap();

    let conn = Connection::open(support::db_path(dir.path())).unwrap();
    rewind_to_0001(&conn);
    drop(conn);

    let upgraded = Store::open(support::options(dir.path())).unwrap();
    assert_eq!(
        upgraded.item(id).await.unwrap().unwrap().msg.as_deref(),
        Some("Imported with unknown legacy status: cancelled"),
        "an import note is a reason, not a stale progress line"
    );
}

/// Rewinds an already-migrated file to the state migration `0001` left it in.
///
/// `user_version` alone is not enough and has not been since `0003`: rolling the counter back on a
/// file that still carries the later migrations' tables makes the next `to_latest` re-run their
/// DDL and fail with "table devices already exists". A test that wants a pre-`0002` database has
/// to undo what came after it, so this drops `0003`'s objects too. **Every migration that creates
/// DDL adds its undo here.**
fn rewind_to_0001(conn: &Connection) {
    conn.execute_batch(
        "DROP INDEX IF EXISTS devices_start_token; \
         DROP INDEX IF EXISTS live_activities_item; \
         DROP TABLE IF EXISTS live_activities; \
         DROP TABLE IF EXISTS devices;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
}
