//! The transient progress cell and the percent normaliser (DESIGN §4.7).
//!
//! None of this is ever persisted: progress lives in the aggregator's memory, is coalesced at a
//! fixed cadence and is diffed against the last frame sent (DESIGN thesis T3).

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::status::Status;

/// A finer-grained, purely cosmetic label for what the job is doing right now.
///
/// PROTOCOL §2.3 tells clients not to switch on it. It is a closed `Copy` enum because
/// [`ProgressCell`] is `Copy`, and because the `command` plugin progress grammar (DESIGN §6.5) has
/// no `phase` capture group — only the built-in providers can set one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PhaseTag {
    /// The video leg of a separate-streams download.
    Video,
    /// The audio leg of a separate-streams download.
    Audio,
    /// HLS/DASH fragment or segment fetching.
    Fragment,
    /// ffmpeg remux / merge.
    Remux,
    /// The `best_remux` audio re-encode hook (DESIGN §13.3).
    AudioSync,
    /// Muxing separately downloaded tracks (`N_m3u8DL-RE`, the gapless fallback).
    Mux,
    /// Subtitle download or conversion.
    Subtitle,
    /// Thumbnail download or embedding.
    Thumbnail,
}

impl PhaseTag {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Fragment => "fragment",
            Self::Remux => "remux",
            Self::AudioSync => "audio_sync",
            Self::Mux => "mux",
            Self::Subtitle => "subtitle",
            Self::Thumbnail => "thumbnail",
        }
    }
}

impl std::fmt::Display for PhaseTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One progress frame as a provider reports it, before normalisation.
///
/// The field names are yt-dlp's `progress_hooks` keys (DESIGN §9.4); the StreamingCommunity and
/// `command` providers fill the same struct from their own parsers. `source_tag` is a hash of the
/// stream / filename / tmpfilename the frame belongs to: a change means a new leg of the job
/// started (video → audio of a merge) and resets the monotonic floor.
///
/// The byte counts are `f64`, not `u64`, because that is what a provider actually reports: yt-dlp
/// sends `total_bytes_estimate` as a float, and legacy's `_number()` coerced a numeric **string**
/// too — the golden corpus pins `"250.5" / "1000.0" ⇒ 25.05`, which integer inputs cannot
/// reproduce. Use [`number`] to build these from untyped JSON. The *wire* stays integral:
/// [`ProgressCell`] and `ItemView` hold `Option<u64>`, and [`to_wire_bytes`] does the conversion.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RawProgress {
    /// Bytes fetched so far.
    pub downloaded_bytes: Option<f64>,
    /// The exact total, when the provider knows it.
    pub total_bytes: Option<f64>,
    /// An estimate. Only trusted when `total_bytes` is `None`.
    pub total_bytes_estimate: Option<f64>,
    /// 0-based fragment index. Signed because a provider can report a negative one.
    pub fragment_index: Option<i64>,
    /// Total fragment count.
    pub fragment_count: Option<i64>,
    /// Bytes per second.
    pub speed: Option<f64>,
    /// Whole seconds remaining.
    pub eta: Option<i64>,
    /// The cosmetic phase label.
    pub phase: Option<PhaseTag>,
    /// Progress of the current postprocessing phase, independent of `percent`.
    pub phase_percent: Option<f64>,
    /// Identity of the stream this frame belongs to. A change resets the monotonic clamp.
    pub source_tag: u64,
}

/// Legacy `_number()` (`app/ytdl.py`), verbatim: `None` for a missing, null or uncoercible value,
/// otherwise `float(value)`.
///
/// The two non-obvious cases are both pinned by the golden corpus and both follow from Python's
/// `float()`: a **string** that parses is accepted (`"250.5" ⇒ 250.5`), because yt-dlp and the
/// community plugins emit them; and a **bool** coerces (`float(True) == 1.0`). `"n/a"`, an object
/// and an array are `None`, which the normaliser treats as "nothing usable, keep the previous
/// value".
#[must_use]
pub fn number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// [`number`], then truncated toward zero for a field the wire types as an integer.
#[must_use]
#[allow(clippy::cast_possible_truncation)] // fragment indices are small by construction
pub fn integer(value: &serde_json::Value) -> Option<i64> {
    number(value).map(|v| v as i64)
}

/// Converts a provider-reported byte count to the unsigned wire type: a negative value clamps to
/// zero and a fractional one rounds to nearest.
///
/// Clamping — rather than dropping — is what reproduces the legacy result: legacy computed
/// `-50 / 1000 * 100` and then clamped the *percent* to `[0, 99.9]`, which is `0.0`. This function
/// is only for the **wire**; [`Normalizer::apply`] does its arithmetic on the raw floats, so the
/// rounding here can never move a percent.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped non-negative first
pub fn to_wire_bytes(value: Option<f64>) -> Option<u64> {
    value.map(|v| {
        if v.is_finite() {
            v.max(0.0).round() as u64
        } else {
            0
        }
    })
}

