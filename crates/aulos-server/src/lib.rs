//! The `aulos-server` binary's library half: configuration loading, the task wiring, the
//! `bgutil-pot` sidecar supervisor, the config watcher, signal handling and the five CLI
//! subcommands (DESIGN §16, §3.1).
//!
//! It is a library as well as a binary for one reason: the DESIGN §16.1 boot order, the DESIGN
//! §16.4 shutdown and the DESIGN §16.2 supervisor are the parts of this system with the fewest
//! natural seams and the most expensive failure modes, and none of them can be tested through
//! `assert_cmd` alone. `tests/server.rs` drives [`wiring::run_with`] directly.
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the subcommand surface | [`cli`] |
//! | boot steps 1–9: config, tracing, dirs, store, importer, options, plugins, probes | [`bootstrap`] |
//! | boot steps 10–16 and the ten-step shutdown | [`wiring`] |
//! | the `EventRouter` subscriber table (§2.2.1) | [`wiring`] |
//! | the three cross-crate seams the binary alone can close | [`adapters`] |
//! | the POT sidecar supervisor | [`pot`] |
//! | `YTDL_OPTIONS` hot reload and the plugin re-scan | [`config_watch`] |
//! | the moving `healthz` components | [`health`] |
//! | signals and the panic policy | [`signals`] |
//! | external-tool probes | [`tools`] |
//! | `check-config`, `import`, `doctor`, `healthcheck` | [`check_config`], [`import_cmd`], [`doctor`], [`healthcheck`] |
//!
//! # BRIEF scope trims applied here
//!
//! - The Prometheus `metrics` endpoint and the DESIGN §16.7 inventory are **CUT**;
//!   `AULOS_METRICS_ENABLED` is still parsed so an existing compose file boots, and the counters
//!   the design mapped onto `healthz` paths are all still reported there.
//! - `print-schema` and `repair-ids` are **CUT**; the allocator boot check logs a `WARN` and
//!   continues (see [`bootstrap::open_store`]) instead of refusing to start.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod adapters;
pub mod bootstrap;
pub mod check_config;
pub mod cli;
pub mod config_watch;
pub mod doctor;
pub mod health;
pub mod healthcheck;
pub mod import_cmd;
pub mod pot;
pub mod signals;
pub mod tools;
pub mod wiring;

pub use wiring::{RunOptions, run, run_with};
