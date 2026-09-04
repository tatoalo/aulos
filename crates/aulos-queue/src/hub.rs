//! The event hub: one `seq` allocator, one serialisation per frame, one broadcast, one replay ring
//! (DESIGN §15.3).
//!
//! Everything a client can receive over the socket that is not per-connection goes through
//! [`EventHub::publish_frame`]. The hub does three things with it, in this order and under one
//! lock, so that `seq`, the ring and the broadcast can never disagree:
//!
//! 1. allocates the next durable `seq` (reserve-before-use, so a crash skips values and can never
//!    re-issue one);
//! 2. serialises the frame **once** into shared [`bytes::Bytes`] and records it in the ring;
//! 3. broadcasts the `Arc<WireFrame>`.
//!
//! N connected clients therefore cost N sends of a refcount, not N JSON encodings.
//!
//! # What is not published here
//!
//! `snapshot`, `resume`, `pong` and `error` are per-connection frames: they answer one client's
//! cursor or one client's frame, so `aulos-api` builds them itself from [`crate::StateView`] and
//! [`EventHub::resume`]. [`EventHub::publish`] refuses them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use aulos_core::{AddReason, BootId, Config, HiLoAllocator, ItemId, ItemView, RemoveReason, Seq};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::ring::{DeltaItem, Fold, FrameBody, FrameKind, MergeCounts, Ring, RingEntry, WireFrame};

/// The WS bus depth (DESIGN §2.3). A lagging reader gets `Lagged(n)` and one fresh snapshot.
pub const BROADCAST_CAPACITY: usize = 256;

/// What a `?since=` cursor is worth (DESIGN §15.3).
#[derive(Clone, Debug)]
pub enum Resume {
    /// Discard local state and take a fresh snapshot: a different boot, a cursor below the replay
    /// window, or a cursor **above** the head (which happens after a database restore).
    Snapshot,
    /// The cursor is the head; nothing to send.
    UpToDate,
    /// The window folded down to these frames. Apply them in order; the cursor becomes `to`.
    Merged {
        /// The cursor the client sent.
        from: Seq,
        /// The cursor it holds afterwards.
        to: Seq,
        /// What went into the fold — the `merged` block of the `resume` frame (PROTOCOL §6.3).
        merged: MergeCounts,
        /// At most one `added`, at most one `completed`, at most one `removed` **per distinct
        /// reason**, at most one `delta`, then the rare passthrough frames — in that order.
        frames: Vec<Arc<WireFrame>>,
    },
}

impl Resume {
    /// A stable name for logs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::UpToDate => "up_to_date",
            Self::Merged { .. } => "merged",
        }
    }
}

struct Inner {
    seq: Arc<dyn HiLoAllocator>,
    boot_id: BootId,
    tx: broadcast::Sender<Arc<WireFrame>>,
    ring: Mutex<Ring>,
    head: AtomicU64,
    frames: AtomicU64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventHub")
            .field("boot_id", &self.boot_id)
            .field("head", &self.head.load(Ordering::Relaxed))
            .field("frames", &self.frames.load(Ordering::Relaxed))
            .field("subscribers", &self.tx.receiver_count())
            .finish_non_exhaustive()
    }
}

/// The process-wide frame bus (DESIGN §15.3).
///
/// Cheap to clone — the aggregator owns one and `aulos-api` holds another, both over the same
/// `Arc`. DESIGN §15.3 writes the struct with the `Mutex<Ring>` inline; sharing it between the
/// aggregator task and every HTTP handler needs the `Arc`, and nothing else changes.
#[derive(Clone, Debug)]
pub struct EventHub {
    inner: Arc<Inner>,
}

