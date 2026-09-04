//! Persistence: a single SQLite (WAL) connection pool behind one async-friendly `Store` handle,
//! forward-only embedded migrations, hi/lo allocators for `ord` and `seq`, typed reads, and the
//! one-shot importer for the legacy `schema_version 2` JSON state files.
//!
//! This is the only crate in the workspace permitted to depend on `rusqlite` (DESIGN §3 rule A2,
//! §7).
