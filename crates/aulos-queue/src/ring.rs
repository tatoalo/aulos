//! Frames, and the dual-representation replay ring behind `?since=` (DESIGN §15.3).
//!
//! A frame is serialised **once** and shared: N connected clients cost N sends of a refcount, not
//! N JSON encodings. That is the direct fix for the legacy "one full-object broadcast per progress
//! hook, double-encoded" problem.
//!
//! The ring keeps two representations of every frame because they serve two jobs:
//!
//! - [`RingEntry::wire`] is the bytes a live subscriber gets, and the bytes a replay would send;
//! - [`RingEntry::body`] is the **structured** payload, so a resume can *merge* instead of replay.
//!   Replaying 180 raw `delta` frames to a client that was away 45 s would send 180 frames of
//!   mostly-stale numbers; [`Ring::merge_after`] folds them into one frame with the last value per
//!   `(id, field)`.
//!
//! # The ring is shared, so an `ack` never trims it
//!
//! Every connected client reads the same ring, so trimming it on one client's cursor would
//! silently break another client's `?since=`. [`Ring::floor`] is advanced **exclusively** by the
//! frame and byte bounds (`AULOS_WS_REPLAY_FRAMES` 512, `AULOS_WS_REPLAY_BYTES` 4 MiB), which is
//! what PROTOCOL §5.11 promises client authors.
//!
//! v1.0: not implemented, see BRIEF — the client → server `ack` frame is CUT, so nothing in this
//! module takes a client cursor at all and the retention rule above is structural rather than a
//! discipline.

use std::collections::VecDeque;
use std::sync::Arc;

use aulos_core::{AddReason, ItemId, ItemView, RemoveReason, Seq};
use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every frame type PROTOCOL §5.2 defines, as the `t` field.
///
/// Not every kind is replayable: `snapshot`, `resume`, `pong` and `error` are built per connection
/// by `aulos-api`, never broadcast, and never enter the ring — see [`Self::is_replayable`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// The complete state, sent on connect and after a lag resync.
    Snapshot,
    /// The summary that replaces a `snapshot` when a `since` cursor was resumable.
    Resume,
    /// Changed fields only.
    Delta,
    /// Full `Item` objects — an upsert.
    Added,
    /// Full `Item` objects, terminal.
    Completed,
    /// Ids and one reason.
    Removed,
    /// One whole `Subscription` object.
    Subscription,
    /// Subscription ids that went away.
    SubscriptionRemoved,
    /// The `YTDL_OPTIONS_FILE` reload result.
    YtdlOptions,
    /// A plugin reload report.
    Providers,
    /// A human-readable warning.
    Notice,
    /// A component status transition.
    Health,
    /// An RTT probe, echoed.
    Pong,
    /// A protocol or auth error, usually followed by a close.
    Error,
}

impl FrameKind {
    /// The wire string, without going through serde.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Resume => "resume",
            Self::Delta => "delta",
            Self::Added => "added",
            Self::Completed => "completed",
            Self::Removed => "removed",
            Self::Subscription => "subscription",
            Self::SubscriptionRemoved => "subscription_removed",
            Self::YtdlOptions => "ytdl_options",
            Self::Providers => "providers",
            Self::Notice => "notice",
            Self::Health => "health",
            Self::Pong => "pong",
            Self::Error => "error",
        }
    }

    /// Whether a frame of this kind is broadcast to every client and kept for replay.
    ///
    /// The four that are not are the per-connection ones: a `snapshot` and a `resume` answer one
    /// client's cursor, and a `pong` or an `error` answers one client's frame.
    #[must_use]
    pub const fn is_replayable(self) -> bool {
        !matches!(
            self,
            Self::Snapshot | Self::Resume | Self::Pong | Self::Error
        )
    }

    /// Every kind, in declaration order.
    pub const ALL: [Self; 14] = [
        Self::Snapshot,
        Self::Resume,
        Self::Delta,
        Self::Added,
        Self::Completed,
        Self::Removed,
        Self::Subscription,
        Self::SubscriptionRemoved,
        Self::YtdlOptions,
        Self::Providers,
        Self::Notice,
        Self::Health,
        Self::Pong,
        Self::Error,
    ];
}

