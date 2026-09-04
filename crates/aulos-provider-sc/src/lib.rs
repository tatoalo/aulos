//! The `streamingcommunity` provider: native scraping of the Inertia version endpoint, the watch
//! page and the embed iframe, two-request season resolution, just-in-time m3u8 re-extraction at
//! download time, the `N_m3u8DL-RE` and ffmpeg engines with a gapless natural-order mux fallback,
//! and ANSI progress-frame parsing.
//!
//! The HTTP client is Chrome-impersonating `wreq` behind the default `sc-impersonate` feature,
//! with plain `reqwest` as the always-available fallback (DESIGN §10, BRIEF scope trims).
//!
//! # Where to look
//!
//! | Concern | Module |
//! |---|---|
//! | the `ScHttp` trait, both clients, the `AULOS_SC_HTTP` selection | [`http`] |
//! | S1 (site version) and S2 (Inertia page) with the version-drift retry | [`inertia`] |
//! | S3 (embed iframe) and S4 (`window.streams` / `masterPlaylist`) | [`embed`] |
//! | `/watch/` entry construction, bit-for-bit legacy id/title strings | [`watch`] |
//! | `/titles/…/season-n` and `/titles/…` — season resolution in 2 requests | [`season`] |
//! | the just-in-time re-extraction used at download time | [`jit`] |
//! | the opaque `MediaEntry::state` blob and its legacy `.info.json` shape | [`state`] |
//! | the one advisory "Source" catalog entry | [`catalog`] |
//! | the [`Provider`](aulos_provider::provider::Provider) implementation | [`provider`] |
//! | output naming, the sidecar, engine selection and the ffmpeg retry | [`engines`] |
//! | the `N_m3u8DL-RE` engine, argv-identical to legacy | [`nm3u8dl`] |
//! | the ffmpeg engine and the `ffprobe` duration probe | [`ffmpeg`] |
//! | the gapless natural-order segment mux | [`mux`] |
//! | the ANSI repaint parser and the ffmpeg progress reader | [`progress`] |
//!
//! # The one thing to know about this provider
//!
//! The m3u8 URL resolved during a scrape is **discarded**. Only the watch URL is persisted, and
//! the stream is re-extracted just in time at download time, because vixcloud tokens expire in
//! minutes (DESIGN §10.3). That is why [`jit::fresh_stream`] exists and why resolution never
//! stores a stream URL anywhere.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod catalog;
pub mod embed;
pub mod engines;
pub mod error;
pub mod ffmpeg;
pub mod http;
pub mod inertia;
pub mod jit;
pub mod mux;
pub mod nm3u8dl;
pub mod progress;
pub mod provider;
pub mod season;
pub mod state;
pub mod watch;

pub use catalog::sc_catalog;
pub use engines::EngineCfg;
pub use error::{ScError, ScErrorCode, ScInitError};
pub use http::{ScHttp, ScReq, ScRes, USER_AGENT};
pub use inertia::SiteVersions;
pub use jit::{StreamTarget, fresh_stream};
pub use mux::{gapless_mux, natural_cmp};
pub use progress::{FfmpegProgress, parse_nm3u8_frame};
pub use provider::{ScProvider, sc_matches};
pub use state::ScState;

/// This provider's stable id, as it appears on an item and in `GET api/v2/providers`.
pub const PROVIDER_ID: &str = "streamingcommunity";

/// The legacy `extractor` string, kept verbatim so imported rows and existing `.info.json`
/// sidecars keep matching (legacy `streamingcommunity.py:178`).
pub const EXTRACTOR: &str = "streamingcommunity";

/// The legacy `extractor_key` string (legacy `streamingcommunity.py:179`).
pub const EXTRACTOR_KEY: &str = "StreamingCommunity";

/// The host substring legacy detected StreamingCommunity URLs with
/// (`streamingcommunity.py:485`).
pub const HOST_NEEDLE: &str = "streamingcommunity";

/// This provider's [`ProviderId`](aulos_core::selection::ProviderId).
///
/// `expect` is allowed here and only here: [`PROVIDER_ID`] is a compile-time literal that
/// satisfies `ProviderId::parse`'s shape, and `the_provider_id_parses` proves it.
#[must_use]
#[allow(
    clippy::expect_used,
    reason = "a compile-time literal id that a unit test proves parses"
)]
pub fn provider_id() -> aulos_core::selection::ProviderId {
    aulos_core::selection::ProviderId::parse(PROVIDER_ID)
        .expect("the literal provider id must parse")
}

#[cfg(test)]
mod testing;

#[cfg(test)]
mod lib_tests {
    #[test]
    fn the_provider_id_parses() {
        assert_eq!(super::provider_id().as_str(), "streamingcommunity");
    }

    #[test]
    fn the_legacy_strings_are_unchanged() {
        assert_eq!(super::EXTRACTOR, "streamingcommunity");
        assert_eq!(super::EXTRACTOR_KEY, "StreamingCommunity");
        assert_eq!(super::HOST_NEEDLE, "streamingcommunity");
    }
}
