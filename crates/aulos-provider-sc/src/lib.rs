//! The `streamingcommunity` provider: native scraping of the Inertia version endpoint, the watch
//! page and the embed iframe, two-request season resolution, just-in-time m3u8 re-extraction at
//! download time, the `N_m3u8DL-RE` and ffmpeg engines with a gapless natural-order mux fallback,
//! and ANSI progress-frame parsing.
//!
//! The HTTP client is Chrome-impersonating `wreq` behind the default `sc-impersonate` feature,
//! with plain `reqwest` as the always-available fallback (DESIGN §10, BRIEF scope trims).
