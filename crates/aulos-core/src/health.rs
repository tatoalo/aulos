//! The `healthz` registry and its wire projection (DESIGN §16.3).
//!
//! [`HealthRegistry`] is an `aulos-core` type because it is **written** by `aulos-server` (§16) and
//! **read** by `aulos-api` (`ApiState.health`), and `aulos-server` depends on `aulos-api` — putting
//! the registry in the binary would be a dependency cycle (DESIGN §3).

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::id::{BootId, Seq};

/// One component's readiness.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentStatus {
    /// Working.
    Ok,
    /// Working, but not as it should be. Reported with HTTP 200 and `"status":"degraded"`.
    Degraded,
    /// Not working. Only the store being unusable makes `healthz` answer 503.
    Down,
    /// Not configured, so not a failure.
    Disabled,
}

impl ComponentStatus {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Disabled => "disabled",
        }
    }

    /// The worse of two statuses, for the roll-up.
    #[must_use]
    pub const fn worse(self, other: Self) -> Self {
        if self.severity() >= other.severity() {
            self
        } else {
            other
        }
    }

    const fn severity(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Ok => 1,
            Self::Degraded => 2,
            Self::Down => 3,
        }
    }
}

impl std::fmt::Display for ComponentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `components.<name>` entry: a status plus free-form detail fields.
///
/// The detail fields differ per component (`wal_bytes` for the store, `restarts` for the POT
/// sidecar, …), so they are a JSON object rather than a union of every component's shape. The
/// status is a typed field so the roll-up cannot be computed from prose.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ComponentHealth {
    /// Readiness.
    pub status: ComponentStatus,
    /// Component-specific detail, flattened into the same JSON object as `status`.
    #[serde(flatten)]
    pub detail: Map<String, Value>,
}

impl ComponentHealth {
    /// A component with no detail.
    #[must_use]
    pub fn new(status: ComponentStatus) -> Self {
        Self {
            status,
            detail: Map::new(),
        }
    }

    /// Adds one detail field.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.detail.insert(key.to_owned(), value.into());
        self
    }
}

/// The `healthz` payload minus the fields only `aulos-api` can fill in.
///
/// `version`, `yt_dlp`, `uptime_s`, `url_prefix`, `v1_shim`, `providers` and `ws` are assembled by
/// the API handler from its own state (DESIGN §16.3); what lives here is the part the registry
/// owns and the part `DomainEvent::HealthChanged` carries.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct HealthView {
    /// The roll-up over `components`.
    pub status: ComponentStatus,
    /// This process's boot id.
    pub boot_id: Option<BootId>,
    /// The last frame sequence issued.
    pub seq: Option<Seq>,
    /// One entry per component, ordered by name so the payload is stable.
    pub components: BTreeMap<String, ComponentHealth>,
}

impl HealthView {
    /// An empty view: `ok`, no components.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            status: ComponentStatus::Ok,
            boot_id: None,
            seq: None,
            components: BTreeMap::new(),
        }
    }

    /// Whether `healthz` should answer 503.
    ///
    /// Only the store being unusable makes the service useless (DESIGN §16.3); every other `down`
    /// component degrades but still answers 200.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        self.components
            .get("store")
            .is_some_and(|c| c.status == ComponentStatus::Down)
    }

    /// The roll-up over `components`: the worst component, **capped at `degraded`** unless the
    /// fatal condition of [`HealthView::is_fatal`] holds.
    ///
    /// The cap is what makes DESIGN §16.3's own example payload representable — `"status":
    /// "degraded"` with `"pot": {"status": "down"}` inside it. A plain "worst wins" fold cannot
    /// produce it: one `down` optional component would make the whole view `down` while `healthz`
    /// still answered `200`, and a body-reading `healthcheck` would then restart a container whose
    /// only problem is a sidecar it does not need. The service is `down` when it is unusable, and
    /// §16.3 says exactly one component decides that.
    #[must_use]
    pub fn roll_up(&self) -> ComponentStatus {
        let worst = self
            .components
            .values()
            .fold(ComponentStatus::Ok, |acc, c| acc.worse(c.status));
        if worst == ComponentStatus::Down && !self.is_fatal() {
            ComponentStatus::Degraded
        } else {
            worst
        }
    }
}

/// The process-wide health map. Cheap to read (one `ArcSwap` load), so `healthz` never blocks.
#[derive(Debug)]
pub struct HealthRegistry {
    inner: ArcSwap<HealthView>,
}