impl std::fmt::Display for FrameKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One serialised frame, shared by every subscriber and by the replay ring.
///
/// Deviation from DESIGN §15.3, which types `text` as `axum::extract::ws::Utf8Bytes`: that would
/// make `aulos-queue` depend on `axum` and invert the DESIGN §3 dependency direction. `bytes` is
/// in DESIGN §18.6 for exactly this job ("pre-serialised WS frames shared across clients without
/// copies") and `Utf8Bytes` converts from `Bytes` without a copy on the `aulos-api` side.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WireFrame {
    /// The frame sequence. Strictly increasing across all kinds within one boot.
    pub seq: Seq,
    /// Which frame this is — also the `t` field of the JSON.
    pub kind: FrameKind,
    /// The complete JSON text, UTF-8, one object.
    pub text: bytes::Bytes,
}

impl WireFrame {
    /// The frame as text. Always succeeds: the bytes came from `serde_json`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.text).unwrap_or_default()
    }

    /// How many bytes the frame occupies, which is what the ring's byte bound counts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Whether the frame carries no bytes at all (only possible if serialisation failed).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// One item's changed fields, in `ItemView::FIELDS` order, `id` first.
///
/// The map is what the diff of DESIGN §15.1 writes into and what a merge folds: per `(id, field)`
/// the last value wins.
#[derive(Clone, PartialEq, Debug)]
pub struct DeltaItem {
    /// Which record. Always serialised, always first.
    pub id: ItemId,
    /// The fields that differed from `last_sent`. An absent key means unchanged; a `null` value
    /// means the field changed **to** null (PROTOCOL §5.4).
    pub fields: IndexMap<&'static str, Value>,
}

impl DeltaItem {
    /// A patch for `id` with no fields yet.
    #[must_use]
    pub fn new(id: ItemId) -> Self {
        Self {
            id,
            fields: IndexMap::new(),
        }
    }

    /// Whether the patch would carry nothing but `id`, in which case it is not worth a wire slot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl Serialize for DeltaItem {
    /// `{ "id": …, <changed fields> }` — `id` first, then the fields in diff order.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(self.fields.len() + 1))?;
        map.serialize_entry("id", &self.id)?;
        for (k, v) in &self.fields {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// A whole `delta` frame in structured form (DESIGN §15.3's `DeltaBatch`).
#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct DeltaBatch {
    /// Server time in unix milliseconds, the frame's `ts` field.
    pub ts: i64,
    /// One patch per changed record.
    pub items: Vec<DeltaItem>,
}

/// The structured payload kept beside the serialised frame so a resume can fold a window.
///
/// Addition to DESIGN §15.3, whose `RingEntry` keeps only `batch: Option<Arc<DeltaBatch>>`: the
/// §15.3 merge table has a rule for `added`, `completed` and `removed` as well, and folding those
/// from the serialised text would mean re-parsing JSON the server just produced.
#[derive(Clone, Debug)]
pub enum FrameBody {
    /// An `added` frame: full objects, an upsert.
    Added {
        /// Why they appeared.
        reason: AddReason,
        /// The objects.
        items: Vec<Arc<ItemView>>,
    },
    /// A `completed` frame: full objects, terminal.
    Completed {
        /// The objects.
        items: Vec<Arc<ItemView>>,
    },
    /// A `removed` frame: ids and one reason for all of them.
    Removed {
        /// Why they went away.
        reason: RemoveReason,
        /// The ids.
        ids: Vec<ItemId>,
    },
    /// A `delta` frame: changed fields only.
    Delta(Arc<DeltaBatch>),
    /// Any other replayable kind — `subscription`, `subscription_removed`, `ytdl_options`,
    /// `providers`, `notice`, `health`. Rare and small, so a fold accumulates them verbatim.
    Other {
        /// Which kind.
        kind: FrameKind,
        /// The payload object, without `t`/`seq`.
        value: Value,
    },
}

impl FrameBody {
    /// The frame kind this body produces.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        match self {
            Self::Added { .. } => FrameKind::Added,
            Self::Completed { .. } => FrameKind::Completed,
            Self::Removed { .. } => FrameKind::Removed,
            Self::Delta(_) => FrameKind::Delta,
            Self::Other { kind, .. } => *kind,
        }
    }
}

