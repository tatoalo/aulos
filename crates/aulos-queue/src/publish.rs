//! The published snapshot — lock-free reads for REST and for a connecting socket (DESIGN §15.2).
//!
//! A REST handler and a newly connected WebSocket session must be able to read the **whole** state
//! without touching the engine or the store, otherwise a burst of app foregroundings queues behind
//! progress work. So the aggregator keeps one immutable [`Published`] generation behind an
//! [`ArcSwap`] and swaps a fresh one in after every flush; a reader does one atomic load
//! (~2 ns, wait-free) and holds `Arc`s.
//!
//! Serving 500 items to a connecting client is therefore a few tens of microseconds and **zero**
//! database round trips, which is the graft that keeps connect-to-list latency flat as history
//! grows.
//!
//! # What is in which array
//!
//! | Array | Contents |
//! |---|---|
//! | [`Published::items`] | every non-terminal record **plus every group**, `ord` ascending |
//! | [`Published::done`] | the most recent `AULOS_MEM_DONE_ITEMS` terminal non-group records, `ord` ascending |
//!
//! They hold the same object type and a client may simply concatenate them (PROTOCOL §5.3); the
//! split exists so `?done=false` can omit the completed window. A group stays in `items` even once
//! its roll-up is terminal, because a client renders it as the header of its children.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use aulos_core::{BootId, GroupId, ItemId, ItemView, Seq, Status};
use serde::{Deserialize, Serialize};

/// One counter per [`Status`], in the PROTOCOL §5.3 `counts` key order.
///
/// The counters cover exactly what the published snapshot carries — `items` plus `done` — not the
/// whole database: [`Published::done_total`] is what says how much history exists beyond the
/// window.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct StatusCounts {
    /// `queued` records.
    pub queued: u32,
    /// `resolving` records.
    pub resolving: u32,
    /// `preparing` records.
    pub preparing: u32,
    /// `downloading` records.
    pub downloading: u32,
    /// `postprocessing` records.
    pub postprocessing: u32,
    /// `finished` records.
    pub finished: u32,
    /// `error` records.
    pub error: u32,
    /// `canceled` records.
    pub canceled: u32,
}

impl StatusCounts {
    /// How many records are in `status`.
    #[must_use]
    pub const fn get(&self, status: Status) -> u32 {
        match status {
            Status::Queued => self.queued,
            Status::Resolving => self.resolving,
            Status::Preparing => self.preparing,
            Status::Downloading => self.downloading,
            Status::Postprocessing => self.postprocessing,
            Status::Finished => self.finished,
            Status::Error => self.error,
            Status::Canceled => self.canceled,
        }
    }

    /// Counts one more record in `status`.
    pub const fn add(&mut self, status: Status) {
        *self.slot(status) += 1;
    }

    /// Counts one fewer record in `status`, saturating at zero.
    pub const fn remove(&mut self, status: Status) {
        let slot = self.slot(status);
        *slot = slot.saturating_sub(1);
    }

    /// Moves one record between statuses. `from == to` is a no-op.
    pub const fn moved(&mut self, from: Status, to: Status) {
        if from as u8 == to as u8 {
            return;
        }
        self.remove(from);
        self.add(to);
    }

    /// Every counter summed.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.queued
            + self.resolving
            + self.preparing
            + self.downloading
            + self.postprocessing
            + self.finished
            + self.error
            + self.canceled
    }

    const fn slot(&mut self, status: Status) -> &mut u32 {
        match status {
            Status::Queued => &mut self.queued,
            Status::Resolving => &mut self.resolving,
            Status::Preparing => &mut self.preparing,
            Status::Downloading => &mut self.downloading,
            Status::Postprocessing => &mut self.postprocessing,
            Status::Finished => &mut self.finished,
            Status::Error => &mut self.error,
            Status::Canceled => &mut self.canceled,
        }
    }
}

/// What the snapshot did **not** include (PROTOCOL §5.3).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Truncated {
    /// `true` when [`Published::done`] is a window rather than the whole history, i.e. when
    /// `done_total` exceeds the window length.
    pub done: bool,
    /// Groups whose children were left out of the snapshot.
    ///
    /// v1.0: not implemented, see BRIEF — `AULOS_SNAPSHOT_GROUP_INLINE` and the WS `watch` frame
    /// are CUT, so every non-terminal child ships inline and this list is always empty. The key
    /// stays on the wire because PROTOCOL §5.3 documents it.
    pub groups: Arc<[GroupId]>,
}

impl Default for Truncated {
    fn default() -> Self {
        Self {
            done: false,
            groups: Arc::from([] as [GroupId; 0]),
        }
    }
}