impl HealthRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::from_pointee(HealthView::empty()),
        }
    }

    /// Records the identity fields, once, at boot.
    pub fn set_identity(&self, boot_id: BootId) {
        self.mutate(|v| v.boot_id = Some(boot_id));
    }

    /// Records the last frame sequence.
    pub fn set_seq(&self, seq: Seq) {
        self.mutate(|v| v.seq = Some(seq));
    }

    /// Sets one component, recomputing the roll-up.
    ///
    /// Returns `true` when the stored view actually changed, so the caller knows whether to
    /// publish a [`crate::event::DomainEvent::HealthChanged`] — publishing on every probe tick
    /// would put an unchanging frame on the wire every second.
    pub fn set(&self, name: &str, health: ComponentHealth) -> bool {
        let before = self.snapshot();
        if before.components.get(name) == Some(&health) {
            return false;
        }
        self.mutate(|v| {
            v.components.insert(name.to_owned(), health);
        });
        true
    }

    /// Removes a component (a plugin hook that went away).
    pub fn remove(&self, name: &str) -> bool {
        if !self.snapshot().components.contains_key(name) {
            return false;
        }
        self.mutate(|v| {
            v.components.remove(name);
        });
        true
    }

    /// The current view. One atomic load.
    #[must_use]
    pub fn snapshot(&self) -> Arc<HealthView> {
        self.inner.load_full()
    }

    /// Applies `f` to a copy and stores it, recomputing the roll-up.
    fn mutate(&self, f: impl FnOnce(&mut HealthView)) {
        let mut next = (*self.snapshot()).clone();
        f(&mut next);
        next.status = next.roll_up();
        self.inner.store(Arc::new(next));
    }
}

impl Default for HealthRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn component_status_serialises_lowercase_and_orders_by_severity() {
        assert_eq!(
            serde_json::to_string(&ComponentStatus::Degraded).unwrap(),
            "\"degraded\""
        );
        assert_eq!(
            ComponentStatus::Ok.worse(ComponentStatus::Degraded),
            ComponentStatus::Degraded
        );
        assert_eq!(
            ComponentStatus::Down.worse(ComponentStatus::Degraded),
            ComponentStatus::Down
        );
        assert_eq!(
            ComponentStatus::Disabled.worse(ComponentStatus::Ok),
            ComponentStatus::Ok,
            "a disabled component never drags the roll-up down"
        );
    }

    #[test]
    fn detail_fields_are_flattened_next_to_status() {
        let c = ComponentHealth::new(ComponentStatus::Ok)
            .with("wal_bytes", 1_048_576)
            .with("latency_ms", 0.42);
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["wal_bytes"], 1_048_576);
        assert_eq!(v["latency_ms"], 0.42);
        assert_eq!(serde_json::from_value::<ComponentHealth>(v).unwrap(), c);
    }

    #[test]
    fn the_roll_up_is_the_worst_component_capped_at_degraded() {
        let r = HealthRegistry::new();
        assert_eq!(r.snapshot().status, ComponentStatus::Ok);
        assert!(r.set("store", ComponentHealth::new(ComponentStatus::Ok)));
        assert!(r.set("pot", ComponentHealth::new(ComponentStatus::Down)));
        // DESIGN §16.3's own example payload: `"status": "degraded"` around a `down` `pot`. A
        // `down` sidecar does not make the server unusable, and `healthz` answers 200 for it.
        assert_eq!(r.snapshot().status, ComponentStatus::Degraded);
        assert!(
            !r.snapshot().is_fatal(),
            "only the store makes healthz answer 503"
        );
        assert!(r.set("store", ComponentHealth::new(ComponentStatus::Down)));
        assert_eq!(
            r.snapshot().status,
            ComponentStatus::Down,
            "the one fatal component is not capped"
        );
        assert!(r.snapshot().is_fatal());
        // …and it stops being fatal, and the roll-up drops back to the cap, when it recovers.
        assert!(r.set("store", ComponentHealth::new(ComponentStatus::Ok)));
        assert_eq!(r.snapshot().status, ComponentStatus::Degraded);
        assert!(!r.snapshot().is_fatal());
    }

    #[test]
    fn a_degraded_component_rolls_up_as_degraded_and_ok_ones_as_ok() {
        let r = HealthRegistry::new();
        assert!(r.set("store", ComponentHealth::new(ComponentStatus::Ok)));
        assert!(r.set("telegram", ComponentHealth::new(ComponentStatus::Disabled)));
        assert_eq!(r.snapshot().status, ComponentStatus::Ok);
        assert!(r.set("deno", ComponentHealth::new(ComponentStatus::Degraded)));
        assert_eq!(r.snapshot().status, ComponentStatus::Degraded);
    }

    #[test]
    fn setting_an_unchanged_component_reports_no_change() {
        let r = HealthRegistry::new();
        let c = ComponentHealth::new(ComponentStatus::Ok).with("version", "6.1.1");
        assert!(r.set("ffmpeg", c.clone()));
        assert!(!r.set("ffmpeg", c), "no change, so no HealthChanged event");
        assert!(r.remove("ffmpeg"));
        assert!(!r.remove("ffmpeg"));
    }

    #[test]
    fn identity_and_seq_survive_component_updates() {
        let r = HealthRegistry::new();
        let boot = BootId::new();
        r.set_identity(boot);
        r.set_seq(Seq(42));
        r.set("queue", ComponentHealth::new(ComponentStatus::Ok));
        let v = r.snapshot();
        assert_eq!(v.boot_id, Some(boot));
        assert_eq!(v.seq, Some(Seq(42)));
    }
}
