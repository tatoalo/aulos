//! The plugin-reload report (DESIGN §6.5).
//!
//! Produced by `Registry::reload_commands` in `aulos-provider`, carried by
//! [`crate::event::DomainEvent::ProvidersReloaded`], and published as a `providers` WS frame so a
//! client's catalogue updates live. It lives in `aulos-core` because it appears in a
//! `DomainEvent` payload (DESIGN §3).

use serde::{Deserialize, Serialize};

use crate::selection::ProviderId;

/// What one `AULOS_PLUGINS_DIR` scan changed.
///
/// A manifest failure yields a `failed` entry and a `Degraded` provider — never a startup failure
/// and never silence (DESIGN §6.4).
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct ReloadReport {
    /// Providers that did not exist before this scan.
    pub added: Vec<ProviderId>,
    /// Providers whose manifest changed.
    pub updated: Vec<ProviderId>,
    /// Providers whose directory disappeared.
    pub removed: Vec<ProviderId>,
    /// Directories whose manifest could not be loaded, with the reason.
    pub failed: Vec<ReloadFailure>,
}

impl ReloadReport {
    /// An empty report — the answer to a scan that changed nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether this scan changed anything at all, so a caller can skip publishing an event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.updated.is_empty()
            && self.removed.is_empty()
            && self.failed.is_empty()
    }

    /// How many providers this scan touched, successfully or not.
    #[must_use]
    pub fn touched(&self) -> usize {
        self.added.len() + self.updated.len() + self.removed.len() + self.failed.len()
    }
}

/// One plugin directory that could not be loaded.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ReloadFailure {
    /// The plugin directory name, which is also the `command:<name>` suffix.
    pub name: Box<str>,
    /// Why it failed, e.g. `"download.command[0] not executable"`. Shown verbatim in `healthz`.
    pub reason: Box<str>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_report_serialises_four_arrays() {
        let v = serde_json::to_value(ReloadReport::empty()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "added": [], "updated": [], "removed": [], "failed": [] })
        );
        assert!(ReloadReport::empty().is_empty());
    }

    #[test]
    fn failures_carry_a_name_and_a_reason() {
        let r = ReloadReport {
            added: vec![ProviderId::parse("command:bandcamp").unwrap()],
            failed: vec![ReloadFailure {
                name: "broken".into(),
                reason: "download.command[0] not executable".into(),
            }],
            ..ReloadReport::empty()
        };
        assert!(!r.is_empty());
        assert_eq!(r.touched(), 2);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["added"][0], "command:bandcamp");
        assert_eq!(
            v["failed"][0]["reason"],
            "download.command[0] not executable"
        );
        assert_eq!(serde_json::from_value::<ReloadReport>(v).unwrap(), r);
    }
}
