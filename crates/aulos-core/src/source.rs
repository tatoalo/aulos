//! Attribution — flat, two always-present fields (DESIGN §4.4).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Who created (or re-created) an item.
///
/// Deliberately a flat closed enum rather than an internally-tagged one with per-variant payloads:
/// two keys, both always present, no hand-written Swift `Decodable`, no nested switch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// `POST api/v2/downloads`.
    ApiV2,
    /// The v1 compatibility shim's `POST <p>add`.
    ApiV1,
    /// The Telegram bot. `ref` is the chat id.
    Telegram,
    /// A subscription check. `ref` is the subscription id.
    Subscription,
    /// Boot recovery re-queued an interrupted item (DESIGN §8.9).
    Restart,
    /// A retry, manual or automatic (DESIGN §8.8).
    Retry,
}

impl SourceKind {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiV2 => "api_v2",
            Self::ApiV1 => "api_v1",
            Self::Telegram => "telegram",
            Self::Subscription => "subscription",
            Self::Restart => "restart",
            Self::Retry => "retry",
        }
    }

    /// Every value, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::ApiV2,
        Self::ApiV1,
        Self::Telegram,
        Self::Subscription,
        Self::Restart,
        Self::Retry,
    ];
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `Item.source` on the wire: `{ kind, ref }`, both keys always present.
///
/// The subscription *name* and the Telegram *message id* are deliberately not here — they are in
/// the database row and in the logs, which is where they are actually used.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SourceRef {
    /// Which surface created the item.
    pub kind: SourceKind,
    /// The chat id, subscription id or request id — always *the key*, may be `null`.
    #[serde(rename = "ref")]
    pub reference: Option<Arc<str>>,
}

impl SourceRef {
    /// A source with no reference.
    #[must_use]
    pub const fn bare(kind: SourceKind) -> Self {
        Self {
            kind,
            reference: None,
        }
    }

    /// A source with a reference.
    #[must_use]
    pub fn with_ref(kind: SourceKind, reference: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            reference: Some(reference.into()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn source_ref_serialises_two_keys_with_ref_spelled_ref() {
        let s = SourceRef::with_ref(SourceKind::Telegram, "12345");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "telegram", "ref": "12345" }));
        assert_eq!(serde_json::from_value::<SourceRef>(v).unwrap(), s);
    }

    #[test]
    fn a_bare_source_still_carries_a_null_ref() {
        let v = serde_json::to_value(SourceRef::bare(SourceKind::ApiV2)).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "api_v2", "ref": null }));
    }

    #[test]
    fn every_kind_serialises_to_its_documented_string() {
        for k in SourceKind::ALL {
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{}\"", k.as_str())
            );
        }
        assert_eq!(
            SourceKind::ALL.map(SourceKind::as_str),
            [
                "api_v2",
                "api_v1",
                "telegram",
                "subscription",
                "restart",
                "retry"
            ]
        );
    }
}
