//! The dedupe key and its policy (DESIGN §8.5).
//!
//! The key function itself lives in `aulos-store`, because the legacy importer has to compute the
//! *same* value for a pre-cutover row as the engine computes for a fresh add — two implementations
//! of a dedupe key would silently defeat dedupe for every imported URL. This module is the
//! runtime-shaped wrapper DESIGN §8.5 and PLAN WP-12 declare, plus the `(canonical, selection)`
//! pair the engine indexes on.

use std::hash::{Hash, Hasher};

use aulos_core::{ProviderId, Selection};
use url::Url;

/// The dedupe key for one target (DESIGN §8.5).
///
/// `provider_id` + `\u{1f}` + the normalised target, where the target is the provider's own
/// canonical id (`media_id`) once resolution has produced one, and otherwise the normalised URL:
/// lower-cased scheme and host, no default port, no fragment, no trailing `/`, and the per-host
/// tracking parameters (`si`, `feature`, `pp`, `utm_*`) removed.
///
/// Delegates to [`aulos_store::canonical_key`] — never a second implementation.
#[must_use]
pub fn canonical_key(provider: &ProviderId, url: &Url, media_id: Option<&str>) -> Box<str> {
    aulos_store::canonical_key(provider.as_str(), url.as_str(), media_id)
}

/// What the engine's dedupe index is keyed on (DESIGN §8.5).
///
/// The `selection` half is what makes re-adding the same video as `mp3` after pulling it as `mp4` a
/// legitimate new item rather than a duplicate.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DedupeKey {
    /// The canonical target, from [`canonical_key`].
    pub canonical: Box<str>,
    /// What was asked for.
    pub selection: Selection,
}

impl DedupeKey {
    /// Pairs a canonical target with a selection.
    #[must_use]
    pub fn new(canonical: Box<str>, selection: Selection) -> Self {
        Self {
            canonical,
            selection,
        }
    }

    /// The key for a request that has not been resolved yet: the picked provider plus the URL.
    #[must_use]
    pub fn for_url(provider: &ProviderId, url: &Url, selection: Selection) -> Self {
        Self::new(canonical_key(provider, url, None), selection)
    }
}

/// Hand-written because [`Selection`] derives `Eq` but not `Hash`, and hashing its four already
/// canonical string forms is both stable and cheap.
impl Hash for DedupeKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical.hash(state);
        self.selection.download_type.as_str().hash(state);
        self.selection.codec.as_str().hash(state);
        self.selection.format.as_str().hash(state);
        self.selection.quality.as_str().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use aulos_core::{Codec, DownloadType, FormatId, QualityId};

    use super::*;

    fn sel(format: &str) -> Selection {
        Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse(format).unwrap(),
            QualityId::parse("best").unwrap(),
        )
    }

    fn provider() -> ProviderId {
        ProviderId::parse("ytdlp").unwrap()
    }

    #[test]
    fn the_key_delegates_to_the_store_implementation() {
        let url = Url::parse("https://www.YouTube.com/watch?v=abc&si=zzz").unwrap();
        assert_eq!(
            &*canonical_key(&provider(), &url, None),
            &*aulos_store::canonical_key("ytdlp", url.as_str(), None)
        );
    }

    #[test]
    fn a_resolved_media_id_collapses_the_url_variants() {
        let a = Url::parse("https://youtu.be/abc").unwrap();
        let b = Url::parse("https://www.youtube.com/watch?v=abc&t=30").unwrap();
        assert_eq!(
            canonical_key(&provider(), &a, Some("abc")),
            canonical_key(&provider(), &b, Some("abc"))
        );
        assert_ne!(
            canonical_key(&provider(), &a, None),
            canonical_key(&provider(), &b, None)
        );
    }

    #[test]
    fn the_selection_is_part_of_the_key() {
        let url = Url::parse("https://example.test/v").unwrap();
        let mp4 = DedupeKey::for_url(&provider(), &url, sel("mp4"));
        let mp3 = DedupeKey::for_url(&provider(), &url, sel("mp3"));
        assert_ne!(mp4, mp3);

        let mut index: HashMap<DedupeKey, u8> = HashMap::new();
        index.insert(mp4.clone(), 1);
        index.insert(mp3, 2);
        assert_eq!(index.len(), 2, "the two selections are two keys");
        assert_eq!(index.get(&mp4), Some(&1), "and the hash agrees with Eq");
    }
}