/// [`to_wire_bytes`] for a fragment index or count: a negative becomes `0`, which is what legacy's
/// `min(max(index, 0), count)` produced.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped into range first
pub fn to_wire_count(value: Option<i64>) -> Option<u32> {
    value.map(|v| v.clamp(0, i64::from(u32::MAX)) as u32)
}

/// The aggregator's per-item progress state. Memory only, never persisted (DESIGN §4.7).
#[derive(Clone, Copy, Debug)]
pub struct ProgressCell {
    /// Normalised percent, `0.0..=100.0`. Never `None`: `0.0` before any progress.
    pub percent: f64,
    /// Bytes per second.
    pub speed: Option<f64>,
    /// Whole seconds remaining.
    pub eta: Option<i64>,
    /// Bytes fetched so far.
    pub downloaded_bytes: Option<u64>,
    /// The exact total.
    pub total_bytes: Option<u64>,
    /// The estimated total.
    pub total_bytes_estimate: Option<u64>,
    /// 0-based fragment index.
    pub fragment_index: Option<u32>,
    /// Total fragment count.
    pub fragment_count: Option<u32>,
    /// The cosmetic phase label.
    pub phase: Option<PhaseTag>,
    /// Postprocessor progress.
    pub phase_percent: Option<f64>,
    /// Hash of stream / filename / tmpfilename; a change resets the clamp.
    pub source_tag: u64,
    /// Set on **every** frame received, even one the channel dropped.
    ///
    /// The receiver bumps this *before* the drop decision, which is what makes a sustained drop
    /// storm distinguishable from a genuinely stalled download.
    pub last_frame_at: Instant,
    /// Set only on frames that were actually applied.
    pub last_applied_at: Instant,
}

impl ProgressCell {
    /// A zeroed cell whose two instants are `at`.
    #[must_use]
    pub fn new(at: Instant) -> Self {
        Self {
            percent: 0.0,
            speed: None,
            eta: None,
            downloaded_bytes: None,
            total_bytes: None,
            total_bytes_estimate: None,
            fragment_index: None,
            fragment_count: None,
            phase: None,
            phase_percent: None,
            source_tag: 0,
            last_frame_at: at,
            last_applied_at: at,
        }
    }

    /// Copies every reported field out of `m` and records `percent`.
    ///
    /// The caller owns the [`Normalizer`] (one per item) and passes the value it returned, so the
    /// monotonic floor survives across frames.
    pub fn apply(&mut self, m: &RawProgress, percent: f64, at: Instant) {
        self.percent = percent;
        self.speed = m.speed;
        self.eta = m.eta;
        self.downloaded_bytes = to_wire_bytes(m.downloaded_bytes);
        self.total_bytes = to_wire_bytes(m.total_bytes);
        self.total_bytes_estimate = to_wire_bytes(m.total_bytes_estimate);
        self.fragment_index = to_wire_count(m.fragment_index);
        self.fragment_count = to_wire_count(m.fragment_count);
        if m.phase.is_some() {
            self.phase = m.phase;
        }
        if m.phase_percent.is_some() {
            self.phase_percent = m.phase_percent;
        }
        self.source_tag = m.source_tag;
        self.last_frame_at = at;
        self.last_applied_at = at;
    }
}

