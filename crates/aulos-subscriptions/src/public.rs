//! V2 subscription WebSocket envelopes.

use aulos_core::id::SubId;
use aulos_core::subscription::SubscriptionView;
use serde_json::{Value, json};

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

#[cfg(test)]
mod tests {
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
}
