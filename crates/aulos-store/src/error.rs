//! The store's error taxonomy (DESIGN §5, §7.1).
//!
//! Every variant answers the two questions the retry policy (DESIGN §8.8) and the HTTP layer
//! (DESIGN §11, §16) ask of a library error: which [`ErrorCode`] does it map to, and is another
//! attempt worth making. [`StoreError::Busy`] and [`StoreError::Locked`] are the two that exist
//! purely so `aulos-api` can answer `503 state_unavailable` with `Retry-After: 1` instead of a
//! `500` (DESIGN §7.1).

use aulos_core::ErrorCode;
use aulos_core::id::ItemId;

/// Everything the store can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// SQLite reported `SQLITE_BUSY`: another connection held the write lock past
    /// `busy_timeout`. Retryable; maps to `503`.
    #[error("the database is busy")]
    Busy,

    /// SQLite reported `SQLITE_LOCKED`: a table-level conflict inside the same connection.
    /// Retryable; maps to `503`.
    #[error("the database is locked")]
    Locked,

    /// A uniqueness or foreign-key constraint rejected the write — a duplicate subscription URL,
    /// or a re-issued `ord`. Not retryable; maps to `409`.
    #[error("constraint violated: {0}")]
    Conflict(Box<str>),

    /// `PRAGMA quick_check` failed, or a migration cannot be applied to this file.
    #[error("the database file is corrupt: {0}")]
    Corrupt(Box<str>),

    /// A migration failed to apply.
    #[error("migration failed: {0}")]
    Migration(Box<str>),

    /// A column could not be decoded into its domain type. This is schema drift or a hand-edited
    /// row, never a normal condition.
    #[error("column {column} could not be decoded: {detail}")]
    Decode {
        /// The offending column, qualified by table when it is ambiguous.
        column: &'static str,
        /// The underlying serde / parse message.
        detail: Box<str>,
    },

    /// A domain value does not fit its column — a `u64` size above [`i64::MAX`], for instance.
    #[error("value out of range for column {column}")]
    OutOfRange {
        /// The offending column.
        column: &'static str,
    },

    /// The row a write targeted no longer exists.
    #[error("item {0} no longer exists")]
    NotFound(ItemId),

    /// The writer thread or the read pool is gone: the store has been closed, or the thread
    /// panicked. Not retryable.
    #[error("the store is closed")]
    Closed,

    /// Any other SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(Box<str>),

    /// The database file could not be opened or its size could not be read.
    #[error("io: {0}")]
    Io(Box<str>),
}

impl StoreError {
    /// The wire error code this failure maps to.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Busy | Self::Locked | Self::Closed => ErrorCode::StateUnavailable,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Corrupt(_)
            | Self::Migration(_)
            | Self::Decode { .. }
            | Self::OutOfRange { .. }
            | Self::Sqlite(_)
            | Self::Io(_) => ErrorCode::Internal,
        }
    }

    /// Whether another attempt is worth making.
    ///
    /// Only lock contention is: a decode failure, a constraint violation and a corrupt file are
    /// all deterministic, and a closed store never re-opens itself.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Busy | Self::Locked)
    }

    /// Whether `aulos-api` should answer `503 state_unavailable` with `Retry-After: 1`
    /// (DESIGN §7.1).
    #[must_use]
    pub const fn is_unavailable(&self) -> bool {
        matches!(self, Self::Busy | Self::Locked | Self::Closed)
    }

    pub(crate) fn decode(column: &'static str, detail: impl std::fmt::Display) -> Self {
        Self::Decode {
            column,
            detail: detail.to_string().into_boxed_str(),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    /// Classifies a `rusqlite` failure, so `SQLITE_BUSY` never reaches a client as a `500`.
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::Error as E;
        use rusqlite::ErrorCode as C;
        match &e {
            E::SqliteFailure(f, msg) => match f.code {
                C::DatabaseBusy => Self::Busy,
                C::DatabaseLocked => Self::Locked,
                C::ConstraintViolation => Self::Conflict(
                    msg.clone()
                        .unwrap_or_else(|| "constraint violation".to_owned())
                        .into_boxed_str(),
                ),
                C::DatabaseCorrupt | C::NotADatabase => {
                    Self::Corrupt(e.to_string().into_boxed_str())
                }
                _ => Self::Sqlite(e.to_string().into_boxed_str()),
            },
            _ => Self::Sqlite(e.to_string().into_boxed_str()),
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Sqlite(format!("json: {e}").into_boxed_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_contention_is_retryable_and_unavailable() {
        for e in [StoreError::Busy, StoreError::Locked] {
            assert!(e.retryable());
            assert!(e.is_unavailable());
            assert_eq!(e.code(), ErrorCode::StateUnavailable);
        }
    }

    #[test]
    fn deterministic_failures_are_not_retryable() {
        let cases = [
            StoreError::Conflict("dup".into()),
            StoreError::Corrupt("bad".into()),
            StoreError::decode("items.url", "nope"),
            StoreError::Closed,
        ];
        for e in cases {
            assert!(!e.retryable(), "{e} must not be retryable");
        }
    }

    #[test]
    fn codes_match_the_taxonomy() {
        assert_eq!(StoreError::Conflict("x".into()).code(), ErrorCode::Conflict);
        assert_eq!(
            StoreError::decode("items.url", "x").code(),
            ErrorCode::Internal
        );
        assert_eq!(
            StoreError::NotFound(ItemId::new()).code(),
            ErrorCode::NotFound
        );
    }
}