/// One ring slot: the bytes plus the structure.
#[derive(Clone, Debug)]
pub struct RingEntry {
    /// The frame sequence.
    pub seq: Seq,
    /// What a replay would send.
    pub wire: Arc<WireFrame>,
    /// What a merge folds.
    pub body: Arc<FrameBody>,
}

/// How many ids and patches went into a fold — the `merged` block of a `resume` frame
/// (PROTOCOL §6.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct MergeCounts {
    /// Distinct ids in the merged `added` frame.
    pub added: usize,
    /// Distinct ids in the merged `completed` frame.
    pub completed: usize,
    /// **Total id count** across every merged `removed` frame, not a frame count.
    pub removed: usize,
    /// Distinct ids in the merged `delta` frame.
    pub delta_items: usize,
}

/// What a window of frames folds down to (DESIGN §15.3).
///
/// The emission order is fixed and identical to the live flush order and to the REST `?since=`
/// order: `added`, `completed`, every `removed` group in reason order, `delta` — then the rare
/// passthrough frames, which are independent of item state.
#[derive(Clone, Debug)]
pub struct Fold {
    /// The cursor the client sent.
    pub from: Seq,
    /// The cursor it will hold afterwards — the head of the folded window.
    pub to: Seq,
    /// The merged `added` frame, if the window contained one.
    pub added: Option<(AddReason, Vec<Arc<ItemView>>)>,
    /// The merged `completed` objects.
    pub completed: Vec<Arc<ItemView>>,
    /// One entry per distinct reason, in the fixed reason order.
    pub removed: Vec<(RemoveReason, Vec<ItemId>)>,
    /// The merged patches, one per id.
    pub delta: Vec<DeltaItem>,
    /// The passthrough frames, in publish order.
    pub others: Vec<(FrameKind, Value)>,
    /// What went into the fold.
    pub counts: MergeCounts,
}

impl Fold {
    /// Whether the fold carries nothing at all, which happens when the window held only frames
    /// that cancelled out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_none()
            && self.completed.is_empty()
            && self.removed.is_empty()
            && self.delta.is_empty()
            && self.others.is_empty()
    }
}

/// The bounded replay ring (DESIGN §15.3).
#[derive(Debug)]
pub struct Ring {
    entries: VecDeque<RingEntry>,
    bytes: u64,
    floor: Seq,
    max_frames: usize,
    max_bytes: u64,
}

