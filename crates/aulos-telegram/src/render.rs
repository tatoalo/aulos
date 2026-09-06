//! The live progress board (DESIGN §12.4) and the discrete notification texts (DESIGN §12.5).
//!
//! In board mode a burst of downloads is **one** message: the rows are the acknowledgement, the
//! progress and the completion, and the glyph at the head of each row is the whole story
//! (`⏳` queued → `⏬` running → `✅`/`❌`/`🚫`). Nothing here is sent twice; the actor edits the
//! same message until it retires it. `per_job` mode keeps the legacy discrete texts instead, so
//! both vocabularies live in this module.
//!
//! Everything is a pure function of a [`JobLine`] slice and a clock reading, so the whole layout —
//! the bars, the overflow, the group aggregate, the retirement — is snapshot-testable without a
//! bot.
//!
//! **Plain text, no Markdown.** Titles come from providers and contain `_`, `*`, `[` and `` ` ``
//! routinely; escaping them correctly for `MarkdownV2` is a bug factory, and a mis-escaped entity
//! makes Telegram reject the whole edit. There is nothing in the board that needs formatting.

use std::sync::Arc;

use aulos_core::id::{ItemId, UnixMs};
use aulos_core::status::Status;

/// How many blocks a progress bar has.
pub const BAR_WIDTH: usize = 10;
/// The filled block.
pub const BAR_FULL: char = '▓';
/// The empty block.
pub const BAR_EMPTY: char = '░';
/// How many job rows the board shows before it collapses (DESIGN §12.4).
pub const MAX_LINES: usize = 12;
/// How many characters of a title survive. A longer one keeps this many and gains a `…`, so the
/// rendered field is at most `MAX_TITLE_CHARS + 1` characters wide.
pub const MAX_TITLE_CHARS: usize = 34;
/// How many characters of a failure reason survive on an `❌` row's detail line.
pub const MAX_ERROR_CHARS: usize = 80;
/// The indent of a row's detail line — the width of the glyph column, so the detail sits under the
/// title rather than under the glyph.
pub const DETAIL_INDENT: &str = "    ";

/// The glyph that opens a row. The row's *only* state indicator (DESIGN §12.4): a batch is one
/// message, so a status change is a glyph change and never a new text.
const GLYPH_WAITING: &str = "⏳";
const GLYPH_RUNNING: &str = "⏬";
const GLYPH_FINISHED: &str = "✅";
const GLYPH_ERROR: &str = "❌";
const GLYPH_CANCELED: &str = "🚫";

/// An out-of-band marker on a board line (DESIGN §12.5: the two warnings are alerts, not state).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// No progress for `TELEGRAM_STALL_TIMEOUT_SECONDS`.
    Stalled,
    /// Running longer than `TELEGRAM_HARD_TIMEOUT_SECONDS`.
    Timeout,
}

impl Mark {
    /// The emoji the board line gains.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Stalled => "⚠️",
            Self::Timeout => "⏱️",
        }
    }
}

/// One job's row on the board.
#[derive(Clone, PartialEq, Debug)]
pub struct JobLine {
    /// Which item.
    pub id: ItemId,
    /// Display title. Empty until resolution names the item.
    pub title: Arc<str>,
    /// The item's URL — what the row shows while [`JobLine::title`] is still blank. A row with no
    /// label at all would be worse than a long URL: the user posted the link, so they recognise it.
    pub url: Arc<str>,
    /// Its status.
    pub status: Status,
    /// `0.0..=100.0`.
    pub percent: f64,
    /// Bytes per second, when known.
    pub speed: Option<f64>,
    /// Seconds remaining, when known.
    pub eta: Option<i64>,
    /// `Some((done, total))` for a group row, `None` for a plain item.
    pub group: Option<(u32, u32)>,
    /// Why an `Error` row failed. The row's detail line, because in board mode there is no
    /// `❌ Download failed` message to carry it (DESIGN §12.4).
    pub error: Option<Arc<str>>,
    /// A stall or hard-timeout marker.
    pub mark: Option<Mark>,
}

impl JobLine {
    /// A plain queued row with no URL fallback.
    #[must_use]
    pub fn new(id: ItemId, title: impl Into<Arc<str>>, status: Status) -> Self {
        Self {
            id,
            title: title.into(),
            url: "".into(),
            status,
            percent: 0.0,
            speed: None,
            eta: None,
            group: None,
            error: None,
            mark: None,
        }
    }

