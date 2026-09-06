//! The `healthz` component this crate publishes (DESIGN §16.3, §25.7).
//!
//! Modelled on `aulos_hooks::HooksHealthHandle`: the notifier is consumed by the wiring, so the
//! counters live behind an `Arc` and [`ApnsHealthHandle`] reads them for as long as the process
//! lives.
//!
//! ```text
//! "apns": {
//!   "status": "ok",
//!   "devices": 2, "live_activities": 1,
//!   "sent_total": 41, "failed_total": 0, "pruned_tokens_total": 1,
//!   "last_error": null, "last_sent_at": 1772582400000
//! }
//! ```
//!
//! The four states are the ones §25.7 names: `disabled` when `APNS_ENABLED=false`, `degraded`
//! when the server is misconfigured (an unreadable `.p8`, or a `403` that survived a JWT remint),
//! `ok` otherwise. `down` is never used — a push service that cannot reach Apple does not make
//! this server unusable.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use aulos_core::health::{ComponentHealth, ComponentStatus, HealthRegistry};
use aulos_core::id::UnixMs;
use serde_json::Value;

/// The `healthz` component name.
pub const COMPONENT: &str = "apns";

/// The live counters. One instance per notifier, shared with every push task.
#[derive(Debug, Default)]
pub struct Counters {
    sent: AtomicU64,
    failed: AtomicU64,
    pruned: AtomicU64,
    /// `0` means "never".
    last_sent_at: AtomicI64,
    last_error: Mutex<Option<String>>,
    /// Set by a non-retryable provider error, cleared by the next delivered push.
    degraded: AtomicBool,
    /// Live Activity registrations the notifier currently has cached.
    live_activities: AtomicU64,
}

impl Counters {
    /// A fresh set.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records a delivered push, clearing the degraded flag.
    pub fn delivered(&self, at: UnixMs) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        self.last_sent_at.store(at, Ordering::Relaxed);
        self.degraded.store(false, Ordering::Relaxed);
    }

    /// Records a push that did not land. `misconfigured` latches the degraded flag.
    pub fn failed(&self, reason: &str, misconfigured: bool) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.set_last_error(reason);
        if misconfigured {
            self.degraded.store(true, Ordering::Relaxed);
        }
    }

    /// Records a token Apple told us to forget.
    pub fn pruned(&self) {
        self.pruned.fetch_add(1, Ordering::Relaxed);
    }

    /// Publishes the cached Live Activity registration count.
    pub fn set_live_activities(&self, n: u64) {
        self.live_activities.store(n, Ordering::Relaxed);
    }

    /// Replaces `last_error` without touching any counter (a store read that failed, say).
    pub fn set_last_error(&self, reason: &str) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason.to_owned());
    }

    /// The counters as a value, minus the gauges only a store read can supply.
    #[must_use]
    pub fn snapshot(&self) -> ApnsHealth {
        let last_sent = self.last_sent_at.load(Ordering::Relaxed);
        ApnsHealth {
            status: if self.degraded.load(Ordering::Relaxed) {
                ComponentStatus::Degraded
            } else {
                ComponentStatus::Ok
            },
            devices: 0,
            live_activities: self.live_activities.load(Ordering::Relaxed),
            sent_total: self.sent.load(Ordering::Relaxed),
            failed_total: self.failed.load(Ordering::Relaxed),
            pruned_tokens_total: self.pruned.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            last_sent_at: (last_sent != 0).then_some(last_sent),
        }
    }
}

/// The `apns` component's status and detail (DESIGN §25.7).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ApnsHealth {
    /// `disabled`, `ok` or `degraded`.
    pub status: ComponentStatus,
    /// Registered devices.
    pub devices: u64,
    /// Live Activity registrations the notifier is tracking.
    pub live_activities: u64,
    /// Pushes Apple accepted.
    pub sent_total: u64,
    /// Pushes that did not land.
    pub failed_total: u64,
    /// Device or activity tokens dropped because Apple said they were dead.
    pub pruned_tokens_total: u64,
    /// The most recent failure, or `None`.
    pub last_error: Option<String>,
    /// When the last push landed, unix ms, or `None`.
    pub last_sent_at: Option<UnixMs>,
}

impl ApnsHealth {
    /// The component `APNS_ENABLED=false` publishes: `disabled`, every counter zero.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            status: ComponentStatus::Disabled,
            devices: 0,
            live_activities: 0,
            sent_total: 0,
            failed_total: 0,
            pruned_tokens_total: 0,
            last_error: None,
            last_sent_at: None,
        }
    }

    /// The component the wiring publishes when `APNS_ENABLED=true` but the notifier could not be
    /// built — an unreadable `.p8`, a blank `APNS_KEY_ID` (DESIGN §25.6: the server logs an ERROR
    /// and keeps running).
    #[must_use]
    pub fn misconfigured(reason: &str) -> Self {
        Self {
            status: ComponentStatus::Degraded,
            last_error: Some(reason.to_owned()),
            ..Self::disabled()
        }
    }

    /// The `healthz` projection.
    #[must_use]
    pub fn component(&self) -> ComponentHealth {
        ComponentHealth::new(self.status)
            .with("devices", self.devices)
            .with("live_activities", self.live_activities)
            .with("sent_total", self.sent_total)
            .with("failed_total", self.failed_total)
            .with("pruned_tokens_total", self.pruned_tokens_total)
            .with(
                "last_error",
                self.last_error
                    .as_deref()
                    .map_or(Value::Null, |e| Value::String(e.to_owned())),
            )
            .with(
                "last_sent_at",
                self.last_sent_at.map_or(Value::Null, Value::from),
            )
    }

    /// Publishes the component. Returns whether the registry actually changed.
    pub fn apply(&self, registry: &HealthRegistry) -> bool {
        registry.set(COMPONENT, self.component())
    }
}

/// Reads [`ApnsHealth`] after the notifier has been handed to the wiring.
#[derive(Clone)]
pub struct ApnsHealthHandle {
    counters: Arc<Counters>,
    store: Arc<dyn aulos_core::ports::DeviceStore>,
}

impl std::fmt::Debug for ApnsHealthHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApnsHealthHandle")
            .field("counters", &self.counters.snapshot())
            .finish_non_exhaustive()
    }
}

impl ApnsHealthHandle {
    /// Binds a counter set to the store the `devices` gauge is read from.
    #[must_use]
    pub fn new(counters: Arc<Counters>, store: Arc<dyn aulos_core::ports::DeviceStore>) -> Self {
        Self { counters, store }
    }

    /// The counters alone. Cheap, and never touches the store.
    #[must_use]
    pub fn counters(&self) -> ApnsHealth {
        self.counters.snapshot()
    }

    /// The counters plus the `devices` gauge.
    ///
    /// A store failure is not a health failure of the APNs component: the `store` component
    /// already reports it, so the gauge stays `0` and `last_error` records what happened.
    pub async fn health(&self) -> ApnsHealth {
        let mut health = self.counters.snapshot();
        match self.store.devices().await {
            Ok(devices) => health.devices = u64::try_from(devices.len()).unwrap_or(u64::MAX),
            Err(e) => {
                let reason = format!("device registrations unreadable: {e}");
                self.counters.set_last_error(&reason);
                health.last_error = Some(reason);
            }
        }
        health
    }
}