impl Ring {
    /// A ring bounded by `max_frames` **and** `max_bytes`, whose window starts just above `floor`.
    ///
    /// `floor` is the sequence value that has already been superseded — normally the durable
    /// allocator's cursor at boot, so the first client to connect with the boot's own `seq` is
    /// `UpToDate` rather than being handed a snapshot.
    #[must_use]
    pub fn new(max_frames: usize, max_bytes: u64, floor: Seq) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            floor,
            max_frames: max_frames.max(1),
            max_bytes: max_bytes.max(1),
        }
    }

    /// The highest sequence that is **no longer** replayable.
    ///
    /// A cursor at or above it can be merged; below it, only a snapshot is honest.
    #[must_use]
    pub const fn floor(&self) -> Seq {
        self.floor
    }

    /// How many frames are retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many bytes the retained frames occupy.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Appends a frame and evicts from the front until both bounds hold.
    ///
    /// The newest entry is always retained, even when it alone exceeds the byte bound: evicting it
    /// would leave a window that can serve nobody.
    pub fn push(&mut self, entry: RingEntry) {
        self.bytes = self.bytes.saturating_add(entry.wire.len() as u64);
        self.entries.push_back(entry);
        while self.entries.len() > 1
            && (self.entries.len() > self.max_frames || self.bytes > self.max_bytes)
        {
            if let Some(old) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.wire.len() as u64);
                self.floor = old.seq;
            }
        }
    }

    /// The retained frames strictly above `since`, oldest first.
    pub fn after(&self, since: Seq) -> impl Iterator<Item = &RingEntry> {
        self.entries.iter().filter(move |e| e.seq > since)
    }

    /// Folds every retained frame above `since` per the DESIGN §15.3 merge table.
    ///
    /// | Kind in the window | Rule |
    /// |---|---|
    /// | `delta` | per `(id, field)`, last value wins |
    /// | `added` | accumulate; if the id is later `removed`, drop both |
    /// | `completed` | accumulate as a full object; supersedes any earlier delta for that id |
    /// | `removed` | accumulate **per reason**; drops any earlier `added`/`completed`/`delta` |
    /// | the rest | accumulate in order |
    ///
    /// The rules are symmetric in time, which is what makes the fold provably equivalent to a
    /// replay (`tests/realtime.rs`): **the last frame that spoke about an id is the only one that
    /// survives**. A `removed` drops the full objects and patches before it, a later
    /// `added`/`completed` drops the removal before it, and an `added` and a `completed` for the
    /// same id keep only whichever came last. Both halves are needed because the emission order is
    /// fixed: without the second, a delete racing a terminal write would fold into "create it,
    /// then delete it" and lose a row the replay keeps; and an `added` after a `completed` would
    /// be overwritten by the terminal object it superseded.
    #[must_use]
    pub fn merge_after(&self, since: Seq) -> Fold {
        let mut added: IndexMap<ItemId, Arc<ItemView>> = IndexMap::new();
        let mut add_reason: Option<AddReason> = None;
        let mut completed: IndexMap<ItemId, Arc<ItemView>> = IndexMap::new();
        let mut removed: IndexMap<RemoveReason, IndexSet<ItemId>> = IndexMap::new();
        let mut delta: IndexMap<ItemId, IndexMap<&'static str, Value>> = IndexMap::new();
        let mut others: Vec<(FrameKind, Value)> = Vec::new();
        let mut to = since;

        for entry in self.after(since) {
            to = std::cmp::max(to, entry.seq);
            match &*entry.body {
                FrameBody::Added { reason, items } => {
                    add_reason = Some(merge_add_reason(add_reason, *reason));
                    for view in items {
                        delta.shift_remove(&view.id);
                        completed.shift_remove(&view.id);
                        forget_removal(&mut removed, view.id);
                        added.insert(view.id, Arc::clone(view));
                    }
                }
                FrameBody::Completed { items } => {
                    for view in items {
                        delta.shift_remove(&view.id);
                        added.shift_remove(&view.id);
                        forget_removal(&mut removed, view.id);
                        completed.insert(view.id, Arc::clone(view));
                    }
                }
                FrameBody::Removed { reason, ids } => {
                    for id in ids {
                        added.shift_remove(id);
                        completed.shift_remove(id);
                        delta.shift_remove(id);
                        forget_removal(&mut removed, *id);
                        removed.entry(*reason).or_default().insert(*id);
                    }
                }
                FrameBody::Delta(batch) => {
                    for item in &batch.items {
                        if removed.values().any(|set| set.contains(&item.id)) {
                            continue;
                        }
                        let slot = delta.entry(item.id).or_default();
                        for (key, value) in &item.fields {
                            slot.insert(*key, value.clone());
                        }
                    }
                }
                FrameBody::Other { kind, value } => others.push((*kind, value.clone())),
            }
        }

        let removed: Vec<(RemoveReason, Vec<ItemId>)> = REASON_ORDER
            .iter()
            .filter_map(|reason| {
                let ids = removed.get(reason)?;
                (!ids.is_empty()).then(|| (*reason, ids.iter().copied().collect()))
            })
            .collect();
        let delta: Vec<DeltaItem> = delta
            .into_iter()
            .filter(|(_, fields)| !fields.is_empty())
            .map(|(id, fields)| DeltaItem { id, fields })
            .collect();
        let counts = MergeCounts {
            added: added.len(),
            completed: completed.len(),
            removed: removed.iter().map(|(_, ids)| ids.len()).sum(),
            delta_items: delta.len(),
        };
        Fold {
            from: since,
            to,
            added: add_reason
                .filter(|_| !added.is_empty())
                .map(|reason| (reason, added.into_values().collect())),
            completed: completed.into_values().collect(),
            removed,
            delta,
            others,
            counts,
        }
    }
}