    /// What the row is labelled with: the title, or the URL while there is no title yet.
    #[must_use]
    pub fn display_title(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.url
        } else {
            &self.title
        }
    }
}

/// The whole board message (DESIGN §12.4).
///
/// | Row | Shape |
/// |---|---|
/// | header | `⬇️ Aulos — {n} active[, {n} queued], {n} done[, {n} failed]` |
/// | queued / resolving | `⏳  {title}` |
/// | running item | `⏬  {title}` then an indented `{bar}  {pct}% · {speed}/s · ETA {eta}` |
/// | running group | `⏬  {title} [{done}/{total}]` then `{bar}  {pct}%` — no byte rate |
/// | postprocessing | `⏬  {title}` then `{bar}  {pct}% · post-processing` |
/// | finished / canceled | `✅ \| 🚫  {title}` |
/// | error | `❌  {title}` then an indented reason |
/// | overflow | `… +{n} finished earlier` above, or `… +{n} more` below |
/// | footer | `updated HH:MM:SS` |
#[must_use]
pub fn render_board(lines: &[JobLine], now_ms: UnixMs) -> String {
    format!("{}\nupdated {}", render_body(lines), clock_time(now_ms))
}

/// The board **without** its `updated HH:MM:SS` footer — the string change detection compares.
///
/// The split matters: the footer moves on every tick, so comparing the whole message against
/// `last_rendered` would make every tick a change and defeat the `message is not modified` guard
/// the whole rate budget depends on (DESIGN §12.4). Only the body is state.
#[must_use]
pub fn render_body(lines: &[JobLine]) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(&live_header(lines));
    out.push_str("\n\n");
    let rows = render_rows(lines);
    if !rows.is_empty() {
        out.push_str(&rows);
        out.push('\n');
    }
    out
}

/// The retirement text the board is edited to 60 s after the last job ends (DESIGN §12.4).
///
/// The same rows, under a closing header and **without** the `updated` footer: the message stops
/// being live, so a clock on it would only ever be wrong. It is the last edit the board receives —
/// the actor forgets it immediately afterwards and the next burst opens a new message.
#[must_use]
pub fn retired_text(lines: &[JobLine]) -> String {
    let mut out = retired_header(lines);
    let rows = render_rows(lines);
    if !rows.is_empty() {
        out.push_str("\n\n");
        out.push_str(&rows);
    }
    out
}

/// `⬇️ Aulos — 2 active, 3 queued, 1 done, 1 failed`, dropping the zero-valued middle terms.
fn live_header(lines: &[JobLine]) -> String {
    let active = lines.iter().filter(|l| l.status.is_running()).count();
    let queued = lines
        .iter()
        .filter(|l| matches!(l.status, Status::Queued | Status::Resolving))
        .count();
    let done = lines
        .iter()
        .filter(|l| l.status == Status::Finished)
        .count();
    let failed = lines.iter().filter(|l| l.status == Status::Error).count();

    let mut out = format!("⬇️ Aulos — {active} active");
    if queued > 0 {
        out.push_str(&format!(", {queued} queued"));
    }
    out.push_str(&format!(", {done} done"));
    if failed > 0 {
        out.push_str(&format!(", {failed} failed"));
    }
    out
}

/// `✅ All done — 4 finished, 1 failed`.
fn retired_header(lines: &[JobLine]) -> String {
    let n = |s: Status| lines.iter().filter(|l| l.status == s).count();
    let (finished, failed, canceled) = (n(Status::Finished), n(Status::Error), n(Status::Canceled));
    let mut out = format!("✅ All done — {finished} finished");
    if failed > 0 {
        out.push_str(&format!(", {failed} failed"));
    }
    if canceled > 0 {
        out.push_str(&format!(", {canceled} canceled"));
    }
    out
}

/// The visible rows plus their overflow notes, joined by newlines and with no trailing one.
fn render_rows(lines: &[JobLine]) -> String {
    let (above, rows, below) = visible(lines);
    let mut out: Vec<String> = Vec::with_capacity(rows.len() + 2);
    out.extend(above);
    out.extend(rows.iter().map(|l| render_line(l)));
    out.extend(below);
    out.join("\n")
}

