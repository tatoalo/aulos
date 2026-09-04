//! The client-facing catalog for this provider (DESIGN §6.6).
//!
//! StreamingCommunity serves exactly one HLS rendition per title: there is no format list, no
//! height ladder and no codec choice, and the file is always a remuxed `mp4`. So instead of
//! advertising the nine-quality `ytdlp` matrix and then ignoring it, the catalog collapses to one
//! **advisory** `mp4`/`best` entry labelled "Source", with a notice the picker can show verbatim.
//! Paste an SC link into the iOS client and the quality control honestly greys out with an
//! explanation — with no client release.
//!
//! [`NamingPolicy::Provider`] is the other half of that honesty: this provider names the file
//! itself (`<sanitised title>.mp4`) and ignores `OUTPUT_TEMPLATE*` entirely, because existing
//! Jellyfin libraries depend on those paths (DESIGN §10.5).

use std::sync::{Arc, LazyLock};

use aulos_core::catalog::{
    DownloadTypeSpec, FormatCatalog, FormatFlags, FormatSpec, NamingPolicy, QualitySpec,
};

/// The notice shown against the single format, verbatim.
pub const SOURCE_NOTICE: &str = "StreamingCommunity serves one stream per title. The format, quality and codec you pick are \
     recorded but not applied: the download is always the source HLS rendition, remuxed to MP4.";

/// The notice shown against the single quality.
pub const SOURCE_QUALITY_NOTICE: &str = "Whatever the site serves — usually 1080p, when available.";

/// The catalog version, bumped on any change here; it is part of the `ETag`.
pub const CATALOG_VERSION: u32 = 1;

static CATALOG: LazyLock<Arc<FormatCatalog>> = LazyLock::new(|| Arc::new(build()));

/// The one shared catalog instance.
///
/// Returned by `Arc` because two cached HTTP endpoints serve it and it is immutable for the life
/// of the process.
#[must_use]
pub fn sc_catalog() -> Arc<FormatCatalog> {
    Arc::clone(&CATALOG)
}

fn build() -> FormatCatalog {
    let source = FormatSpec {
        id: "mp4".into(),
        label: "Source".into(),
        qualities: vec![QualitySpec {
            id: "best".into(),
            label: "Source".into(),
            notice: Some(SOURCE_QUALITY_NOTICE.into()),
        }],
        default_quality: "best".into(),
        // Empty means "codec does not apply": send `"auto"` and hide the control.
        codecs: vec![],
        notice: Some(SOURCE_NOTICE.into()),
        flags: FormatFlags {
            // The server accepts the choice but does not honour it — the definition of advisory.
            advisory: true,
            // Both engines remux through ffmpeg, and the gapless fallback needs it too.
            requires_ffmpeg: true,
            lossy_remux: false,
            slow: false,
        },
    };
    FormatCatalog {
        provider: crate::provider_id(),
        version: CATALOG_VERSION,
        naming: NamingPolicy::Provider,
        download_types: vec![DownloadTypeSpec {
            id: "video".into(),
            label: "Video".into(),
            formats: vec![source],
            default_format: "mp4".into(),
            // No `folder`, `custom_name_prefix` or `chapter_template`: `NamingPolicy::Provider`
            // means this provider names and places the file, so advertising them would be a lie.
            options: vec![],
        }],
    }
}

#[cfg(test)]
mod tests {
    use aulos_core::catalog::MergedCatalog;
    use aulos_core::selection::DownloadType;

    use super::*;

    #[test]
    fn the_catalog_is_one_advisory_source_entry() {
        let c = sc_catalog();
        assert_eq!(c.provider.as_str(), "streamingcommunity");
        assert_eq!(c.naming, NamingPolicy::Provider);
        assert_eq!(c.download_types.len(), 1);
        let dt = &c.download_types[0];
        assert_eq!(&*dt.id, "video");
        assert_eq!(dt.download_type(), Some(DownloadType::Video));
        assert_eq!(&*dt.default_format, "mp4");
        assert_eq!(dt.formats.len(), 1);
        let f = dt.format("mp4").expect("the mp4 format");
        assert_eq!(&*f.label, "Source");
        assert_eq!(&*f.default_quality, "best");
        assert_eq!(f.qualities.len(), 1);
        assert_eq!(&*f.qualities[0].label, "Source");
        assert!(f.codecs.is_empty(), "codec must not apply");
        assert!(f.flags.advisory, "the picker must be told it is advisory");
        assert!(f.flags.requires_ffmpeg);
        assert_eq!(f.notice.as_deref(), Some(SOURCE_NOTICE));
        assert_eq!(
            f.quality("best").and_then(|q| q.notice.as_deref()),
            Some(SOURCE_QUALITY_NOTICE)
        );
    }

    #[test]
    fn the_instance_is_shared_not_rebuilt() {
        assert!(Arc::ptr_eq(&sc_catalog(), &sc_catalog()));
    }

    #[test]
    fn the_bot_keyboard_projection_collapses_to_one_button() {
        let formats = sc_catalog().bot_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(&*formats[0].id, "mp4");
        assert_eq!(formats[0].qualities.len(), 1);
    }

    #[test]
    fn it_merges_with_the_ytdlp_catalog_without_losing_the_notice() {
        let merged =
            MergedCatalog::merge(&[Arc::new(aulos_core::catalog::ytdlp_catalog()), sc_catalog()]);
        assert_eq!(merged.providers.len(), 2);
        // `Template` wins the merge, because the ytdlp provider is the one that honours it.
        assert_eq!(merged.naming, NamingPolicy::Template);
    }
}
