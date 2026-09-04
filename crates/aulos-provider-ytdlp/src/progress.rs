//! `progress` frame → [`RawProgress`]: the DESIGN §9.4 mapping.
//!
//! Three rules carry all the behaviour, and each one is a fix for something legacy got wrong:
//!
//! 1. **`tmpfilename` is not sticky** (Δ C18). Legacy did `self.tmpfilename = status.get(...)`
//!    on every frame, so any frame without the key *cleared* the partial-file path — which is
//!    why partial-file cleanup usually found nothing to delete. Here the stored value is only
//!    replaced when the frame actually carries one, and [`ProgressState::partials`] accumulates
//!    every distinct partial so cancellation can remove them all.
//! 2. **`source_tag` comes from `stream`, not from the filename.** Legacy hashed
//!    `filename or tmpfilename`, which meant the monotonic-percent floor reset on a rename as
//!    well as on a genuine new leg. The shim now names the leg (`video` / `audio` / `fragment`),
//!    so the reset is deterministic: the audio leg of a merge starts at 0 %, and only then.
//! 3. **`status == "finished"` is per stream, not per item.** It never terminates anything; only
//!    the `result` frame does. The frame is still forwarded, because it is the one that carries
//!    the leg's final byte count.
//!
//! The numeric coercion is [`aulos_core::progress::number`] — legacy's `_number()`, which accepts
//! a numeric string and a bool — so a `command`-style shim emitting `"250.5"` behaves exactly as
//! the WP-00 golden corpus says it must.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use aulos_core::progress::{PhaseTag, RawProgress, integer, number};

use crate::frames::ProgressFrame;

/// The `status` of one `progress` frame, typed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameStatus {
    /// Bytes are moving on this stream.
    Downloading,
    /// **This stream** is complete. Not the item (DESIGN §9.4).
    Finished,
    /// yt-dlp reported a per-stream problem. Becomes a notice; only `error` is terminal.
    Error,
    /// Anything else a future yt-dlp might send.
    Other,
}

impl FrameStatus {
    /// Parses the wire value.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("downloading") => Self::Downloading,
            Some("finished") => Self::Finished,
            Some("error") => Self::Error,
            _ => Self::Other,
        }
    }
}

/// The cosmetic [`PhaseTag`] a `stream` name maps to.
///
/// `unknown` deliberately yields `None` rather than a made-up tag: [`aulos_core::progress::
/// ProgressCell`] only overwrites `phase` when the frame carries one, so `None` means "leave the
/// label alone" instead of flickering it.
#[must_use]
pub fn phase_of(stream: Option<&str>) -> Option<PhaseTag> {
    match stream {
        Some("video") => Some(PhaseTag::Video),
        Some("audio") => Some(PhaseTag::Audio),
        Some("fragment") => Some(PhaseTag::Fragment),
        _ => None,
    }
}

