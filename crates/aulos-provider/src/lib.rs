//! The provider abstraction every downloader implements: the `Provider` trait, `MediaEntry`, the
//! `ProgressSink`, the scoring registry, process-group spawn/kill helpers, the `plugin.toml`
//! manifest model shared by `command` providers and community `[[hook]]`s, and (behind the `fake`
//! feature) a scripted no-network provider for the integration tests.
//!
//! No provider crate may depend on `aulos-store` or `aulos-queue` (DESIGN §3 rule A1, §6).
