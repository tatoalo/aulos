//! Ownership and removal of a queue item's scratch directory.

use std::io;
use std::path::{Path, PathBuf};

use aulos_core::{ItemId, paths::Paths};

#[derive(Clone)]
pub(crate) struct ScratchDir {
    root: PathBuf,
    id: ItemId,
    protected: Vec<PathBuf>,
}

impl ScratchDir {
    pub(crate) fn new(paths: &Paths, id: ItemId, output: Option<&Path>) -> Self {
        let mut protected = vec![paths.download.clone(), paths.audio_download.clone()];
        protected.extend(output.map(Path::to_path_buf));
        Self {
            root: paths.temp.clone(),
            id,
            protected,
        }
    }

    pub(crate) fn remove(&self) -> bool {
        if let Err(error) = self.remove_checked() {
            tracing::warn!(item = %self.id, path = %self.root.join(self.id.to_string()).display(),
                %error, "could not remove the item temp directory");
            return false;
        }
        true
    }

    fn remove_checked(&self) -> io::Result<()> {
        // Construct the path from the typed ULID, never from a provider-supplied filename.
        let path = self.root.join(self.id.to_string());
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let refuse = || io::Error::other("refusing to remove an unowned or protected temp path");
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(refuse());
        }
        let root = self.root.canonicalize()?;
        let path = path.canonicalize()?;
        if path != root.join(self.id.to_string()) || path.parent() != Some(root.as_path()) {
            return Err(refuse());
        }
        for protected in &self.protected {
            let protected = protected.canonicalize()?;
            if protected.starts_with(&path) {
                return Err(refuse());
            }
        }
        // remove_dir_all unlinks nested symlinks without following their targets.
        match std::fs::remove_dir_all(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> Paths {
        Paths {
            download: root.join("downloads"),
            audio_download: root.join("downloads"),
            temp: root.join("downloads/.aulos-tmp"),
            state: root.join("state"),
        }
    }

    #[test]
    fn removes_foreign_files_but_keeps_the_output_and_other_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = paths(dir.path());
        paths.temp = paths.download.clone();
        let id = ItemId::new();
        let scratch = paths.temp.join(id.to_string());
        let other = paths.temp.join(ItemId::new().to_string());
        std::fs::create_dir_all(scratch.join("episode")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(scratch.join("episode/raw.xml"), b"Jellyfin playlist").unwrap();
        let output = paths.download.join("episode.mp4");
        std::fs::write(&output, b"movie").unwrap();
        let owned = ScratchDir::new(&paths, id, Some(&paths.download));
        owned.remove_checked().unwrap();
        owned.remove_checked().unwrap();
        assert!(!scratch.exists());
        assert!(other.exists());
        assert_eq!(std::fs::read(output).unwrap(), b"movie");
    }

    #[test]
    fn refuses_download_audio_and_output_directories_including_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let id = ItemId::new();
        let scratch = paths.temp.join(id.to_string());
        std::fs::create_dir_all(scratch.join("output")).unwrap();
        for output in [&scratch, &scratch.join("output")] {
            assert!(
                ScratchDir::new(&paths, id, Some(output))
                    .remove_checked()
                    .is_err()
            );
        }
        for audio in [false, true] {
            let mut configured = paths.clone();
            if audio {
                configured.audio_download = scratch.clone();
            } else {
                configured.download = scratch.clone();
            }
            assert!(
                ScratchDir::new(&configured, id, None)
                    .remove_checked()
                    .is_err()
            );
        }
        assert!(scratch.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_item_directory_and_does_not_follow_nested_links() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.temp).unwrap();
        let id = ItemId::new();
        let scratch = paths.temp.join(id.to_string());
        let output = paths.download.join("episode.mp4");
        std::fs::write(&output, b"movie").unwrap();
        symlink(&paths.download, &scratch).unwrap();
        let owned = ScratchDir::new(&paths, id, None);
        assert!(owned.remove_checked().is_err());
        std::fs::remove_file(&scratch).unwrap();
        std::fs::create_dir(&scratch).unwrap();
        symlink(&paths.download, scratch.join("foreign-link")).unwrap();
        owned.remove_checked().unwrap();
        assert!(output.exists());
    }
}
