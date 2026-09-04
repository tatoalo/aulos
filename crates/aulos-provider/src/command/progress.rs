//! The `[progress]` grammar: `json_lines` and `regex` over hostile child output
//! (DESIGN §6.5.1).
//!
//! A downloader's progress output is the least well-behaved text in the system. It repaints with
//! `\r` instead of printing lines, it wraps every number in ANSI colour, it emits partial lines
//! whenever the pipe buffer flushes mid-frame, and it spells `KiB` as `KB` while meaning 1024.
//! Four options in the manifest cover all of it:
//!
//! | Option | What it fixes |
//! |---|---|
//! | `strip_ansi` | colour codes around the numbers the patterns have to match |
//! | `cr_as_newline` | a Spectre.Console / ffmpeg repaint, which is one "line" per minute |
//! | `last_match_wins` | a chunk containing twenty repaints: only the newest is the truth |
//! | `min_interval_ms` | a tool that repaints 200×/s, throttled at the *source* |
//!
//! Numbers go through [`crate::humansize`] and [`aulos_core::progress::number`], never through an
//! ad-hoc `as u64` cast: that is what keeps a fractional `12.5MiB` and a numeric string `"250.5"`
//! reading the same here as in the yt-dlp and StreamingCommunity parsers
//! (`docs/INTEGRATION-NOTES.md`, WP-02).

use std::collections::BTreeMap;
use std::time::Duration;

use aulos_core::progress::{PhaseTag, RawProgress, integer, number};
use aulos_core::status::TerminalStatus;
use regex::Regex;
use tokio::time::Instant;

use super::manifest::ManifestError;
use crate::humansize::{parse_bytes, parse_hms, parse_rate};
use crate::proc::strip_ansi;
use crate::sink::Stage;

/// The capture-group names a `regex` pattern may use (DESIGN §6.5.1).
pub const GROUP_NAMES: [&str; 9] = [
    "percent",
    "downloaded",
    "total",
    "speed",
    "eta",
    "status",
    "fragment_index",
    "fragment_count",
    "msg",
];

/// The default source-side rate cap (DESIGN §6.5.1).
pub const DEFAULT_MIN_INTERVAL_MS: u64 = 250;

/// `progress.kind` (DESIGN §6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ProgressKind {
    /// The plugin prints no progress at all. The item shows `downloading` with no percent.
    #[default]
    None,
    /// One JSON object per line.
    JsonLines,
    /// Named capture groups over arbitrary text.
    Regex,
}

impl ProgressKind {
    /// The manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::JsonLines => "json_lines",
            Self::Regex => "regex",
        }
    }
}

/// `progress.source` (DESIGN §6.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ProgressSource {
    /// stdout only. The default.
    #[default]
    Stdout,
    /// stderr only — where ffmpeg and most CLI progress bars write.
    Stderr,
    /// Both, interleaved in arrival order.
    Both,
}

impl ProgressSource {
    /// The manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Both => "both",
        }
    }

    /// Whether this source reads the child's stdout.
    #[must_use]
    pub const fn reads_stdout(self) -> bool {
        matches!(self, Self::Stdout | Self::Both)
    }

    /// Whether this source reads the child's stderr.
    #[must_use]
    pub const fn reads_stderr(self) -> bool {
        matches!(self, Self::Stderr | Self::Both)
    }
}

/// How a captured number is read (DESIGN §6.5.1's `progress.units` table).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Unit {
    /// Try a plain number first, then a 1024-based size or an `h:m:s` duration. The default, and
    /// the reason a manifest usually needs no `units` table at all.
    #[default]
    Auto,
    /// A plain decimal number. `"1024"` is one thousand and twenty-four, never one kibibyte.
    Number,
    /// A 1024-based byte count or rate: `12.5MiB`, `900 KB/s`, `1.2 GB`.
    Bytes,
    /// A duration: `01:30`, `1:02:03`, `90s`, `1h2m3s`.
    Hms,
}

impl Unit {
    /// The manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Number => "number",
            Self::Bytes => "bytes",
            Self::Hms => "hms",
        }
    }

    fn parse_name(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => Self::Auto,
            "number" | "raw" => Self::Number,
            "bytes" | "size" => Self::Bytes,
            "hms" | "duration" | "seconds" => Self::Hms,
            _ => return None,
        })
    }

    /// Reads a captured size or rate as bytes.
    fn bytes(self, raw: &str) -> Option<f64> {
        match self {
            Self::Number => raw.trim().parse::<f64>().ok(),
            Self::Bytes => parse_rate(raw).or_else(|| parse_bytes(raw)),
            Self::Hms => None,
            Self::Auto => raw
                .trim()
                .parse::<f64>()
                .ok()
                .or_else(|| parse_rate(raw))
                .or_else(|| parse_bytes(raw)),
        }
    }

    /// Reads a captured duration as whole seconds.
    fn seconds(self, raw: &str) -> Option<i64> {
        match self {
            Self::Number => raw.trim().parse::<f64>().ok().map(|v| v as i64),
            Self::Bytes => None,
            Self::Hms | Self::Auto => parse_hms(raw),
        }
    }
}

