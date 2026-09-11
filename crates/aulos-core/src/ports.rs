//! Cross-crate seams: the [`HookStore`] and [`DeviceStore`] ports, [`HookPhase`] and
//! [`FieldUpdate`].
//!
//! These types are declared here because they are the *vocabulary* two crates share without
//! either depending on the other:
//!
//! - `aulos-hooks` reaches item state through [`HookStore`] and nothing else, so it needs neither
//!   `aulos-store` nor `aulos-queue` (DESIGN §3 rule A1, §13). The implementation is
//!   `aulos-queue::EngineHookStore`, **not** `Store`: the read is delegated to the store's read
//!   pool, but both writes become `EngineCmd`s so the engine's item cache and the aggregator's
//!   `last_sent` observe them and the change reaches clients as a `delta` (DESIGN §13.3).
//! - [`HookPhase`] is read by the engine and declared by hooks.
//! - [`FieldUpdate`] is the one three-state patch convention every nullable `WriteOp` column uses
//!   (DESIGN §7.1); it is named in signatures by `aulos-store`, `aulos-queue` and their tests.
//! - [`DeviceStore`] is how the APNs notifier (`aulos-apns`, DESIGN §25) reads and prunes device
//!   registrations without depending on `aulos-store`; `aulos-store` implements it and `aulos-api`
//!   writes registrations through it.

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::id::{ItemId, UnixMs};
use crate::item::EntryBlob;

/// When a hook runs relative to the terminal status write (DESIGN §13).
///
/// `PreTerminal` hooks run while the item is still `postprocessing`, **before** the engine writes
/// the terminal status — that is what lets the `best_remux` audio re-encode keep the legacy
/// behaviour of running inside the download without needing a `Finished → Postprocessing` edge
/// that DESIGN §4.2 does not have. `PostTerminal` hooks run after it, which is what every notifier
/// wants.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    /// Runs before the terminal status is written; delays it, and can never change it.
    PreTerminal,
    /// Runs after the terminal status is written.
    #[default]
    PostTerminal,
}

impl HookPhase {
    /// The stable name used in `healthz` (`components.audio_sync.phase`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreTerminal => "pre_terminal",
            Self::PostTerminal => "post_terminal",
        }
    }
}

impl std::fmt::Display for HookPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three-state patch for a nullable column (DESIGN §7.1).
///
/// There is exactly one convention in the whole `WriteOp` enum and this is it: [`Self::Keep`]
/// leaves the column alone, [`Self::Clear`] writes SQL `NULL`, [`Self::Set`] writes the value.
/// `Option<T>` is never used to mean "unchanged" on a nullable column, because that overload is
/// how a retry ends up leaving a stale `error` on a `queued` row.
///
/// It is externally tagged, so `Keep`, `Clear` and `Set(null)` are three distinguishable JSON
/// values and no writer can express "unchanged" and "null" with the same one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldUpdate<T> {
    /// Leave the column as it is.
    Keep,
    /// Write SQL `NULL`.
    Clear,
    /// Write this value.
    Set(T),
}

impl<T> Default for FieldUpdate<T> {
    /// [`FieldUpdate::Keep`] — hand-written rather than derived so `T` needs no `Default`.
    fn default() -> Self {
        Self::Keep
    }
}

impl<T> FieldUpdate<T> {
    /// Whether this patch touches the column.
    #[must_use]
    pub const fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    /// Applies the patch to a column's current value.
    pub fn apply_to(self, slot: &mut Option<T>) {
        match self {
            Self::Keep => {}
            Self::Clear => *slot = None,
            Self::Set(v) => *slot = Some(v),
        }
    }

    /// Maps the payload, leaving `Keep`/`Clear` alone.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> FieldUpdate<U> {
        match self {
            Self::Keep => FieldUpdate::Keep,
            Self::Clear => FieldUpdate::Clear,
            Self::Set(v) => FieldUpdate::Set(f(v)),
        }
    }

    /// Borrows the payload.
    #[must_use]
    pub fn as_ref(&self) -> FieldUpdate<&T> {
        match self {
            Self::Keep => FieldUpdate::Keep,
            Self::Clear => FieldUpdate::Clear,
            Self::Set(v) => FieldUpdate::Set(v),
        }
    }

    /// The value to write, or `None` for both `Keep` and `Clear`.
    ///
    /// Use only where "unchanged" and "null" genuinely coincide — an INSERT, for instance.
    #[must_use]
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Keep | Self::Clear => None,
            Self::Set(v) => Some(v),
        }
    }
}

impl<T> From<Option<T>> for FieldUpdate<T> {
    /// `Some(v)` → `Set(v)`, `None` → `Clear`. Deliberately **not** `Keep`: a caller that has an
    /// `Option` in hand knows the value, so "leave it alone" is not what it means.
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => Self::Set(v),
            None => Self::Clear,
        }
    }
}

