//! The shared human-size and duration parsers every progress parser needs (DESIGN §6.5.1).
//!
//! One rule, stated once: **the suffixes are 1024-based, `KB` and `KiB` alike.** That is not a
//! convenience — `N_m3u8DL-RE` prints `KB`/`MB` while meaning `KiB`/`MiB`, and so do ffmpeg and
//! most download tools, so a 1000-based reading would make every StreamingCommunity progress bar
//! and every `units = "auto"` plugin report a size 2.4 % low at the gigabyte scale. A tool that
//! genuinely means 1000 can multiply on its side.
//!
//! These are deliberately permissive parsers over hostile text (ANSI-stripped repaint frames,
//! partial lines, `N/A` placeholders): anything unparseable is `None`, which every caller treats
//! as "nothing usable, keep the previous value".

/// One byte unit and its 1024-based multiplier.
///
/// Both the IEC (`KiB`) and the colloquial (`KB`, `K`) spellings of each power are accepted and
/// mean the same thing.
pub const UNITS: [(&str, f64); 15] = [
    ("kib", 1024.0),
    ("mib", 1_048_576.0),
    ("gib", 1_073_741_824.0),
    ("tib", 1_099_511_627_776.0),
    ("pib", 1_125_899_906_842_624.0),
    ("kb", 1024.0),
    ("mb", 1_048_576.0),
    ("gb", 1_073_741_824.0),
    ("tb", 1_099_511_627_776.0),
    ("pb", 1_125_899_906_842_624.0),
    ("k", 1024.0),
    ("m", 1_048_576.0),
    ("g", 1_073_741_824.0),
    ("t", 1_099_511_627_776.0),
    ("b", 1.0),
];

/// Placeholders every tool uses for "I do not know", which parse to `None` rather than `0`.
const PLACEHOLDERS: [&str; 6] = ["n/a", "na", "-", "--", "unknown", "inf"];

/// Parses a byte count: `"1024"`, `"12.5MiB"`, `"12.5 MB"`, `"1 b"`, `"~3.4GiB"`.
///
/// Returns `None` for an empty string, a placeholder, a bare unit, a negative value or anything
/// that does not start with a number. The result is `f64` because that is what
/// [`aulos_core::progress::RawProgress`] holds; a fractional `12.5MiB` must not lose its fraction
/// before the percent arithmetic runs.
#[must_use]
pub fn parse_bytes(s: &str) -> Option<f64> {
    let t = s.trim().trim_start_matches(['~', '≈', '=']).trim();
    let lower = t.to_ascii_lowercase();
    if lower.is_empty() || PLACEHOLDERS.contains(&lower.as_str()) {
        return None;
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',' || c == '+'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let num = num.replace([',', '+'], "");
    let value: f64 = num.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let unit = unit.trim().to_ascii_lowercase();
    if unit.is_empty() {
        return Some(value);
    }
    // Longest match first, so "kib" is not read as "k" with a trailing "ib".
    let multiplier = UNITS
        .iter()
        .find(|(u, _)| unit == *u || unit.strip_suffix("ytes") == Some(*u))
        .map(|(_, m)| *m)?;
    Some(value * multiplier)
}

/// Parses a transfer rate into bytes per second: `"1.2MiB/s"`, `"900 KB/s"`, `"12 Kbps"`.
///
/// The `/s`, `ps` and `/sec` suffixes are stripped and then [`parse_bytes`] does the work, so the
/// same 1024-based rule applies. A bit-per-second spelling (`Kbps`) is read as bytes, because that
/// is what every tool this parses actually means by it.
#[must_use]
pub fn parse_rate(s: &str) -> Option<f64> {
    let t = s.trim();
    let t = t
        .strip_suffix("/sec")
        .or_else(|| t.strip_suffix("/Sec"))
        .or_else(|| t.strip_suffix("/s"))
        .or_else(|| t.strip_suffix("/S"))
        .or_else(|| t.strip_suffix("ps"))
        .or_else(|| t.strip_suffix("PS"))
        .unwrap_or(t);
    parse_bytes(t.trim())
}

/// Parses a duration into whole seconds: `"03"`, `"01:30"`, `"1:02:03"`, `"1h2m3s"`, `"90s"`,
/// `"2m"`, `"01:02:03.500"`.
///
/// Returns `None` for a placeholder (`"--:--"`, `"N/A"`, `"Unknown"`) and for anything with a
/// non-numeric, non-unit component. Fractions are truncated toward zero, because `eta` is integer
/// seconds on the wire (BRIEF §6).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // durations here are minutes to hours, not centuries
pub fn parse_hms(s: &str) -> Option<i64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    if lower.is_empty()
        || PLACEHOLDERS.contains(&lower.as_str())
        || lower.chars().all(|c| c == '-' || c == ':' || c == '.')
    {
        return None;
    }

    if t.contains(':') {
        let mut total = 0f64;
        let mut parts = 0usize;
        for part in t.split(':') {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let v: f64 = part.parse().ok()?;
            if v < 0.0 {
                return None;
            }
            total = total * 60.0 + v;
            parts += 1;
            if parts > 3 {
                return None;
            }
        }
        return Some(total as i64);
    }

    // `1h2m3s` / `90s` / `2m` / `90`.
    let mut total = 0f64;
    let mut current = String::new();
    let mut saw_unit = false;
    for c in lower.chars() {
        match c {
            '0'..='9' | '.' => current.push(c),
            'h' | 'm' | 's' | 'd' => {
                let v: f64 = current.parse().ok()?;
                current.clear();
                saw_unit = true;
                total += v * match c {
                    'd' => 86_400.0,
                    'h' => 3_600.0,
                    'm' => 60.0,
                    _ => 1.0,
                };
            }
            ' ' => {}
            _ => return None,
        }
    }
    if !current.is_empty() {
        let v: f64 = current.parse().ok()?;
        if saw_unit {
            // A trailing bare number after units, e.g. "1m30", is ambiguous. Read it as seconds.
            total += v;
        } else {
            total = v;
        }
    } else if !saw_unit {
        return None;
    }
    if total < 0.0 {
        return None;
    }
    Some(total as i64)
}

