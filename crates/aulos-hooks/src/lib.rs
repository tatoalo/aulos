//! Post-completion hooks: the debouncing dispatcher plus the `jellyfin` library refresh, `nfo`
//! generation for StreamingCommunity items, the `best_remux` audio-sync ffmpeg pass, and community
//! hooks declared as `[[hook]]` tables in a `plugin.toml`.
//!
//! Item state is reached only through `aulos_core::ports::HookStore`, never through `aulos-store`
//! or `aulos-queue` (DESIGN §3, §13).
