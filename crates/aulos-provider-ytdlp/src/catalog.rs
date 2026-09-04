//! The `ytdlp` format/quality catalog (DESIGN §6.6).
//!
//! The catalog *data* lives in [`aulos_core::catalog`], not here, and that is deliberate: it is a
//! wire type served by `GET api/v2/capabilities` and `GET api/v2/catalog`, it is what
//! `DownloadRequest` validation looks selections up in, and the Telegram keyboard is a documented
//! projection of it ([`aulos_core::FormatCatalog::bot_formats`]). One catalogue, one definition,
//! upstream of everything that reads it.
//!
//! What this module owns is the *provider's* view of it: the `Arc` the [`Provider::catalog`]
//! implementation returns, and the assertion — in `tests/golden_formats.rs` — that every tuple the
//! catalog admits has a golden selector vector, so a catalog entry cannot be added without a
//! proof of what it produces.
//!
//! [`Provider::catalog`]: aulos_provider::Provider::catalog

use std::sync::Arc;

use aulos_core::catalog::{FormatCatalog, YTDLP_CATALOG};

/// The process-wide `ytdlp` catalog.
///
/// Built once on first use and shared: two cached HTTP endpoints serve it and it is immutable for
/// the life of the process, so every caller clones the `Arc` rather than the tree.
#[must_use]
pub fn ytdlp_catalog() -> Arc<FormatCatalog> {
    Arc::clone(&YTDLP_CATALOG)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_shared_not_rebuilt() {
        let a = ytdlp_catalog();
        let b = ytdlp_catalog();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.provider.as_str(), "ytdlp");
    }
}
