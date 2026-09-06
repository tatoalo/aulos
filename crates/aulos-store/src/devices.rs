//! The `devices` and `live_activities` tables, and the [`DeviceStore`] port on [`Store`]
//! (DESIGN §25, PROTOCOL §4.8).
//!
//! This is the whole of `aulos_core::ports::DeviceStore` on the store handle: `aulos-api` writes
//! registrations through it and the APNs notifier reads and prunes through it, so neither crate
//! ever names `rusqlite` (DESIGN §3 rule A2).
//!
//! Unlike [`crate::items`], **nothing here is engine-mediated**. A device registration is not
//! queue state: it changes no [`aulos_core::Item`], produces no `delta`, and no client renders it.
//! Writing it straight through [`Store::write`] is therefore correct rather than a shortcut — a
//! detour through the engine would only add a hop and a way for the registration to be lost while
//! the engine is busy.
//!
//! Removals are idempotent by construction: every one of them is a `DELETE … WHERE`, which affects
//! zero rows and reports success when the row has already gone. That is the port's documented
//! contract, and it matters because APNs tells the notifier about dead tokens the app may already
//! have deleted through the REST route.

use aulos_core::{
    ApnsEnvironment, DeviceRecord, DeviceStore, ItemId, LiveActivityRecord, PortError, UnixMs,
};
use rusqlite::{Connection, Row, params};

use crate::Store;
use crate::error::StoreError;
use crate::ops::{Durability, WriteOp};

/// The column list every `devices` read shares, in the order [`row_to_device`] expects.
const DEVICE_COLUMNS: &str = "token, platform, bundle_id, environment, alerts, \
     live_activity_start_token, app_version, registered_at, last_seen_at";

/// The column list every `live_activities` read shares.
const ACTIVITY_COLUMNS: &str = "device_token, item_id, update_token, environment, registered_at";

/// Parses an `environment` column against [`ApnsEnvironment`]'s own wire spellings.
///
/// The `CHECK` constraint makes anything else impossible through this crate, so a failure here is
/// schema drift or a hand-edited row — [`StoreError::Decode`], not a silent default.
fn environment_from_str(s: &str, column: &'static str) -> Result<ApnsEnvironment, StoreError> {
    match s {
        "sandbox" => Ok(ApnsEnvironment::Sandbox),
        "production" => Ok(ApnsEnvironment::Production),
        other => Err(StoreError::decode(
            column,
            format!("unknown APNs environment {other:?}"),
        )),
    }
}

/// Decodes one `devices` row selected with [`DEVICE_COLUMNS`].
fn row_to_device(row: &Row<'_>) -> Result<DeviceRecord, StoreError> {
    let environment: String = row.get(3)?;
    Ok(DeviceRecord {
        token: row.get::<_, String>(0)?.into_boxed_str(),
        platform: row.get::<_, String>(1)?.into_boxed_str(),
        bundle_id: row.get::<_, String>(2)?.into_boxed_str(),
        environment: environment_from_str(&environment, "devices.environment")?,
        alerts: row.get::<_, i64>(4)? != 0,
        live_activity_start_token: row.get::<_, Option<String>>(5)?.map(String::into_boxed_str),
        app_version: row.get::<_, Option<String>>(6)?.map(String::into_boxed_str),
        registered_at: row.get(7)?,
        last_seen_at: row.get(8)?,
    })
}

/// Decodes one `live_activities` row selected with [`ACTIVITY_COLUMNS`].
fn row_to_activity(row: &Row<'_>) -> Result<LiveActivityRecord, StoreError> {
    let item_raw: String = row.get(1)?;
    let environment: String = row.get(3)?;
    Ok(LiveActivityRecord {
        device_token: row.get::<_, String>(0)?.into_boxed_str(),
        item_id: item_raw
            .parse()
            .map_err(|e| StoreError::decode("live_activities.item_id", e))?,
        update_token: row.get::<_, String>(2)?.into_boxed_str(),
        environment: environment_from_str(&environment, "live_activities.environment")?,
        registered_at: row.get(4)?,
    })
}

/// Deletes every Live Activity row for `item` **and for its children**.
///
/// The child sweep is what a foreign key on `item_id` would have done and cannot: deleting a group
/// cascades to its children through `items.group_id`, so by the time the `items` `DELETE` has run
/// the children's ids are unrecoverable. This is therefore called *before* the item rows go, from
/// [`crate::items::apply`]'s [`WriteOp::DeleteItems`] arm.
pub(crate) fn remove_for_item(conn: &Connection, item: ItemId) -> Result<(), StoreError> {
    conn.prepare_cached(
        "DELETE FROM live_activities WHERE item_id = ?1 \
         OR item_id IN (SELECT id FROM items WHERE group_id = ?1)",
    )?
    .execute([item.to_string()])?;
    Ok(())
}