/// One immutable generation of the whole queue state (DESIGN §15.2).
///
/// Cheap to clone (eight `Arc`s and a handful of scalars) and never mutated in place: the
/// aggregator builds the next generation and swaps it in.
#[derive(Clone, Debug)]
pub struct Published {
    /// The frame sequence this generation is current as of.
    ///
    /// Never **newer** than the last frame put on the socket — the aggregator republishes after
    /// emitting a flush's frames, so a REST reader can never observe state a socket reader has not
    /// been told about (DESIGN §15.1).
    pub seq: Seq,
    /// The process that produced it. A client whose `since` cursor carries a different `boot_id`
    /// is handed a snapshot, never a delta.
    pub boot_id: BootId,
    /// Every non-terminal record plus every group, `ord` ascending then `id` ascending.
    pub items: Arc<[Arc<ItemView>]>,
    /// The most recent `AULOS_MEM_DONE_ITEMS` terminal records, `ord` ascending then `id`.
    pub done: Arc<[Arc<ItemView>]>,
    /// `id` → position: an index into [`Self::items`], or `items.len() + i` for the `i`-th entry
    /// of [`Self::done`].
    ///
    /// Rebuilt only when membership changes, so a tick that merely moves numbers hands the next
    /// generation the same allocation (pointer-equal across the swap).
    pub by_id: Arc<HashMap<ItemId, u32>>,
    /// The status histogram over `items` + `done`.
    pub counts: StatusCounts,
    /// How many terminal records exist in total, including those outside the window.
    pub done_total: u64,
    /// What the snapshot left out.
    pub truncated: Truncated,
}

impl Published {
    /// An empty generation, for a process that has not recovered anything yet.
    #[must_use]
    pub fn empty(boot_id: BootId) -> Self {
        Self {
            seq: Seq(0),
            boot_id,
            items: Arc::from([] as [Arc<ItemView>; 0]),
            done: Arc::from([] as [Arc<ItemView>; 0]),
            by_id: Arc::new(HashMap::new()),
            counts: StatusCounts::default(),
            done_total: 0,
            truncated: Truncated::default(),
        }
    }

    /// How many records this generation carries in total.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len() + self.done.len()
    }

    /// Whether it carries none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One record by id, from either array, without a scan.
    #[must_use]
    pub fn get(&self, id: ItemId) -> Option<&Arc<ItemView>> {
        let at = *self.by_id.get(&id)? as usize;
        self.items
            .get(at)
            .or_else(|| self.done.get(at - self.items.len()))
    }

    /// Every record, `items` then `done` — the concatenation PROTOCOL §5.3 tells a client it may
    /// form.
    pub fn all(&self) -> impl Iterator<Item = &Arc<ItemView>> {
        self.items.iter().chain(self.done.iter())
    }
}

/// The reader's handle on the published snapshot (DESIGN §15.2).
///
/// Handed out by [`crate::Aggregator::new`] and held by `aulos-api`'s state. Cloning it shares the
/// same cell, so every reader sees the same generation.
#[derive(Clone, Debug)]
pub struct StateView(Arc<ArcSwap<Published>>);

impl StateView {
    /// A view over a fresh, empty cell. `aulos-api` never constructs one; the aggregator does.
    #[must_use]
    pub fn new(boot_id: BootId) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(Published::empty(boot_id))))
    }

    /// The current generation. Wait-free, and safe to hold across an `await` — the aggregator is
    /// never blocked by a reader.
    #[must_use]
    pub fn load(&self) -> arc_swap::Guard<Arc<Published>> {
        self.0.load()
    }

    /// The current generation as an owned `Arc`, for a caller that wants to keep it.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Published> {
        self.0.load_full()
    }

    /// Swaps in the next generation. Only the aggregator calls this.
    pub(crate) fn store(&self, next: Arc<Published>) {
        self.0.store(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::tests_support::view;

    #[test]
    fn the_counts_key_order_is_the_protocol_order() {
        let json = serde_json::to_string(&StatusCounts::default()).unwrap();
        assert_eq!(
            json,
            r#"{"queued":0,"resolving":0,"preparing":0,"downloading":0,"postprocessing":0,"finished":0,"error":0,"canceled":0}"#
        );
    }

    #[test]
    fn counters_move_and_never_underflow() {
        let mut c = StatusCounts::default();
        for s in Status::ALL {
            c.add(s);
        }
        assert_eq!(c.total(), 8);
        for s in Status::ALL {
            assert_eq!(c.get(s), 1);
        }
        c.moved(Status::Queued, Status::Downloading);
        assert_eq!(c.queued, 0);
        assert_eq!(c.downloading, 2);
        c.moved(Status::Error, Status::Error);
        assert_eq!(c.error, 1, "a self-edge is a no-op");
        c.remove(Status::Queued);
        assert_eq!(c.queued, 0, "saturating, not wrapping");
    }

    #[test]
    fn get_reaches_both_arrays() {
        let a = view(Status::Downloading, 1);
        let b = view(Status::Finished, 2);
        let mut by_id = HashMap::new();
        by_id.insert(a.id, 0);
        by_id.insert(b.id, 1);
        let p = Published {
            items: Arc::from([Arc::clone(&a)]),
            done: Arc::from([Arc::clone(&b)]),
            by_id: Arc::new(by_id),
            ..Published::empty(BootId::new())
        };
        assert_eq!(p.len(), 2);
        assert!(!p.is_empty());
        assert_eq!(p.get(a.id).map(|v| v.id), Some(a.id));
        assert_eq!(p.get(b.id).map(|v| v.id), Some(b.id));
        assert!(p.get(view(Status::Queued, 3).id).is_none());
        assert_eq!(p.all().count(), 2);
    }

    #[test]
    fn an_empty_generation_serves_nothing_and_truncates_nothing() {
        let v = StateView::new(BootId::new());
        let p = v.load();
        assert!(p.is_empty());
        assert!(!p.truncated.done);
        assert!(p.truncated.groups.is_empty());
        assert_eq!(p.seq, Seq(0));
        assert_eq!(v.snapshot().seq, Seq(0));
    }
}
