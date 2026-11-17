//! Download history and active queue matches for resolved subscription releases.

use aulos_core::{ItemId, UnixMs};
use rusqlite::{Connection, OptionalExtension, params};

use crate::StoreError;

/// Both identities of a resolved release, before and after queue resolution.
#[derive(Clone, Debug)]
pub struct ReleaseIdentity {
    /// Provider and media ID, using the shared canonical-key function.
    pub canonical_key: Box<str>,
    /// Provider and normalized URL, used while an item is still resolving.
    pub url_key: Box<str>,
}

/// Successful download history is separate from active queue state.
#[derive(Clone, Debug, Default)]
pub struct ReleaseState {
    /// Last successful download, even if its item or file has since been deleted.
    pub downloaded_at: Option<UnixMs>,
    /// An existing active download of the same media and download type.
    pub queued_item_id: Option<ItemId>,
}

pub(crate) fn lookup(
    conn: &Connection,
    identities: &[ReleaseIdentity],
    download_type: &str,
) -> Result<Vec<ReleaseState>, StoreError> {
    let mut history = conn.prepare_cached(
        "SELECT MAX(downloaded_at) FROM download_history \
         WHERE canonical_key IN (?1, ?2) AND download_type = ?3",
    )?;
    let mut active = conn.prepare_cached(
        "SELECT id FROM items WHERE canonical_key IN (?1, ?2) AND kind = 'item' \
         AND status NOT IN ('finished', 'error', 'canceled') \
         AND json_extract(request_json, '$.selection.download_type') = ?3 ORDER BY ord LIMIT 1",
    )?;
    identities
        .iter()
        .map(|key| {
            let args = params![&*key.canonical_key, &*key.url_key, download_type];
            let downloaded_at = history.query_row(args, |r| r.get(0))?;
            let id: Option<String> = active.query_row(args, |r| r.get(0)).optional()?;
            let queued_item_id = id
                .map(|id| id.parse().map_err(|e| StoreError::decode("items.id", e)))
                .transpose()?;
            Ok(ReleaseState {
                downloaded_at,
                queued_item_id,
            })
        })
        .collect()
}
