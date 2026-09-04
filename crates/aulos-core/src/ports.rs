//! Cross-crate seams: the [`HookStore`] port, [`HookPhase`] and [`FieldUpdate`].
//!
//! These three types are declared here because they are the *vocabulary* two crates share without
//! either depending on the other:
//!
//! - `aulos-hooks` reaches item state through [`HookStore`] and nothing else, so it needs neither
//!   `aulos-store` nor `aulos-queue` (DESIGN §3 rule A1, §13). The implementation is
//!   `aulos-queue::EngineHookStore`, **not** `Store`: the read is delegated to the store's read
//!   pool, but both writes become `EngineCmd`s so the engine's item cache and the aggregator's
//!   `last_sent` observe them and the change reaches clients as a `delta` (DESIGN §13.3).
//! - [`HookPhase`] is read by the engine and declared by hooks.
//! - [`FieldUpdate`] is the one three-state patch convention every nullable `WriteOp` column uses
//!   (DESIGN §7.1); it is named in signatures by `aulos-store`, `aulos-queue` and their tests.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::id::ItemId;
use crate::item::EntryBlob;

/// When a hook runs relative to the terminal status write (DESIGN §13).
///
/// `PreTerminal` hooks run while the item is still `postprocessing`, **before** the engine writes
/// the terminal status — that is what lets the `best_remux` audio re-encode keep the legacy
/// behaviour of running inside the download without needing a `Finished → Postprocessing` edge
/// that DESIGN §4.2 does not have. `PostTerminal` hooks run after it, which is what every notifier
/// wants.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    /// Runs before the terminal status is written; delays it, and can never change it.
    PreTerminal,
    /// Runs after the terminal status is written.
    #[default]
    PostTerminal,
}

impl HookPhase {
    /// The stable name used in `healthz` (`components.audio_sync.phase`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreTerminal => "pre_terminal",
            Self::PostTerminal => "post_terminal",
        }
    }
}

impl std::fmt::Display for HookPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three-state patch for a nullable column (DESIGN §7.1).
///
/// There is exactly one convention in the whole `WriteOp` enum and this is it: [`Self::Keep`]
/// leaves the column alone, [`Self::Clear`] writes SQL `NULL`, [`Self::Set`] writes the value.
/// `Option<T>` is never used to mean "unchanged" on a nullable column, because that overload is
/// how a retry ends up leaving a stale `error` on a `queued` row.
///
/// It is externally tagged, so `Keep`, `Clear` and `Set(null)` are three distinguishable JSON
/// values and no writer can express "unchanged" and "null" with the same one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldUpdate<T> {
    /// Leave the column as it is.
    Keep,
    /// Write SQL `NULL`.
    Clear,
    /// Write this value.
    Set(T),
}

impl<T> Default for FieldUpdate<T> {
    /// [`FieldUpdate::Keep`] — hand-written rather than derived so `T` needs no `Default`.
    fn default() -> Self {
        Self::Keep
    }
}

impl<T> FieldUpdate<T> {
    /// Whether this patch touches the column.
    #[must_use]
    pub const fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    /// Applies the patch to a column's current value.
    pub fn apply_to(self, slot: &mut Option<T>) {
        match self {
            Self::Keep => {}
            Self::Clear => *slot = None,
            Self::Set(v) => *slot = Some(v),
        }
    }

    /// Maps the payload, leaving `Keep`/`Clear` alone.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> FieldUpdate<U> {
        match self {
            Self::Keep => FieldUpdate::Keep,
            Self::Clear => FieldUpdate::Clear,
            Self::Set(v) => FieldUpdate::Set(f(v)),
        }
    }

    /// Borrows the payload.
    #[must_use]
    pub fn as_ref(&self) -> FieldUpdate<&T> {
        match self {
            Self::Keep => FieldUpdate::Keep,
            Self::Clear => FieldUpdate::Clear,
            Self::Set(v) => FieldUpdate::Set(v),
        }
    }

    /// The value to write, or `None` for both `Keep` and `Clear`.
    ///
    /// Use only where "unchanged" and "null" genuinely coincide — an INSERT, for instance.
    #[must_use]
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Keep | Self::Clear => None,
            Self::Set(v) => Some(v),
        }
    }
}

impl<T> From<Option<T>> for FieldUpdate<T> {
    /// `Some(v)` → `Set(v)`, `None` → `Clear`. Deliberately **not** `Keep`: a caller that has an
    /// `Option` in hand knows the value, so "leave it alone" is not what it means.
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => Self::Set(v),
            None => Self::Clear,
        }
    }
}

/// The three item-state operations a hook is allowed to perform (DESIGN §7.1, §13).
///
/// Implemented by `aulos-queue::EngineHookStore`. The hook tests run against a `HashMap`-backed
/// fake with no SQLite and no engine at all.
#[async_trait::async_trait]
pub trait HookStore: Send + Sync {
    /// Reads the compacted provider entry, for the NFO hook (DESIGN §13.2).
    ///
    /// `None` when the row has no blob (a plain yt-dlp child) or it was already dropped.
    ///
    /// # Errors
    /// [`PortError`] if the row is gone or the store is unreachable.
    async fn entry_blob(&self, id: ItemId) -> Result<Option<EntryBlob>, PortError>;

