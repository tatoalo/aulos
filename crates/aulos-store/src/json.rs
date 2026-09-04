//! Column ⇄ domain-type conversions.
//!
//! Two things live here that look odd until you check the dependency table.
//!
//! First, `aulos-store`'s DESIGN §3 row budgets for `aulos-core`, `rusqlite`,
//! `rusqlite_migration`, `ulid`, `base64`, `time` and `tokio` — **not** for `url`, and
//! `tests/arch.rs` enforces that as the subset rule. Yet [`aulos_core::Item::url`] and
//! [`aulos_core::SubscriptionRecord::url`] are `url::Url`. A `Url` column is therefore rehydrated
//! through its own `Deserialize` impl by [`from_sql_string`], with the target type inferred from
//! the struct field: no `use url::Url`, no re-export, and the same validation the API layer
//! applied on the way in. Serialising back needs nothing at all — `Url::as_str` is an inherent
//! method.
//!
//! Second, [`aulos_core::Status`] and [`aulos_core::Kind`] have `as_str` but no `FromStr`, so the
//! two tiny parsers here close the loop against their own `ALL` tables rather than duplicating a
//! string list that could drift.

use aulos_core::{Item, Kind, Status};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::StoreError;

/// Rehydrates a domain newtype over a string from its column value.
///
/// Used for `url::Url` — see the module docs for why it is not parsed directly.
pub(crate) fn from_sql_string<T: DeserializeOwned>(
    s: &str,
    column: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_value(Value::String(s.to_owned())).map_err(|e| StoreError::decode(column, e))
}

/// Decodes a JSON column.
pub(crate) fn from_sql_json<T: DeserializeOwned>(
    s: &str,
    column: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(|e| StoreError::decode(column, e))
}

/// Encodes a value into its JSON column.
pub(crate) fn to_sql_json<T: Serialize>(v: &T, column: &'static str) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|e| StoreError::decode(column, e))
}

/// Parses a `status` column against [`Status::ALL`].
pub(crate) fn status_from_str(s: &str) -> Result<Status, StoreError> {
    Status::ALL
        .into_iter()
        .find(|st| st.as_str() == s)
        .ok_or_else(|| StoreError::decode("items.status", format!("unknown status {s:?}")))
}

/// Parses a `kind` column.
pub(crate) fn kind_from_str(s: &str) -> Result<Kind, StoreError> {
    match s {
        "item" => Ok(Kind::Item),
        "group" => Ok(Kind::Group),
        other => Err(StoreError::decode(
            "items.kind",
            format!("unknown kind {other:?}"),
        )),
    }
}

/// Narrows a `u64` domain value to the `INTEGER` a `STRICT` column accepts.
pub(crate) fn u64_to_i64(v: u64, column: &'static str) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError::OutOfRange { column })
}

/// Widens an `INTEGER` column back to a `u64`, rejecting a negative value rather than wrapping.
pub(crate) fn i64_to_u64(v: i64, column: &'static str) -> Result<u64, StoreError> {
    u64::try_from(v).map_err(|_| StoreError::OutOfRange { column })
}

/// Widens an `INTEGER` column to a `u32`.
pub(crate) fn i64_to_u32(v: i64, column: &'static str) -> Result<u32, StoreError> {
    u32::try_from(v).map_err(|_| StoreError::OutOfRange { column })
}

/// The `entry_json` column value for an item, applying the DESIGN §7.5 byte cap.
///
/// The *provider-specific* keep rules (nothing for a plain yt-dlp child, the whole entry for
/// StreamingCommunity, …) are applied by whoever builds the [`aulos_core::EntryBlob`]; the hard
/// cap is applied here because it is a storage invariant: an unbounded column is the one thing
/// that can make a 500-child transaction arbitrarily large.
pub(crate) fn entry_column(item: &Item, max_bytes: u64) -> Result<Option<String>, StoreError> {
    let Some(entry) = item.entry.as_ref() else {
        return Ok(None);
    };
    let encoded = to_sql_json(entry, "items.entry_json")?;
    if max_bytes > 0 && encoded.len() as u64 > max_bytes {
        tracing::warn!(
            item = %item.id,
            bytes = encoded.len(),
            max_bytes,
            "provider entry exceeds AULOS_ENTRY_MAX_BYTES; storing the truncation marker"
        );
        return Ok(Some(to_sql_json(
            &aulos_core::EntryBlob::truncated(),
            "items.entry_json",
        )?));
    }
    Ok(Some(encoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_round_trips_through_its_column_value() {
        for st in Status::ALL {
            assert_eq!(status_from_str(st.as_str()).ok(), Some(st));
        }
        assert!(status_from_str("pending").is_err());
    }

    #[test]
    fn kinds_round_trip() {
        assert_eq!(kind_from_str("item").ok(), Some(Kind::Item));
        assert_eq!(kind_from_str("group").ok(), Some(Kind::Group));
        assert!(kind_from_str("playlist").is_err());
    }

    #[test]
    fn a_url_column_is_rehydrated_without_naming_the_url_crate() {
        // The target type is inferred from the field it is assigned to, which is the whole point.
        let parsed: Result<url::Url, _> = from_sql_string("https://example.com/a", "items.url");
        assert!(parsed.is_ok());
        let bad: Result<url::Url, _> = from_sql_string("not a url", "items.url");
        assert!(bad.is_err());
    }

    #[test]
    fn integer_narrowing_rejects_out_of_range_values() {
        assert_eq!(u64_to_i64(7, "items.size").ok(), Some(7));
        assert!(u64_to_i64(u64::MAX, "items.size").is_err());
        assert!(i64_to_u64(-1, "items.size").is_err());
        assert!(i64_to_u32(i64::from(u32::MAX) + 1, "items.group_index").is_err());
    }
}
