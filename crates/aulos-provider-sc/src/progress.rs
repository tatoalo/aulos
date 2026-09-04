//! Progress parsing for both engines (DESIGN §10.5) — the `N_m3u8DL-RE` ANSI frame parser and the
//! ffmpeg `-progress pipe:1` reader.
//!
//! # Why "last match wins" is the whole design
//!
//! `N_m3u8DL-RE` renders its progress with Spectre.Console, which **repaints**: it moves the
//! cursor up, erases the line and prints the whole row again. When that output is a pipe rather
//! than a terminal, several repaints arrive in one `read(2)`, separated by CSI sequences instead of
//! newlines — and the oldest of them is almost always `0/100 0.00%`. A parser that takes the first
//! match therefore pins the UI at zero for the whole download, which is exactly the bug legacy
//! fixed by scanning every match and keeping the **last** one (`app/ytdl.py:129-168`).
//!
//! # What changed from legacy, and why
//!
//! Legacy put the *segment* counters into `downloaded_bytes`/`total_bytes` — "325 bytes of 2000
//! bytes" for segment 325 of 2000 — so the byte counters were nonsense for every SC download and
//! the percent jumped when the size line finally appeared. Here the segment counters go to
//! [`RawProgress::fragment_index`]/[`RawProgress::fragment_count`], which
//! [`Normalizer`](aulos_core::progress::Normalizer) already knows how to turn into a percent, and
//! the byte fields stay `None` until a real `512 MB / 3.20 GB` row is parsed (DESIGN §10.5 Δ).

use std::sync::LazyLock;
use std::time::Duration;

use aulos_core::progress::{PhaseTag, RawProgress};
use regex::Regex;
use tokio::time::Instant;

/// How often a frame is forwarded to the sink, for both engines (DESIGN §10.5: "at most every
/// 0.5 s"; legacy `app/ytdl.py:672`, `:745`).
pub const MIN_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// The five patterns the `N_m3u8DL-RE` parser needs, compiled once.
///
/// Byte-identical to legacy's (`app/ytdl.py:108-117`) except for the ETA lookahead, which Rust's
/// `regex` cannot express — see [`etas`].
struct Patterns {
    /// CSI and OSC escape sequences.
    ansi: Regex,
    /// `<index>/<count> <percent>%`.
    segment: Regex,
    /// `<done> <unit> / <total> <unit>`.
    size: Regex,
    /// `<n> <unit>ps`.
    speed: Regex,
    /// `hh:mm:ss`, filtered by [`etas`].
    eta: Regex,
}

impl Patterns {
    /// Compiles them, or `None` if one does not compile — which a unit test rules out.
    fn compile() -> Option<Self> {
        Some(Self {
            ansi: Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x1b\x07]*(?:\x07|\x1b\\))").ok()?,
            segment: Regex::new(r"(\d+)/(\d+)\s+([\d.]+)%").ok()?,
            size: Regex::new(r"([\d.]+)\s*(KB|MB|GB)\s*/\s*([\d.]+)\s*(KB|MB|GB)").ok()?,
            speed: Regex::new(r"([\d.]+)\s*(KB|MB|GB)ps").ok()?,
            eta: Regex::new(r"(\d{2}):(\d{2}):(\d{2})").ok()?,
        })
    }
}

static PATTERNS: LazyLock<Option<Patterns>> = LazyLock::new(Patterns::compile);

/// The 1024-based multiplier for one of the three units the tool prints.
///
/// `KB`/`MB`/`GB` are Spectre.Console's labels for KiB/MiB/GiB; legacy's table
/// (`app/ytdl.py:119-126`) is 1024-based and the corpus depends on it, so a "512 MB" row is
/// 536 870 912 bytes and not 512 000 000.
const fn multiplier(unit: &str) -> Option<f64> {
    match unit.as_bytes() {
        b"KB" => Some(1024.0),
        b"MB" => Some(1024.0 * 1024.0),
        b"GB" => Some(1024.0 * 1024.0 * 1024.0),
        _ => None,
    }
}

