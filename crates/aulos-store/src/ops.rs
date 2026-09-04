//! The write vocabulary (DESIGN §7.1).
//!
//! Every mutation the whole server can perform on persisted state is one of these variants, and
//! they only ever arrive through [`crate::Store::write`], which means there is exactly one place
//! where SQL is written and exactly one thread that writes it.
//!
//! Two conventions are load-bearing:
//!
//! - A nullable column is patched with [`FieldUpdate`], never with `Option`. "Unchanged" and
//!   "set to null" are different instructions, and overloading `Option` to mean both is how a
//!   retry ends up leaving a stale `error` on a `queued` row.
//! - A variant that changes only one column exists as its own variant rather than as an optional
//!   field on a bigger one. [`WriteOp::SetAutoStart`] is the clearest case: pause and start change
//!   *only* `auto_start` (DESIGN §8.7), the status stays `queued`, and folding it into
//!   [`WriteOp::SetStatus`] would make the whole pause feature unexpressible without also
//!   re-asserting a status.

use aulos_core::{
    ChatConfig, EntryBlob, FieldUpdate, FileRef, FileSlot, Item, ItemId, ProviderId, RelPath,
    SourceRef, Status, SubId, SubscriptionRecord, UnixMs, WireError,
};
use serde_json::Value;

/// How hard the writer works to get a batch onto the platter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Durability {
    /// Join the current batch; the writer extends it until `AULOS_DB_FLUSH_MS` expires or 256 jobs
    /// have accumulated. This is what turns "500 playlist inserts" into ~2 transactions instead of
    /// legacy's 500 whole-file JSON rewrites with 1 000 `fsync`s.
    #[default]
    Batched,
    /// Short-circuit the batch extension and commit now, with `PRAGMA synchronous = FULL` for the
    /// duration. For the writes whose loss would be user-visible: the importer's final state, and
    /// the shutdown witness.
    Sync,
}