impl EventHub {
    /// Wraps the store's durable `seq` allocator and this process's boot id.
    ///
    /// `seq` comes from `Store::seq_allocator()`: the store keeps its `ord`/`seq` cursors private
    /// and hands out exactly these two accessors, so the hub can be given one without the store
    /// knowing what a frame is.
    #[must_use]
    pub fn new(seq: Arc<dyn HiLoAllocator>, boot_id: BootId, cfg: &Config) -> Self {
        let floor = Seq(u64::try_from(seq.current()).unwrap_or(0));
        let ring = Ring::new(cfg.ws_replay_frames as usize, cfg.ws_replay_bytes, floor);
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                seq,
                boot_id,
                tx,
                ring: Mutex::new(ring),
                head: AtomicU64::new(floor.0),
                frames: AtomicU64::new(0),
            }),
        }
    }

    /// This process's boot id, carried in every `snapshot` and in `healthz`.
    #[must_use]
    pub fn boot_id(&self) -> BootId {
        self.inner.boot_id
    }

    /// The sequence of the most recently published frame.
    #[must_use]
    pub fn head(&self) -> Seq {
        Seq(self.inner.head.load(Ordering::Acquire))
    }

    /// The highest sequence that is no longer replayable.
    #[must_use]
    pub fn floor(&self) -> Seq {
        Seq(self.with_ring(Ring::floor).0)
    }

    /// How many frames have been published since boot.
    #[must_use]
    pub fn frames_published(&self) -> u64 {
        self.inner.frames.load(Ordering::Relaxed)
    }

    /// How many frames and bytes the replay ring is holding — the DESIGN §15.5 memory bound.
    #[must_use]
    pub fn ring_usage(&self) -> (usize, u64) {
        self.with_ring(|r| (r.len(), r.bytes()))
    }

    /// A new subscription to the bus. Subscribe **before** building a snapshot, so no frame can
    /// slip between the two (DESIGN §15.4 step 2).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<WireFrame>> {
        self.inner.tx.subscribe()
    }

    /// How many sockets are currently reading the bus.
    #[must_use]
    pub fn subscribers(&self) -> usize {
        self.inner.tx.receiver_count()
    }

    /// Publishes one structured frame: allocate, serialise once, record, broadcast.
    ///
    /// This is the aggregator's entry point, and the only one that preserves the structure a
    /// resume needs to fold. Returns the sequence the frame was issued under, which is what a
    /// mutating REST response echoes as `X-Aulos-Seq` (PROTOCOL §6.4).
    pub fn publish_frame(&self, body: FrameBody) -> Seq {
        let kind = body.kind();
        // The lock is held across allocate → serialise → record → broadcast on purpose: a frame's
        // `seq`, its position in the ring and its position on the bus are one fact, and two
        // concurrent publishers that allocated first and locked second could interleave them.
        let mut ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let seq = Seq(u64::try_from(self.inner.seq.next()).unwrap_or(0));
        let Some(text) = encode(kind, seq, &body) else {
            return seq;
        };
        let wire = Arc::new(WireFrame { seq, kind, text });
        ring.push(RingEntry {
            seq,
            wire: Arc::clone(&wire),
            body: Arc::new(body),
        });
        self.inner.head.store(seq.0, Ordering::Release);
        self.inner.frames.fetch_add(1, Ordering::Relaxed);
        let _ = self.inner.tx.send(wire);
        drop(ring);
        seq
    }

    /// Publishes an already-shaped payload under `kind` (DESIGN §15.3's signature).
    ///
    /// `body` must serialise to a JSON **object**; its keys are flattened next to `t` and `seq`.
    /// Use it for the passthrough kinds — `subscription`, `subscription_removed`, `ytdl_options`,
    /// `providers`, `notice`, `health`. The four item kinds go through [`Self::publish_frame`]
    /// instead, because a `Value` cannot be folded by the resume merge.
    pub fn publish(&self, kind: FrameKind, body: impl Serialize) -> Seq {
        if !kind.is_replayable() {
            tracing::error!(%kind, "refused to broadcast a per-connection frame kind");
            return self.head();
        }
        match serde_json::to_value(&body) {
            Ok(value @ Value::Object(_)) => self.publish_frame(FrameBody::Other { kind, value }),
            Ok(_) => {
                tracing::error!(%kind, "frame body did not serialise to an object");
                self.head()
            }
            Err(e) => {
                tracing::error!(%kind, error = %e, "frame body did not serialise");
                self.head()
            }
        }
    }

    /// Decides what a `?since=` cursor is worth, and folds the window when it is resumable
    /// (DESIGN §15.3).
    ///
    /// ```text
    /// boot mismatch  ⇒ Snapshot   (a different process, or a restored backup)
    /// since == head  ⇒ UpToDate
    /// since >  head  ⇒ Snapshot   (a restore rolled the server back; an empty delta list here
    ///                              would tell the client it was current when it had missed
    ///                              everything)
    /// since <  floor ⇒ Snapshot   (the gap is older than the replay window)
    /// otherwise      ⇒ Merged
    /// ```
    #[must_use]
    pub fn resume(&self, since: Seq, boot: Option<BootId>) -> Resume {
        if boot.is_some_and(|b| b != self.inner.boot_id) {
            return Resume::Snapshot;
        }
        let head = self.head();
        if since == head {
            return Resume::UpToDate;
        }
        if since > head {
            return Resume::Snapshot;
        }
        let fold = {
            let ring = self
                .inner
                .ring
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if since < ring.floor() {
                return Resume::Snapshot;
            }
            ring.merge_after(since)
        };
        Resume::Merged {
            from: fold.from,
            to: fold.to,
            merged: fold.counts,
            frames: encode_fold(&fold),
        }
    }

    fn with_ring<T>(&self, f: impl FnOnce(&Ring) -> T) -> T {
        let ring = self
            .inner
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f(&ring)
    }
}