/// `int(float(value) * multiplier)` — legacy `_nm3u8_size_bytes`, truncation included.
///
/// The truncation matters: `3.20 GB` is `3435973836.8` bytes and legacy reported `3435973836`.
fn size_bytes(value: &str, unit: &str) -> Option<f64> {
    Some((value.parse::<f64>().ok()? * multiplier(unit)?).trunc())
}

/// Every `hh:mm:ss` in `text` that is followed by whitespace or the end of the text.
///
/// Legacy expressed the condition as the lookahead `(?=\s|$)`, which Rust's `regex` does not
/// support. Matching the bare timestamp and filtering on the next character is equivalent for
/// every input the tool produces: both engines scan left to right for non-overlapping matches, and
/// the filter rejects exactly the timestamps the lookahead would have.
fn etas(p: &Patterns, text: &str) -> Option<i64> {
    let mut last = None;
    for c in p.eta.captures_iter(text) {
        let whole = c.get(0)?;
        let next = text[whole.end()..].chars().next();
        if next.is_some_and(|c| !c.is_whitespace()) {
            continue;
        }
        let (h, m, s) = (
            c.get(1)?.as_str().parse::<i64>().ok()?,
            c.get(2)?.as_str().parse::<i64>().ok()?,
            c.get(3)?.as_str().parse::<i64>().ok()?,
        );
        last = Some(h * 3600 + m * 60 + s);
    }
    last
}

/// Removes the CSI and OSC escape sequences legacy's `_NM3U8_ANSI_RE` removed.
///
/// [`aulos_provider::proc::strip_ansi`] also drops carriage returns, which would glue every
/// repaint of a progress row into one unreadable line; the error tail and the parser both want the
/// frames kept apart, so this leaves `\r` alone and the callers decide what it means.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    PATTERNS
        .as_ref()
        .map_or_else(|| s.to_owned(), |p| p.ansi.replace_all(s, "").into_owned())
}

/// Extracts the newest progress frame from one read of `N_m3u8DL-RE`'s console output.
///
/// Returns `None` unless a segment row **or** a size row was found, mirroring legacy's
/// `if not status: return {}` gate: a chunk that carries only a speed or an ETA is a repaint
/// artefact, not a frame, and forwarding it would overwrite good numbers with nothing.
///
/// The frame is tagged [`PhaseTag::Fragment`], which is what the segment fetch is.
#[must_use]
pub fn parse_nm3u8_frame(chunk: &str) -> Option<RawProgress> {
    let p = PATTERNS.as_ref()?;
    // Legacy: strip the escapes, then treat a carriage return as a line break so the ETA's
    // "followed by whitespace" condition still holds at a frame boundary.
    let text = strip_ansi(chunk).replace('\r', "\n");

    let mut out = RawProgress {
        phase: Some(PhaseTag::Fragment),
        ..RawProgress::default()
    };
    let mut usable = false;

    if let Some(c) = p.segment.captures_iter(&text).last() {
        out.fragment_index = c.get(1).and_then(|m| m.as_str().parse().ok());
        out.fragment_count = c.get(2).and_then(|m| m.as_str().parse().ok());
        usable = true;
    }
    if let Some(c) = p.size.captures_iter(&text).last() {
        let (d, du) = (c.get(1)?.as_str(), c.get(2)?.as_str());
        let (t, tu) = (c.get(3)?.as_str(), c.get(4)?.as_str());
        out.downloaded_bytes = size_bytes(d, du);
        out.total_bytes = size_bytes(t, tu);
        usable = true;
    }
    if !usable {
        return None;
    }

    if let Some(c) = p.speed.captures_iter(&text).last() {
        out.speed = size_bytes(c.get(1)?.as_str(), c.get(2)?.as_str());
    }
    out.eta = etas(p, &text);
    Some(out)
}

