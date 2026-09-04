//! The two wire projections and the two WebSocket envelopes (DESIGN §14.1, PROTOCOL §5.9, §9).
//!
//! [`aulos_core::subscription::SubscriptionView`] is the v2 object: the legacy `to_public_dict()`
//! thirteen keys plus `next_due`, `consecutive_failures` and `checking`. This module adds the two
//! things that are *not* the view:
//!
//! - [`to_v1_dict`], which emits **exactly** the legacy thirteen and divides `last_checked` by
//!   1000 into a float, matching legacy's `time.time()`;
//! - [`v2_frame`] / [`v2_removed_frame`], the `subscription` and `subscription_removed` envelopes.
//!
//! The envelopes live here because this crate is the only producer of `SubscriptionChanged` and
//! `SubscriptionRemoved`, so this is where "the array is an array even for one deletion" can be
//! pinned by a test. `aulos-api` (WP-14) serialises the frames it sends; the snapshot tests below
//! are the normative shapes it must match.

use aulos_core::id::SubId;
use aulos_core::subscription::{SubError, SubscriptionView};
use serde_json::{Map, Value, json};

/// The v1 shim's projection: exactly [`SubscriptionView::V1_KEYS`], in legacy order, with
/// `last_checked` as **fractional seconds** rather than milliseconds.
///
/// Legacy stored `time.time()` and emitted it untouched, so an iOS build that formats it as a
/// Unix timestamp keeps working. `null` stays `null`.
#[must_use]
pub fn to_v1_dict(view: &SubscriptionView) -> Value {
    let mut out = Map::with_capacity(SubscriptionView::V1_KEYS.len());
    out.insert("id".to_owned(), json!(view.id.as_str()));
    out.insert("name".to_owned(), json!(&*view.name));
    out.insert("url".to_owned(), json!(&*view.url));
    out.insert("enabled".to_owned(), json!(view.enabled));
    out.insert(
        "check_interval_minutes".to_owned(),
        json!(view.check_interval_minutes),
    );
    out.insert("download_type".to_owned(), json!(view.download_type));
    out.insert("codec".to_owned(), json!(view.codec));
    out.insert("format".to_owned(), json!(&*view.format));
    out.insert("quality".to_owned(), json!(&*view.quality));
    out.insert("folder".to_owned(), json!(&*view.folder));
    out.insert(
        "last_checked".to_owned(),
        view.last_checked
            .map_or(Value::Null, |ms| json!(ms as f64 / 1_000.0)),
    );
    out.insert("seen_count".to_owned(), json!(view.seen_count));
    out.insert(
        "error".to_owned(),
        view.error
            .as_ref()
            .map_or(Value::Null, |e| Value::String(e.to_string())),
    );
    debug_assert_eq!(out.len(), SubscriptionView::V1_KEYS.len());
    Value::Object(out)
}

/// The `subscription` frame (PROTOCOL §5.9): `{"t":"subscription","seq":…,"subscription":{…}}`.
#[must_use]
pub fn v2_frame(seq: u64, view: &SubscriptionView) -> Value {
    json!({ "t": "subscription", "seq": seq, "subscription": view })
}

/// The `subscription_removed` frame (PROTOCOL §5.9): `{"t":"subscription_removed","seq":…,
/// "ids":[…]}`.
///
/// `ids` is an **array**, even for a single deletion, where legacy emitted a bare id string.
#[must_use]
pub fn v2_removed_frame(seq: u64, ids: &[SubId]) -> Value {
    let ids: Vec<&str> = ids.iter().map(SubId::as_str).collect();
    json!({ "t": "subscription_removed", "seq": seq, "ids": ids })
}

/// The legacy `_coerce_bool` port, for `POST <p>subscriptions/update`'s `enabled` field
/// (DESIGN §14.3 step 10).
///
/// Legacy accepted JSON booleans plus the string forms `true|1|on` / `false|0|off`, case- and
/// whitespace-insensitive, and raised `ValueError("enabled must be a boolean")` on anything else —
/// which FastAPI turned into a leaked 500. Here that becomes a
/// [`SubError::Invalid`] on field `enabled`, i.e. a `400 validation_failed`, with the legacy
/// message preserved byte-for-byte.
///
/// # Errors
/// [`SubError::Invalid`] when `value` is neither a boolean nor one of the six accepted strings.
pub fn parse_enabled(value: &Value) -> Result<bool, SubError> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Ok(true),
            "false" | "0" | "off" => Ok(false),
            _ => Err(bad_enabled()),
        },
        _ => Err(bad_enabled()),
    }
}

/// The legacy message, verbatim.
pub const ENABLED_NOT_A_BOOLEAN: &str = "enabled must be a boolean";

fn bad_enabled() -> SubError {
    SubError::Invalid {
        field: "enabled".into(),
        message: ENABLED_NOT_A_BOOLEAN.into(),
    }
}

#[cfg(test)]
mod tests {
    use aulos_core::error::ErrorCode;
    use aulos_core::paths::RelDir;
    use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
    use aulos_core::subscription::SubscriptionRecord;
    use url::Url;

