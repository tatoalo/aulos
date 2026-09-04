//! How the store is opened (DESIGN §7.1, §17.3).

use std::path::{Path, PathBuf};

use aulos_core::config::{Config, DbSynchronous};

/// Everything the store needs from the effective configuration.
///
/// A struct rather than a `&Config` parameter for one reason: the store's own tests, the importer
/// (WP-05) and `aulos-server doctor` all want a store over a temporary file with a five-millisecond
/// flush window, and none of them wants to fabricate a whole [`Config`] to get one.
/// [`StoreOptions::from_config`] is the production path.
#[derive(Clone, Debug)]
pub struct StoreOptions {
    /// The database file. `AULOS_DB_PATH`, defaulting to `<STATE_DIR>/aulos.db`.
    pub path: PathBuf,
    /// `AULOS_DB_READERS` — the size of the read pool. Clamped to at least 1.
    pub readers: u32,
    /// `AULOS_DB_FLUSH_MS` — how long the writer extends a batch before committing.
    pub flush_ms: u64,
    /// `AULOS_DB_SYNCHRONOUS`.
    pub synchronous: DbSynchronous,
    /// `busy_timeout`, in milliseconds. DESIGN §7.1 pins 5 000; the tests lower it so the
    /// `SQLITE_BUSY` mapping can be asserted without a five-second wait.
    pub busy_timeout_ms: u64,
    /// `AULOS_ENTRY_MAX_BYTES` — the hard cap on `items.entry_json` (DESIGN §7.5). `0` disables it.
    pub entry_max_bytes: u64,
    /// `AULOS_MEM_DONE_ITEMS` — how many terminal rows [`crate::BootState`] loads into the
    /// engine's done window (DESIGN §8.9).
    pub done_window: u32,
    /// `AULOS_V1_HISTORY_MAX` — the hard cap the v1 shim's `done[]` is served under
    /// (DESIGN §11.4). `0` means unlimited, which is the default and full legacy fidelity.
    pub v1_history_max: u32,
}

impl StoreOptions {
    /// The DESIGN §17.3 defaults over a given database file.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            readers: 4,
            flush_ms: 200,
            synchronous: DbSynchronous::Normal,
            busy_timeout_ms: 5_000,
            entry_max_bytes: 262_144,
            done_window: 500,
            v1_history_max: 0,
        }
    }

    /// The production path: everything from the effective configuration.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            path: cfg.db_path.clone(),
            readers: cfg.db_readers,
            flush_ms: cfg.db_flush_ms,
            synchronous: cfg.db_synchronous,
            busy_timeout_ms: 5_000,
            entry_max_bytes: cfg.entry_max_bytes,
            done_window: cfg.mem_done_items,
            v1_history_max: cfg.v1_history_max,
        }
    }

    /// A shorter flush window — the tests' and the importer's escape hatch from the 200 ms one.
    #[must_use]
    pub fn with_flush_ms(mut self, ms: u64) -> Self {
        self.flush_ms = ms;
        self
    }

    /// A smaller read pool.
    #[must_use]
    pub fn with_readers(mut self, n: u32) -> Self {
        self.readers = n;
        self
    }

    /// A shorter `busy_timeout`.
    #[must_use]
    pub fn with_busy_timeout_ms(mut self, ms: u64) -> Self {
        self.busy_timeout_ms = ms;
        self
    }

    /// The read pool size, never zero.
    #[must_use]
    pub fn effective_readers(&self) -> usize {
        self.readers.max(1) as usize
    }

    /// The `PRAGMA synchronous` value.
    #[must_use]
    pub const fn synchronous_pragma(&self) -> &'static str {
        match self.synchronous {
            DbSynchronous::Normal => "NORMAL",
            DbSynchronous::Full => "FULL",
        }
    }

    /// The effective `v1_done` limit: the caller's request, capped by `AULOS_V1_HISTORY_MAX`.
    ///
    /// `0` and `None` both mean unlimited on either side, so the cap can only ever tighten.
    #[must_use]
    pub fn v1_limit(&self, requested: Option<u32>) -> Option<u32> {
        let cap = (self.v1_history_max > 0).then_some(self.v1_history_max);
        match (requested.filter(|n| *n > 0), cap) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, cap) => cap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_limit_lets_the_cap_only_tighten() {
        let mut o = StoreOptions::new("x.db");
        assert_eq!(o.v1_limit(None), None, "0 = unlimited on both sides");
        assert_eq!(o.v1_limit(Some(10)), Some(10));
        assert_eq!(o.v1_limit(Some(0)), None);
        o.v1_history_max = 100;
        assert_eq!(o.v1_limit(None), Some(100));
        assert_eq!(o.v1_limit(Some(10)), Some(10));
        assert_eq!(o.v1_limit(Some(1_000)), Some(100));
    }

    #[test]
    fn the_read_pool_is_never_empty() {
        assert_eq!(
            StoreOptions::new("x").with_readers(0).effective_readers(),
            1
        );
        assert_eq!(
            StoreOptions::new("x").with_readers(7).effective_readers(),
            7
        );
    }

    #[test]
    fn synchronous_maps_to_the_pragma_token() {
        let mut o = StoreOptions::new("x");
        assert_eq!(o.synchronous_pragma(), "NORMAL");
        o.synchronous = DbSynchronous::Full;
        assert_eq!(o.synchronous_pragma(), "FULL");
    }
}