/// The ffmpeg `-progress pipe:1` reader (DESIGN §10.5, legacy `app/ytdl.py:660-686`).
///
/// ffmpeg writes `key=value` lines and terminates each group with `progress=continue|end`, so this
/// accumulates the three keys it cares about and emits a frame when a group closes — but never
/// more often than [`MIN_PROGRESS_INTERVAL`].
///
/// The derived fields are legacy's arithmetic verbatim, including its two quirks: `out_time_ms` is
/// divided by 1e6 (the key is microseconds despite the name, so the result really is seconds), and
/// `speed` is reported as *bytes per second* reconstructed from ffmpeg's playback-rate multiplier
/// times the average bitrate so far.
#[derive(Clone, Copy, Debug)]
pub struct FfmpegProgress {
    /// The `ffprobe` duration in seconds, when the probe succeeded.
    duration: Option<f64>,
    /// `out_time_ms / 1e6`, in seconds.
    out_time_s: f64,
    /// `total_size`, in bytes.
    total_size: f64,
    /// `speed=<x>x`, a playback-rate multiplier.
    speed_x: f64,
    /// When a frame was last emitted. Seeded with the spawn instant, so a first group that arrives
    /// immediately is suppressed exactly as legacy's `last_update = time.time()` did.
    last: Instant,
    /// The emission floor.
    min_interval: Duration,
}

impl FfmpegProgress {
    /// A reader for a job that started at `started`.
    #[must_use]
    pub const fn new(duration: Option<f64>, started: Instant, min_interval: Duration) -> Self {
        Self {
            duration,
            out_time_s: 0.0,
            total_size: 0.0,
            speed_x: 0.0,
            last: started,
            min_interval,
        }
    }

    /// The probed duration, for logging.
    #[must_use]
    pub const fn duration(&self) -> Option<f64> {
        self.duration
    }

    /// Feeds one line, returning a frame when a group closed and the interval has elapsed.
    pub fn feed(&mut self, line: &str, now: Instant) -> Option<RawProgress> {
        let line = line.trim();
        let (key, value) = line.split_once('=')?;
        match key {
            // Legacy gated on `value.isdigit()`, which rejects `N/A` and a negative value.
            "out_time_ms" if value.bytes().all(|b| b.is_ascii_digit()) => {
                self.out_time_s = value.parse::<f64>().ok()? / 1_000_000.0;
            }
            "total_size" if value.bytes().all(|b| b.is_ascii_digit()) => {
                self.total_size = value.parse::<f64>().ok()?;
            }
            "speed" => {
                if let Some(x) = value.strip_suffix('x')
                    && let Ok(v) = x.trim().parse::<f64>()
                {
                    self.speed_x = v;
                }
            }
            "progress" => {
                if now.duration_since(self.last) < self.min_interval {
                    return None;
                }
                self.last = now;
                return Some(self.frame());
            }
            _ => {}
        }
        None
    }