/// What a captured `status` text translates to (DESIGN §6.5.1's `status_map`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusTarget {
    /// One of the three running statuses.
    Stage(Stage),
    /// A terminal status the plugin is announcing. Only ever *advisory*: the engine still writes
    /// the terminal status from the download's return value (DESIGN §6.2).
    Terminal(TerminalStatus),
}

impl StatusTarget {
    /// The wire status name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stage(s) => s.as_str(),
            Self::Terminal(t) => t.as_str(),
        }
    }

    fn parse_name(s: &str) -> Option<Self> {
        Some(match s {
            "preparing" => Self::Stage(Stage::Preparing),
            "downloading" => Self::Stage(Stage::Downloading),
            "postprocessing" | "mux" | "remux" => Self::Stage(Stage::Postprocessing),
            "finished" => Self::Terminal(TerminalStatus::Finished),
            "error" => Self::Terminal(TerminalStatus::Error),
            "canceled" | "cancelled" => Self::Terminal(TerminalStatus::Canceled),
            _ => return None,
        })
    }
}

/// A validated `[progress]` table (DESIGN §6.5.1).
#[derive(Debug)]
pub struct ProgressSpec {
    /// `json_lines`, `regex` or `none`.
    pub kind: ProgressKind,
    /// Which stream(s) to read.
    pub source: ProgressSource,
    /// Remove ANSI escapes and `\r` before matching.
    pub strip_ansi: bool,
    /// Treat a bare `\r` as a line terminator.
    pub cr_as_newline: bool,
    /// Within one read chunk, the last match of each group wins.
    pub last_match_wins: bool,
    /// Source-side rate cap.
    pub min_interval_ms: u64,
    /// The compiled patterns, in declaration order.
    pub patterns: Vec<Regex>,
    /// Per-group unit overrides.
    pub units: BTreeMap<Box<str>, Unit>,
    /// Captured `status` text → Aulos status.
    pub status_map: BTreeMap<Box<str>, StatusTarget>,
}

impl Default for ProgressSpec {
    /// The DESIGN §6.5.1 defaults, with `kind = "none"`: the table's own values, so a
    /// `ProgressSpec::default()` behaves like an empty `[progress]` section rather than like a
    /// zeroed struct.
    fn default() -> Self {
        Self {
            kind: ProgressKind::None,
            source: ProgressSource::Stdout,
            strip_ansi: true,
            cr_as_newline: true,
            last_match_wins: true,
            min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
            patterns: Vec::new(),
            units: BTreeMap::new(),
            status_map: BTreeMap::new(),
        }
    }
}