/// The three item-state operations a hook is allowed to perform (DESIGN §7.1, §13).
///
/// Implemented by `aulos-queue::EngineHookStore`. The hook tests run against a `HashMap`-backed
/// fake with no SQLite and no engine at all.
#[async_trait::async_trait]
pub trait HookStore: Send + Sync {
    /// Reads the compacted provider entry, for the NFO hook (DESIGN §13.2).
    ///
    /// `None` when the row has no blob (a plain yt-dlp child) or it was already dropped.
    ///
    /// # Errors
    /// [`PortError`] if the row is gone or the store is unreachable.
    async fn entry_blob(&self, id: ItemId) -> Result<Option<EntryBlob>, PortError>;

    /// Drops the entry blob once the NFO hook has consumed it.
    ///
    /// # Errors
    /// [`PortError`] if the row is gone or the write could not be applied.
    async fn drop_entry_blob(&self, id: ItemId) -> Result<(), PortError>;

    /// Records the new file size after a hook rewrote the produced file (DESIGN §13.3).
    ///
    /// Engine-mediated, so the change reaches every connected client as a `delta`; writing SQLite
    /// directly would leave `size` wrong on every client until a restart.
    ///
    /// # Errors
    /// [`PortError`] if the row is gone or the write could not be applied.
    async fn set_size(&self, id: ItemId, size: u64) -> Result<(), PortError>;
}

/// Which APNs gateway a device's tokens belong to (DESIGN §25).
///
/// A token minted by a Debug/simulator build is only valid against the sandbox gateway, a
/// TestFlight/App Store token only against production; the app reports which it is at
/// registration and the notifier routes each push accordingly.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApnsEnvironment {
    /// `api.sandbox.push.apple.com`.
    Sandbox,
    /// `api.push.apple.com`.
    Production,
}

impl ApnsEnvironment {
    /// The lowercase wire spelling, as the app sends it and the store persists it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }
}

/// One registered device: `PUT api/v2/devices/{token}` (PROTOCOL §4.8, DESIGN §25).
///
/// `token` is the lowercase-hex APNs device token and the natural key. `live_activity_start_token`
/// is the Live Activity push-to-start token (iOS 17.2+), present only when the device offered one.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// Lowercase hex APNs device token; the key.
    pub token: Box<str>,
    /// `"ios"` today.
    pub platform: Box<str>,
    /// The app's bundle id, which is also the `apns-topic`.
    pub bundle_id: Box<str>,
    /// Which gateway the token belongs to.
    pub environment: ApnsEnvironment,
    /// Whether completion/failure alerts are wanted.
    pub alerts: bool,
    /// The Live Activity push-to-start token, when offered.
    pub live_activity_start_token: Option<Box<str>>,
    /// Which installation of the app this device is, when it reported one (`X-Aulos-Install`,
    /// PROTOCOL §1.3, §4.8).
    ///
    /// It is matched against `Item.source.ref` to route an alert or a Live Activity start to the
    /// install a download was added from rather than to every device in the household
    /// (DESIGN §25.2). `None` is an app build that predates the field, and matches every item.
    pub install_id: Option<Box<str>>,
    /// Free-form app version string, for logs.
    pub app_version: Option<Box<str>>,
    /// When the token was first registered.
    pub registered_at: UnixMs,
    /// When the registration was last refreshed.
    pub last_seen_at: UnixMs,
}

/// One live Live Activity: the update token a device forwarded for one item
/// (`PUT api/v2/devices/{token}/live-activities/{item_id}`).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LiveActivityRecord {
    /// The owning device's token (a [`DeviceRecord::token`]).
    pub device_token: Box<str>,
    /// The item the activity tracks.
    pub item_id: ItemId,
    /// The activity's push-to-update token, lowercase hex. Rotates; the app re-forwards it.
    pub update_token: Box<str>,
    /// Copied from the device at registration, so a push needs no join.
    pub environment: ApnsEnvironment,
    /// When the token was (last) forwarded.
    pub registered_at: UnixMs,
}

/// Device and Live Activity registrations, as the APNs notifier sees them (DESIGN §25).
///
/// Implemented by `aulos-store`; `aulos-api` writes through it and `aulos-apns` reads and prunes
/// through it. Removals are idempotent: removing an unknown token is `Ok(())`, because APNs tells
/// the notifier about dead tokens (`410 Unregistered`) that the app may already have deleted.
#[async_trait::async_trait]
pub trait DeviceStore: Send + Sync {
    /// Inserts or refreshes a device; a repeat `PUT` updates every field but `registered_at`.
    ///
    /// # Errors
    /// [`PortError::Store`] if the write could not be applied.
    async fn upsert_device(&self, device: DeviceRecord) -> Result<(), PortError>;

    /// Forgets a device and every Live Activity registered under it. Idempotent.
    ///
    /// # Errors
    /// [`PortError::Store`] if the write could not be applied.
    async fn remove_device(&self, token: &str) -> Result<(), PortError>;

    /// Every registered device.
    ///
    /// # Errors
    /// [`PortError::Store`] if the read failed.
    async fn devices(&self) -> Result<Vec<DeviceRecord>, PortError>;