/// The stable hash a leg identity turns into.
///
/// Any change resets the normaliser's monotonic floor, so it must be stable for the life of one
/// job and different for two different legs. It is never persisted or compared across processes.
#[must_use]
pub fn source_tag(stream: Option<&str>, fallback: Option<&str>) -> u64 {
    let key = stream.or(fallback).unwrap_or("");
    if key.is_empty() {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    // `0` means "no leg yet" in `ProgressCell`, so never hand it back for a real stream.
    hasher.finish() | 1
}

/// Per-job progress bookkeeping: the sticky filenames and the set of partials to clean up.
#[derive(Clone, Debug, Default)]
pub struct ProgressState {
    filename: Option<Box<str>>,
    tmpfilename: Option<Box<str>>,
    partials: Vec<Box<str>>,
    frames: u64,
    last_elapsed: Option<f64>,
}

impl ProgressState {
    /// A fresh state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The last `filename` any frame reported.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// The current partial-file path — the *last* one reported, not the last frame's value.
    #[must_use]
    pub fn tmpfilename(&self) -> Option<&str> {
        self.tmpfilename.as_deref()
    }

    /// Every distinct partial file this job has been seen writing.
    ///
    /// A merge produces one per leg, so cancelling a half-merged download has more than one
    /// `.part` to remove — the case legacy's single sticky field could not represent.
    #[must_use]
    pub fn partials(&self) -> &[Box<str>] {
        &self.partials
    }

    /// How many `progress` frames have been applied.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    /// The largest `elapsed` any frame reported, which is the shim's own clock.
    #[must_use]
    pub const fn last_elapsed(&self) -> Option<f64> {
        self.last_elapsed
    }

    /// Maps one frame, updating the sticky state.
    pub fn apply(&mut self, frame: &ProgressFrame) -> RawProgress {
        self.frames += 1;

        if let Some(name) = frame.filename.as_deref().filter(|s| !s.is_empty()) {
            self.filename = Some(name.into());
        }
        // Rule 1: only a frame that carries the key may replace the stored partial.
        if let Some(tmp) = frame.tmpfilename.as_deref().filter(|s| !s.is_empty())
            && self.tmpfilename.as_deref() != Some(tmp)
        {
            if !self.partials.iter().any(|p| &**p == tmp) {
                self.partials.push(tmp.into());
            }
            self.tmpfilename = Some(tmp.into());
        }
        if let Some(elapsed) = frame.elapsed {
            self.last_elapsed = Some(elapsed);
        }

        let status = FrameStatus::parse(frame.status.as_deref());
        let downloaded = frame.downloaded_bytes.as_ref().and_then(number);
        let mut total = frame.total_bytes.as_ref().and_then(number);

        // Rule 3: the per-stream "finished" hint. The shim already equalises the byte pair when
        // yt-dlp gives it one; this covers the frames where yt-dlp reports only the counter, so
        // the leg reads as complete instead of stalling at its last estimate.
        if status == FrameStatus::Finished && total.is_none() {
            total = downloaded;
        }

        RawProgress {
            downloaded_bytes: downloaded,
            total_bytes: total,
            total_bytes_estimate: frame.total_bytes_estimate.as_ref().and_then(number),
            fragment_index: frame.fragment_index.as_ref().and_then(integer),
            fragment_count: frame.fragment_count.as_ref().and_then(integer),
            speed: frame.speed.as_ref().and_then(number),
            eta: frame.eta.as_ref().and_then(integer),
            phase: phase_of(frame.stream.as_deref()),
            phase_percent: None,
            // Rule 2: the leg identity, with the legacy filename hash as the fallback for a shim
            // old enough not to send `stream`.
            source_tag: source_tag(
                frame.stream.as_deref(),
                frame
                    .filename
                    .as_deref()
                    .or(frame.tmpfilename.as_deref())
                    .or(self.tmpfilename.as_deref()),
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::progress::Normalizer;
    use aulos_core::status::Status;
    use serde_json::json;

    use super::*;

    fn frame(json: serde_json::Value) -> ProgressFrame {
        serde_json::from_value(json).expect("frame")
    }

    #[test]
    fn the_status_vocabulary_is_closed() {
        assert_eq!(
            FrameStatus::parse(Some("downloading")),
            FrameStatus::Downloading
        );
        assert_eq!(FrameStatus::parse(Some("finished")), FrameStatus::Finished);
        assert_eq!(FrameStatus::parse(Some("error")), FrameStatus::Error);
        assert_eq!(FrameStatus::parse(Some("paused")), FrameStatus::Other);
        assert_eq!(FrameStatus::parse(None), FrameStatus::Other);
    }

    #[test]
    fn streams_map_onto_phase_tags() {
        assert_eq!(phase_of(Some("video")), Some(PhaseTag::Video));
        assert_eq!(phase_of(Some("audio")), Some(PhaseTag::Audio));
        assert_eq!(phase_of(Some("fragment")), Some(PhaseTag::Fragment));
        assert_eq!(phase_of(Some("unknown")), None);
        assert_eq!(phase_of(None), None);
    }

    #[test]
    fn the_source_tag_changes_per_leg_and_is_never_zero_for_a_real_stream() {
        let video = source_tag(Some("video"), None);
        let audio = source_tag(Some("audio"), None);
        assert_ne!(video, audio);
        assert_ne!(video, 0);
        assert_eq!(video, source_tag(Some("video"), Some("/other/name.part")));
        assert_eq!(source_tag(None, None), 0);
        // The pre-`stream` fallback is the legacy filename hash.
        assert_ne!(
            source_tag(None, Some("a.part")),
            source_tag(None, Some("b.part"))
        );
    }

    #[test]
    fn tmpfilename_is_not_sticky_and_every_partial_is_remembered() {
        let mut state = ProgressState::new();
        state.apply(&frame(json!({
            "status": "downloading",
            "tmpfilename": "/d/Rick.f616.mp4.part",
            "downloaded_bytes": 1,
            "stream": "video"
        })));
        // A frame with no `tmpfilename` must NOT clear it. This is Δ C18.
        state.apply(&frame(
            json!({ "status": "downloading", "downloaded_bytes": 2 }),
        ));
        assert_eq!(state.tmpfilename(), Some("/d/Rick.f616.mp4.part"));

        state.apply(&frame(json!({
            "status": "downloading",
            "tmpfilename": "/d/Rick.f140.m4a.part",
            "stream": "audio"
        })));
        assert_eq!(state.tmpfilename(), Some("/d/Rick.f140.m4a.part"));
        assert_eq!(
            state.partials(),
            [
                Box::from("/d/Rick.f616.mp4.part"),
                Box::from("/d/Rick.f140.m4a.part")
            ]
        );
        assert_eq!(state.frames(), 3);
    }

    #[test]
    fn a_repeated_partial_is_recorded_once() {
        let mut state = ProgressState::new();
        for _ in 0..5 {
            state.apply(&frame(
                json!({ "status": "downloading", "tmpfilename": "/d/a.part" }),
            ));
        }
        assert_eq!(state.partials().len(), 1);
    }

    #[test]
    fn the_design_transcript_frames_map_as_documented() {
        let mut state = ProgressState::new();
        let first = state.apply(&frame(json!({
            "status": "downloading",
            "filename": null,
            "tmpfilename": "/downloads/Rick.f616.mp4.part",
            "downloaded_bytes": 262_144,
            "total_bytes": null,
            "total_bytes_estimate": 58_720_256,
            "speed": 1_310_720.0,
            "eta": 44,
            "elapsed": 0.3,
            "stream": "video"
        })));
        assert_eq!(first.downloaded_bytes, Some(262_144.0));
        assert_eq!(first.total_bytes, None);
        assert_eq!(first.total_bytes_estimate, Some(58_720_256.0));
        assert_eq!(first.eta, Some(44));
        assert_eq!(first.phase, Some(PhaseTag::Video));
        assert_eq!(state.last_elapsed(), Some(0.3));

        let finished = state.apply(&frame(json!({
            "status": "finished",
            "filename": "/downloads/Rick.f616.mp4",
            "downloaded_bytes": 58_720_256,
            "total_bytes": 58_720_256,
            "elapsed": 5.4,
            "stream": "video"
        })));
        assert_eq!(state.filename(), Some("/downloads/Rick.f616.mp4"));
        assert_eq!(finished.total_bytes, Some(58_720_256.0));
    }

    #[test]
    fn a_finished_frame_without_a_total_reads_as_a_complete_leg() {
        let mut state = ProgressState::new();
        let raw = state.apply(&frame(json!({
            "status": "finished",
            "downloaded_bytes": 4096,
            "stream": "audio"
        })));
        assert_eq!(raw.total_bytes, Some(4096.0));
        // Which the normaliser reads as a full leg, clamped while the item is still active.
        let mut norm = Normalizer::new();
        let percent = norm.apply(&raw, Status::Downloading);
        assert!((percent - 99.9).abs() < 1e-9, "got {percent}");
    }

    #[test]
    fn the_merge_leg_change_resets_the_monotonic_floor() {
        let mut state = ProgressState::new();
        let mut norm = Normalizer::new();
        let video = state.apply(&frame(json!({
            "status": "downloading",
            "tmpfilename": "/d/v.part",
            "downloaded_bytes": 90,
            "total_bytes": 100,
            "stream": "video"
        })));
        assert!((norm.apply(&video, Status::Downloading) - 90.0).abs() < 1e-9);
        let audio = state.apply(&frame(json!({
            "status": "downloading",
            "tmpfilename": "/d/a.part",
            "downloaded_bytes": 1,
            "total_bytes": 100,
            "stream": "audio"
        })));
        // Without the reset the floor would pin this at 90 %.
        assert!((norm.apply(&audio, Status::Downloading) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_stringly_typed_frame_is_coerced_the_legacy_way() {
        let mut state = ProgressState::new();
        let raw = state.apply(&frame(json!({
            "status": "downloading",
            "downloaded_bytes": "250.5",
            "total_bytes": "1000.0",
            "fragment_index": "3",
            "fragment_count": 10,
            "eta": "12",
            "speed": "n/a"
        })));
        assert_eq!(raw.downloaded_bytes, Some(250.5));
        assert_eq!(raw.total_bytes, Some(1000.0));
        assert_eq!(raw.fragment_index, Some(3));
        assert_eq!(raw.fragment_count, Some(10));
        assert_eq!(raw.eta, Some(12));
        assert_eq!(raw.speed, None, "an uncoercible value is absent, not zero");
        let mut norm = Normalizer::new();
        assert!((norm.apply(&raw, Status::Downloading) - 25.05).abs() < 1e-9);
    }
}