/// One persisted mutation.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum WriteOp {
    /// Insert a batch of rows. `ord` must already be allocated (DESIGN §4.1).
    InsertItems {
        /// The rows, in insertion order.
        items: Vec<Item>,
    },

    /// The status transition, with the DESIGN §7.1 timestamp rules.
    ///
    /// `updated_at` is always written; `started_at` on the first `Preparing` only; `finished_at`
    /// on a terminal status, and back to `NULL` on a terminal → non-terminal transition with
    /// `started_at` kept, so "when did this first start" survives a retry.
    SetStatus {
        /// The row.
        id: ItemId,
        /// The new status.
        status: Status,
        /// The human stage text.
        msg: FieldUpdate<Box<str>>,
        /// The terminal error, or the DESIGN §8.4 pre-download problem on a `queued` item.
        error: FieldUpdate<WireError>,
        /// `None` leaves the column unchanged.
        auto_start: Option<bool>,
        /// The timestamp the rules above are applied with.
        at: UnixMs,
    },

    /// Pause / start. The **only** thing that changes is `auto_start` (DESIGN §8.7).
    SetAutoStart {
        /// The row.
        id: ItemId,
        /// `false` parks the item in the legacy `pending` bucket.
        auto_start: bool,
        /// `updated_at`.
        at: UnixMs,
    },

    /// Re-attribution: boot recovery writes `kind: "restart"` (DESIGN §8.9), a retry writes
    /// `kind: "retry"` (DESIGN §4.4). `source` is on the wire, so it needs a write path of its own.
    SetSource {
        /// The row.
        id: ItemId,
        /// The new attribution.
        source: SourceRef,
    },

    /// What resolution learned: the provider, its own id, the real title, the compacted entry and
    /// the dedupe key.
    SetResolved {
        /// The row.
        id: ItemId,
        /// The provider that resolved it.
        provider: ProviderId,
        /// The provider's own id.
        media_id: Option<Box<str>>,
        /// The real title.
        title: Box<str>,
        /// The compacted provider entry (DESIGN §7.5).
        entry: Option<EntryBlob>,
        /// The DESIGN §8.5 dedupe key.
        canonical_key: Box<str>,
    },

    /// A single item became a playlist/channel/season container (DESIGN §8.6). The id is kept.
    PromoteToGroup {
        /// The row.
        id: ItemId,
        /// The declared child count.
        children_total: u32,
        /// The container's title.
        title: Box<str>,
    },

    /// The produced file and its size. Both columns are written; `None` writes SQL `NULL`.
    SetOutput {
        /// The row.
        id: ItemId,
        /// Relative to the item's download root.
        filename: Option<RelPath>,
        /// Bytes on disk.
        size: Option<u64>,
    },

    /// Size only, leaving `filename` alone — what a hook that rewrote the produced file needs
    /// (DESIGN §13.3). Reached only through `EngineHookStore`, never from `aulos-hooks` directly.
    SetSize {
        /// The row.
        id: ItemId,
        /// The new size in bytes.
        size: u64,
    },

    /// Append one produced auxiliary file to its list.
    PushFile {
        /// The row.
        id: ItemId,
        /// Which list.
        slot: FileSlot,
        /// The file.
        file: FileRef,
    },

    /// Drop the provider entry blob — the terminal transition for a plain item, and the NFO hook
    /// for a StreamingCommunity one (DESIGN §7.5, §13.2).
    DropEntryBlob {
        /// The row.
        id: ItemId,
    },

    /// `attempt += 1` (DESIGN §8.8).
    BumpAttempt {
        /// The row.
        id: ItemId,
    },

    /// Arm or disarm `CLEAR_COMPLETED_AFTER` for this row (DESIGN §8.10). Persisted, so the timer
    /// survives a restart — legacy lost it.
    SetClearAfter {
        /// The row.
        id: ItemId,
        /// When the row should be removed, or `None` to disarm.
        at: Option<UnixMs>,
    },

    /// Delete rows. Children of a deleted group go with it via `ON DELETE CASCADE`.
    DeleteItems(Vec<ItemId>),

    /// Insert or replace a subscription. `Box`ed because the record is by far the largest variant
    /// and every `WriteOp` pays for the biggest one.
    UpsertSubscription(Box<SubscriptionRecord>),

    /// Record media ids this subscription has now seen. First sighting wins, so `seen_at` keeps
    /// telling the truth about when the id first appeared.
    MarkSeen {
        /// The subscription.
        sub: SubId,
        /// The media ids.
        ids: Vec<Box<str>>,
        /// The sighting time.
        at: UnixMs,
    },

    /// Keep only the newest `keep` seen ids for this subscription. Legacy rewrote a
    /// 50 000-element JSON array on every check; this is one `DELETE`.
    PruneSeen {
        /// The subscription.
        sub: SubId,
        /// How many to keep. `0` clears the set.
        keep: u32,
    },

    /// Delete subscriptions. Their `subscription_seen` rows cascade.
    DeleteSubscriptions(Vec<SubId>),

    /// Persist one chat's Telegram defaults.
    UpsertTelegramChat {
        /// The chat.
        chat_id: i64,
        /// Its stored defaults.
        config: ChatConfig,
    },

    /// Runtime overrides (the uploaded cookiefile) and hook bookkeeping. `None` deletes the key.
    SetKv {
        /// The key.
        key: Box<str>,
        /// The value, or `None` to delete.
        value: Option<Value>,
    },

    /// One `meta` key (DESIGN §7.2). Added in WP-05: the importer's provenance keys
    /// (`imported_from`, `imported_at`, `import_report`) must land in the **same transaction** as
    /// the rows they describe (DESIGN §7.6.6), and the two existing `meta` writers — the schema
    /// seed and the id allocators — both write outside the actor on their own connections.
    ///
    /// Not a `FieldUpdate`: `meta` values are never null and are never cleared.
    SetMeta {
        /// The key.
        key: Box<str>,
        /// The value, always a string — `meta.value` is `TEXT NOT NULL`.
        value: Box<str>,
    },
}

impl WriteOp {
    /// The item this op targets, when it targets exactly one.
    ///
    /// Used by the writer to report a `NotFound` against the right id, and by the engine's tests
    /// to assert that ops for one id are applied in submission order.
    #[must_use]
    pub const fn item(&self) -> Option<ItemId> {
        match self {
            Self::SetStatus { id, .. }
            | Self::SetAutoStart { id, .. }
            | Self::SetSource { id, .. }
            | Self::SetResolved { id, .. }
            | Self::PromoteToGroup { id, .. }
            | Self::SetOutput { id, .. }
            | Self::SetSize { id, .. }
            | Self::PushFile { id, .. }
            | Self::DropEntryBlob { id }
            | Self::BumpAttempt { id }
            | Self::SetClearAfter { id, .. } => Some(*id),
            Self::InsertItems { .. }
            | Self::DeleteItems(_)
            | Self::UpsertSubscription(_)
            | Self::MarkSeen { .. }
            | Self::PruneSeen { .. }
            | Self::DeleteSubscriptions(_)
            | Self::UpsertTelegramChat { .. }
            | Self::SetKv { .. }
            | Self::SetMeta { .. } => None,
        }
    }

