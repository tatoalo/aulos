//! The live progress board (DESIGN §12.4) and the five discrete notification texts
//! (DESIGN §12.5).
//!
//! Everything here is a pure function of a [`JobLine`] slice and a clock reading, so the whole
//! layout — the bars, the overflow, the group aggregate, the linger markers — is snapshot-testable
//! without a bot.
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
/// How many job lines the board shows before it collapses into `… +N more`.
pub const MAX_LINES: usize = 12;
/// How many characters of a title survive. A longer one keeps this many and gains a `…`, so the
/// rendered field is at most `MAX_TITLE_CHARS + 1` characters wide.
pub const MAX_TITLE_CHARS: usize = 34;
/// The indent of an active line's detail row.
pub const DETAIL_INDENT: &str = "              ";

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
    /// Display title.
    pub title: Arc<str>,
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
    /// A stall or hard-timeout marker.
    pub mark: Option<Mark>,
}

impl JobLine {
    /// A plain queued row.
    #[must_use]
    pub fn new(id: ItemId, title: impl Into<Arc<str>>, status: Status) -> Self {
        Self {
            id,
            title: title.into(),
            status,
            percent: 0.0,
            speed: None,
            eta: None,
            group: None,
            mark: None,
        }
    }
}

/// The whole board message (DESIGN §12.4).
///
/// | Row | Shape |
/// |---|---|
/// | header | `⬇️ Aulos — {n} active, {n} done` plus `, {n} failed` when any failed |
/// | running item | `{bar}  {pct}%  {title}` then an indented `{speed} · ETA {eta}` |
/// | running group | `{bar}  {pct}%  {title} [{done}/{total}]` — the aggregate **instead of** a byte rate |
/// | queued item | `⏳  {pct}%  {title}  (queued)` |
/// | terminal item | `✅ \| ❌ \| 🚫  {title}` |
/// | overflow | `… +{n} more` |
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
    let active = lines.iter().filter(|l| l.status.is_running()).count();
    let done = lines
        .iter()
        .filter(|l| l.status == Status::Finished)
        .count();
    let failed = lines.iter().filter(|l| l.status == Status::Error).count();

    let mut out = String::with_capacity(256);
    out.push_str(&format!("⬇️ Aulos — {active} active, {done} done"));
    if failed > 0 {
        out.push_str(&format!(", {failed} failed"));
    }
    out.push_str("\n\n");

    for line in lines.iter().take(MAX_LINES) {
        out.push_str(&render_line(line));
        out.push('\n');
    }
    if lines.len() > MAX_LINES {
        out.push_str(&format!("… +{} more\n", lines.len() - MAX_LINES));
    }

    out
}

