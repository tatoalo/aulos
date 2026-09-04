//! The one channel a running job talks to the rest of the server through (DESIGN §6.2, §2.3).
//!
//! Two properties matter and they pull in opposite directions, so the sink has two kinds of
//! method:
//!
//! - **Progress is lossy.** A downloader may report ten frames a second per stream and must never
//!   block on a slow aggregator, so [`ProgressSink::progress`] is a non-blocking `try_send` that
//!   drops on a full channel and counts the drop. Losing a percent tick is invisible; stalling a
//!   download to deliver one is not.
//! - **Everything else is lossless.** [`ProgressSink::stage`] and [`ProgressSink::file`] are
//!   awaited and never dropped: they carry state transitions and produced artifacts, and the
//!   engine persists them.
//!
//! [`ProgressSink::log`] does not use the channel at all — it is a structured passthrough to
//! `tracing` and never reaches a client.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aulos_core::event::Level;
use aulos_core::id::ItemId;
use aulos_core::item::{FileRef, FileSlot};
use aulos_core::progress::RawProgress;
use aulos_core::status::Status;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// The channel capacity of DESIGN §2.3: 8192 `ProgressMsg`s.
pub const PROGRESS_CHANNEL_CAPACITY: usize = 8192;

/// A finer-grained state transition a provider reports (DESIGN §6.2).
///
/// These are the three *running* statuses. A provider never reports a terminal status — the
/// engine writes those from the download's return value, which is what makes "a hook can never
/// change an item's status" enforceable (DESIGN §13).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Slot acquired, process spawned, nothing downloaded yet.
    Preparing,
    /// Bytes are moving.
    Downloading,
    /// Merging, remuxing, converting subtitles, embedding thumbnails.
    Postprocessing,
}

impl Stage {
    /// The [`Status`] this stage writes onto the item.
    #[must_use]
    pub const fn status(self) -> Status {
        match self {
            Self::Preparing => Status::Preparing,
            Self::Downloading => Status::Downloading,
            Self::Postprocessing => Status::Postprocessing,
        }
    }

    /// The wire string, which is also the status name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.status().as_str()
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One message on the single progress channel (DESIGN §2.3).
///
/// The aggregator (WP-13) is the only consumer: it applies `Progress` through the item's
/// `Normalizer` and batches it at `AULOS_WS_BATCH_MS`, and forwards `Stage` and `File` to the
/// engine while pulling the next flush forward to `AULOS_WS_URGENT_MS`.
#[derive(Clone, PartialEq, Debug)]
pub enum ProgressMsg {
    /// A raw progress frame. **Droppable** — see the module docs.
    Progress {
        /// The item it belongs to.
        id: ItemId,
        /// The frame, before normalisation.
        raw: RawProgress,
    },
    /// A state transition, optionally with the human message that goes with it.
    Stage {
        /// The item it belongs to.
        id: ItemId,
        /// The new stage.
        stage: Stage,
        /// Text for `ItemView.msg`. `None` leaves the previous message alone.
        msg: Option<Box<str>>,
    },
    /// A produced auxiliary file.
    File {
        /// The item it belongs to.
        id: ItemId,
        /// Which list it belongs in.
        slot: FileSlot,
        /// The file.
        file: FileRef,
    },
}

impl ProgressMsg {
    /// The item this message is about.
    #[must_use]
    pub const fn item_id(&self) -> ItemId {
        match *self {
            Self::Progress { id, .. } | Self::Stage { id, .. } | Self::File { id, .. } => id,
        }
    }