/// Drops `id` from every reason bucket, because a later frame has superseded its removal.
fn forget_removal(removed: &mut IndexMap<RemoveReason, IndexSet<ItemId>>, id: ItemId) {
    for set in removed.values_mut() {
        set.shift_remove(&id);
    }
}

/// The fixed order a flush and a fold emit `removed` frames in (DESIGN §15.1, PROTOCOL §5.7).
///
/// DESIGN and PROTOCOL spell the four reasons `deleted`, `cleared`, `auto_cleared`,
/// `group_cascade`; `aulos_core::RemoveReason` names the last two `Expired` and `Replaced` and
/// puts them in the same order, so this is the declaration order of the enum.
pub const REASON_ORDER: [RemoveReason; 4] = [
    RemoveReason::Deleted,
    RemoveReason::Cleared,
    RemoveReason::Expired,
    RemoveReason::Replaced,
];

/// The reason a merged `added` frame carries when the window held several.
///
/// PROTOCOL §6.3 allows exactly one `added` frame after a `resume`, so the reasons have to
/// collapse: the most specific wins, because `expanded` is what tells a client a playlist turned
/// into a group. `reason` is cosmetic — the upsert is keyed on `id` either way.
const fn merge_add_reason(current: Option<AddReason>, next: AddReason) -> AddReason {
    let Some(current) = current else { return next };
    match (current, next) {
        (AddReason::Expanded, _) | (_, AddReason::Expanded) => AddReason::Expanded,
        (AddReason::Retried, _) | (_, AddReason::Retried) => AddReason::Retried,
        _ => AddReason::Created,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::tests_support::view;
    use aulos_core::Status;

    fn frame(seq: u64, body: FrameBody, size: usize) -> RingEntry {
        let kind = body.kind();
        RingEntry {
            seq: Seq(seq),
            wire: Arc::new(WireFrame {
                seq: Seq(seq),
                kind,
                text: bytes::Bytes::from(vec![b'x'; size]),
            }),
            body: Arc::new(body),
        }
    }

    fn delta(seq: u64, id: ItemId, fields: &[(&'static str, Value)]) -> RingEntry {
        let mut item = DeltaItem::new(id);
        for (k, v) in fields {
            item.fields.insert(*k, v.clone());
        }
        frame(
            seq,
            FrameBody::Delta(Arc::new(DeltaBatch {
                ts: 0,
                items: vec![item],
            })),
            64,
        )
    }

    #[test]
    fn the_four_per_connection_kinds_are_not_replayable() {
        for kind in FrameKind::ALL {
            let replayable = kind.is_replayable();
            let expected = !matches!(
                kind,
                FrameKind::Snapshot | FrameKind::Resume | FrameKind::Pong | FrameKind::Error
            );
            assert_eq!(replayable, expected, "{kind}");
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }

    #[test]
    fn a_delta_item_serialises_id_first_then_its_fields_in_order() {
        let id = ItemId::new();
        let mut item = DeltaItem::new(id);
        item.fields.insert("percent", Value::from(43.9));
        item.fields.insert("speed", Value::Null);
        assert!(!item.is_empty());
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(
            json,
            format!("{{\"id\":\"{id}\",\"percent\":43.9,\"speed\":null}}")
        );
    }

    #[test]
    fn the_frame_bound_evicts_the_oldest_and_advances_the_floor() {
        let mut ring = Ring::new(3, 1 << 20, Seq(10));
        for seq in 11..=13 {
            ring.push(frame(seq, FrameBody::Completed { items: vec![] }, 10));
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.floor(), Seq(10));
        ring.push(frame(14, FrameBody::Completed { items: vec![] }, 10));
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.floor(), Seq(11), "seq 11 is gone");
        assert_eq!(ring.after(Seq(11)).count(), 3);
        assert_eq!(ring.bytes(), 30);
    }

    /// The DESIGN §15.3 bound that matters for a 500-item playlist: a burst of large `added`
    /// frames must evict on **bytes**, long before the 512-frame cap.
    #[test]
    fn the_byte_bound_evicts_before_the_frame_bound() {
        let mut ring = Ring::new(512, 4_096, Seq(0));
        for seq in 1..=10 {
            ring.push(frame(seq, FrameBody::Completed { items: vec![] }, 1_000));
        }
        assert!(ring.len() < 512, "the frame cap was never reached");
        assert!(ring.bytes() <= 4_096, "{} bytes retained", ring.bytes());
        assert!(ring.floor() > Seq(0), "the floor advanced");
    }

    #[test]
    fn a_single_oversized_frame_is_still_retained() {
        let mut ring = Ring::new(512, 16, Seq(0));
        ring.push(frame(1, FrameBody::Completed { items: vec![] }, 4_096));
        assert_eq!(ring.len(), 1, "evicting it would serve nobody");
        assert!(!ring.is_empty());
    }

    #[test]
    fn a_delta_folds_to_the_last_value_per_id_and_field() {
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        let id = ItemId::new();
        ring.push(delta(
            1,
            id,
            &[("percent", 10.0.into()), ("eta", 90.into())],
        ));
        ring.push(delta(2, id, &[("percent", 20.0.into())]));
        ring.push(delta(
            3,
            id,
            &[("percent", 30.0.into()), ("speed", Value::Null)],
        ));
        let fold = ring.merge_after(Seq(0));
        assert_eq!(fold.from, Seq(0));
        assert_eq!(fold.to, Seq(3));
        assert_eq!(fold.delta.len(), 1);
        let fields = &fold.delta[0].fields;
        assert_eq!(fields["percent"], Value::from(30.0));
        assert_eq!(fields["eta"], Value::from(90), "an untouched key survives");
        assert_eq!(
            fields["speed"],
            Value::Null,
            "null is a value, not an absence"
        );
        assert_eq!(fold.counts.delta_items, 1);
    }

    #[test]
    fn added_then_removed_drops_both() {
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        let v = view(Status::Queued, 1);
        ring.push(frame(
            1,
            FrameBody::Added {
                reason: AddReason::Created,
                items: vec![Arc::clone(&v)],
            },
            64,
        ));
        ring.push(delta(2, v.id, &[("percent", 5.0.into())]));
        ring.push(frame(
            3,
            FrameBody::Removed {
                reason: RemoveReason::Deleted,
                ids: vec![v.id],
            },
            32,
        ));
        let fold = ring.merge_after(Seq(0));
        assert!(fold.added.is_none(), "the added was cancelled out");
        assert!(fold.delta.is_empty(), "so was its patch");
        assert_eq!(fold.removed, vec![(RemoveReason::Deleted, vec![v.id])]);
        assert_eq!(fold.counts.removed, 1);
        assert!(!fold.is_empty());
    }

    #[test]
    fn a_completed_supersedes_an_earlier_delta_and_a_later_one_survives() {
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        let v = view(Status::Finished, 1);
        ring.push(delta(1, v.id, &[("percent", 99.0.into())]));
        ring.push(frame(
            2,
            FrameBody::Completed {
                items: vec![Arc::clone(&v)],
            },
            128,
        ));
        let fold = ring.merge_after(Seq(0));
        assert_eq!(fold.completed.len(), 1);
        assert!(fold.delta.is_empty(), "the full object won");

        // A hook's `set_size` writeback lands after the terminal frame and must not be lost.
        ring.push(delta(3, v.id, &[("size", 4_096.into())]));
        let fold = ring.merge_after(Seq(0));
        assert_eq!(fold.completed.len(), 1);
        assert_eq!(fold.delta.len(), 1);
        assert_eq!(fold.delta[0].fields["size"], Value::from(4_096));
    }

    /// The fold's ordering rule in both directions, which is what makes it equal to a replay: for
    /// one id, only the **last** frame that spoke about it survives.
    #[test]
    fn only_the_last_word_on_an_id_survives_a_fold() {
        let v = view(Status::Finished, 1);
        let full = |seq: u64, terminal: bool| {
            if terminal {
                frame(
                    seq,
                    FrameBody::Completed {
                        items: vec![Arc::clone(&v)],
                    },
                    96,
                )
            } else {
                frame(
                    seq,
                    FrameBody::Added {
                        reason: AddReason::Created,
                        items: vec![Arc::clone(&v)],
                    },
                    96,
                )
            }
        };
        let gone = |seq: u64| {
            frame(
                seq,
                FrameBody::Removed {
                    reason: RemoveReason::Deleted,
                    ids: vec![v.id],
                },
                32,
            )
        };

        // added → completed: the terminal object wins, and the `added` is not re-sent.
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        ring.push(full(1, false));
        ring.push(full(2, true));
        let fold = ring.merge_after(Seq(0));
        assert!(fold.added.is_none());
        assert_eq!(fold.completed.len(), 1);

        // completed → added (a retry re-queued it): the `added` wins.
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        ring.push(full(1, true));
        ring.push(full(2, false));
        let fold = ring.merge_after(Seq(0));
        assert!(fold.completed.is_empty());
        assert_eq!(fold.added.as_ref().map(|(_, i)| i.len()), Some(1));

        // removed → completed (a delete racing a terminal write): the row comes back, because that
        // is what replaying the two frames in order would leave.
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        ring.push(gone(1));
        ring.push(full(2, true));
        let fold = ring.merge_after(Seq(0));
        assert!(fold.removed.is_empty());
        assert_eq!(fold.completed.len(), 1);

        // completed → removed: the row goes.
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        ring.push(full(1, true));
        ring.push(gone(2));
        let fold = ring.merge_after(Seq(0));
        assert!(fold.completed.is_empty());
        assert_eq!(fold.removed.len(), 1);
    }

    #[test]
    fn removals_fold_per_reason_in_the_documented_order() {
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        let a = ItemId::new();
        let b = ItemId::new();
        let c = ItemId::new();
        ring.push(frame(
            1,
            FrameBody::Removed {
                reason: RemoveReason::Expired,
                ids: vec![c],
            },
            32,
        ));
        ring.push(frame(
            2,
            FrameBody::Removed {
                reason: RemoveReason::Deleted,
                ids: vec![a, b],
            },
            32,
        ));
        let fold = ring.merge_after(Seq(0));
        assert_eq!(
            fold.removed,
            vec![
                (RemoveReason::Deleted, vec![a, b]),
                (RemoveReason::Expired, vec![c]),
            ],
            "the fixed reason order, not publish order"
        );
        assert_eq!(fold.counts.removed, 3);
    }

    #[test]
    fn the_merged_add_reason_prefers_the_most_specific() {
        assert_eq!(
            merge_add_reason(None, AddReason::Created),
            AddReason::Created
        );
        assert_eq!(
            merge_add_reason(Some(AddReason::Created), AddReason::Expanded),
            AddReason::Expanded
        );
        assert_eq!(
            merge_add_reason(Some(AddReason::Expanded), AddReason::Created),
            AddReason::Expanded
        );
        assert_eq!(
            merge_add_reason(Some(AddReason::Created), AddReason::Retried),
            AddReason::Retried
        );
    }

    #[test]
    fn passthrough_frames_accumulate_in_order() {
        let mut ring = Ring::new(512, 1 << 20, Seq(0));
        for (seq, code) in [(1u64, "stalled"), (2, "pot_down")] {
            ring.push(frame(
                seq,
                FrameBody::Other {
                    kind: FrameKind::Notice,
                    value: serde_json::json!({ "code": code }),
                },
                48,
            ));
        }
        let fold = ring.merge_after(Seq(0));
        assert_eq!(fold.others.len(), 2);
        assert_eq!(fold.others[0].1["code"], "stalled");
        assert_eq!(fold.others[1].1["code"], "pot_down");
    }

    #[test]
    fn an_empty_window_folds_to_nothing_and_keeps_the_cursor() {
        let ring = Ring::new(512, 1 << 20, Seq(7));
        let fold = ring.merge_after(Seq(7));
        assert!(fold.is_empty());
        assert_eq!(fold.to, Seq(7));
        assert_eq!(fold.counts, MergeCounts::default());
    }
}