impl ProgressSpec {
    /// Validates the raw `[progress]` values (DESIGN §6.5.2).
    ///
    /// # Errors
    /// [`ManifestError::Invalid`] for an unknown `kind`/`source`/`unit`/`status_map` value, a
    /// pattern that does not compile, a pattern with no capture group, and a capture group that is
    /// not in [`GROUP_NAMES`].
    #[allow(clippy::too_many_arguments)] // one argument per manifest key; a struct would just move them
    pub fn validate(
        kind: Option<&str>,
        source: Option<&str>,
        strip_ansi_opt: Option<bool>,
        cr_as_newline: Option<bool>,
        last_match_wins: Option<bool>,
        min_interval_ms: Option<u64>,
        patterns: &[String],
        units: &BTreeMap<String, String>,
        status_map: &BTreeMap<String, String>,
    ) -> Result<Self, ManifestError> {
        let kind = match kind {
            None => {
                if patterns.is_empty() {
                    ProgressKind::None
                } else {
                    // Patterns with no `kind` can only have meant `regex`; saying so beats
                    // silently ignoring them.
                    ProgressKind::Regex
                }
            }
            Some("none") => ProgressKind::None,
            Some("json_lines") => ProgressKind::JsonLines,
            Some("regex") => ProgressKind::Regex,
            Some(other) => {
                return Err(ManifestError::invalid(
                    "progress.kind",
                    format!("{other:?} is not one of json_lines | regex | none"),
                ));
            }
        };
        let source = match source {
            None | Some("stdout") => ProgressSource::Stdout,
            Some("stderr") => ProgressSource::Stderr,
            Some("both") => ProgressSource::Both,
            Some(other) => {
                return Err(ManifestError::invalid(
                    "progress.source",
                    format!("{other:?} is not one of stdout | stderr | both"),
                ));
            }
        };

        let mut compiled = Vec::with_capacity(patterns.len());
        for (i, src) in patterns.iter().enumerate() {
            let key = format!("progress.patterns[{i}]");
            let re = Regex::new(src).map_err(|e| {
                ManifestError::invalid(
                    key.clone(),
                    format!(
                        "{src:?} does not compile: {}",
                        e.to_string()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                )
            })?;
            let named: Vec<&str> = re.capture_names().flatten().collect();
            if named.is_empty() {
                return Err(ManifestError::invalid(
                    key,
                    format!(
                        "{src:?} has no named capture group; one of {GROUP_NAMES:?} is required"
                    ),
                ));
            }
            if let Some(unknown) = named.iter().find(|n| !GROUP_NAMES.contains(*n)) {
                return Err(ManifestError::invalid(
                    key,
                    format!("capture group {unknown:?} is not one of {GROUP_NAMES:?}"),
                ));
            }
            compiled.push(re);
        }
        if kind == ProgressKind::Regex && compiled.is_empty() {
            return Err(ManifestError::invalid(
                "progress.patterns",
                "progress.kind = \"regex\" needs at least one pattern",
            ));
        }

        let mut unit_map = BTreeMap::new();
        for (field, value) in units {
            if !GROUP_NAMES.contains(&field.as_str()) {
                return Err(ManifestError::invalid(
                    format!("progress.units.{field}"),
                    format!("{field:?} is not one of {GROUP_NAMES:?}"),
                ));
            }
            let unit = Unit::parse_name(value).ok_or_else(|| {
                ManifestError::invalid(
                    format!("progress.units.{field}"),
                    format!("{value:?} is not one of auto | number | bytes | hms"),
                )
            })?;
            unit_map.insert(Box::from(field.as_str()), unit);
        }

        let mut statuses = BTreeMap::new();
        for (from, to) in status_map {
            let target = StatusTarget::parse_name(to).ok_or_else(|| {
                ManifestError::invalid(
                    format!("progress.status_map.{from}"),
                    format!(
                        "{to:?} is not one of preparing | downloading | postprocessing | finished | error | canceled"
                    ),
                )
            })?;
            statuses.insert(Box::from(from.as_str()), target);
        }

        Ok(Self {
            kind,
            source,
            strip_ansi: strip_ansi_opt.unwrap_or(true),
            cr_as_newline: cr_as_newline.unwrap_or(true),
            last_match_wins: last_match_wins.unwrap_or(true),
            min_interval_ms: min_interval_ms.unwrap_or(DEFAULT_MIN_INTERVAL_MS),
            patterns: compiled,
            units: unit_map,
            status_map: statuses,
        })
    }

    /// Whether this spec parses anything at all.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        !matches!(self.kind, ProgressKind::None)
    }

    fn unit(&self, field: &str) -> Unit {
        self.units.get(field).copied().unwrap_or_default()
    }
}

/// One coalesced progress reading (DESIGN §6.5.1).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ProgressUpdate {
    /// The numeric fields that were seen. Absent fields stay `None`, which the aggregator's
    /// normaliser treats as "keep the previous value".
    pub raw: RawProgress,
    /// The status the plugin announced, when `status_map` translated one.
    pub status: Option<StatusTarget>,
    /// A human message for `ItemView.msg`.
    pub msg: Option<String>,
}

impl ProgressUpdate {
    /// Whether this update carries nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw == RawProgress::default() && self.status.is_none() && self.msg.is_none()
    }

    fn merge(&mut self, other: &Self) {
        let r = &mut self.raw;
        let o = &other.raw;
        macro_rules! take {
            ($($f:ident),+) => { $( if o.$f.is_some() { r.$f = o.$f; } )+ };
        }
        take!(
            downloaded_bytes,
            total_bytes,
            total_bytes_estimate,
            fragment_index,
            fragment_count,
            speed,
            eta,
            phase,
            phase_percent
        );
        if other.status.is_some() {
            self.status = other.status;
        }
        if other.msg.is_some() {
            self.msg = other.msg.clone();
        }
    }
}

/// Frames a child's byte stream into lines and applies a [`ProgressSpec`] to them.
///
/// The parser is fed raw chunks exactly as they arrive from the pipe, because that is the only way
/// to see a `\r` repaint at all: a `\n`-only line reader would buffer a whole minute of a progress
/// bar before yielding anything.
#[derive(Debug)]
pub struct ProgressParser {
    spec: ProgressSpec,
    carry: String,
    pending: ProgressUpdate,
    last_emit: Option<Instant>,
    /// Percent captured but not yet convertible to bytes, kept so a `percent`-only plugin still
    /// produces a moving bar.
    pending_percent: Option<f64>,
}

impl ProgressParser {
    /// A parser for `spec`.
    #[must_use]
    pub fn new(spec: ProgressSpec) -> Self {
        Self {
            spec,
            carry: String::new(),
            pending: ProgressUpdate::default(),
            last_emit: None,
            pending_percent: None,
        }
    }

