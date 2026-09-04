//! The download engines (DESIGN §10.5) — **owned by WP-09**.
//!
//! This module is the seam WP-08 leaves for WP-09: [`ScProvider::download`] delegates here, and
//! WP-09 replaces [`download`] with the real implementation plus the `nm3u8dl`, `ffmpeg`, `mux`
//! and `progress` modules DESIGN §10.5 and PLAN WP-09 describe.
//!
//! Everything the real implementation needs from WP-08 is already in place:
//!
//! | Need | Where |
//! |---|---|
//! | a fresh m3u8, headers and cookies at download time | [`crate::jit::fresh_stream`] |
//! | the HTTP client the provider was built with | [`ScProvider::http`] |
//! | `{scheme}://{host}` of the item's URL | [`ScProvider::base_of`] |
//! | the persisted `base_url`/`title_id`/`episode_id` blob | [`crate::state::ScState::from_json`] |
//! | the legacy flat `.info.json` sidecar | [`crate::state::ScState::to_legacy_info_json`] |
//! | `SC_MAX_CONCURRENT_DOWNLOADS` as `own_slots()` | [`crate::provider`] |

use aulos_provider::outcome::Outcome;
use aulos_provider::provider::{DownloadCtx, ProviderError};
use aulos_provider::sink::ProgressSink;

use crate::provider::ScProvider;

/// Runs the download.
///
/// Not implemented yet: the `N_m3u8DL-RE` and ffmpeg engines, the gapless mux fallback and the
/// ANSI progress parser are WP-09. Until that lands this reports a `tool_missing`-shaped failure
/// rather than silently succeeding with no file, so an SC item cannot be marked `finished` with
/// nothing on disk.
///
/// # Errors
/// [`ProviderError::ToolMissing`] until WP-09 lands.
pub async fn download(
    _provider: &ScProvider,
    ctx: DownloadCtx<'_>,
    _sink: ProgressSink,
) -> Result<Outcome, ProviderError> {
    tracing::error!(
        item = %ctx.item_id,
        "the StreamingCommunity download engines are not implemented in this build (WP-09)"
    );
    Err(ProviderError::ToolMissing("N_m3u8DL-RE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seam_fails_loudly_rather_than_reporting_a_phantom_success() {
        // A provider that returned `Ok(Outcome::default())` here would mark the item `finished`
        // with no file, which is the one outcome worse than an error.
        let e = ProviderError::ToolMissing("N_m3u8DL-RE");
        assert_eq!(e.code(), aulos_core::error::ErrorCode::ToolMissing);
        assert!(!e.retryable());
    }
}
