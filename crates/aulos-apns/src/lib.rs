//! Apple Push Notification service for the iOS client (DESIGN §25).
//!
//! This crate is the second implementation of `aulos_core::event::Notifier` (DESIGN §12.6, after
//! `aulos-telegram`) and it exists so the phone can hear about a download it is not looking at:
//! one alert when a top-level item or a group reaches a terminal status, and a Live Activity that
//! runs on the lock screen from the moment bytes start moving until the item finishes.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the ES256 provider token, cached and reminted every 50 minutes | [`jwt`] |
//! | the HTTP/2 client, the response taxonomy and the retry ladder | [`client`] |
//! | the payload shapes, as pure functions of an `ItemView` | [`payload`] |
//! | the event handling, throttling and the bounded task set | [`notifier`] |
//! | the `healthz` component | [`health`] |
//!
//! # What it depends on
//!
//! `aulos-core` and nothing else from the workspace. Device registrations are reached through the
//! `aulos_core::ports::DeviceStore` port — `aulos-store` implements it, `aulos-api` writes through
//! it, and this crate reads and prunes through it — which is what keeps `aulos-apns` testable
//! against a `HashMap` with no SQLite anywhere in its test tree (DESIGN §3).
//!
//! # Wiring it up
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use aulos_apns::{ApnsHealth, ApnsNotifier};
//! # use aulos_core::clock::{Clock, SystemClock};
//! # use aulos_core::config::{Config, RawEnv};
//! # use aulos_core::health::HealthRegistry;
//! # use aulos_core::ports::DeviceStore;
//! # fn wire(cfg: &Config, store: Arc<dyn DeviceStore>, registry: &HealthRegistry) {
//! let clock: Arc<dyn Clock> = Arc::new(SystemClock);
//! match ApnsNotifier::new(cfg, store, clock) {
//!     Ok(None) => {
//!         ApnsHealth::disabled().apply(registry);
//!     }
//!     Ok(Some(notifier)) => {
//!         let health = notifier.health_handle();
//!         // subscribe `notifier` to the EventRouter, poll `health` for `healthz`
//!         let _ = health;
//!     }
//!     Err(e) => {
//!         // DESIGN §25.6: log and keep running, never refuse to start.
//!         tracing::error!(error = %e, "APNs is enabled but misconfigured; push is disabled");
//!         ApnsHealth::misconfigured(&e.to_string()).apply(registry);
//!     }
//! }
//! # }
//! ```

pub mod client;
pub mod error;
pub mod health;
pub mod jwt;
pub mod notifier;
pub mod payload;

pub use client::{
    ApnsClient, DEFAULT_BACKOFF, Outcome, PRODUCTION_BASE, Push, PushKind, REQUEST_TIMEOUT,
    SANDBOX_BASE,
};
pub use error::ApnsError;
pub use health::{ApnsHealth, ApnsHealthHandle, COMPONENT, Counters};
pub use jwt::{ProviderToken, REMINT_AFTER};
pub use notifier::{
    ApnsNotifier, ID, PROGRESS_INTERVAL, PUSH_CONCURRENCY, UPDATE_INTERVAL, live_activity_topic,
};

// Re-exported so the wiring and a test harness have one `use` line for the port vocabulary this
// crate shares with `aulos-store` and `aulos-api` (DESIGN §25.1).
pub use aulos_core::event::Notifier;
pub use aulos_core::ports::{
    ApnsEnvironment, DeviceRecord, DeviceStore, LiveActivityRecord, PortError, ProgressReader,
};