/// The merged frames a `resume` is followed by, in the fixed order.
///
/// Every one of them carries `seq = fold.to`: the fold replays a window rather than issuing new
/// frames, and PROTOCOL §6.3 tells the client its cursor is `to` once it has applied them all.
/// Issuing fresh sequences instead would consume `seq` per resuming client and desynchronise `to`.
fn encode_fold(fold: &Fold) -> Vec<Arc<WireFrame>> {
    let seq = fold.to;
    let mut out: Vec<Arc<WireFrame>> = Vec::new();
    let mut push = |body: FrameBody| {
        let kind = body.kind();
        if let Some(text) = encode(kind, seq, &body) {
            out.push(Arc::new(WireFrame { seq, kind, text }));
        }
    };
    if let Some((reason, items)) = &fold.added {
        push(FrameBody::Added {
            reason: *reason,
            items: items.clone(),
        });
    }
    if !fold.completed.is_empty() {
        push(FrameBody::Completed {
            items: fold.completed.clone(),
        });
    }
    for (reason, ids) in &fold.removed {
        push(FrameBody::Removed {
            reason: *reason,
            ids: ids.clone(),
        });
    }
    if !fold.delta.is_empty() {
        push(FrameBody::Delta(Arc::new(crate::ring::DeltaBatch {
            ts: 0,
            items: fold.delta.clone(),
        })));
    }
    for (kind, value) in &fold.others {
        push(FrameBody::Other {
            kind: *kind,
            value: value.clone(),
        });
    }
    out
}

/// Serialises one frame to its complete JSON text: `t`, `seq`, then the payload's own keys.
fn encode(kind: FrameKind, seq: Seq, body: &FrameBody) -> Option<bytes::Bytes> {
    let json = match body {
        FrameBody::Added { reason, items } => serde_json::to_string(&AddedFrame {
            t: kind,
            seq,
            reason: *reason,
            items,
        }),
        FrameBody::Completed { items } => serde_json::to_string(&CompletedFrame {
            t: kind,
            seq,
            items,
        }),
        FrameBody::Removed { reason, ids } => serde_json::to_string(&RemovedFrame {
            t: kind,
            seq,
            ids,
            reason: *reason,
        }),
        FrameBody::Delta(batch) => serde_json::to_string(&DeltaFrame {
            t: kind,
            seq,
            ts: batch.ts,
            items: &batch.items,
        }),
        FrameBody::Other { value, .. } => serde_json::to_string(&OtherFrame {
            t: kind,
            seq,
            body: value,
        }),
    };
    match json {
        Ok(text) => Some(bytes::Bytes::from(text)),
        Err(e) => {
            tracing::error!(%kind, %seq, error = %e, "a frame could not be serialised");
            None
        }
    }
}

#[derive(Serialize)]
struct AddedFrame<'a> {
    t: FrameKind,
    seq: Seq,
    reason: AddReason,
    items: &'a [Arc<ItemView>],
}

#[derive(Serialize)]
struct CompletedFrame<'a> {
    t: FrameKind,
    seq: Seq,
    items: &'a [Arc<ItemView>],
}

#[derive(Serialize)]
struct RemovedFrame<'a> {
    t: FrameKind,
    seq: Seq,
    ids: &'a [ItemId],
    reason: RemoveReason,
}

#[derive(Serialize)]
struct DeltaFrame<'a> {
    t: FrameKind,
    seq: Seq,
    ts: i64,
    items: &'a [DeltaItem],
}

