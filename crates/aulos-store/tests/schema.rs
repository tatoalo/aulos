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
