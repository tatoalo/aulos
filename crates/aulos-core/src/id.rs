//! Identity and ordering (DESIGN §4.1).
//!
//! Two counters with different meanings — `ord` (the client sort key) and `seq` (the protocol
//! cursor) — come from the same durable, reserve-before-use hi/lo allocator, declared here as the
//! [`HiLoAllocator`] trait. The implementation lives in `aulos-store` (WP-04).

use std::fmt;
use std::str::FromStr;

use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

/// A queue item's immutable identity: a 26-character Crockford base32 ULID.
///
/// `url` is data; this is the only key. It is minted in the API handler before validation
/// completes and never changes for the record's life — including when a single item is promoted
/// to a group after a playlist resolve (DESIGN §4.6, §8.6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(Ulid);

impl ItemId {
    /// Mints a fresh id from the current wall clock and the system RNG.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// Wraps an existing ULID (the importer preserves legacy ordering by minting from a timestamp).
    #[must_use]
    pub const fn from_ulid(u: Ulid) -> Self {
        Self(u)
    }

    /// The underlying ULID.
    #[must_use]
    pub const fn as_ulid(self) -> Ulid {
        self.0
    }
}

impl Default for ItemId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ItemId({})", self.0)
    }
}

impl FromStr for ItemId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s)
            .map(Self)
            .map_err(|_| IdError::NotAUlid(s.into()))
    }
}

/// A group is an `items` row with `kind = "group"`, so it shares the item id space (DESIGN §4.1).
pub type GroupId = ItemId;

/// The item creation order — the client's sort key. `ORDER BY ord ASC, id ASC` everywhere.
pub type Ord0 = i64;

/// A unix timestamp in **milliseconds**. Every time on the wire uses this unit (PROTOCOL §2.3).
pub type UnixMs = i64;

/// A subscription id: a validated string newtype, deliberately **not** a [`Ulid`].
///
/// Imported legacy ids are UUIDv4 strings and must stay stable so any script or bookmark that
/// stored one keeps working; new ids are minted as ULIDs, so the shape is uniform going forward
/// without a second representation or a `legacy_id` column. Pattern: `^[A-Za-z0-9_-]{1,64}$`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubId(Box<str>);

impl SubId {
    /// Mints a fresh subscription id as a ULID.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new().to_string().into_boxed_str())
    }

    /// Validates and wraps an existing id (a legacy UUID, or a ULID we minted earlier).
    ///
    /// # Errors
    /// [`IdError::SubIdShape`] when `s` does not match `^[A-Za-z0-9_-]{1,64}$`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        if Self::is_valid(s) {
            Ok(Self(s.into()))
        } else {
            Err(IdError::SubIdShape(s.into()))
        }
    }

    /// Whether `s` matches the documented pattern. Hand-written so the crate needs no regex here.
    #[must_use]
    pub fn is_valid(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SubId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SubId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for SubId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SubId({})", self.0)
    }
}

impl FromStr for SubId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for SubId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SubId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&raw)
            .map_err(|_| DeError::invalid_value(Unexpected::Str(&raw), &"^[A-Za-z0-9_-]{1,64}$"))
    }
}

/// The frame sequence — the protocol cursor (DESIGN §4.1, PROTOCOL §6).
///
/// It orders *frames*, not rows, so there is no `seq` column: the durable high-water mark lives in
/// `meta.seq_hwm`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Minted once per process start and carried in every `snapshot` and in `healthz`.
///
/// A client whose `since` cursor came from a different `boot_id` is handed a full snapshot, never
/// a delta — which is what makes a restored older database safe (DESIGN §4.1, §15.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BootId(Ulid);

impl BootId {
    /// Mints the id for this process.
    #[must_use]
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// Wraps an existing ULID (tests and the importer).
    #[must_use]
    pub const fn from_ulid(u: Ulid) -> Self {
        Self(u)
    }
}

impl Default for BootId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for BootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BootId({})", self.0)
    }
}

impl FromStr for BootId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s)
            .map(Self)
            .map_err(|_| IdError::NotAUlid(s.into()))
    }
}

/// A durable, reserve-before-use hi/lo counter (DESIGN §4.1).
///
/// [`Self::next`] blocks (~60 µs) once per block to reserve the next range in `meta` **before** any
/// value from it is handed out, so a crash skips up to `block - 1` values and can never re-issue
/// one. Block sizes: `ord` 256, `seq` 1024.
pub trait HiLoAllocator: Send + Sync {
    /// Hands out the next value.
    fn next(&self) -> i64;
    /// The value the next call to [`Self::next`] will *not* return — i.e. the current cursor.
    fn current(&self) -> i64;
}

/// Failures parsing an identifier.
#[derive(Debug, thiserror::Error)]
pub enum IdError {
    /// The string is not a 26-character Crockford base32 ULID.
    #[error("not a ULID: {0:?}")]
    NotAUlid(Box<str>),
    /// The string does not match `^[A-Za-z0-9_-]{{1,64}}$`.
    #[error("invalid subscription id {0:?}: expected ^[A-Za-z0-9_-]{{1,64}}$")]
    SubIdShape(Box<str>),
}

impl IdError {
    /// The wire error code an id failure maps to.
    #[must_use]
    pub const fn code(&self) -> crate::error::ErrorCode {
        crate::error::ErrorCode::ValidationFailed
    }

    /// Id failures are never retryable.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        false
    }
}

// v1.0: not implemented, see BRIEF — `ConnId` and the engine watch registry are CUT, so the
// `u64` connection newtype DESIGN §8.1 declares here has no constructor and no consumer.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn item_id_round_trips_through_string() {
        let id = ItemId::new();
        assert_eq!(id.to_string().len(), 26);
        assert_eq!(id.to_string().parse::<ItemId>().unwrap(), id);
        assert!("nope".parse::<ItemId>().is_err());
    }

    #[test]
    fn item_id_serialises_transparently() {
        let id = ItemId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ItemId>(&json).unwrap(), id);
    }

    #[test]
    fn sub_id_accepts_legacy_uuids_and_ulids() {
        assert!(SubId::parse("9c1f0f38-8f7a-4a7c-9f2f-1f4d1d0f4a11").is_ok());
        assert!(SubId::parse(&Ulid::new().to_string()).is_ok());
        assert_eq!(SubId::new().as_str().len(), 26);
    }

    #[test]
    fn sub_id_rejects_bad_shapes() {
        for bad in ["", "has space", "slash/es", "dots.dots", &"x".repeat(65)] {
            assert!(SubId::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(serde_json::from_str::<SubId>("\"a/b\"").is_err());
        assert_eq!(
            serde_json::from_str::<SubId>("\"abc-123_XY\"")
                .unwrap()
                .as_str(),
            "abc-123_XY"
        );
    }

    #[test]
    fn seq_is_a_transparent_number() {
        assert_eq!(serde_json::to_string(&Seq(42)).unwrap(), "42");
    }
}