    use super::*;

    fn view(checking: bool) -> SubscriptionView {
        let mut r = SubscriptionRecord::new(
            SubId::parse("9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f").unwrap(),
            "Veritasium",
            Url::parse("https://www.youtube.com/@veritasium").unwrap(),
            Selection::new(
                DownloadType::Video,
                Codec::Auto,
                FormatId::parse("any").unwrap(),
                QualityId::parse("best").unwrap(),
            ),
        );
        r.last_checked = Some(1_757_000_100_500);
        r.next_due = Some(1_757_003_700_000);
        r.seen_count = 314;
        r.folder = Some(RelDir::parse("Science").unwrap());
        r.to_view(checking)
    }

    #[test]
    fn the_v2_projection_is_sixteen_keys() {
        let v = serde_json::to_value(view(false)).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 16);
        assert_eq!(obj["last_checked"], 1_757_000_100_500_i64, "milliseconds");
        assert_eq!(obj["next_due"], 1_757_003_700_000_i64);
        assert_eq!(obj["consecutive_failures"], 0);
        assert_eq!(obj["checking"], false);
    }

    #[test]
    fn the_v1_projection_is_exactly_the_legacy_thirteen() {
        let v = to_v1_dict(&view(true));
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 13);
        for k in SubscriptionView::V1_KEYS {
            assert!(obj.contains_key(k), "{k} missing");
        }
        for k in SubscriptionView::V2_ADDITIONAL_KEYS {
            assert!(!obj.contains_key(k), "{k} must not leak into v1");
        }
    }

    /// Legacy stored `time.time()`, a float of seconds. The v2 object is milliseconds; the shim
    /// divides. A `.5` on the millisecond keeps the fraction, which is what proves it is a float.
    #[test]
    fn v1_last_checked_is_float_seconds() {
        let v = to_v1_dict(&view(false));
        assert_eq!(v["last_checked"], json!(1_757_000_100.5));
        assert!(v["last_checked"].is_f64());

        let mut view = view(false);
        view.last_checked = None;
        assert_eq!(to_v1_dict(&view)["last_checked"], Value::Null);
    }

    #[test]
    fn v1_keeps_the_legacy_empty_string_folder_and_null_error() {
        let mut v = view(false);
        v.folder = "".into();
        v.error = Some("boom".into());
        let d = to_v1_dict(&v);
        assert_eq!(d["folder"], "");
        assert_eq!(d["error"], "boom");
    }

    #[test]
    fn the_subscription_frame_is_the_protocol_envelope() {
        let frame = v2_frame(4_291, &view(false));
        assert_eq!(frame["t"], "subscription");
        assert_eq!(frame["seq"], 4_291);
        let sub = frame["subscription"].as_object().unwrap();
        assert_eq!(sub.len(), 16);
        assert_eq!(sub["id"], "9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f");
        let mut keys: Vec<&str> = frame
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["seq", "subscription", "t"], "and nothing else");
    }

    /// PROTOCOL §5.9: an **array**, even for one deletion, where legacy emitted a bare id string.
    #[test]
    fn the_removed_frame_carries_an_array_even_for_one_id() {
        let one = SubId::parse("9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f").unwrap();
        let frame = v2_removed_frame(7, std::slice::from_ref(&one));
        assert_eq!(frame["t"], "subscription_removed");
        assert_eq!(frame["seq"], 7);
        assert!(frame["ids"].is_array(), "never a bare string");
        assert_eq!(
            frame["ids"],
            json!(["9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f"])
        );

        let two = v2_removed_frame(8, &[one, SubId::parse("01JC").unwrap()]);
        assert_eq!(
            two["ids"],
            json!(["9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f", "01JC"])
        );
        assert_eq!(v2_removed_frame(9, &[])["ids"], json!([]));
    }

    #[test]
    fn enabled_accepts_the_legacy_token_set() {
        for (raw, want) in [
            (json!(true), true),
            (json!(false), false),
            (json!("true"), true),
            (json!("TRUE"), true),
            (json!(" on "), true),
            (json!("1"), true),
            (json!("false"), false),
            (json!("OFF"), false),
            (json!("0"), false),
        ] {
            assert_eq!(parse_enabled(&raw).unwrap(), want, "{raw}");
        }
    }

    /// DESIGN §14.3 step 10: a bad `enabled` is a `400 validation_failed`, not a leaked 500.
    #[test]
    fn a_bad_enabled_is_a_field_level_validation_failure() {
        for raw in [
            json!("yes"),
            json!(1),
            json!(null),
            json!([true]),
            json!({}),
        ] {
            let err = parse_enabled(&raw).unwrap_err();
            assert_eq!(err.code(), ErrorCode::ValidationFailed, "{raw}");
            assert_eq!(err.to_string(), "enabled must be a boolean");
            assert_eq!(err.to_wire().field.as_deref(), Some("enabled"));
        }
    }
}