    /// The frame legacy assembled at `progress=`.
    fn frame(&self) -> RawProgress {
        let mut raw = RawProgress::default();
        if self.total_size > 0.0 {
            raw.downloaded_bytes = Some(self.total_size);
        }
        if let Some(d) = self.duration
            && d > 0.0
            && self.out_time_s > 0.0
        {
            raw.total_bytes_estimate = Some((self.total_size / (self.out_time_s / d)).trunc());
            if self.speed_x > 0.0 {
                raw.eta = Some(((d - self.out_time_s) / self.speed_x) as i64);
                raw.speed = Some(self.speed_x * (self.total_size / self.out_time_s));
            }
        }
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture of the shape DESIGN §10.5 describes: three repaints in one read, the first of
    /// them the useless `0/100 0.00%` row, separated by "erase line" + "cursor up" CSI pairs and
    /// one OSC title sequence, with no newline anywhere.
    const REPAINTS: &str = include_str!("../tests/fixtures/sc/nm3u8_repaints.txt");

    #[test]
    fn the_patterns_compile() {
        assert!(PATTERNS.is_some(), "the progress patterns must compile");
    }

    #[test]
    fn the_last_repaint_wins_and_a_leading_zero_frame_never_pins_progress() {
        let f = parse_nm3u8_frame(REPAINTS).expect("a frame");
        assert_eq!(f.fragment_index, Some(325));
        assert_eq!(f.fragment_count, Some(2000));
        assert_eq!(f.downloaded_bytes, Some(512.0 * 1024.0 * 1024.0));
        assert_eq!(
            f.total_bytes,
            Some(f64::trunc(3.20 * 1024.0 * 1024.0 * 1024.0))
        );
        assert_eq!(f.speed, Some(8.50 * 1024.0 * 1024.0));
        assert_eq!(f.eta, Some(75));
        assert_eq!(f.phase, Some(PhaseTag::Fragment));
    }

    #[test]
    fn the_legacy_capture_parses_the_way_the_legacy_unit_test_pinned_it() {
        // `app/tests/test_ytdl_utils.py::test_nm3u8_progress_uses_latest_repaint_frame`, with the
        // one deliberate change of DESIGN §10.5: the segment counters are fragments now.
        let raw = "video 0/100 0.00% 0 MB/0 MB 0 KBps 00:00:00\
                   \u{1b}[2K\u{1b}[1A\
                   video 325/2000 16.25% 512 MB/3.20 GB 8.50 MBps 00:01:15";
        let f = parse_nm3u8_frame(raw).expect("a frame");
        assert_eq!(f.downloaded_bytes, Some(536_870_912.0));
        assert_eq!(f.total_bytes, Some(3_435_973_836.0));
        assert_eq!(f.speed, Some(8_912_896.0));
        assert_eq!(f.eta, Some(75));
        assert_eq!(
            (f.fragment_index, f.fragment_count),
            (Some(325), Some(2000))
        );
    }

    #[test]
    fn the_units_are_1024_based() {
        let f = parse_nm3u8_frame("1/2 50.00% 1 KB/1 GB 1 MBps").expect("a frame");
        assert_eq!(f.downloaded_bytes, Some(1024.0));
        assert_eq!(f.total_bytes, Some(1_073_741_824.0));
        assert_eq!(f.speed, Some(1_048_576.0));
    }

    #[test]
    fn a_segment_only_frame_leaves_the_byte_fields_null() {
        let f = parse_nm3u8_frame("\rvideo 7/463 1.51%\r").expect("a frame");
        assert_eq!((f.fragment_index, f.fragment_count), (Some(7), Some(463)));
        assert_eq!(f.downloaded_bytes, None, "no real size was reported yet");
        assert_eq!(f.total_bytes, None);
        assert_eq!(f.speed, None);
        assert_eq!(f.eta, None);
    }

    #[test]
    fn a_size_only_frame_reports_bytes_without_fragments() {
        let f = parse_nm3u8_frame("12.50 MB/100.00 MB 1.00 MBps 00:00:30").expect("a frame");
        assert_eq!(f.fragment_index, None);
        assert_eq!(f.downloaded_bytes, Some(12.5 * 1024.0 * 1024.0));
        assert_eq!(f.eta, Some(30));
    }

    #[test]
    fn a_chunk_with_no_frame_in_it_is_none() {
        for chunk in [
            "",
            "Loading URL...",
            "\u{1b}[2K\u{1b}[1A",
            // Speed and ETA alone are a repaint artefact, not a frame (legacy's gate).
            "1.00 MBps 00:01:00",
            "\u{1b}]0;N_m3u8DL-RE\u{7}",
        ] {
            assert!(parse_nm3u8_frame(chunk).is_none(), "{chunk:?}");
        }
    }

    #[test]
    fn an_eta_that_is_not_followed_by_whitespace_is_not_an_eta() {
        // Spectre prints "00:01:15" as a standalone column; a timestamp glued to more digits is
        // part of something else (legacy's `(?=\s|$)`).
        let f = parse_nm3u8_frame("1/2 50.00% 00:01:15x").expect("a frame");
        assert_eq!(f.eta, None);
        let f = parse_nm3u8_frame("1/2 50.00% 00:01:15").expect("a frame");
        assert_eq!(f.eta, Some(75));
        let f = parse_nm3u8_frame("1/2 50.00% 00:01:15 rest").expect("a frame");
        assert_eq!(f.eta, Some(75));
    }

    #[test]
    fn an_osc_sequence_is_stripped_like_a_csi_one() {
        let f = parse_nm3u8_frame("\u{1b}]0;title\u{7}5/10 50.00%").expect("a frame");
        assert_eq!((f.fragment_index, f.fragment_count), (Some(5), Some(10)));
    }

    // -- ffmpeg -------------------------------------------------------------------------------

    fn ffmpeg_stream() -> Vec<String> {
        include_str!("../tests/fixtures/sc/ffmpeg_progress.txt")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_ffmpeg_stream_yields_derived_estimates_and_eta() {
        let start = Instant::now();
        let mut p = FfmpegProgress::new(Some(1200.0), start, MIN_PROGRESS_INTERVAL);
        assert_eq!(p.duration(), Some(1200.0));
        let mut frames = Vec::new();
        for (i, line) in ffmpeg_stream().iter().enumerate() {
            // One simulated second per line, so every `progress=` group clears the 0.5 s floor.
            let now = start + Duration::from_secs(i as u64 + 1);
            if let Some(f) = p.feed(line, now) {
                frames.push(f);
            }
        }
        assert_eq!(frames.len(), 2, "one frame per `progress=` group");

        let first = frames[0];
        // out_time_ms=60000000 -> 60 s of 1200 s, total_size=10485760.
        assert_eq!(first.downloaded_bytes, Some(10_485_760.0));
        assert_eq!(
            first.total_bytes_estimate,
            Some(f64::trunc(10_485_760.0 / (60.0 / 1200.0)))
        );
        assert_eq!(first.eta, Some(((1200.0 - 60.0) / 12.5) as i64));
        assert_eq!(first.speed, Some(12.5 * (10_485_760.0 / 60.0)));
        // The exact total is never claimed: only ffprobe's duration is known, not the byte size.
        assert_eq!(first.total_bytes, None);

        let second = frames[1];
        assert!(
            second.downloaded_bytes > first.downloaded_bytes,
            "the second group must have moved"
        );
        assert!(second.eta < first.eta);
    }

    #[tokio::test(start_paused = true)]
    async fn frames_are_throttled_to_the_half_second_floor() {
        let start = Instant::now();
        let mut p = FfmpegProgress::new(Some(100.0), start, MIN_PROGRESS_INTERVAL);
        assert!(
            p.feed("progress=continue", start + Duration::from_millis(200))
                .is_none(),
            "a group inside the first 500 ms is suppressed, as legacy's clock did"
        );
        assert!(
            p.feed("progress=continue", start + Duration::from_millis(600))
                .is_some()
        );
        assert!(
            p.feed("progress=continue", start + Duration::from_millis(700))
                .is_none()
        );
        assert!(
            p.feed("progress=end", start + Duration::from_millis(1200))
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_duration_probe_still_reports_bytes() {
        let start = Instant::now();
        let mut p = FfmpegProgress::new(None, start, MIN_PROGRESS_INTERVAL);
        p.feed("total_size=2048", start);
        p.feed("out_time_ms=1000000", start);
        let f = p
            .feed("progress=continue", start + Duration::from_secs(1))
            .expect("a frame");
        assert_eq!(f.downloaded_bytes, Some(2048.0));
        assert_eq!(f.total_bytes_estimate, None, "no duration, no estimate");
        assert_eq!(f.eta, None);
        assert_eq!(f.speed, None);
    }

    #[tokio::test(start_paused = true)]
    async fn unusable_values_are_ignored_exactly_as_legacy_ignored_them() {
        let start = Instant::now();
        let mut p = FfmpegProgress::new(Some(10.0), start, MIN_PROGRESS_INTERVAL);
        for line in [
            "out_time_ms=N/A",
            "out_time_ms=-5",
            "total_size=N/A",
            "speed=N/A",
            "speed=",
            "bitrate=1000kbits/s",
            "a line with no equals sign",
        ] {
            assert!(p.feed(line, start).is_none());
        }
        let f = p
            .feed("progress=continue", start + Duration::from_secs(1))
            .expect("a frame");
        assert_eq!(f.downloaded_bytes, None);
        assert_eq!(f.total_bytes_estimate, None);
    }
}
