//! The `ytdlp` catch-all provider: the Rust port of the legacy `dl_formats` format selector and
//! option builder, the output template, and the client for the thin Python shim
//! (`python/ytdlp_runner.py`) that owns the actual `yt_dlp` API calls.
//!
//! Rust owns option construction, process lifecycle, process-group kill, timeouts and progress
//! normalisation; the shim only relays JSON lines (DESIGN §9).
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the yt-dlp format selector, a literal port of `dl_formats.get_format` | [`formats`] |
//! | the yt-dlp option dict and its postprocessor layering | [`opts`] |
//! | `outtmpl` pre-resolution and the `mode = "outtmpl"` shim job | [`outtmpl`] |
//! | the format / quality catalogue this provider advertises | [`catalog`] |
//!
//! # The one deliberate behaviour change
//!
//! Legacy appended an `Exec` postprocessor running `app/audio_sync_fix.py` after a
//! `{video, mp4, best_remux}` download. That step is now the in-process `audio_sync` completion
//! hook of DESIGN §13.3, so [`opts::get_opts`] does **not** emit it (Δ C9). Everything else this
//! crate produces is byte-identical to the Python it replaces, proven by the WP-00 golden corpus
//! in `tests/golden_formats.rs` and `tests/golden_opts.rs` rather than by inspection.
//!
//! # What is not here yet
//!
//! `runner`, `progress` and `errmap` — the shim transport of DESIGN §9.1–§9.7 and
//! `python/ytdlp_runner.py` itself — are WP-07, together with the `Provider` implementation that
//! ties the four modules below to a running process. This package supplies what that
//! implementation constructs: the `format` string, the option dict, the output templates and the
//! catalogue.

pub mod catalog;
pub mod formats;
pub mod opts;
pub mod outtmpl;

// ---------------------------------------------------------------------------
// Flat re-exports, so a caller writes `use aulos_provider_ytdlp::{get_format, get_opts}` rather
// than three module paths. This is the surface DESIGN §9.8 and PLAN WP-06 name.
// ---------------------------------------------------------------------------

pub use catalog::ytdlp_catalog;
pub use formats::{
    AUDIO_FORMATS, CAPTION_MODES, CODEC_FILTER_MAP, FormatError, VIDEO_FORMATS, codec_filter,
    get_format, get_format_raw,
};
pub use opts::{get_opts, get_opts_raw, normalize_caption_mode, normalize_subtitle_language};
pub use outtmpl::{OutTmpl, OutTmplError, OutTmplJob, build_outtmpl};