/// Formats a byte count the way [`parse_bytes`] reads it back: 1024-based, one decimal, IEC
/// suffix. `1536 ⇒ "1.5 KiB"`.
///
/// Used by the Telegram renderer and by log lines; never by the wire, which is always integral
/// bytes.
#[must_use]
pub fn format_bytes(bytes: f64) -> String {
    if !bytes.is_finite() || bytes < 0.0 {
        return "0 B".to_owned();
    }
    const SUFFIXES: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes;
    let mut i = 0;
    while value >= 1024.0 && i + 1 < SUFFIXES.len() {
        value /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", value.round())
    } else {
        format!("{value:.1} {}", SUFFIXES[i])
    }
}

/// Formats whole seconds as `HH:MM:SS`, dropping the hours when they are zero: `90 ⇒ "01:30"`.
#[must_use]
pub fn format_hms(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{sec:02}")
    } else {
        format!("{m:02}:{sec:02}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_table_is_1024_based_for_both_spellings() {
        let table: &[(&str, Option<f64>)] = &[
            ("0", Some(0.0)),
            ("1024", Some(1024.0)),
            ("1b", Some(1.0)),
            ("1 B", Some(1.0)),
            ("1K", Some(1024.0)),
            ("1KB", Some(1024.0)),
            ("1KiB", Some(1024.0)),
            ("1 kb", Some(1024.0)),
            ("1MB", Some(1_048_576.0)),
            ("1MiB", Some(1_048_576.0)),
            ("1GB", Some(1_073_741_824.0)),
            ("1GiB", Some(1_073_741_824.0)),
            ("1TiB", Some(1_099_511_627_776.0)),
            ("1PiB", Some(1_125_899_906_842_624.0)),
            ("12.5 MiB", Some(13_107_200.0)),
            ("~3.5GiB", Some(3_758_096_384.0)),
            ("1,024", Some(1024.0)),
            ("1 bytes", Some(1.0)),
            ("1 kbytes", Some(1024.0)),
            // Not usable.
            ("", None),
            ("   ", None),
            ("N/A", None),
            ("n/a", None),
            ("--", None),
            ("Unknown", None),
            ("MiB", None),
            ("-5MiB", None),
            ("1ZiB", None),
            ("abc", None),
        ];
        for (input, want) in table {
            assert_eq!(parse_bytes(input), *want, "parse_bytes({input:?})");
        }
    }

    #[test]
    fn rates_drop_their_per_second_suffix() {
        let table: &[(&str, Option<f64>)] = &[
            ("1.2MiB/s", Some(1_258_291.2)),
            ("900 KB/s", Some(921_600.0)),
            ("900KB/sec", Some(921_600.0)),
            ("12Kbps", Some(12_288.0)),
            ("0/s", Some(0.0)),
            ("N/A/s", None),
            ("--", None),
        ];
        for (input, want) in table {
            let got = parse_rate(input);
            match (got, want) {
                (Some(g), Some(w)) => assert!((g - w).abs() < 1e-6, "parse_rate({input:?}) = {g}"),
                (g, w) => assert_eq!(g, *w, "parse_rate({input:?})"),
            }
        }
    }

    #[test]
    fn durations_parse_in_every_shape_a_tool_prints() {
        let table: &[(&str, Option<i64>)] = &[
            ("0", Some(0)),
            ("03", Some(3)),
            ("90", Some(90)),
            ("01:30", Some(90)),
            ("1:02:03", Some(3723)),
            ("01:02:03", Some(3723)),
            ("01:02:03.500", Some(3723)),
            ("00:00:00", Some(0)),
            ("90s", Some(90)),
            ("2m", Some(120)),
            ("1h2m3s", Some(3723)),
            ("1h", Some(3600)),
            ("1d", Some(86_400)),
            ("1m 30s", Some(90)),
            ("1m30", Some(90)),
            // Not usable.
            ("", None),
            ("--:--", None),
            ("-", None),
            ("N/A", None),
            ("Unknown", None),
            ("::", None),
            ("1:2:3:4", None),
            ("1:xx", None),
            ("soon", None),
        ];
        for (input, want) in table {
            assert_eq!(parse_hms(input), *want, "parse_hms({input:?})");
        }
    }

    #[test]
    fn formatting_round_trips_through_the_parser() {
        assert_eq!(format_bytes(0.0), "0 B");
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1536.0), "1.5 KiB");
        assert_eq!(format_bytes(1_048_576.0), "1.0 MiB");
        assert_eq!(format_bytes(-1.0), "0 B");
        assert_eq!(format_bytes(f64::NAN), "0 B");
        assert_eq!(parse_bytes(&format_bytes(1536.0)), Some(1536.0));

        assert_eq!(format_hms(0), "00:00");
        assert_eq!(format_hms(90), "01:30");
        assert_eq!(format_hms(3723), "01:02:03");
        assert_eq!(format_hms(-5), "00:00");
        assert_eq!(parse_hms(&format_hms(3723)), Some(3723));
    }
}