/// Which rows survive the `MAX_LINES` cap, and the note that stands in for the rest.
///
/// DESIGN §12.4: a terminal row never *drops* while the board lives — the ✅ is the receipt for a
/// download the user asked for, and a batch of thirteen must not silently lose the first one. So
/// overflow hides the **oldest terminal** rows and says how many, which keeps everything still
/// happening on screen. Only when the live rows alone overrun the cap is there nothing left to
/// trade, and the board falls back to the plain tail collapse — which then shows the oldest *live*
/// rows, because a receipt that pushed a running download off the board would defeat the point.
fn visible(lines: &[JobLine]) -> (Option<String>, Vec<&JobLine>, Option<String>) {
    if lines.len() <= MAX_LINES {
        return (None, lines.iter().collect(), None);
    }
    let live = lines.iter().filter(|l| !l.status.is_terminal()).count();
    if live > MAX_LINES {
        // The collapse takes its rows from the **live** ones only. `jobs` is insertion-ordered and
        // a terminal row now lives as long as the board does, so a plain `take(MAX_LINES)` over the
        // whole list would fill the twelve slots with the ✅ receipts of an earlier burst and hide
        // every row still moving — the exact opposite of what this branch is for. The receipts are
        // what gets traded away here: DESIGN §12.4 protects them only "while there is something
        // left to trade", and once the live rows alone overrun the cap there is not.
        let kept: Vec<&JobLine> = lines
            .iter()
            .filter(|l| !l.status.is_terminal())
            .take(MAX_LINES)
            .collect();
        let hidden = lines.len() - kept.len();
        return (None, kept, Some(format!("… +{hidden} more")));
    }
    let hidden = lines.len() - MAX_LINES;
    let mut budget = hidden;
    let kept: Vec<&JobLine> = lines
        .iter()
        .filter(|l| {
            if budget > 0 && l.status.is_terminal() {
                budget -= 1;
                false
            } else {
                true
            }
        })
        .collect();
    (Some(format!("… +{hidden} finished earlier")), kept, None)
}

/// One job's row (plus its indented detail line, when it has one).
#[must_use]
pub fn render_line(line: &JobLine) -> String {
    let title = truncate(line.display_title(), MAX_TITLE_CHARS);
    let mark = line.mark.map(Mark::glyph).unwrap_or_default();
    let suffix = if mark.is_empty() {
        String::new()
    } else {
        format!("  {mark}")
    };

    match line.status {
        Status::Finished => format!("{GLYPH_FINISHED}  {title}"),
        Status::Canceled => format!("{GLYPH_CANCELED}  {title}"),
        Status::Error => format!(
            "{GLYPH_ERROR}  {title}\n{DETAIL_INDENT}{}",
            error_detail(line.error.as_deref())
        ),
        Status::Queued | Status::Resolving => format!("{GLYPH_WAITING}  {title}{suffix}"),
        Status::Preparing | Status::Downloading | Status::Postprocessing => {
            let head = match line.group {
                Some((d, t)) => format!("{GLYPH_RUNNING}  {title} [{d}/{t}]{suffix}"),
                None => format!("{GLYPH_RUNNING}  {title}{suffix}"),
            };
            format!("{head}\n{DETAIL_INDENT}{}", progress_detail(line))
        }
    }
}

/// A running row's detail line: `▓▓▓▓▓▓▓░░░   68% · 3.1 MB/s · ETA 0:41`.
///
/// The bar and the percentage are always there; the rest is whatever is known. A group carries no
/// byte rate at all (DESIGN §12.4: a channel of 500 videos has no single speed), and a
/// postprocessing row says what it is doing instead — there are no bytes moving to report.
#[must_use]
pub fn progress_detail(line: &JobLine) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    parts.push(format!(
        "{}  {:>3.0}%",
        bar(line.percent),
        line.percent.clamp(0.0, 100.0)
    ));
    if line.status == Status::Postprocessing {
        parts.push("post-processing".to_owned());
    } else if line.group.is_none() {
        let rate = detail_row(line.speed, line.eta);
        if !rate.is_empty() {
            parts.push(rate);
        }
    }
    parts.join(" · ")
}

/// A failed row's detail line: the reason, first line only, truncated.
///
/// yt-dlp failures are routinely a paragraph with a stack of `ERROR:` lines; a board row can carry
/// one line of it, and the operator has the API and the logs for the rest.
#[must_use]
pub fn error_detail(reason: Option<&str>) -> String {
    let reason = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("Download failed");
    let first = reason.lines().next().unwrap_or(reason).trim();
    truncate(first, MAX_ERROR_CHARS)
}