    /// Inserts or refreshes a Live Activity registration, keyed on `(device_token, item_id)`.
    ///
    /// # Errors
    /// [`PortError::Store`] if the write could not be applied.
    async fn upsert_live_activity(&self, activity: LiveActivityRecord) -> Result<(), PortError>;

    /// Forgets one Live Activity registration. Idempotent.
    ///
    /// # Errors
    /// [`PortError::Store`] if the write could not be applied.
    async fn remove_live_activity(&self, device_token: &str, item: ItemId)
    -> Result<(), PortError>;

    /// The Live Activity registrations that track `item`, across devices.
    ///
    /// # Errors
    /// [`PortError::Store`] if the read failed.
    async fn live_activities_for(&self, item: ItemId)
    -> Result<Vec<LiveActivityRecord>, PortError>;

    /// Forgets every Live Activity registration for `item` (after the final `end` push, or when
    /// the item is deleted). Idempotent.
    ///
    /// # Errors
    /// [`PortError::Store`] if the write could not be applied.
    async fn remove_live_activities_for(&self, item: ItemId) -> Result<(), PortError>;
}

/// What can go wrong on a port call.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PortError {
    /// The row no longer exists — it was deleted while the hook ran.
    #[error("item {0} no longer exists")]
    NotFound(ItemId),
    /// The engine or store is shutting down.
    #[error("the engine is unavailable")]
    Unavailable,
    /// The store reported a failure.
    #[error("store error: {0}")]
    Store(Box<str>),
}

impl PortError {
    /// The wire error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Unavailable | Self::Store(_) => ErrorCode::StateUnavailable,
        }
    }

    /// A shutting-down engine or a busy store is worth another try; a deleted row is not.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable | Self::Store(_))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    type Patch = FieldUpdate<Box<str>>;

    #[test]
    fn the_three_states_are_distinguishable_after_serde() {
        let cases: [(Patch, &str); 3] = [
            (FieldUpdate::Keep, "\"keep\""),
            (FieldUpdate::Clear, "\"clear\""),
            (FieldUpdate::Set("x".into()), "{\"set\":\"x\"}"),
        ];
        let mut wire = Vec::new();
        for (patch, expected) in cases {
            let json = serde_json::to_string(&patch).unwrap();
            assert_eq!(json, expected);
            assert_eq!(serde_json::from_str::<Patch>(&json).unwrap(), patch);
            wire.push(json);
        }
        wire.sort();
        wire.dedup();
        assert_eq!(wire.len(), 3, "no two states share a wire form");
    }

    #[test]
    fn set_of_a_nullable_payload_is_not_clear() {
        type Nullable = FieldUpdate<Option<u64>>;
        let set_null: Nullable = FieldUpdate::Set(None);
        let clear: Nullable = FieldUpdate::Clear;
        assert_ne!(
            serde_json::to_string(&set_null).unwrap(),
            serde_json::to_string(&clear).unwrap()
        );
    }

    #[test]
    fn apply_to_implements_the_documented_semantics() {
        let mut slot = Some(Box::<str>::from("old"));
        Patch::Keep.apply_to(&mut slot);
        assert_eq!(slot.as_deref(), Some("old"));
        Patch::Set("new".into()).apply_to(&mut slot);
        assert_eq!(slot.as_deref(), Some("new"));
        Patch::Clear.apply_to(&mut slot);
        assert_eq!(slot, None);
    }

    #[test]
    fn from_option_never_produces_keep() {
        assert_eq!(Patch::from(Some("a".into())), FieldUpdate::Set("a".into()));
        assert_eq!(Patch::from(None), FieldUpdate::Clear);
        assert!(Patch::Keep.is_keep());
        assert!(!Patch::Clear.is_keep());
    }

    #[test]
    fn map_and_as_ref_leave_keep_and_clear_alone() {
        assert_eq!(Patch::Keep.map(|s| s.len()), FieldUpdate::Keep);
        assert_eq!(Patch::Clear.map(|s| s.len()), FieldUpdate::Clear);
        assert_eq!(
            Patch::Set("abc".into()).map(|s| s.len()),
            FieldUpdate::Set(3)
        );
        let set: Patch = FieldUpdate::Set("abc".into());
        assert_eq!(set.as_ref().map(|s| s.len()), FieldUpdate::Set(3));
    }

    #[test]
    fn hook_phase_names_match_healthz() {
        assert_eq!(HookPhase::PreTerminal.as_str(), "pre_terminal");
        assert_eq!(HookPhase::default(), HookPhase::PostTerminal);
        assert_eq!(
            serde_json::to_string(&HookPhase::PreTerminal).unwrap(),
            "\"pre_terminal\""
        );
    }

    #[test]
    fn port_error_codes_and_retryability() {
        let id = ItemId::new();
        assert_eq!(PortError::NotFound(id).code(), ErrorCode::NotFound);
        assert!(!PortError::NotFound(id).retryable());
        assert!(PortError::Unavailable.retryable());
        assert_eq!(
            PortError::Store("busy".into()).code(),
            ErrorCode::StateUnavailable
        );
    }
}
