//! What a successful download reports back (DESIGN §6.1).

use aulos_core::item::FileRef;
use aulos_core::paths::RelPath;
use serde_json::Value;

/// The result of [`crate::provider::Provider::download`].
///
/// Everything is optional because a provider is allowed to succeed without knowing everything:
/// a `newest_in_dir` plugin has a filename but no size until the engine stats it, and only the
/// providers that carry rich metadata fill `entry_final`.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Outcome {
    /// The produced file, relative to the item's download root.
    pub filename: Option<RelPath>,
    /// Bytes on disk.
    pub size: Option<u64>,
    /// Per-chapter files, when `split_by_chapters` was requested.
    pub chapter_files: Vec<FileRef>,
    /// Subtitle tracks written alongside the media file.
    pub subtitle_files: Vec<FileRef>,
    /// The final provider metadata, for the NFO hook (DESIGN §13.2).
    pub entry_final: Option<Value>,
}

impl Outcome {
    /// The ordinary outcome: one file of a known size.
    #[must_use]
    pub fn file(filename: RelPath, size: u64) -> Self {
        Self {
            filename: Some(filename),
            size: Some(size),
            ..Self::default()
        }
    }

    /// Adds a chapter file.
    #[must_use]
    pub fn with_chapter(mut self, f: FileRef) -> Self {
        self.chapter_files.push(f);
        self
    }

    /// Adds a subtitle file.
    #[must_use]
    pub fn with_subtitle(mut self, f: FileRef) -> Self {
        self.subtitle_files.push(f);
        self
    }

    /// Attaches the final provider metadata.
    #[must_use]
    pub fn with_entry_final(mut self, v: Value) -> Self {
        self.entry_final = Some(v);
        self
    }

    /// Every auxiliary file this outcome produced, chapters first.
    pub fn artifacts(&self) -> impl Iterator<Item = &FileRef> {
        self.chapter_files.iter().chain(self.subtitle_files.iter())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn file_ref(name: &str) -> FileRef {
        FileRef {
            filename: name.into(),
            size: Some(10),
            download_url: None,
            lang: None,
        }
    }

    #[test]
    fn the_default_outcome_is_empty() {
        let o = Outcome::default();
        assert!(o.filename.is_none());
        assert!(o.size.is_none());
        assert_eq!(o.artifacts().count(), 0);
    }

    #[test]
    fn builders_accumulate_artifacts_in_order() {
        let o = Outcome::file(RelPath::parse("Clip.mp4").unwrap(), 1234)
            .with_chapter(file_ref("Clip - 01.mp4"))
            .with_subtitle(file_ref("Clip.en.srt"))
            .with_entry_final(serde_json::json!({ "id": "x" }));
        assert_eq!(o.filename.as_ref().map(RelPath::as_str), Some("Clip.mp4"));
        assert_eq!(o.size, Some(1234));
        let names: Vec<_> = o.artifacts().map(|f| f.filename.to_string()).collect();
        assert_eq!(names, ["Clip - 01.mp4", "Clip.en.srt"]);
        assert_eq!(o.entry_final.unwrap()["id"], "x");
    }
}
