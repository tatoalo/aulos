//! The one read the v1 compatibility shim needs and no other surface does (DESIGN §11.4).
//!
//! `GET <p>history` projects the legacy `id` field as "the provider's own media id when there is
//! one, else the ULID", with the legacy `"<prefix>.<id>"` dotting reproduced. For the `done[]`
//! array that is free — [`crate::Store::v1_done`] returns whole [`aulos_core::Item`] rows, which
//! carry `media_id`. For `queue[]` and `pending[]` it is not: those come from the aggregator's
//! published snapshot, because that is the **only** place transient progress (`percent`, `speed`,
//! `eta`) exists, and [`aulos_core::ItemView`] deliberately carries no `media_id` — v2 has exactly
//! one identifier (PROTOCOL §0 rule 3).
//!
//! So the shim needs a side lookup for the live set, and it must not be a second full item read:
//! the whole point of DESIGN §11.4 sourcing `queue`/`pending` from memory is that `/history` costs
//! one query, not two page walks. [`live_media_ids`] is that lookup — two columns, no parameters,
//! served off the `(status, ord)` index, over a set that is bounded by the queue's working size.
//!
//! Added additively for WP-15 (see `docs/INTEGRATION-NOTES.md`): a new module and one `pub mod`
//! line, no existing signature touched.

use std::collections::HashMap;
use std::str::FromStr;

use aulos_core::ItemId;
use rusqlite::Connection;

use crate::Store;
use crate::error::StoreError;

/// The `media_id` of every **non-terminal** item that has one, keyed by [`ItemId`].
///
/// Rows with a `NULL` `media_id` — anything that has not resolved yet — are simply absent, so the
/// caller's `map.get(id)` is already the "when present" half of DESIGN §11.4's rule.
///
/// # Errors
/// [`StoreError`] on a decode failure or an unreachable read pool.
pub async fn live_media_ids(store: &Store) -> Result<HashMap<ItemId, Box<str>>, StoreError> {
    store.read(read_live_media_ids).await
}

/// The query, split out so it is a plain `fn` the read pool can take.
fn read_live_media_ids(conn: &Connection) -> Result<HashMap<ItemId, Box<str>>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, media_id FROM items \
         WHERE media_id IS NOT NULL AND status NOT IN ('finished','error','canceled')",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = HashMap::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        let id = ItemId::from_str(&raw).map_err(|e| StoreError::decode("items.id", e))?;
        let media_id: String = row.get(1)?;
        out.insert(id, media_id.into_boxed_str());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use aulos_core::{Item, Status};

    /// The statuses excluded here must be exactly [`Status::is_terminal`], so this lookup and
    /// [`crate::Store::v1_done`] partition the table between them with no row in both and none in
    /// neither.
    #[test]
    fn the_excluded_statuses_are_the_terminal_ones() {
        let excluded = ["finished", "error", "canceled"];
        for status in Status::ALL {
            assert_eq!(
                excluded.contains(&status.as_str()),
                status.is_terminal(),
                "{status} is on the wrong side of the partition"
            );
        }
    }

    /// A compile-time reminder that the column this reads still exists on the row type.
    #[test]
    fn the_row_type_still_carries_a_media_id() {
        fn takes(item: &Item) -> Option<&str> {
            item.media_id.as_deref()
        }
        let _ = takes as fn(&Item) -> Option<&str>;
    }
}
