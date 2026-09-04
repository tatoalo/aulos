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
//! | the job object sent to the shim on stdin | [`job`] |
//! | the frame model the shim answers with on fd 3 | [`frames`] |
//! | spawn, transport, ordering checks, cancellation, `--replay` | [`runner`] |
//! | `progress` frame → `RawProgress` | [`progress`] |
//! | shim error `code` → `ProviderError` | [`errmap`] |
//! | the `Provider` implementation itself | [`provider`] |
//!
//! # The one deliberate behaviour change
//!
//! Legacy appended an `Exec` postprocessor running `app/audio_sync_fix.py` after a
//! `{video, mp4, best_remux}` download. That step is now the in-process `audio_sync` completion
//! hook of DESIGN §13.3, so [`opts::get_opts`] does **not** emit it (Δ C9). Everything else this
//! crate produces is byte-identical to the Python it replaces, proven by the WP-00 golden corpus
//! in `tests/golden_formats.rs` and `tests/golden_opts.rs` rather than by inspection.
//!
//! # The one structural decision worth reading before the code
//!
//! The protocol is on **fd 3**, not on stdout, and the shim points its own stdout at
//! `/dev/null` for the whole run. This is a deliberate override of BRIEF §9, recorded in
//! DESIGN §23.1 B1: the BgUtils POT plugin, `yt-dlp-ejs` and its `deno` grandchildren print to
//! stdout, and a yt-dlp `logger` object silences yt-dlp but neither a plugin nor a grandchild.
//! A protocol on stdout would therefore corrupt silently and intermittently — the most
//! expensive class of bug this system can have. [`runner`] and `python/ytdlp_runner.py` are
//! the two halves of that decision, and `tests/shim.rs` has the regression test for it.

pub mod catalog;
pub mod errmap;
pub mod formats;
pub mod frames;
pub mod job;
pub mod opts;
pub mod outtmpl;
pub mod progress;
pub mod provider;
pub mod runner;

// ---------------------------------------------------------------------------
// Flat re-exports, so a caller writes `use aulos_provider_ytdlp::{get_format, get_opts}` rather
// than three module paths. This is the surface DESIGN §9.8 and PLAN WP-06 name.
// ---------------------------------------------------------------------------

pub use catalog::ytdlp_catalog;
pub use errmap::{known_codes, to_provider_error};
pub use formats::{
    AUDIO_FORMATS, CAPTION_MODES, CODEC_FILTER_MAP, FormatError, VIDEO_FORMATS, codec_filter,
    get_format, get_format_raw,
};
pub use frames::{Body, Frame, MAX_LINE_BYTES, PROTOCOL};
pub use job::{ExtractOpts, Job, Mode, Policy, shim_watchdog_ms};
pub use opts::{get_opts, get_opts_raw, normalize_caption_mode, normalize_subtitle_language};
pub use outtmpl::{OutTmpl, OutTmplError, OutTmplJob, build_outtmpl};
pub use progress::{FrameStatus, ProgressState};
pub use provider::YtdlpProvider;
pub use runner::{
    DEFAULT_PYTHON, DEFAULT_RUNNER_PATH, RunnerHandle, RunnerOutcome, ShimIdentity, run_job,
};