/// Applies one device-shaped op. `Ok(false)` when the op is not one.
pub(crate) fn apply(conn: &Connection, op: &WriteOp, _now: UnixMs) -> Result<bool, StoreError> {
    match op {
        WriteOp::UpsertDevice(device) => {
            // Every column but `registered_at` is refreshed: a repeat `PUT` is the app telling the
            // server "this token is still mine, and here is what has changed about it" — a new
            // Live Activity start token after an iOS restart, a new build's `app_version`, the
            // alerts switch flipped in Settings. `registered_at` keeps saying when the token was
            // first seen, which is the only thing a repeat cannot know.
            conn.prepare_cached(
                "INSERT INTO devices (token, platform, bundle_id, environment, alerts, \
                 live_activity_start_token, app_version, registered_at, last_seen_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(token) DO UPDATE SET \
                   platform                  = excluded.platform, \
                   bundle_id                 = excluded.bundle_id, \
                   environment               = excluded.environment, \
                   alerts                    = excluded.alerts, \
                   live_activity_start_token = excluded.live_activity_start_token, \
                   app_version               = excluded.app_version, \
                   last_seen_at              = excluded.last_seen_at",
            )?
            .execute(params![
                &*device.token,
                &*device.platform,
                &*device.bundle_id,
                device.environment.as_str(),
                i64::from(device.alerts),
                device.live_activity_start_token.as_deref(),
                device.app_version.as_deref(),
                device.registered_at,
                device.last_seen_at,
            ])?;
            Ok(true)
        }
        WriteOp::RemoveDevice { token } => {
            // `live_activities` goes with it through `ON DELETE CASCADE`.
            conn.prepare_cached("DELETE FROM devices WHERE token = ?1")?
                .execute([&**token])?;
            Ok(true)
        }
        WriteOp::UpsertLiveActivity(activity) => {
            // `registered_at` *is* refreshed here, unlike a device's: a Live Activity update token
            // rotates and the app re-forwards it, and the column documents the last forwarding.
            conn.prepare_cached(
                "INSERT INTO live_activities \
                   (device_token, item_id, update_token, environment, registered_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(device_token, item_id) DO UPDATE SET \
                   update_token  = excluded.update_token, \
                   environment   = excluded.environment, \
                   registered_at = excluded.registered_at",
            )?
            .execute(params![
                &*activity.device_token,
                activity.item_id.to_string(),
                &*activity.update_token,
                activity.environment.as_str(),
                activity.registered_at,
            ])?;
            Ok(true)
        }
        WriteOp::RemoveLiveActivity { device_token, item } => {
            conn.prepare_cached(
                "DELETE FROM live_activities WHERE device_token = ?1 AND item_id = ?2",
            )?
            .execute(params![&**device_token, item.to_string()])?;
            Ok(true)
        }
        WriteOp::RemoveLiveActivitiesFor { item } => {
            remove_for_item(conn, *item)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// [`DeviceStore::devices`] — every registration, oldest first.
pub(crate) fn all(conn: &Connection) -> Result<Vec<DeviceRecord>, StoreError> {
    let sql = format!("SELECT {DEVICE_COLUMNS} FROM devices ORDER BY registered_at, token");
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_device(row)?);
    }
    Ok(out)
}

/// [`DeviceStore::live_activities_for`] — every activity tracking one item.
pub(crate) fn activities_for(
    conn: &Connection,
    item: ItemId,
) -> Result<Vec<LiveActivityRecord>, StoreError> {
    let sql = format!(
        "SELECT {ACTIVITY_COLUMNS} FROM live_activities WHERE item_id = ?1 ORDER BY device_token"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query([item.to_string()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_activity(row)?);
    }
    Ok(out)
}

/// A store failure, as the port spells it.
///
/// Everything collapses to [`PortError::Store`]: [`PortError::NotFound`] names an *item*, and none
/// of these calls can fail for want of one — a removal is idempotent and an upsert names a device.
fn port(e: &StoreError) -> PortError {
    PortError::Store(e.to_string().into_boxed_str())
}

#[async_trait::async_trait]
impl DeviceStore for Store {
    async fn upsert_device(&self, device: DeviceRecord) -> Result<(), PortError> {
        self.write(
            vec![WriteOp::UpsertDevice(Box::new(device))],
            Durability::Batched,
        )
        .await
        .map_err(|e| port(&e))
    }

    async fn remove_device(&self, token: &str) -> Result<(), PortError> {
        self.write(
            vec![WriteOp::RemoveDevice {
                token: token.into(),
            }],
            Durability::Batched,
        )
        .await
        .map_err(|e| port(&e))
    }

    async fn devices(&self) -> Result<Vec<DeviceRecord>, PortError> {
        self.read(all).await.map_err(|e| port(&e))
    }

    async fn upsert_live_activity(&self, activity: LiveActivityRecord) -> Result<(), PortError> {
        self.write(
            vec![WriteOp::UpsertLiveActivity(Box::new(activity))],
            Durability::Batched,
        )
        .await
        .map_err(|e| port(&e))
    }

    async fn remove_live_activity(
        &self,
        device_token: &str,
        item: ItemId,
    ) -> Result<(), PortError> {
        self.write(
            vec![WriteOp::RemoveLiveActivity {
                device_token: device_token.into(),
                item,
            }],
            Durability::Batched,
        )
        .await
        .map_err(|e| port(&e))
    }

    async fn live_activities_for(
        &self,
        item: ItemId,
    ) -> Result<Vec<LiveActivityRecord>, PortError> {
        self.read(move |c| activities_for(c, item))
            .await
            .map_err(|e| port(&e))
    }

    async fn remove_live_activities_for(&self, item: ItemId) -> Result<(), PortError> {
        self.write(
            vec![WriteOp::RemoveLiveActivitiesFor { item }],
            Durability::Batched,
        )
        .await
        .map_err(|e| port(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environments_round_trip_through_their_column_value() {
        for env in [ApnsEnvironment::Sandbox, ApnsEnvironment::Production] {
            assert_eq!(
                environment_from_str(env.as_str(), "devices.environment").ok(),
                Some(env)
            );
        }
        assert!(environment_from_str("staging", "devices.environment").is_err());
    }
}