    /// Drops the entry blob once the NFO hook has consumed it.
    ///
    /// # Errors
    /// [`PortError`] if the row is gone or the write could not be applied.
    async fn drop_entry_blob(&self, id: ItemId) -> Result<(), PortError>;

    /// Records the new file size after a hook rewrote the produced file (DESIGN §13.3).
    ///
    /// Engine-mediated, so the change reaches every connected client as a `delta`; writing SQLite
    /// directly would leave `size` wrong on every client until a restart.
    ///
    /// # Errors
    /// [`PortError`] if the row is gone or the write could not be applied.
    async fn set_size(&self, id: ItemId, size: u64) -> Result<(), PortError>;
}

/// What can go wrong on a port call.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PortError {
    /// The row no longer exists — it was deleted while the hook ran.
    #[error("item {0} no longer exists")]
    NotFound(ItemId),
    /// The engine or store is shutting down.
    #[error("the engine is unavailable")]
    Unavailable,
    /// The store reported a failure.
    #[error("store error: {0}")]
    Store(Box<str>),
}

impl PortError {
    /// The wire error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Unavailable | Self::Store(_) => ErrorCode::StateUnavailable,
        }
    }

    /// A shutting-down engine or a busy store is worth another try; a deleted row is not.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable | Self::Store(_))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    type Patch = FieldUpdate<Box<str>>;

    #[test]
    fn the_three_states_are_distinguishable_after_serde() {
        let cases: [(Patch, &str); 3] = [
            (FieldUpdate::Keep, "\"keep\""),
            (FieldUpdate::Clear, "\"clear\""),
            (FieldUpdate::Set("x".into()), "{\"set\":\"x\"}"),
        ];
        let mut wire = Vec::new();
        for (patch, expected) in cases {
            let json = serde_json::to_string(&patch).unwrap();
            assert_eq!(json, expected);
            assert_eq!(serde_json::from_str::<Patch>(&json).unwrap(), patch);
            wire.push(json);
        }
        wire.sort();
        wire.dedup();
        assert_eq!(wire.len(), 3, "no two states share a wire form");
    }

    #[test]
    fn set_of_a_nullable_payload_is_not_clear() {
        type Nullable = FieldUpdate<Option<u64>>;
        let set_null: Nullable = FieldUpdate::Set(None);
        let clear: Nullable = FieldUpdate::Clear;
        assert_ne!(
            serde_json::to_string(&set_null).unwrap(),
            serde_json::to_string(&clear).unwrap()
        );
    }

    #[test]
    fn apply_to_implements_the_documented_semantics() {
        let mut slot = Some(Box::<str>::from("old"));
        Patch::Keep.apply_to(&mut slot);
        assert_eq!(slot.as_deref(), Some("old"));
        Patch::Set("new".into()).apply_to(&mut slot);
        assert_eq!(slot.as_deref(), Some("new"));
        Patch::Clear.apply_to(&mut slot);
        assert_eq!(slot, None);
    }

    #[test]
    fn from_option_never_produces_keep() {
        assert_eq!(Patch::from(Some("a".into())), FieldUpdate::Set("a".into()));
        assert_eq!(Patch::from(None), FieldUpdate::Clear);
        assert!(Patch::Keep.is_keep());
        assert!(!Patch::Clear.is_keep());
    }

    #[test]
    fn map_and_as_ref_leave_keep_and_clear_alone() {
        assert_eq!(Patch::Keep.map(|s| s.len()), FieldUpdate::Keep);
        assert_eq!(Patch::Clear.map(|s| s.len()), FieldUpdate::Clear);
        assert_eq!(
            Patch::Set("abc".into()).map(|s| s.len()),
            FieldUpdate::Set(3)
        );
        let set: Patch = FieldUpdate::Set("abc".into());
        assert_eq!(set.as_ref().map(|s| s.len()), FieldUpdate::Set(3));
    }

    #[test]
    fn hook_phase_names_match_healthz() {
        assert_eq!(HookPhase::PreTerminal.as_str(), "pre_terminal");
        assert_eq!(HookPhase::default(), HookPhase::PostTerminal);
        assert_eq!(
            serde_json::to_string(&HookPhase::PreTerminal).unwrap(),
            "\"pre_terminal\""
        );
    }

    #[test]
    fn port_error_codes_and_retryability() {
        let id = ItemId::new();
        assert_eq!(PortError::NotFound(id).code(), ErrorCode::NotFound);
        assert!(!PortError::NotFound(id).retryable());
        assert!(PortError::Unavailable.retryable());
        assert_eq!(
            PortError::Store("busy".into()).code(),
            ErrorCode::StateUnavailable
        );
    }
}