    /// A stable name for logs and for the `WriteOp` coverage test.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::InsertItems { .. } => "insert_items",
            Self::SetStatus { .. } => "set_status",
            Self::SetAutoStart { .. } => "set_auto_start",
            Self::SetSource { .. } => "set_source",
            Self::SetResolved { .. } => "set_resolved",
            Self::PromoteToGroup { .. } => "promote_to_group",
            Self::SetOutput { .. } => "set_output",
            Self::SetSize { .. } => "set_size",
            Self::PushFile { .. } => "push_file",
            Self::DropEntryBlob { .. } => "drop_entry_blob",
            Self::BumpAttempt { .. } => "bump_attempt",
            Self::SetClearAfter { .. } => "set_clear_after",
            Self::DeleteItems(_) => "delete_items",
            Self::UpsertSubscription(_) => "upsert_subscription",
            Self::MarkSeen { .. } => "mark_seen",
            Self::PruneSeen { .. } => "prune_seen",
            Self::DeleteSubscriptions(_) => "delete_subscriptions",
            Self::UpsertTelegramChat { .. } => "upsert_telegram_chat",
            Self::SetKv { .. } => "set_kv",
            Self::SetMeta { .. } => "set_meta",
        }
    }

    /// Every variant name, so the round-trip test can assert it covers all of them.
    ///
    /// DESIGN §7.1 lists nineteen variants and calls them "eighteen" in prose; the enum is the
    /// authority and this list is asserted against it. WP-05 added the twentieth, `set_meta`.
    pub const NAMES: [&'static str; 20] = [
        "insert_items",
        "set_status",
        "set_auto_start",
        "set_source",
        "set_resolved",
        "promote_to_group",
        "set_output",
        "set_size",
        "push_file",
        "drop_entry_blob",
        "bump_attempt",
        "set_clear_after",
        "delete_items",
        "upsert_subscription",
        "mark_seen",
        "prune_seen",
        "delete_subscriptions",
        "upsert_telegram_chat",
        "set_kv",
        "set_meta",
    ];
}

/// The `Retry` triple of DESIGN §7.1/§8.8, as one helper so the three surfaces that retry an item
/// cannot each get it slightly wrong.
///
/// `SetStatus { Queued, msg: Clear, error: Clear, auto_start: Some(true) }` + `BumpAttempt` +
/// `SetSource { kind: "retry" }`. The status write is what nulls `finished_at` while keeping
/// `started_at`.
#[must_use]
pub fn retry_ops(id: ItemId, at: UnixMs, source: SourceRef) -> Vec<WriteOp> {
    vec![
        WriteOp::SetStatus {
            id,
            status: Status::Queued,
            msg: FieldUpdate::Clear,
            error: FieldUpdate::Clear,
            auto_start: Some(true),
            at,
        },
        WriteOp::BumpAttempt { id },
        WriteOp::SetSource { id, source },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use aulos_core::SourceKind;

    #[test]
    fn names_are_unique_and_complete() {
        let mut names = WriteOp::NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), WriteOp::NAMES.len(), "no duplicate names");
    }

    #[test]
    fn retry_is_the_documented_triple() {
        let id = ItemId::new();
        let ops = retry_ops(id, 42, SourceRef::bare(SourceKind::Retry));
        assert_eq!(
            ops.iter().map(WriteOp::name).collect::<Vec<_>>(),
            ["set_status", "bump_attempt", "set_source"]
        );
        for op in &ops {
            assert_eq!(op.item(), Some(id));
        }
        let WriteOp::SetStatus {
            status,
            msg,
            error,
            auto_start,
            ..
        } = &ops[0]
        else {
            panic!("first op must be the status write");
        };
        assert_eq!(*status, Status::Queued);
        assert_eq!(*msg, FieldUpdate::Clear);
        assert_eq!(*error, FieldUpdate::Clear);
        assert_eq!(*auto_start, Some(true));
    }

    #[test]
    fn batch_wide_ops_target_no_single_item() {
        assert_eq!(WriteOp::DeleteItems(vec![ItemId::new()]).item(), None);
        assert_eq!(
            WriteOp::SetKv {
                key: "cookiefile".into(),
                value: None
            }
            .item(),
            None
        );
    }
}