    /// Whether this message may be dropped when the channel is full (DESIGN §2.3).
    #[must_use]
    pub const fn is_droppable(&self) -> bool {
        matches!(self, Self::Progress { .. })
    }
}

/// Builds a per-item [`ProgressSink`] over the one `ProgressMsg` channel (DESIGN §13.3).
///
/// The hooks dispatcher is handed one of these at spawn so a hook's `phase_percent` flows through
/// the ordinary aggregator path rather than a second, differently-behaved channel.
#[derive(Clone, Debug)]
pub struct ProgressSinkFactory {
    tx: mpsc::Sender<ProgressMsg>,
    dropped: Arc<AtomicU64>,
}

impl ProgressSinkFactory {
    /// Wraps the sending half of the progress channel.
    #[must_use]
    pub fn new(tx: mpsc::Sender<ProgressMsg>) -> Self {
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Creates the channel at the DESIGN §2.3 capacity and returns the factory and the receiver.
    #[must_use]
    pub fn channel() -> (Self, mpsc::Receiver<ProgressMsg>) {
        let (tx, rx) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        (Self::new(tx), rx)
    }

    /// A sink bound to one item.
    #[must_use]
    pub fn for_item(&self, id: ItemId) -> ProgressSink {
        ProgressSink {
            tx: self.tx.clone(),
            id,
            dropped: Arc::clone(&self.dropped),
        }
    }

    /// How many progress frames have been dropped for a full channel.
    ///
    /// This is what `aulos_progress_dropped_total` would have counted; the Prometheus endpoint is
    /// CUT for v1.0 (see BRIEF), so the counter is exposed here for `healthz` and for tests.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The per-item handle a provider reports through (DESIGN §6.2).
#[derive(Clone, Debug)]
pub struct ProgressSink {
    tx: mpsc::Sender<ProgressMsg>,
    id: ItemId,
    dropped: Arc<AtomicU64>,
}

impl ProgressSink {
    /// The item this sink reports for.
    #[must_use]
    pub const fn item_id(&self) -> ItemId {
        self.id
    }

    /// Reports one progress frame. **Lossy, non-blocking, latest-wins.** Safe to call 1000×/s.
    ///
    /// A full channel drops the frame and bumps [`ProgressSinkFactory::dropped`]; a closed channel
    /// (the aggregator is gone, i.e. shutdown) is silently ignored, because a provider mid-kill has
    /// nothing useful to do about it.
    pub fn progress(&self, p: RawProgress) {
        if let Err(e) = self.tx.try_send(ProgressMsg::Progress {
            id: self.id,
            raw: p,
        }) {
            match e {
                mpsc::error::TrySendError::Full(_) => {
                    let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        tracing::debug!(item = %self.id, dropped = n, "progress channel full");
                    }
                }
                mpsc::error::TrySendError::Closed(_) => {
                    tracing::trace!(item = %self.id, "progress channel closed");
                }
            }
        }
    }

    /// Reports a state transition. **Lossless** — awaited, never dropped.
    ///
    /// A closed channel is not an error for the caller: it means the server is shutting down and
    /// the job is about to be killed.
    pub async fn stage(&self, s: Stage, msg: Option<Box<str>>) {
        self.send(ProgressMsg::Stage {
            id: self.id,
            stage: s,
            msg,
        })
        .await;
    }

    /// Reports a produced auxiliary file. **Lossless** — awaited, never dropped.
    pub async fn file(&self, slot: FileSlot, f: FileRef) {
        self.send(ProgressMsg::File {
            id: self.id,
            slot,
            file: f,
        })
        .await;
    }

    /// Logs, span-attached to the item. Never reaches a client.
    pub fn log(&self, level: Level, msg: &str) {
        match level {
            Level::Info => tracing::info!(item = %self.id, "{msg}"),
            Level::Warn => tracing::warn!(item = %self.id, "{msg}"),
            Level::Error => tracing::error!(item = %self.id, "{msg}"),
        }
    }

    async fn send(&self, m: ProgressMsg) {
        if self.tx.send(m).await.is_err() {
            tracing::trace!(item = %self.id, "progress channel closed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn a_frame() -> RawProgress {
        RawProgress {
            downloaded_bytes: Some(10.0),
            total_bytes: Some(100.0),
            ..RawProgress::default()
        }
    }

    #[test]
    fn stage_maps_onto_the_running_statuses() {
        assert_eq!(Stage::Preparing.status(), Status::Preparing);
        assert_eq!(Stage::Downloading.status(), Status::Downloading);
        assert_eq!(Stage::Postprocessing.status(), Status::Postprocessing);
        for s in [Stage::Preparing, Stage::Downloading, Stage::Postprocessing] {
            assert!(s.status().is_running(), "{s} must hold a slot");
            assert_eq!(s.to_string(), s.status().as_str());
        }
    }

    #[tokio::test]
    async fn the_factory_binds_one_channel_to_many_items() {
        let (factory, mut rx) = ProgressSinkFactory::channel();
        let a = ItemId::new();
        let b = ItemId::new();
        factory.for_item(a).progress(a_frame());
        factory.for_item(b).stage(Stage::Downloading, None).await;
        let first = rx.recv().await.unwrap();
        assert_eq!(first.item_id(), a);
        assert!(first.is_droppable());
        let second = rx.recv().await.unwrap();
        assert_eq!(second.item_id(), b);
        assert!(!second.is_droppable());
        assert_eq!(factory.dropped(), 0);
    }

    #[tokio::test]
    async fn progress_is_dropped_on_a_full_channel_and_counted() {
        let (tx, _rx) = mpsc::channel(2);
        let factory = ProgressSinkFactory::new(tx);
        let sink = factory.for_item(ItemId::new());
        for _ in 0..10 {
            sink.progress(a_frame());
        }
        assert_eq!(factory.dropped(), 8, "2 fit, 8 were dropped");
    }

    #[tokio::test]
    async fn a_closed_channel_is_not_a_panic() {
        let (factory, rx) = ProgressSinkFactory::channel();
        let sink = factory.for_item(ItemId::new());
        drop(rx);
        sink.progress(a_frame());
        sink.stage(Stage::Postprocessing, Some("merging".into()))
            .await;
        sink.file(
            FileSlot::Subtitle,
            FileRef {
                filename: "a.srt".into(),
                size: None,
                download_url: None,
                lang: None,
            },
        )
        .await;
        sink.log(Level::Warn, "still alive");
        assert_eq!(factory.dropped(), 0, "a closed channel is not a drop");
    }

    #[tokio::test]
    async fn stage_and_file_carry_their_payload() {
        let (factory, mut rx) = ProgressSinkFactory::channel();
        let id = ItemId::new();
        let sink = factory.for_item(id);
        assert_eq!(sink.item_id(), id);
        sink.stage(Stage::Preparing, Some("Starting".into())).await;
        sink.file(
            FileSlot::Chapter,
            FileRef {
                filename: "Clip - 01.mp4".into(),
                size: Some(7),
                download_url: None,
                lang: None,
            },
        )
        .await;
        match rx.recv().await.unwrap() {
            ProgressMsg::Stage { stage, msg, .. } => {
                assert_eq!(stage, Stage::Preparing);
                assert_eq!(msg.as_deref(), Some("Starting"));
            }
            other => panic!("expected a stage, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            ProgressMsg::File { slot, file, .. } => {
                assert_eq!(slot, FileSlot::Chapter);
                assert_eq!(file.size, Some(7));
            }
            other => panic!("expected a file, got {other:?}"),
        }
    }
}