    /// The spec this parser was built from.
    #[must_use]
    pub const fn spec(&self) -> &ProgressSpec {
        &self.spec
    }

    /// The percent the most recent frame reported, when it reported one.
    #[must_use]
    pub const fn percent(&self) -> Option<f64> {
        self.pending_percent
    }

    /// Feeds one chunk of child output.
    ///
    /// Returns an update when the throttle allows one, or immediately when the chunk announced a
    /// status change — a `mux → postprocessing` transition must not wait 250 ms behind a rate cap.
    /// Everything else accumulates into the pending update and is delivered by the next call that
    /// is allowed to emit, so no capture is ever lost, only coalesced.
    pub fn feed(&mut self, chunk: &str, now: Instant) -> Option<ProgressUpdate> {
        if !self.spec.is_enabled() {
            return None;
        }
        let mut chunk_update = ProgressUpdate::default();
        let mut saw_any = false;
        let lines = self.split(chunk);
        for line in lines {
            let line = if self.spec.strip_ansi {
                strip_ansi(&line)
            } else {
                line
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed = match self.spec.kind {
                ProgressKind::None => None,
                ProgressKind::JsonLines => {
                    parse_json_line(&self.spec, trimmed, &mut self.pending_percent)
                }
                ProgressKind::Regex => {
                    parse_regex_line(&self.spec, trimmed, &mut self.pending_percent)
                }
            };
            if let Some(update) = parsed {
                saw_any = true;
                if self.spec.last_match_wins {
                    chunk_update.merge(&update);
                } else if chunk_update.is_empty() {
                    chunk_update = update;
                }
            }
        }
        if !saw_any {
            return None;
        }
        self.pending.merge(&chunk_update);
        let status_changed = chunk_update.status.is_some();
        let ready = status_changed
            || match self.last_emit {
                None => true,
                Some(prev) => {
                    now.saturating_duration_since(prev)
                        >= Duration::from_millis(self.spec.min_interval_ms)
                }
            };
        if !ready {
            return None;
        }
        self.last_emit = Some(now);
        Some(std::mem::take(&mut self.pending))
    }

    /// Delivers whatever is still pending, ignoring the throttle. Called once when the child
    /// exits, so the final frame of a fast download is never swallowed by the rate cap.
    pub fn flush(&mut self) -> Option<ProgressUpdate> {
        if self.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }

    /// Splits a chunk into lines, carrying a partial line to the next call.
    fn split(&mut self, chunk: &str) -> Vec<String> {
        self.carry.push_str(chunk);
        let mut out = Vec::new();
        let terminators: &[char] = if self.spec.cr_as_newline {
            &['\n', '\r']
        } else {
            &['\n']
        };
        while let Some(at) = self.carry.find(terminators) {
            let mut line: String = self.carry[..at].to_owned();
            if !self.spec.cr_as_newline && line.ends_with('\r') {
                line.pop();
            }
            out.push(line);
            let next = at + self.carry[at..].chars().next().map_or(1, char::len_utf8);
            self.carry = self.carry[next..].to_owned();
        }
        // A very long line with no terminator at all must not grow without bound. 64 KiB is far
        // more than any progress frame and far less than a memory problem.
        if self.carry.len() > 64 * 1024 {
            out.push(std::mem::take(&mut self.carry));
        }
        out
    }
}

/// Applies every `[progress]` pattern to one line (DESIGN §6.5.1).
///
/// A free function rather than a method so the immutable borrow of `spec.patterns` and the
/// mutable borrow of the parser's `pending_percent` are two disjoint fields.
fn parse_regex_line(
    spec: &ProgressSpec,
    line: &str,
    pending_percent: &mut Option<f64>,
) -> Option<ProgressUpdate> {
    let mut update = ProgressUpdate::default();
    let mut hit = false;
    // Only a percent captured *on this line* may synthesise byte counts. The parser's sticky
    // `pending_percent` exists for `ProgressParser::percent()`; using it here would let a later
    // status-only line overwrite a real byte count with a percent-derived one.
    let mut line_percent = None;
    for re in &spec.patterns {
        let mut last = None;
        for caps in re.captures_iter(line) {
            last = Some(caps);
            if !spec.last_match_wins {
                break;
            }
        }
        let Some(caps) = last else { continue };
        hit = true;
        for name in GROUP_NAMES {
            let Some(m) = caps.name(name) else { continue };
            let raw = m.as_str();
            match name {
                "percent" => {
                    if let Some(v) = spec.unit("percent").bytes(raw) {
                        line_percent = Some(v.clamp(0.0, 100.0));
                        *pending_percent = line_percent;
                    }
                }
                "downloaded" => update.raw.downloaded_bytes = spec.unit("downloaded").bytes(raw),
                "total" => update.raw.total_bytes = spec.unit("total").bytes(raw),
                "speed" => update.raw.speed = spec.unit("speed").bytes(raw),
                "eta" => update.raw.eta = spec.unit("eta").seconds(raw),
                "fragment_index" => {
                    update.raw.fragment_index = spec.unit("fragment_index").seconds(raw);
                }
                "fragment_count" => {
                    update.raw.fragment_count = spec.unit("fragment_count").seconds(raw);
                }
                "status" => update.status = spec.status_map.get(raw).copied(),
                "msg" => update.msg = Some(raw.to_owned()),
                _ => {}
            }
        }
    }
    if !hit {
        return None;
    }
    apply_percent(&mut update, line_percent);
    Some(update)
}

/// Reads one `json_lines` progress frame (DESIGN §6.5.1).
fn parse_json_line(
    spec: &ProgressSpec,
    line: &str,
    pending_percent: &mut Option<f64>,
) -> Option<ProgressUpdate> {
    if !line.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let obj = value.as_object()?;
    // A `result` frame is the download's success signal, not a progress frame.
    if obj.get("t").and_then(serde_json::Value::as_str) == Some("result") {
        return None;
    }
    let get =
        |keys: &[&str]| -> Option<&serde_json::Value> { keys.iter().find_map(|k| obj.get(*k)) };
    let mut update = ProgressUpdate::default();
    if let Some(v) = get(&["downloaded", "downloaded_bytes"]) {
        update.raw.downloaded_bytes = json_bytes(v, spec.unit("downloaded"));
    }
    if let Some(v) = get(&["total", "total_bytes"]) {
        update.raw.total_bytes = json_bytes(v, spec.unit("total"));
    }
    if let Some(v) = get(&["total_bytes_estimate"]) {
        update.raw.total_bytes_estimate = json_bytes(v, spec.unit("total"));
    }
    if let Some(v) = get(&["speed"]) {
        update.raw.speed = json_bytes(v, spec.unit("speed"));
    }
    if let Some(v) = get(&["eta"]) {
        update.raw.eta = json_seconds(v, spec.unit("eta"));
    }
    if let Some(v) = get(&["fragment_index"]) {
        update.raw.fragment_index = integer(v);
    }
    if let Some(v) = get(&["fragment_count"]) {
        update.raw.fragment_count = integer(v);
    }
    let mut line_percent = None;
    if let Some(v) = get(&["percent"])
        && let Some(p) = number(v)
    {
        line_percent = Some(p.clamp(0.0, 100.0));
        *pending_percent = line_percent;
    }
    if let Some(s) = get(&["status", "stage"]).and_then(serde_json::Value::as_str) {
        update.status = spec
            .status_map
            .get(s)
            .copied()
            .or_else(|| StatusTarget::parse_name(s));
    }
    if let Some(s) = get(&["msg", "message"]).and_then(serde_json::Value::as_str) {
        update.msg = Some(s.to_owned());
    }
    if let Some(s) = get(&["phase"]).and_then(serde_json::Value::as_str) {
        update.raw.phase = phase_tag(s);
    }
    if let Some(v) = get(&["phase_percent"]) {
        update.raw.phase_percent = number(v);
    }
    if update.is_empty() && line_percent.is_none() {
        return None;
    }
    apply_percent(&mut update, line_percent);
    Some(update)
}

/// Turns a bare `percent` into the byte pair the normaliser works in, so a plugin that only
/// prints `47.3%` still produces a moving bar. A real byte count always wins.
fn apply_percent(update: &mut ProgressUpdate, percent: Option<f64>) {
    let Some(percent) = percent else {
        return;
    };
    if update.raw.downloaded_bytes.is_some() && update.raw.total_bytes.is_some() {
        return;
    }
    if let (Some(total), None) = (update.raw.total_bytes, update.raw.downloaded_bytes) {
        update.raw.downloaded_bytes = Some(total * percent / 100.0);
        return;
    }
    update.raw.downloaded_bytes = Some(percent);
    update.raw.total_bytes = Some(100.0);
}

fn json_bytes(v: &serde_json::Value, unit: Unit) -> Option<f64> {
    match v {
        serde_json::Value::String(s) => unit.bytes(s),
        other => number(other),
    }
}

fn json_seconds(v: &serde_json::Value, unit: Unit) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => unit.seconds(s),
        other => integer(other),
    }
}