/// One job's row (plus its detail row, when it has one).
#[must_use]
pub fn render_line(line: &JobLine) -> String {
    let title = truncate(&line.title, MAX_TITLE_CHARS);
    let mark = line.mark.map(Mark::glyph).unwrap_or_default();
    let suffix = if mark.is_empty() {
        String::new()
    } else {
        format!("  {mark}")
    };

    match line.status {
        Status::Finished => format!("✅  {title}"),
        Status::Error => format!("❌  {title}"),
        Status::Canceled => format!("🚫  {title}"),
        Status::Queued | Status::Resolving => {
            let note = if line.status == Status::Resolving {
                "(resolving)"
            } else {
                "(queued)"
            };
            format!("⏳  {:.0}%  {title}  {note}{suffix}", line.percent)
        }
        Status::Preparing | Status::Downloading | Status::Postprocessing => {
            let head = format!(
                "{}  {:>3.0}%  {title}",
                bar(line.percent),
                line.percent.clamp(0.0, 100.0)
            );
            match line.group {
                // A group's aggregate replaces the byte rate: a channel of 500 videos has no
                // single speed, and "12 of 500 done" is the number the user wants.
                Some((d, t)) => format!("{head} [{d}/{t}]{suffix}"),
                None => {
                    let detail = detail_row(line.speed, line.eta);
                    if detail.is_empty() {
                        format!("{head}{suffix}")
                    } else {
                        format!("{head}{suffix}\n{DETAIL_INDENT}{detail}")
                    }
                }
            }
        }
    }
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

/// The retirement text the board is edited to 60 s after the last job ends (DESIGN §12.4).
#[must_use]
pub fn retired_text(finished: usize, failed: usize, canceled: usize) -> String {
    let word = if finished == 1 {
        "download"
    } else {
        "downloads"
    };
    let mut out = format!("✅ {finished} {word} finished");
    if failed > 0 {
        out.push_str(&format!(" · ❌ {failed} failed"));
    }
    if canceled > 0 {
        out.push_str(&format!(" · 🚫 {canceled} canceled"));
    }
    out
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

/// The five discrete notification texts (DESIGN §12.5), byte-identical to legacy.
pub mod notify {
    /// `✅ Download complete: {title}` plus `\nFile: {filename}` when known.
    #[must_use]
    pub fn finished(title: &str, filename: Option<&str>) -> String {
        match filename {
            Some(f) => format!("✅ Download complete: {title}\nFile: {f}"),
            None => format!("✅ Download complete: {title}"),
        }
    }

    /// `❌ Download failed: {title}\n{msg}`, where `msg` falls back to `Download failed`.
    #[must_use]
    pub fn failed(title: &str, message: Option<&str>) -> String {
        let message = message
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("Download failed");
        format!("❌ Download failed: {title}\n{message}")
    }

    /// `⚠️ Download seems stalled for {secs}s:\n{url}`.
    #[must_use]
    pub fn stalled(secs: u64, url: &str) -> String {
        format!("⚠️ Download seems stalled for {secs}s:\n{url}")
    }

    /// `⏱️ Download is taking longer than expected ({secs}s):\n{url}`.
    #[must_use]
    pub fn hard_timeout(secs: u64, url: &str) -> String {
        format!("⏱️ Download is taking longer than expected ({secs}s):\n{url}")
    }

    /// `Queued {n} link(s) with current chat config.`
    #[must_use]
    pub fn queued(count: usize) -> String {
        format!("Queued {count} link(s) with current chat config.")
    }

    /// `Too many links in one message ({got}). Maximum allowed: {max}.`
    #[must_use]
    pub fn too_many_urls(got: usize, max: u32) -> String {
        format!("Too many links in one message ({got}). Maximum allowed: {max}.")
    }

    /// `Ignored invalid links:\n- <url> (<reason>)`, one line per rejection.
    #[must_use]
    pub fn ignored(rejected: &[(String, String)]) -> String {
        let body = rejected
            .iter()
            .map(|(url, reason)| format!("- {url} ({reason})"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Ignored invalid links:\n{body}")
    }

    /// `Some links failed:\n- <url>: <msg>`, one line per failure.
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

    /// DESIGN §12.4: a group row carries `[done/total]` **instead of** a byte rate.
    #[test]
    fn a_group_row_carries_its_aggregate_instead_of_a_rate() {
        let mut g = line("Lo-fi beats", Status::Downloading, 21.0);
        g.group = Some((12, 500));
        g.speed = Some(1_468_006.0);
        g.eta = Some(372);
        let rendered = render_line(&g);
        assert_eq!(rendered, "▓▓░░░░░░░░   21%  Lo-fi beats [12/500]");
        assert!(!rendered.contains("MB/s"));
        assert!(!rendered.contains('\n'), "no detail row for a group");
    }

    #[test]
    fn terminal_rows_use_their_own_glyphs() {
        assert_eq!(
            render_line(&line("Veritasium", Status::Finished, 100.0)),
            "✅  Veritasium"
        );
        assert_eq!(
            render_line(&line("Broken", Status::Error, 40.0)),
            "❌  Broken"
        );
        assert_eq!(
            render_line(&line("Dropped", Status::Canceled, 40.0)),
            "🚫  Dropped"
        );
    }

    #[test]
    fn a_queued_row_says_so_and_a_resolving_one_says_resolving() {
        assert_eq!(
            render_line(&line("Big Buck Bunny", Status::Queued, 0.0)),
            "⏳  0%  Big Buck Bunny  (queued)"
        );
        assert_eq!(
            render_line(&line("Not yet known", Status::Resolving, 0.0)),
            "⏳  0%  Not yet known  (resolving)"
        );
    }

    #[test]
    fn a_marked_row_gains_its_glyph() {
        let mut l = line("Stuck", Status::Downloading, 12.0);
        l.mark = Some(Mark::Stalled);
        assert!(render_line(&l).contains("⚠️"));
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
            "⬇️ Aulos — 2 active, 1 done\n\
             \n\
             ▓▓▓▓▓▓▓░░░   68%  Rick Astley - Never Gonna Give You…\n\
             \u{20}             3.1 MB/s · ETA 0:41\n\
             ▓▓░░░░░░░░   21%  Lo-fi beats [12/500]\n\
             ⏳  0%  Big Buck Bunny  (queued)\n\
             ✅  Veritasium - The Big Misconception\n\
             \n\
             updated 14:02:11"
        );
    }

    #[test]
    fn the_header_reports_failures_when_there_are_any() {
        let board = render_board(
            &[
                line("a", Status::Downloading, 1.0),
                line("b", Status::Error, 0.0),
                line("c", Status::Finished, 100.0),
            ],
            1_772_582_400_000,
        );
        assert!(board.starts_with("⬇️ Aulos — 1 active, 1 done, 1 failed\n"));
    }

    /// DESIGN §12.4: at most twelve lines, then `… +N more`.
    #[test]
    fn the_board_collapses_past_twelve_lines() {
        let lines: Vec<JobLine> = (0..20)
            .map(|i| line(&format!("Job {i}"), Status::Queued, 0.0))
            .collect();
        let board = render_board(&lines, 1_772_582_400_000);
        assert!(board.contains("Job 11"), "the twelfth is shown");
        assert!(!board.contains("Job 12"), "the thirteenth is not");
        assert!(board.contains("… +8 more"));

        // Exactly twelve does not overflow.
        let board = render_board(&lines[..12], 1_772_582_400_000);
        assert!(!board.contains("more"));
    }

    #[test]
    fn an_empty_board_is_still_well_formed() {
        let board = render_board(&[], 1_772_582_400_000);
        assert_eq!(board, "⬇️ Aulos — 0 active, 0 done\n\n\nupdated 00:00:00");
    }

    #[test]
    fn the_retirement_text_counts_what_happened() {
        assert_eq!(
            retired_text(4, 1, 0),
            "✅ 4 downloads finished · ❌ 1 failed"
        );
        assert_eq!(retired_text(1, 0, 0), "✅ 1 download finished");
        assert_eq!(
            retired_text(0, 0, 2),
            "✅ 0 downloads finished · 🚫 2 canceled"
        );
    }

    /// DESIGN §12.5, byte-identical to legacy.
    #[test]
    fn the_five_discrete_messages_are_byte_identical() {
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
