//! Source publication dates, normalized without inventing a time of day.

use serde_json::{Map, Value};
use time::{Date, Month, OffsetDateTime};

pub(crate) fn published_date(entry: &Map<String, Value>) -> Option<Box<str>> {
    date_field(entry, "release_date")
        .or_else(|| timestamp_field(entry, "release_timestamp"))
        .or_else(|| date_field(entry, "upload_date"))
        .or_else(|| timestamp_field(entry, "timestamp"))
        .map(|date| {
            format!(
                "{:04}-{:02}-{:02}",
                date.year(),
                u8::from(date.month()),
                date.day()
            )
            .into()
        })
}

fn date_field(entry: &Map<String, Value>, key: &str) -> Option<Date> {
    let raw = entry.get(key)?.as_str()?;
    if raw.len() != 8 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = raw[..4].parse().ok()?;
    let month = Month::try_from(raw[4..6].parse::<u8>().ok()?).ok()?;
    let day = raw[6..].parse().ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

fn timestamp_field(entry: &Map<String, Value>, key: &str) -> Option<Date> {
    let seconds = entry.get(key)?.as_i64().filter(|seconds| *seconds > 0)?;
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .map(OffsetDateTime::date)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publication_dates_preserve_calendar_days_and_prefer_release_over_upload() {
        let entry = json!({"release_date": "20261117", "upload_date": "20261101"});
        assert_eq!(
            published_date(entry.as_object().unwrap()).as_deref(),
            Some("2026-11-17")
        );
        let entry = json!({"upload_date": "20240229"});
        assert_eq!(
            published_date(entry.as_object().unwrap()).as_deref(),
            Some("2024-02-29")
        );
    }

    #[test]
    fn timestamps_use_utc_and_invalid_dates_are_omitted() {
        let entry = json!({"timestamp": 1_794_873_600});
        assert_eq!(
            published_date(entry.as_object().unwrap()).as_deref(),
            Some("2026-11-17")
        );
        for entry in [
            json!({}),
            json!({"upload_date": "20260230"}),
            json!({"upload_date": "🌞202611"}),
            json!({"timestamp": -1}),
        ] {
            assert!(published_date(entry.as_object().unwrap()).is_none());
        }
    }
}
