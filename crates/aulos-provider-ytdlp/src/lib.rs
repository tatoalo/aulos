//! The `ytdlp` catch-all provider: the Rust port of the legacy `dl_formats` format selector and
//! option builder, the output template, and the client for the thin Python shim
//! (`python/ytdlp_runner.py`) that owns the actual `yt_dlp` API calls.
//!
//! Rust owns option construction, process lifecycle, process-group kill, timeouts and progress
//! normalisation; the shim only relays JSON lines (DESIGN §9).