#[derive(Serialize)]
struct OtherFrame<'a> {
    t: FrameKind,
    seq: Seq,
    #[serde(flatten)]
    body: &'a Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::tests_support::{config, counter, view};
    use aulos_core::Status;

    fn hub() -> EventHub {
        EventHub::new(counter(10), BootId::new(), &config(&[]))
    }

    #[test]
    fn a_published_frame_carries_t_then_seq_then_its_payload() {
        let hub = hub();
        let v = view(Status::Queued, 1);
        let seq = hub.publish_frame(FrameBody::Added {
            reason: AddReason::Created,
            items: vec![Arc::clone(&v)],
        });
        assert_eq!(seq, Seq(11), "the allocator's next value");
        assert_eq!(hub.head(), Seq(11));
        assert_eq!(hub.frames_published(), 1);
        let mut rx = hub.subscribe();
        // The frame was published before the subscription, so the ring is where it lives.
        assert!(rx.try_recv().is_err());
        let seq2 = hub.publish_frame(FrameBody::Removed {
            reason: RemoveReason::Deleted,
            ids: vec![v.id],
        });
        let frame = rx.try_recv().unwrap();
        assert_eq!(frame.seq, seq2);
        assert_eq!(frame.kind, FrameKind::Removed);
        let json: Value = serde_json::from_str(frame.as_str()).unwrap();
        assert_eq!(json["t"], "removed");
        assert_eq!(json["seq"], 12);
        assert_eq!(json["reason"], "deleted");
        assert_eq!(json["ids"][0], v.id.to_string());
        assert!(
            frame.as_str().starts_with(r#"{"t":"removed","seq":12,"#),
            "{}",
            frame.as_str()
        );
        assert!(!frame.is_empty());
        assert_eq!(frame.len(), frame.as_str().len());
    }

    /// The whole point of the hub: one JSON encoding, N refcount sends.
    #[test]
    fn five_subscribers_receive_pointer_equal_arcs() {
        let hub = hub();
        let mut subs: Vec<_> = (0..5).map(|_| hub.subscribe()).collect();
        assert_eq!(hub.subscribers(), 5);
        hub.publish_frame(FrameBody::Completed {
            items: vec![view(Status::Finished, 1)],
        });
        let first = subs[0].try_recv().unwrap();
        for rx in &mut subs[1..] {
            let got = rx.try_recv().unwrap();
            assert!(Arc::ptr_eq(&first, &got), "the frame was serialised twice");
        }
    }

    #[test]
    fn a_passthrough_body_is_flattened_next_to_t_and_seq() {
        let hub = hub();
        let seq = hub.publish(
            FrameKind::Notice,
            serde_json::json!({ "level": "warning", "code": "stalled", "message": "no" }),
        );
        assert_eq!(seq, Seq(11));
        let fold = match hub.resume(Seq(10), Some(hub.boot_id())) {
            Resume::Merged { frames, .. } => frames,
            other => panic!("expected a merge, got {}", other.name()),
        };
        let json: Value = serde_json::from_str(fold[0].as_str()).unwrap();
        assert_eq!(json["t"], "notice");
        assert_eq!(json["code"], "stalled");
        assert_eq!(json["seq"], 11);
    }

    #[test]
    fn a_per_connection_kind_is_never_broadcast() {
        let hub = hub();
        let mut rx = hub.subscribe();
        for kind in [
            FrameKind::Snapshot,
            FrameKind::Resume,
            FrameKind::Pong,
            FrameKind::Error,
        ] {
            assert_eq!(hub.publish(kind, serde_json::json!({})), Seq(10));
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(hub.frames_published(), 0);
    }

    #[test]
    fn a_non_object_body_is_refused_rather_than_corrupting_a_frame() {
        let hub = hub();
        assert_eq!(hub.publish(FrameKind::Notice, 7), Seq(10));
        assert_eq!(hub.frames_published(), 0);
    }

    #[test]
    fn resume_answers_the_five_documented_cases() {
        let hub = hub();
        let boot = hub.boot_id();
        assert!(matches!(hub.resume(Seq(10), Some(boot)), Resume::UpToDate));
        assert!(
            matches!(hub.resume(Seq(10), Some(BootId::new())), Resume::Snapshot),
            "a different boot id"
        );
        assert!(
            matches!(hub.resume(Seq(99), Some(boot)), Resume::Snapshot),
            "a cursor above the head, i.e. a restored database"
        );
        for _ in 0..3 {
            hub.publish_frame(FrameBody::Completed {
                items: vec![view(Status::Finished, 1)],
            });
        }
        assert!(matches!(hub.resume(Seq(13), Some(boot)), Resume::UpToDate));
        match hub.resume(Seq(11), Some(boot)) {
            Resume::Merged { from, to, .. } => {
                assert_eq!((from, to), (Seq(11), Seq(13)));
            }
            other => panic!("expected a merge, got {}", other.name()),
        }
        assert!(
            matches!(hub.resume(Seq(10), None), Resume::Merged { .. }),
            "no boot id means the client is trusting us"
        );
    }

    #[test]
    fn a_cursor_below_the_floor_takes_a_snapshot() {
        let hub = EventHub::new(
            counter(0),
            BootId::new(),
            &config(&[("AULOS_WS_REPLAY_FRAMES", "2")]),
        );
        let boot = hub.boot_id();
        for _ in 0..4 {
            hub.publish_frame(FrameBody::Completed {
                items: vec![view(Status::Finished, 1)],
            });
        }
        assert_eq!(hub.floor(), Seq(2), "frames 1 and 2 were evicted");
        assert!(matches!(hub.resume(Seq(1), Some(boot)), Resume::Snapshot));
        assert!(matches!(
            hub.resume(Seq(2), Some(boot)),
            Resume::Merged { .. }
        ));
        let (frames, bytes) = hub.ring_usage();
        assert_eq!(frames, 2);
        assert!(bytes > 0);
    }

    /// The regression test for a shared ring being trimmed by one client's optimisation
    /// (DESIGN §15.3, PROTOCOL §5.11). There is no `ack` to call — the frame is CUT for v1.0 —
    /// and this test is what proves the retention rule is structural: reading the head does not
    /// move the floor, so a second, slower client's window is untouched.
    #[test]
    fn one_client_catching_up_never_shortens_another_clients_window() {
        let hub = hub();
        let boot = hub.boot_id();
        for _ in 0..5 {
            hub.publish_frame(FrameBody::Completed {
                items: vec![view(Status::Finished, 1)],
            });
        }
        let floor = hub.floor();
        // Client A resumes all the way to the head, repeatedly.
        for _ in 0..3 {
            assert!(matches!(
                hub.resume(Seq(11), Some(boot)),
                Resume::Merged { .. }
            ));
            assert!(matches!(
                hub.resume(hub.head(), Some(boot)),
                Resume::UpToDate
            ));
        }
        assert_eq!(hub.floor(), floor, "reading must not evict");
        // Client B is still far behind and must still be mergeable.
        match hub.resume(Seq(10), Some(boot)) {
            Resume::Merged { from, to, .. } => assert_eq!((from, to), (Seq(10), Seq(15))),
            other => panic!("expected a merge, got {}", other.name()),
        }
    }

    #[test]
    fn a_merged_window_emits_added_completed_removed_delta_in_that_order() {
        let hub = hub();
        let boot = hub.boot_id();
        let a = view(Status::Queued, 1);
        let b = view(Status::Finished, 2);
        let c = view(Status::Queued, 3);
        hub.publish_frame(FrameBody::Added {
            reason: AddReason::Created,
            items: vec![Arc::clone(&a)],
        });
        hub.publish_frame(FrameBody::Delta(Arc::new(crate::ring::DeltaBatch {
            ts: 7,
            items: vec![{
                let mut d = DeltaItem::new(a.id);
                d.fields.insert("percent", Value::from(12.5));
                d
            }],
        })));
        hub.publish_frame(FrameBody::Completed {
            items: vec![Arc::clone(&b)],
        });
        hub.publish_frame(FrameBody::Removed {
            reason: RemoveReason::Expired,
            ids: vec![c.id],
        });
        hub.publish_frame(FrameBody::Removed {
            reason: RemoveReason::Deleted,
            ids: vec![b.id],
        });
        let Resume::Merged {
            frames, merged, to, ..
        } = hub.resume(Seq(10), Some(boot))
        else {
            panic!("expected a merge");
        };
        let kinds: Vec<FrameKind> = frames.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            vec![
                FrameKind::Added,
                FrameKind::Removed,
                FrameKind::Removed,
                FrameKind::Delta,
            ],
            "b was added-then-removed so its `completed` is dropped; \
             the two removals keep the reason order"
        );
        let reasons: Vec<String> = frames
            .iter()
            .filter(|f| f.kind == FrameKind::Removed)
            .map(|f| {
                serde_json::from_str::<Value>(f.as_str()).unwrap()["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(reasons, ["deleted", "auto_cleared"]);
        assert!(frames.iter().all(|f| f.seq == to));
        assert_eq!(merged.removed, 2);
        assert_eq!(merged.delta_items, 1);
    }
}