/// `3.1 MB/s · ETA 0:41`, or as much of it as is known.
#[must_use]
pub fn detail_row(speed: Option<f64>, eta: Option<i64>) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(2);
    if let Some(s) = speed.filter(|s| *s > 0.0) {
        parts.push(format!("{}/s", format_bytes(s)));
    }
    if let Some(e) = eta.filter(|e| *e >= 0) {
        parts.push(format!("ETA {}", format_eta(e)));
    }
    parts.join(" · ")
}

/// A ten-block bar.
#[must_use]
pub fn bar(percent: f64) -> String {
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(BAR_WIDTH);
    let mut out = String::with_capacity(BAR_WIDTH * 3);
    for _ in 0..filled {
        out.push(BAR_FULL);
    }
    for _ in filled..BAR_WIDTH {
        out.push(BAR_EMPTY);
    }
    out
}

/// `title` cut to `max` characters, with a trailing `…` when it was cut.
#[must_use]
pub fn truncate(title: &str, max: usize) -> String {
    let count = title.chars().count();
    if count <= max {
        return title.to_owned();
    }
    let kept: String = title.chars().take(max).collect();
    format!("{}…", kept.trim_end())
}

/// `M:SS`, or `H:MM:SS` past an hour.
#[must_use]
pub fn format_eta(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, sec) = (s / 3_600, (s % 3_600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// `HH:MM:SS` of the UTC day. Hand-rolled because DESIGN §3 does not budget `time` here, and one
/// modulo is cheaper than a dependency.
#[must_use]
pub fn clock_time(now_ms: UnixMs) -> String {
    let secs_of_day = (now_ms.div_euclid(1_000)).rem_euclid(86_400);
    let (h, m, s) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{h:02}:{m:02}:{s:02}")
}

/// `1.4 MB`, `931 KB`, `12 B` — one decimal above a kilobyte, none below.
#[must_use]
pub fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if !bytes.is_finite() || bytes < 1.0 {
        return "0 B".to_owned();
    }
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1_024.0 && unit + 1 < UNITS.len() {
        value /= 1_024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// The discrete notification texts (DESIGN §12.5).
///
/// Which of these a chat sees depends on `AULOS_TELEGRAM_BOARD`. In `per_job` mode all of them
/// fire, byte-identical to legacy. In `board` mode the four *state* texts — the queue
/// acknowledgement and the two terminal messages — are rows on the board instead, and only the
/// alerts that are not board state survive: [`notify::too_many_urls`], [`notify::ignored`],
/// [`notify::failures`], [`notify::already_queued`] and the two watchdog warnings.
pub mod notify {
    /// `✅ Download complete: {title}` plus `\nFile: {filename}` when known. `per_job` only.
    #[must_use]
    pub fn finished(title: &str, filename: Option<&str>) -> String {
        match filename {
            Some(f) => format!("✅ Download complete: {title}\nFile: {f}"),
            None => format!("✅ Download complete: {title}"),
        }
    }

    /// `❌ Download failed: {title}\n{msg}`, where `msg` falls back to `Download failed`.
    /// `per_job` only.
    #[must_use]
    pub fn failed(title: &str, message: Option<&str>) -> String {
        let message = message
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("Download failed");
        format!("❌ Download failed: {title}\n{message}")
    }

    /// `⚠️ Download seems stalled for {secs}s:\n{url}`. Both modes: an alert is not board state.
    #[must_use]
    pub fn stalled(secs: u64, url: &str) -> String {
        format!("⚠️ Download seems stalled for {secs}s:\n{url}")
    }

    /// `⏱️ Download is taking longer than expected ({secs}s):\n{url}`. Both modes.
    #[must_use]
    pub fn hard_timeout(secs: u64, url: &str) -> String {
        format!("⏱️ Download is taking longer than expected ({secs}s):\n{url}")
    }

    /// `Queued {n} link(s) with current chat config.` — the legacy acknowledgement, `per_job`
    /// only. In board mode the board that appears on the next tick *is* the acknowledgement.
    #[must_use]
    pub fn queued(count: usize) -> String {
        format!("Queued {count} link(s) with current chat config.")
    }

    /// `Already queued: {n} link(s).`
    ///
    /// Board mode's one exception to "the board is the acknowledgement": when every URL in the
    /// message matched a live item the board does not change at all, so silence would be
    /// indistinguishable from the bot having ignored the message.
    #[must_use]
    pub fn already_queued(count: usize) -> String {
        format!("Already queued: {count} link(s).")
    }

    /// `Too many links in one message ({got}). Maximum allowed: {max}.` Both modes.
    #[must_use]
    pub fn too_many_urls(got: usize, max: u32) -> String {
        format!("Too many links in one message ({got}). Maximum allowed: {max}.")
    }

    /// `Ignored invalid links:\n- <url> (<reason>)`, one line per rejection. Both modes.
    #[must_use]
    pub fn ignored(rejected: &[(String, String)]) -> String {
        let body = rejected
            .iter()
            .map(|(url, reason)| format!("- {url} ({reason})"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Ignored invalid links:\n{body}")
    }

    /// `Some links failed:\n- <url>: <msg>`, one line per failure. Both modes: the batch is
    /// all-or-nothing, so there is no board to put this on.
    #[must_use]
    pub fn failures(failed: &[(String, String)]) -> String {
        let body = failed
            .iter()
            .map(|(url, msg)| format!("- {url}: {msg}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Some links failed:\n{body}")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn line(title: &str, status: Status, percent: f64) -> JobLine {
        JobLine {
            percent,
            ..JobLine::new(ItemId::new(), title, status)
        }
    }

    #[test]
    fn the_bar_is_ten_blocks_and_clamps() {
        assert_eq!(bar(0.0), "░░░░░░░░░░");
        assert_eq!(bar(100.0), "▓▓▓▓▓▓▓▓▓▓");
        assert_eq!(bar(68.0), "▓▓▓▓▓▓▓░░░");
        assert_eq!(bar(21.0), "▓▓░░░░░░░░");
        assert_eq!(bar(-5.0), "░░░░░░░░░░");
        assert_eq!(bar(150.0), "▓▓▓▓▓▓▓▓▓▓");
        for p in [0.0, 3.3, 49.9, 50.0, 99.9, 100.0] {
            assert_eq!(bar(p).chars().count(), BAR_WIDTH, "{p}");
        }
    }

    #[test]
    fn titles_are_truncated_with_an_ellipsis() {
        assert_eq!(truncate("short", 34), "short");
        assert_eq!(
            truncate("Rick Astley - Never Gonna Give You Up", 34),
            "Rick Astley - Never Gonna Give You…"
        );
        // Multi-byte titles are cut on character boundaries, not byte ones.
        let cjk = "日本語のタイトルはとても長いのでここで切られます";
        let cut = truncate(cjk, 5);
        assert_eq!(cut.chars().count(), 6, "five kept plus the ellipsis");
        assert!(cut.ends_with('…'));
        assert_eq!(cut, "日本語のタ…");
    }

    #[test]
    fn bytes_and_etas_are_human() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(12.0), "12 B");
        assert_eq!(format_bytes(1_024.0), "1.0 KB");
        assert_eq!(format_bytes(3_250_586.0), "3.1 MB");
        assert_eq!(format_bytes(f64::NAN), "0 B");

        assert_eq!(format_eta(0), "0:00");
        assert_eq!(format_eta(41), "0:41");
        assert_eq!(format_eta(372), "6:12");
        assert_eq!(format_eta(3_723), "1:02:03");
        assert_eq!(format_eta(-9), "0:00");
    }

    #[test]
    fn the_footer_clock_is_the_utc_time_of_day() {
        // 1772582400000 is 2026-03-04T00:00:00Z.
        assert_eq!(clock_time(1_772_582_400_000), "00:00:00");
        assert_eq!(clock_time(1_772_582_400_000 + 50_531_000), "14:02:11");
        assert_eq!(clock_time(0), "00:00:00");
    }

    #[test]
    fn a_detail_row_shows_only_what_is_known() {
        assert_eq!(
            detail_row(Some(3_250_586.0), Some(41)),
            "3.1 MB/s · ETA 0:41"
        );
        assert_eq!(detail_row(Some(3_250_586.0), None), "3.1 MB/s");
        assert_eq!(detail_row(None, Some(41)), "ETA 0:41");
        assert_eq!(detail_row(None, None), "");
        assert_eq!(detail_row(Some(0.0), None), "", "a zero rate is not news");
    }

    /// DESIGN §12.4: the glyph is the state, and the bar moved down to the detail line.
    #[test]
    fn a_running_row_is_a_glyph_a_title_and_an_indented_bar() {
        let mut l = line("Big Buck Bunny", Status::Downloading, 68.0);
        l.speed = Some(3_250_586.0);
        l.eta = Some(41);
        assert_eq!(
            render_line(&l),
            "⏬  Big Buck Bunny\n    ▓▓▓▓▓▓▓░░░   68% · 3.1 MB/s · ETA 0:41"
        );

        // Nothing known but the percentage: the bar still carries the row.
        let bare = line("Big Buck Bunny", Status::Preparing, 0.0);
        assert_eq!(
            render_line(&bare),
            "⏬  Big Buck Bunny\n    ░░░░░░░░░░    0%"
        );
    }

    /// A postprocessing row has no bytes moving, so it says what it is doing instead.
    #[test]
    fn a_postprocessing_row_says_so_instead_of_a_rate() {
        let mut l = line("Merging", Status::Postprocessing, 90.0);
        l.speed = Some(3_250_586.0);
        assert_eq!(
            render_line(&l),
            "⏬  Merging\n    ▓▓▓▓▓▓▓▓▓░   90% · post-processing"
        );
    }

    /// DESIGN §12.4: a group row carries `[done/total]` **instead of** a byte rate.
    #[test]
    fn a_group_row_carries_its_aggregate_instead_of_a_rate() {
        let mut g = line("Lo-fi beats", Status::Downloading, 21.0);
        g.group = Some((12, 500));
        g.speed = Some(1_468_006.0);
        g.eta = Some(372);
        let rendered = render_line(&g);
        assert_eq!(rendered, "⏬  Lo-fi beats [12/500]\n    ▓▓░░░░░░░░   21%");
        assert!(!rendered.contains("MB/s"));
    }

    #[test]
    fn terminal_rows_use_their_own_glyphs() {
        assert_eq!(
            render_line(&line("Veritasium", Status::Finished, 100.0)),
            "✅  Veritasium"
        );
        assert_eq!(
            render_line(&line("Dropped", Status::Canceled, 40.0)),
            "🚫  Dropped"
        );
    }

    /// DESIGN §12.4: in board mode there is no `❌ Download failed` message, so the reason is the
    /// row's detail line — one line of it, truncated.
    #[test]
    fn a_failed_row_carries_its_reason_on_the_detail_line() {
        let mut l = line("Broken", Status::Error, 40.0);
        l.error = Some("HTTP Error 403: Forbidden".into());
        assert_eq!(render_line(&l), "❌  Broken\n    HTTP Error 403: Forbidden");

        l.error = None;
        assert_eq!(render_line(&l), "❌  Broken\n    Download failed");

        // Only the first line, and only 80 characters of it.
        l.error = Some("Unable to download webpage\nTraceback (most recent call last):".into());
        assert_eq!(
            render_line(&l),
            "❌  Broken\n    Unable to download webpage"
        );
        let long: String = std::iter::repeat_n('x', 200).collect();
        l.error = Some(long.as_str().into());
        let detail = render_line(&l);
        let last = detail.lines().last().expect("a detail line");
        assert_eq!(
            last.trim().chars().count(),
            MAX_ERROR_CHARS + 1,
            "80 + the ellipsis"
        );
        assert!(last.ends_with('…'));
    }

    /// A row the resolver has not named yet still has to be identifiable, and the user posted the
    /// link themselves.
    #[test]
    fn a_row_with_no_title_yet_shows_its_url() {
        let mut l = line("", Status::Queued, 0.0);
        l.url = "https://a.test/watch/1".into();
        assert_eq!(render_line(&l), "⏳  https://a.test/watch/1");
        assert_eq!(l.display_title(), "https://a.test/watch/1");

        l.title = "Now it has a name".into();
        assert_eq!(render_line(&l), "⏳  Now it has a name");
    }

    #[test]
    fn a_waiting_row_is_the_hourglass_whether_queued_or_resolving() {
        assert_eq!(
            render_line(&line("Big Buck Bunny", Status::Queued, 0.0)),
            "⏳  Big Buck Bunny"
        );
        assert_eq!(
            render_line(&line("Not yet known", Status::Resolving, 0.0)),
            "⏳  Not yet known"
        );
    }

    #[test]
    fn a_marked_row_gains_its_glyph() {
        let mut l = line("Stuck", Status::Downloading, 12.0);
        l.mark = Some(Mark::Stalled);
        let head = render_line(&l);
        assert!(head.starts_with("⏬  Stuck  ⚠️"), "{head}");
        l.mark = Some(Mark::Timeout);
        assert!(render_line(&l).contains("⏱️"));
        l.mark = None;
        assert!(!render_line(&l).contains("⚠️"));
    }

    /// The whole board, for a mix of active, queued, group and terminal rows.
    #[test]
    fn the_board_renders_the_documented_layout() {
        let mut active = line(
            "Rick Astley - Never Gonna Give You Up",
            Status::Downloading,
            68.0,
        );
        active.speed = Some(3_250_586.0);
        active.eta = Some(41);
        let mut group = line("Lo-fi beats", Status::Downloading, 21.0);
        group.group = Some((12, 500));
        let queued = line("Big Buck Bunny", Status::Queued, 0.0);
        let done = line(
            "Veritasium - The Big Misconception",
            Status::Finished,
            100.0,
        );

        let board = render_board(
            &[active, group, queued, done],
            1_772_582_400_000 + 50_531_000,
        );
        assert_eq!(
            board,
            "⬇️ Aulos — 2 active, 1 queued, 1 done\n\
             \n\
             ⏬  Rick Astley - Never Gonna Give You…\n\
             \u{20}   ▓▓▓▓▓▓▓░░░   68% · 3.1 MB/s · ETA 0:41\n\
             ⏬  Lo-fi beats [12/500]\n\
             \u{20}   ▓▓░░░░░░░░   21%\n\
             ⏳  Big Buck Bunny\n\
             ✅  Veritasium - The Big Misconception\n\
             \n\
             updated 14:02:11"
        );
    }

    #[test]
    fn the_header_counts_what_is_worth_counting() {
        let board = render_board(
            &[
                line("a", Status::Downloading, 1.0),
                line("b", Status::Error, 0.0),
                line("c", Status::Finished, 100.0),
            ],
            1_772_582_400_000,
        );
        assert!(
            board.starts_with("⬇️ Aulos — 1 active, 1 done, 1 failed\n"),
            "{board}"
        );

        // `queued` appears only when there is something queued, and it sits before `done`.
        let board = render_board(
            &[
                line("a", Status::Downloading, 1.0),
                line("b", Status::Queued, 0.0),
                line("c", Status::Resolving, 0.0),
            ],
            1_772_582_400_000,
        );
        assert!(
            board.starts_with("⬇️ Aulos — 1 active, 2 queued, 0 done\n"),
            "{board}"
        );
    }

    /// DESIGN §12.4: overflow hides the **oldest terminal** rows, so nothing still happening ever
    /// falls off the board.
    #[test]
    fn overflow_hides_the_oldest_finished_rows_first() {
        let mut lines: Vec<JobLine> = (0..10)
            .map(|i| line(&format!("Done {i}"), Status::Finished, 100.0))
            .collect();
        lines.extend((0..5).map(|i| line(&format!("Live {i}"), Status::Downloading, 5.0)));

        let board = render_board(&lines, 1_772_582_400_000);
        assert!(board.contains("… +3 finished earlier"), "{board}");
        for hidden in ["Done 0", "Done 1", "Done 2"] {
            assert!(!board.contains(hidden), "{hidden} is hidden: {board}");
        }
        assert!(board.contains("Done 3"), "the fourth survives: {board}");
        for i in 0..5 {
            assert!(
                board.contains(&format!("Live {i}")),
                "a live row never drops: {board}"
            );
        }
        // The note is the first line of the list, above the rows.
        let body: Vec<&str> = board.lines().collect();
        assert_eq!(body[2], "… +3 finished earlier");
    }

    /// Only when the live rows alone overrun the cap is there nothing left to trade.
    #[test]
    fn the_board_falls_back_to_a_tail_collapse_when_everything_is_live() {
        let lines: Vec<JobLine> = (0..20)
            .map(|i| line(&format!("Job {i}"), Status::Queued, 0.0))
            .collect();
        let board = render_board(&lines, 1_772_582_400_000);
        assert!(board.contains("Job 11"), "the twelfth is shown");
        assert!(!board.contains("Job 12"), "the thirteenth is not");
        assert!(board.contains("… +8 more"));
        assert!(!board.contains("finished earlier"));

        // Exactly twelve does not overflow.
        let board = render_board(&lines[..12], 1_772_582_400_000);
        assert!(!board.contains("more"));
    }

    /// The case a board accumulates into once terminal rows stop expiring: twelve receipts already
    /// on the board and thirteen new links behind them. The cap must be spent on the live rows —
    /// a board reporting "13 queued" while showing twelve ✅ and nothing moving is useless.
    #[test]
    fn a_tail_collapse_spends_its_twelve_rows_on_the_live_ones_not_on_old_receipts() {
        let mut lines: Vec<JobLine> = (0..12)
            .map(|i| line(&format!("Done {i}"), Status::Finished, 100.0))
            .collect();
        lines.extend((0..13).map(|i| line(&format!("Live {i}"), Status::Queued, 0.0)));

        let board = render_board(&lines, 1_772_582_400_000);
        assert!(
            board.starts_with("⬇️ Aulos — 0 active, 13 queued"),
            "{board}"
        );
        for i in 0..12 {
            assert!(
                board.contains(&format!("Live {i}")),
                "the live rows own the board: {board}"
            );
        }
        assert!(!board.contains("Live 12"), "the thirteenth is collapsed");
        assert!(
            !board.contains("Done "),
            "the old receipts give way: {board}"
        );
        assert!(board.contains("… +13 more"), "{board}");
    }

    #[test]
    fn an_empty_board_is_still_well_formed() {
        let board = render_board(&[], 1_772_582_400_000);
        assert_eq!(board, "⬇️ Aulos — 0 active, 0 done\n\n\nupdated 00:00:00");
    }

    /// DESIGN §12.4: the last edit keeps the rows, changes the header and drops the clock.
    #[test]
    fn the_retirement_text_keeps_the_rows_and_loses_the_footer() {
        let mut bad = line("Bad", Status::Error, 40.0);
        bad.error = Some("HTTP 403".into());
        let lines = vec![
            line("Good", Status::Finished, 100.0),
            bad,
            line("Dropped", Status::Canceled, 10.0),
        ];
        assert_eq!(
            retired_text(&lines),
            "✅ All done — 1 finished, 1 failed, 1 canceled\n\
             \n\
             ✅  Good\n\
             ❌  Bad\n\
             \u{20}   HTTP 403\n\
             🚫  Dropped"
        );
        assert!(!retired_text(&lines).contains("updated"));

        // The zero-valued terms are dropped, and the overflow rule still applies.
        assert_eq!(
            retired_text(&[line("Good", Status::Finished, 100.0)]),
            "✅ All done — 1 finished\n\n✅  Good"
        );
        let many: Vec<JobLine> = (0..15)
            .map(|i| line(&format!("Done {i}"), Status::Finished, 100.0))
            .collect();
        assert!(retired_text(&many).contains("… +3 finished earlier"));
        assert_eq!(retired_text(&[]), "✅ All done — 0 finished");
    }

    /// DESIGN §12.5, byte-identical to legacy. `per_job` mode still sends every one of these.
    #[test]
    fn the_discrete_messages_are_byte_identical() {
        assert_eq!(
            notify::finished("Clip", Some("Clip.mp4")),
            "✅ Download complete: Clip\nFile: Clip.mp4"
        );
        assert_eq!(notify::finished("Clip", None), "✅ Download complete: Clip");
        assert_eq!(
            notify::failed("Clip", Some("HTTP 403")),
            "❌ Download failed: Clip\nHTTP 403"
        );
        assert_eq!(
            notify::failed("Clip", None),
            "❌ Download failed: Clip\nDownload failed"
        );
        assert_eq!(
            notify::failed("Clip", Some("   ")),
            "❌ Download failed: Clip\nDownload failed"
        );
        assert_eq!(
            notify::stalled(180, "https://a.test/x"),
            "⚠️ Download seems stalled for 180s:\nhttps://a.test/x"
        );
        assert_eq!(
            notify::hard_timeout(7_200, "https://a.test/x"),
            "⏱️ Download is taking longer than expected (7200s):\nhttps://a.test/x"
        );
    }

    #[test]
    fn the_reply_texts_are_byte_identical() {
        assert_eq!(
            notify::queued(3),
            "Queued 3 link(s) with current chat config."
        );
        assert_eq!(notify::already_queued(2), "Already queued: 2 link(s).");
        assert_eq!(
            notify::too_many_urls(14, 10),
            "Too many links in one message (14). Maximum allowed: 10."
        );
        assert_eq!(
            notify::ignored(&[
                (
                    "http://localhost/x".to_owned(),
                    "local network hosts are not allowed".to_owned()
                ),
                (
                    "ftp://a.test".to_owned(),
                    "only http/https URLs are allowed".to_owned()
                ),
            ]),
            "Ignored invalid links:\n\
             - http://localhost/x (local network hosts are not allowed)\n\
             - ftp://a.test (only http/https URLs are allowed)"
        );
        assert_eq!(
            notify::failures(&[(
                "https://a.test/x".to_owned(),
                "Unsupported resource".to_owned()
            )]),
            "Some links failed:\n- https://a.test/x: Unsupported resource"
        );
    }
}