fn phase_tag(s: &str) -> Option<PhaseTag> {
    Some(match s {
        "video" => PhaseTag::Video,
        "audio" => PhaseTag::Audio,
        "fragment" => PhaseTag::Fragment,
        "remux" => PhaseTag::Remux,
        "audio_sync" => PhaseTag::AudioSync,
        "mux" => PhaseTag::Mux,
        "subtitle" => PhaseTag::Subtitle,
        "thumbnail" => PhaseTag::Thumbnail,
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn spec(kind: &str, patterns: &[&str]) -> ProgressSpec {
        let pats: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
        ProgressSpec::validate(
            Some(kind),
            None,
            None,
            None,
            None,
            Some(0),
            &pats,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .expect("valid spec")
    }

    fn bandcamp_spec() -> ProgressSpec {
        let patterns: Vec<String> = [
            r"(?P<percent>[\d.]+)%\s+(?P<downloaded>[\d.]+\s*[KMG]i?B)\s*/\s*(?P<total>[\d.]+\s*[KMG]i?B)",
            r"(?P<speed>[\d.]+\s*[KMG]i?B)/s",
            r"ETA\s+(?P<eta>\d{1,2}:\d{2}(:\d{2})?)",
            r"stage=(?P<status>fetch|mux|done)",
        ]
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
        let units = [
            ("downloaded", "auto"),
            ("total", "auto"),
            ("speed", "auto"),
            ("eta", "hms"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let status_map = [
            ("fetch", "downloading"),
            ("mux", "postprocessing"),
            ("done", "finished"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        ProgressSpec::validate(
            Some("regex"),
            Some("both"),
            Some(true),
            Some(true),
            Some(true),
            Some(0),
            &patterns,
            &units,
            &status_map,
        )
        .expect("the DESIGN §6.5.4 example must validate")
    }

    #[tokio::test]
    async fn regex_mode_survives_ansi_repaints_and_partial_lines() {
        let mut p = ProgressParser::new(bandcamp_spec());
        let now = Instant::now();
        // ANSI colour around every number, `\r` repaints, and a line split across two chunks.
        let chunk = "\u{1b}[32m12.0%\u{1b}[0m 1.2MiB / 10.0MiB  1.5MiB/s ETA 00:07\r\
                     \u{1b}[32m47.5%\u{1b}[0m 4.75MiB / 10.0MiB  2.0MiB/s ETA 00:03\r";
        let u = p.feed(chunk, now).expect("an update");
        // Last match wins: the 47.5 % frame, not the 12 % one.
        assert_eq!(p.percent(), Some(47.5));
        assert_eq!(u.raw.downloaded_bytes, Some(4.75 * 1024.0 * 1024.0));
        assert_eq!(u.raw.total_bytes, Some(10.0 * 1024.0 * 1024.0));
        assert_eq!(u.raw.speed, Some(2.0 * 1024.0 * 1024.0));
        assert_eq!(u.raw.eta, Some(3));
        assert!(u.status.is_none());

        // A frame arriving in two pieces is parsed once it is complete, not twice half-parsed.
        assert!(p.feed("99.9% 9.99MiB / 10.0", now).is_none());
        let u = p
            .feed("MiB  1.0MiB/s ETA 00:00\n", now)
            .expect("the completed line");
        assert_eq!(p.percent(), Some(99.9));
        assert_eq!(u.raw.total_bytes, Some(10.0 * 1024.0 * 1024.0));
    }

    #[tokio::test]
    async fn status_map_translates_mux_to_postprocessing() {
        let mut p = ProgressParser::new(bandcamp_spec());
        let now = Instant::now();
        let u = p.feed("stage=fetch\n", now).expect("an update");
        assert_eq!(u.status, Some(StatusTarget::Stage(Stage::Downloading)));
        let u = p.feed("stage=mux\n", now).expect("an update");
        assert_eq!(u.status, Some(StatusTarget::Stage(Stage::Postprocessing)));
        assert_eq!(u.status.map(StatusTarget::as_str), Some("postprocessing"));
        let u = p.feed("stage=done\n", now).expect("an update");
        assert_eq!(
            u.status,
            Some(StatusTarget::Terminal(TerminalStatus::Finished))
        );
        // An unmapped status text is ignored rather than guessed at.
        let mut p = ProgressParser::new(spec("regex", &[r"s=(?P<status>\w+)"]));
        assert_eq!(p.feed("s=whatever\n", now).and_then(|u| u.status), None);
    }

    #[tokio::test]
    async fn units_auto_is_1024_based() {
        let mut p = ProgressParser::new(spec(
            "regex",
            &[r"(?P<downloaded>[\d.]+\s*\S+B)\s*/\s*(?P<total>[\d.]+\s*\S+B)"],
        ));
        let now = Instant::now();
        // `KB` means `KiB` — the N_m3u8DL-RE / ffmpeg convention (DESIGN §6.5.1).
        let u = p.feed("1 KB / 1 MB\n", now).expect("an update");
        assert_eq!(u.raw.downloaded_bytes, Some(1024.0));
        assert_eq!(u.raw.total_bytes, Some(1024.0 * 1024.0));
        let u = p.feed("1.5 GiB / 2 GiB\n", now).expect("an update");
        assert_eq!(u.raw.downloaded_bytes, Some(1.5 * 1024.0 * 1024.0 * 1024.0));
    }

    #[tokio::test]
    async fn json_lines_mode_reads_both_spellings() {
        let mut p = ProgressParser::new(spec("json_lines", &[]));
        let now = Instant::now();
        let u = p
            .feed(
                "{\"downloaded_bytes\":512,\"total_bytes\":2048,\"speed\":1024.5,\"eta\":7}\n",
                now,
            )
            .expect("an update");
        assert_eq!(u.raw.downloaded_bytes, Some(512.0));
        assert_eq!(u.raw.total_bytes, Some(2048.0));
        assert_eq!(u.raw.speed, Some(1024.5));
        assert_eq!(u.raw.eta, Some(7));
        // The short spellings, plus a numeric string and a human size, as legacy `_number()`
        // tolerated (docs/INTEGRATION-NOTES.md, WP-02).
        let u = p
            .feed(
                "{\"downloaded\":\"250.5\",\"total\":\"1000.0\",\"speed\":\"1.5MiB/s\",\"eta\":\"01:30\",\"status\":\"postprocessing\",\"msg\":\"merging\"}\n",
                now,
            )
            .expect("an update");
        assert_eq!(u.raw.downloaded_bytes, Some(250.5));
        assert_eq!(u.raw.total_bytes, Some(1000.0));
        assert_eq!(u.raw.speed, Some(1.5 * 1024.0 * 1024.0));
        assert_eq!(u.raw.eta, Some(90));
        assert_eq!(u.status, Some(StatusTarget::Stage(Stage::Postprocessing)));
        assert_eq!(u.msg.as_deref(), Some("merging"));
        // Non-JSON noise and a `result` frame are not progress.
        assert!(p.feed("Downloading album...\n", now).is_none());
        assert!(
            p.feed("{\"t\":\"result\",\"path\":\"/a/b.flac\"}\n", now)
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_percent_only_plugin_still_moves_the_bar() {
        let mut p = ProgressParser::new(spec("regex", &[r"(?P<percent>[\d.]+)%"]));
        let now = Instant::now();
        let u = p.feed("47.3%\n", now).expect("an update");
        assert_eq!(p.percent(), Some(47.3));
        assert_eq!(u.raw.downloaded_bytes, Some(47.3));
        assert_eq!(u.raw.total_bytes, Some(100.0));
        // Out-of-range percentages are clamped rather than trusted.
        let u = p.feed("250%\n", now).expect("an update");
        assert_eq!(u.raw.downloaded_bytes, Some(100.0));
    }

    #[tokio::test(start_paused = true)]
    async fn min_interval_throttles_but_never_loses_a_capture() {
        let mut spec = spec("regex", &[r"(?P<percent>[\d.]+)% (?P<speed>\d+)"]);
        spec.min_interval_ms = 250;
        let mut p = ProgressParser::new(spec);
        let t0 = Instant::now();
        assert!(p.feed("1% 10\n", t0).is_some(), "the first frame is free");
        assert!(p.feed("2% 20\n", t0).is_none(), "throttled");
        assert!(p.feed("3% 30\n", t0).is_none(), "still throttled");
        // The throttled captures are not lost — the next allowed emit carries the newest of them.
        let later = t0 + Duration::from_millis(250);
        let u = p.feed("4% 40\n", later).expect("an update");
        assert_eq!(u.raw.speed, Some(40.0));
        assert_eq!(p.percent(), Some(4.0));
        // A status change jumps the queue.
        let mut spec = bandcamp_spec();
        spec.min_interval_ms = 60_000;
        let mut p = ProgressParser::new(spec);
        assert!(p.feed("1.0% 1.0MiB / 10.0MiB\n", t0).is_some());
        assert!(p.feed("2.0% 2.0MiB / 10.0MiB\n", t0).is_none());
        let u = p
            .feed("stage=mux\n", t0)
            .expect("a status change is urgent");
        assert_eq!(u.status, Some(StatusTarget::Stage(Stage::Postprocessing)));
        // …and it carried the throttled numbers with it.
        assert_eq!(u.raw.downloaded_bytes, Some(2.0 * 1024.0 * 1024.0));
        assert!(p.flush().is_none(), "nothing left pending");
    }

    #[tokio::test]
    async fn cr_as_newline_can_be_turned_off() {
        let patterns = vec![r"(?P<percent>[\d.]+)%".to_owned()];
        let mut off = ProgressSpec::validate(
            Some("regex"),
            None,
            Some(true),
            Some(false),
            Some(true),
            Some(0),
            &patterns,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(!off.cr_as_newline);
        off.strip_ansi = false;
        let mut p = ProgressParser::new(off);
        let now = Instant::now();
        // With `cr_as_newline` off, a `\r`-only repaint is not yet a line.
        assert!(p.feed("10%\r20%\r", now).is_none());
        let u = p.feed("30%\n", now).expect("the newline completes it");
        // The whole buffered blob matched: last match wins, so 30 %.
        assert_eq!(p.percent(), Some(30.0));
        assert!(u.raw.downloaded_bytes.is_some());
    }

    #[test]
    fn validation_rejects_every_bad_progress_value() {
        let none = BTreeMap::new();
        let bad_kind = ProgressSpec::validate(
            Some("magic"),
            None,
            None,
            None,
            None,
            None,
            &[],
            &none,
            &none,
        )
        .unwrap_err();
        assert_eq!(
            bad_kind.to_string(),
            "progress.kind: \"magic\" is not one of json_lines | regex | none"
        );

        let bad_source = ProgressSpec::validate(
            Some("none"),
            Some("stdio"),
            None,
            None,
            None,
            None,
            &[],
            &none,
            &none,
        )
        .unwrap_err();
        assert!(
            bad_source.to_string().contains("progress.source"),
            "{bad_source}"
        );

        let uncompilable = ProgressSpec::validate(
            Some("regex"),
            None,
            None,
            None,
            None,
            None,
            &["(?P<percent>[".to_owned()],
            &none,
            &none,
        )
        .unwrap_err();
        assert!(
            uncompilable
                .to_string()
                .starts_with("progress.patterns[0]: "),
            "{uncompilable}"
        );
        assert!(uncompilable.to_string().contains("does not compile"));

        let no_groups = ProgressSpec::validate(
            Some("regex"),
            None,
            None,
            None,
            None,
            None,
            &[r"\d+%".to_owned()],
            &none,
            &none,
        )
        .unwrap_err();
        assert!(
            no_groups.to_string().contains("no named capture group"),
            "{no_groups}"
        );

        let unknown_group = ProgressSpec::validate(
            Some("regex"),
            None,
            None,
            None,
            None,
            None,
            &[r"(?P<pct>\d+)%".to_owned()],
            &none,
            &none,
        )
        .unwrap_err();
        assert!(
            unknown_group.to_string().contains("capture group \"pct\""),
            "{unknown_group}"
        );

        let empty_regex = ProgressSpec::validate(
            Some("regex"),
            None,
            None,
            None,
            None,
            None,
            &[],
            &none,
            &none,
        )
        .unwrap_err();
        assert!(
            empty_regex
                .to_string()
                .contains("needs at least one pattern")
        );

        let bad_unit = ProgressSpec::validate(
            Some("none"),
            None,
            None,
            None,
            None,
            None,
            &[],
            &[("speed".to_owned(), "furlongs".to_owned())]
                .into_iter()
                .collect(),
            &none,
        )
        .unwrap_err();
        assert!(
            bad_unit.to_string().contains("progress.units.speed"),
            "{bad_unit}"
        );

        let bad_unit_field = ProgressSpec::validate(
            Some("none"),
            None,
            None,
            None,
            None,
            None,
            &[],
            &[("velocity".to_owned(), "auto".to_owned())]
                .into_iter()
                .collect(),
            &none,
        )
        .unwrap_err();
        assert!(
            bad_unit_field
                .to_string()
                .contains("progress.units.velocity"),
            "{bad_unit_field}"
        );

        let bad_status = ProgressSpec::validate(
            Some("none"),
            None,
            None,
            None,
            None,
            None,
            &[],
            &none,
            &[("fetch".to_owned(), "running".to_owned())]
                .into_iter()
                .collect(),
        )
        .unwrap_err();
        assert!(
            bad_status.to_string().contains("progress.status_map.fetch"),
            "{bad_status}"
        );
    }

    #[test]
    fn the_defaults_are_the_design_table() {
        let none = BTreeMap::new();
        let s =
            ProgressSpec::validate(None, None, None, None, None, None, &[], &none, &none).unwrap();
        assert_eq!(s.kind, ProgressKind::None);
        assert_eq!(s.source, ProgressSource::Stdout);
        assert!(s.strip_ansi);
        assert!(s.cr_as_newline);
        assert!(s.last_match_wins);
        assert_eq!(s.min_interval_ms, DEFAULT_MIN_INTERVAL_MS);
        assert!(!s.is_enabled());
        assert!(s.source.reads_stdout());
        assert!(!s.source.reads_stderr());
        assert!(ProgressSource::Both.reads_stdout() && ProgressSource::Both.reads_stderr());
        // Patterns with no `kind` are read as `regex` rather than silently ignored.
        let s = ProgressSpec::validate(
            None,
            None,
            None,
            None,
            None,
            None,
            &[r"(?P<percent>\d+)".to_owned()],
            &none,
            &none,
        )
        .unwrap();
        assert_eq!(s.kind, ProgressKind::Regex);
        assert_eq!(ProgressKind::JsonLines.as_str(), "json_lines");
        assert_eq!(Unit::default(), Unit::Auto);
        assert_eq!(Unit::Bytes.as_str(), "bytes");
        assert_eq!(ProgressSource::Stderr.as_str(), "stderr");
    }
}