impl Default for ProgressCell {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

/// The upper clamp while a download is active. DESIGN Appendix B K1 keeps the legacy quirk.
pub const ACTIVE_CEILING: f64 = 99.9;

/// Turns a provider's raw frame into the `percent` a client sees.
///
/// A line-by-line port of legacy `_calculate_progress_percent` (`app/ytdl.py`), with the one
/// signature change that it returns `f64` rather than `Option<f64>`: "nothing usable" yields the
/// previous value, or `0.0` when there is no previous value, because `ItemView.percent` is
/// documented as never null.
///
/// | Case | Behaviour |
/// |---|---|
/// | `status == Finished` | `100.0` |
/// | `total_bytes` exact and `> 0` | `downloaded / total * 100` |
/// | fragments known | `floor = idx/count*100`, `ceil = min((idx+1)/count*100, 99.9)`, result = `estimate.clamp(floor, ceil)`; with no estimate, `floor` |
/// | no fragments and `total_bytes_estimate <= downloaded_bytes` | the estimate is ignored (the bogus 1 KiB/1 KiB HLS frame) |
/// | nothing usable | keep the previous value |
/// | `source_tag` changed | reset the monotonic floor |
/// | always | clamp to `[0.0, 99.9]` while active, never decrease below the previous value |
#[derive(Clone, Copy, Debug, Default)]
pub struct Normalizer {
    prev: Option<f64>,
    source_tag: u64,
}

impl Normalizer {
    /// A normaliser with no history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prev: None,
            source_tag: 0,
        }
    }

    /// A normaliser seeded with a previous value — how the golden replay threads its vectors, and
    /// how boot recovery restores the floor of a resumed item.
    #[must_use]
    pub const fn with_previous(prev: Option<f64>) -> Self {
        Self {
            prev,
            source_tag: 0,
        }
    }

    /// The monotonic floor currently in force, if any.
    #[must_use]
    pub const fn previous(&self) -> Option<f64> {
        self.prev
    }

    /// Drops the monotonic floor. Called when the job starts a new leg (a new `source_tag`), and
    /// available explicitly for a retry, which restarts from zero.
    pub const fn reset(&mut self) {
        self.prev = None;
    }

    /// Normalises one frame.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // fragment counts stay far inside f64's exact range
    pub fn apply(&mut self, m: &RawProgress, status: Status) -> f64 {
        if m.source_tag != self.source_tag {
            self.source_tag = m.source_tag;
            self.reset();
        }

        if status == Status::Finished {
            self.prev = Some(100.0);
            return 100.0;
        }

        let downloaded = m.downloaded_bytes;
        let exact_total = m.total_bytes;
        let estimate_total = m.total_bytes_estimate;

        let mut percent: Option<f64> = None;

        // `downloaded is not None and exact_total and exact_total > 0`
        match (downloaded, exact_total) {
            (Some(d), Some(t)) if t > 0.0 => percent = Some(d / t * 100.0),
            _ => {
                let estimate_percent = match (downloaded, estimate_total) {
                    (Some(d), Some(t)) if t > 0.0 => Some(d / t * 100.0),
                    _ => None,
                };

                match (m.fragment_count, m.fragment_index) {
                    (Some(count), Some(index)) if count > 0 => {
                        let bounded = index.clamp(0, count) as f64;
                        let count = count as f64;
                        let floor = bounded / count * 100.0;
                        let ceiling = ((bounded + 1.0) / count * 100.0).min(ACTIVE_CEILING);
                        percent = Some(match estimate_percent {
                            Some(e) => e.max(floor).min(ceiling),
                            None => floor,
                        });
                    }
                    _ => {
                        if let (Some(e), Some(d), Some(t)) =
                            (estimate_percent, downloaded, estimate_total)
                        {
                            // Ignore the common early HLS estimate that reports 1 KiB / 1 KiB.
                            if t > d {
                                percent = Some(e);
                            }
                        }
                    }
                }
            }
        }

        let Some(mut percent) = percent else {
            // "keep previous": the floor is untouched, so a later usable frame is not clamped to a
            // value this frame never produced.
            return self.prev.unwrap_or(0.0);
        };

        percent = percent.clamp(0.0, ACTIVE_CEILING);
        if let Some(prev) = self.prev
            && percent < prev
        {
            return prev;
        }
        self.prev = Some(percent);
        percent
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn frame(downloaded: Option<f64>, total: Option<f64>, estimate: Option<f64>) -> RawProgress {
        RawProgress {
            downloaded_bytes: downloaded,
            total_bytes: total,
            total_bytes_estimate: estimate,
            ..RawProgress::default()
        }
    }

    #[test]
    fn finished_is_exactly_one_hundred() {
        let mut n = Normalizer::new();
        assert!((n.apply(&RawProgress::default(), Status::Finished) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn exact_total_bytes_wins_and_is_clamped() {
        let mut n = Normalizer::new();
        assert!(
            (n.apply(&frame(Some(50.0), Some(200.0), None), Status::Downloading) - 25.0).abs()
                < 1e-9
        );
        let mut n = Normalizer::new();
        assert!(
            (n.apply(&frame(Some(100.0), Some(100.0), None), Status::Downloading) - ACTIVE_CEILING)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn fragments_bound_the_estimate() {
        let mut n = Normalizer::new();
        let f = RawProgress {
            downloaded_bytes: Some(1024.0),
            total_bytes_estimate: Some(1024.0),
            fragment_index: Some(0),
            fragment_count: Some(463),
            ..RawProgress::default()
        };
        // The bogus 100% estimate is capped by the fragment ceiling of 1/463.
        let p = n.apply(&f, Status::Downloading);
        assert!((p - (1.0 / 463.0 * 100.0)).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn fragments_with_no_estimate_use_the_floor() {
        let mut n = Normalizer::new();
        let f = RawProgress {
            fragment_index: Some(10),
            fragment_count: Some(100),
            ..RawProgress::default()
        };
        assert!((n.apply(&f, Status::Downloading) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_bogus_estimate_is_ignored_without_fragments() {
        let mut n = Normalizer::new();
        assert_eq!(
            n.apply(
                &frame(Some(1024.0), None, Some(1024.0)),
                Status::Downloading
            ),
            0.0
        );
        assert_eq!(n.previous(), None, "the floor must stay unset");
    }

    #[test]
    fn nothing_usable_keeps_the_previous_value() {
        let mut n = Normalizer::with_previous(Some(42.5));
        assert!((n.apply(&RawProgress::default(), Status::Downloading) - 42.5).abs() < 1e-9);
    }

    #[test]
    fn percent_never_decreases() {
        let mut n = Normalizer::new();
        let a = n.apply(&frame(Some(80.0), Some(100.0), None), Status::Downloading);
        let b = n.apply(&frame(Some(10.0), Some(100.0), None), Status::Downloading);
        assert!((a - b).abs() < f64::EPSILON, "{a} vs {b}");
    }

    #[test]
    fn a_source_tag_change_resets_the_floor() {
        let mut n = Normalizer::new();
        let mut f = frame(Some(90.0), Some(100.0), None);
        f.source_tag = 1;
        assert!((n.apply(&f, Status::Downloading) - 90.0).abs() < 1e-9);
        f = frame(Some(5.0), Some(100.0), None);
        f.source_tag = 2;
        assert!((n.apply(&f, Status::Downloading) - 5.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn phase_is_sticky_across_frames_that_omit_it() {
        let at = Instant::now();
        let mut cell = ProgressCell::new(at);
        let mut f = RawProgress {
            phase: Some(PhaseTag::Video),
            ..RawProgress::default()
        };
        cell.apply(&f, 10.0, at);
        assert_eq!(cell.phase, Some(PhaseTag::Video));
        f.phase = None;
        cell.apply(&f, 20.0, at);
        assert_eq!(cell.phase, Some(PhaseTag::Video));
    }

    #[test]
    fn number_ports_the_legacy_coercion_rules() {
        use serde_json::json;
        assert_eq!(number(&json!(250)), Some(250.0));
        assert_eq!(number(&json!(250.5)), Some(250.5));
        assert_eq!(number(&json!("250")), Some(250.0));
        assert_eq!(number(&json!("250.5")), Some(250.5));
        assert_eq!(number(&json!(" 250 ")), Some(250.0));
        assert_eq!(number(&json!("n/a")), None);
        assert_eq!(number(&json!(null)), None);
        assert_eq!(number(&json!(true)), Some(1.0), "Python float(True) == 1.0");
        assert_eq!(number(&json!(false)), Some(0.0));
        assert_eq!(number(&json!({"a": 1})), None);
        assert_eq!(number(&json!([1])), None);
        assert_eq!(integer(&json!("463")), Some(463));
        assert_eq!(integer(&json!(-5.9)), Some(-5));
    }

    #[test]
    fn wire_conversions_clamp_and_round() {
        assert_eq!(to_wire_bytes(None), None);
        assert_eq!(to_wire_bytes(Some(-50.0)), Some(0));
        assert_eq!(to_wire_bytes(Some(250.5)), Some(251));
        assert_eq!(to_wire_bytes(Some(f64::NAN)), Some(0));
        assert_eq!(to_wire_count(None), None);
        assert_eq!(to_wire_count(Some(-5)), Some(0));
        assert_eq!(to_wire_count(Some(463)), Some(463));
        assert_eq!(to_wire_count(Some(i64::MAX)), Some(u32::MAX));
    }

    #[test]
    fn a_negative_total_is_not_usable() {
        // Exactly as legacy's `exact_total > 0` guard decided.
        let mut n = Normalizer::new();
        let f = RawProgress {
            downloaded_bytes: Some(10.0),
            total_bytes: Some(-1000.0),
            ..RawProgress::default()
        };
        assert_eq!(n.apply(&f, Status::Downloading), 0.0);
        assert_eq!(n.previous(), None, "the floor stays unset");
    }

    #[test]
    fn a_fractional_byte_count_keeps_its_precision_in_the_percent() {
        let mut n = Normalizer::new();
        let f = RawProgress {
            downloaded_bytes: Some(250.5),
            total_bytes: Some(1000.0),
            ..RawProgress::default()
        };
        assert_eq!(n.apply(&f, Status::Downloading), 25.05);
    }

    #[test]
    fn phase_tag_serialises_snake_case() {
        assert_eq!(
            serde_json::to_string(&PhaseTag::AudioSync).unwrap(),
            "\"audio_sync\""
        );
    }
}
