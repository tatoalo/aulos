//! Relative-path newtypes and the containment check (DESIGN §4.5, §16.6).
//!
//! The legacy server compared a resolved path against its base with `str.startswith`, which let
//! `/downloads-evil` pass as inside `/downloads`. [`contain`] compares **components** of
//! canonicalised paths instead, so that bug is not expressible, and it rejects a symlink that
//! escapes the base.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::selection::DownloadType;

/// A validated relative **directory**: the request's `folder` (DESIGN §4.3).
///
/// Invariants: non-empty, `/`-separated, no `..` component, no leading separator, no drive
/// prefix, no trailing separator. Backslashes are rejected rather than translated, because on the
/// Linux target `\` is a legal file-name character and silently rewriting it would rename files.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RelDir(Arc<str>);

/// A validated relative **file** path: an item's produced `filename`, relative to its download
/// root (DESIGN §4.5).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct RelPath(Arc<str>);

macro_rules! rel_newtype {
    ($t:ty, $what:literal) => {
        impl $t {
            /// Validates and wraps a relative path.
            ///
            /// # Errors
            /// [`PathError::Empty`], [`PathError::NotRelative`], [`PathError::ParentTraversal`] or
            /// [`PathError::Backslash`] per the type's invariants.
            pub fn parse(s: &str) -> Result<Self, PathError> {
                let s = validate_relative(s, $what)?;
                Ok(Self(s.into()))
            }

            /// The path as a `/`-separated string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The path as a [`Path`].
            #[must_use]
            pub fn as_path(&self) -> &Path {
                Path::new(&*self.0)
            }
        }

        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!(stringify!($t), "({:?})"), &*self.0)
            }
        }

        impl std::str::FromStr for $t {
            type Err = PathError;
            fn from_str(s: &str) -> Result<Self, PathError> {
                Self::parse(s)
            }
        }

        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }

        impl AsRef<Path> for $t {
            fn as_ref(&self) -> &Path {
                self.as_path()
            }
        }
    };
}

rel_newtype!(RelDir, "folder");
rel_newtype!(RelPath, "filename");

/// Shared validation for both newtypes: returns the normalised `/`-separated form.
fn validate_relative(s: &str, what: &'static str) -> Result<String, PathError> {
    if s.is_empty() {
        return Err(PathError::Empty { what });
    }
    if s.contains('\\') {
        return Err(PathError::Backslash {
            what,
            value: s.into(),
        });
    }
    if s.starts_with('/') {
        return Err(PathError::NotRelative {
            what,
            value: s.into(),
        });
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in s.split('/') {
        match seg {
            "" | "." => {} // collapse `a//b` and `./a`
            ".." => {
                return Err(PathError::ParentTraversal {
                    what,
                    value: s.into(),
                });
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(PathError::Empty { what });
    }
    Ok(parts.join("/"))
}

/// The four filesystem roots the process is allowed to write to (DESIGN §17.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// `DOWNLOAD_DIR` — video and everything that is not audio.
    pub download: PathBuf,
    /// `AUDIO_DOWNLOAD_DIR` — used when `download_type == audio`.
    pub audio_download: PathBuf,
    /// `TEMP_DIR` — yt-dlp `paths.temp`, `N_m3u8DL-RE --tmp-dir`.
    pub temp: PathBuf,
    /// `STATE_DIR` — importer input, `cookies.txt`, the default database directory.
    pub state: PathBuf,
}

impl Paths {
    /// The download root an item of this type lands under.
    #[must_use]
    pub fn root_for(&self, download_type: DownloadType) -> &Path {
        match download_type {
            DownloadType::Audio => &self.audio_download,
            _ => &self.download,
        }
    }

    /// The absolute, containment-checked output directory for an item.
    ///
    /// # Errors
    /// Any [`PathError`] [`contain`] can produce — in particular
    /// [`PathError::Escapes`] when `folder` resolves outside the root through a symlink.
    pub fn out_dir(
        &self,
        download_type: DownloadType,
        folder: Option<&RelDir>,
    ) -> Result<PathBuf, PathError> {
        let root = self.root_for(download_type);
        match folder {
            None => contain(root, Path::new("")),
            Some(f) => contain(root, f.as_path()),
        }
    }
}

/// Resolves `candidate` against `base` and proves the result is **inside** `base`.
///
/// `base` must exist; it is canonicalised, so symlinks and `..` in the configured root are
/// resolved once. `candidate` may be relative (joined onto `base`) or absolute (checked as-is),
/// and may name a directory that does not exist yet — the longest existing ancestor is
/// canonicalised and the remaining components are appended, so a symlink anywhere along the
/// existing part cannot smuggle the target out of `base`.
///
/// Containment is a **component-wise** prefix test ([`Path::starts_with`]), which is why
/// `/downloads-evil` is rejected against `/downloads` where the legacy `startswith` accepted it.
///
/// # Errors
/// - [`PathError::ParentTraversal`] if `candidate` contains a `..` component.
/// - [`PathError::Base`] if `base` cannot be canonicalised (usually: it does not exist).
/// - [`PathError::Escapes`] if the resolved path is not inside `base`.
/// - [`PathError::Io`] for any other filesystem error.
pub fn contain(base: &Path, candidate: &Path) -> Result<PathBuf, PathError> {
    for c in candidate.components() {
        if c == Component::ParentDir {
            return Err(PathError::ParentTraversal {
                what: "path",
                value: candidate.to_string_lossy().into_owned().into_boxed_str(),
            });
        }
    }

    let base_c = base.canonicalize().map_err(|source| PathError::Base {
        path: base.to_path_buf(),
        source,
    })?;

    let target = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base_c.join(candidate)
    };

    let resolved = resolve_longest_existing(&target)?;

    if resolved.starts_with(&base_c) {
        Ok(resolved)
    } else {
        Err(PathError::Escapes {
            base: base_c,
            candidate: resolved,
        })
    }
}

/// Canonicalises the longest existing ancestor of `target` and re-appends the missing tail.
fn resolve_longest_existing(target: &Path) -> Result<PathBuf, PathError> {
    let mut probe = target.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();

    loop {
        match probe.canonicalize() {
            Ok(c) => {
                let mut out = c;
                for seg in tail.iter().rev() {
                    out.push(seg);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(name) = probe.file_name().map(std::ffi::OsStr::to_os_string) else {
                    // No file name left: the whole path (including its root) is missing.
                    return Err(PathError::Io {
                        path: target.to_path_buf(),
                        source: e,
                    });
                };
                tail.push(name);
                if !probe.pop() {
                    return Err(PathError::Io {
                        path: target.to_path_buf(),
                        source: e,
                    });
                }
            }
            Err(source) => {
                return Err(PathError::Io {
                    path: probe,
                    source,
                });
            }
        }
    }
}

/// Characters that are invalid in a Windows/NTFS path component.
const WINDOWS_INVALID: [char; 7] = ['\\', ':', '*', '?', '"', '<', '>'];

/// Replaces characters that are invalid in a Windows path component with `_`.
///
/// A line-by-line port of legacy `_sanitize_path_component` (`app/ytdl.py`): the character class
/// is exactly `[\\:*?"<>|]`, applied before playlist and channel titles are substituted into an
/// output template, so a download does not fail on an NTFS-mounted volume. Forward slash is
/// deliberately **not** in the set — legacy did not sanitise it, and a template may legitimately
/// produce a nested path.
#[must_use]
pub fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '|' || WINDOWS_INVALID.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Failures of path validation and containment.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The value was empty, or collapsed to nothing.
    #[error("{what} must not be empty")]
    Empty {
        /// Which field was being validated.
        what: &'static str,
    },
    /// The value contained a backslash, which is a legal file-name character on Linux.
    #[error("{what} must not contain a backslash: {value:?}")]
    Backslash {
        /// Which field was being validated.
        what: &'static str,
        /// The offending value.
        value: Box<str>,
    },
    /// The value was absolute where a relative path was required.
    #[error("{what} must be relative: {value:?}")]
    NotRelative {
        /// Which field was being validated.
        what: &'static str,
        /// The offending value.
        value: Box<str>,
    },
    /// The value contained a `..` component.
    #[error("{what} must not contain \"..\": {value:?}")]
    ParentTraversal {
        /// Which field was being validated.
        what: &'static str,
        /// The offending value.
        value: Box<str>,
    },
    /// The base directory could not be canonicalised.
    #[error("base directory {path} is unusable: {source}")]
    Base {
        /// The configured base.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The resolved path is outside the base directory.
    #[error("{candidate} is outside {base}")]
    Escapes {
        /// The canonicalised base.
        base: PathBuf,
        /// The resolved candidate.
        candidate: PathBuf,
    },
    /// Any other filesystem error.
    #[error("path {path} is unusable: {source}")]
    Io {
        /// The path being resolved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

impl PathError {
    /// The wire error code a path failure maps to.
    ///
    /// Everything that is the *request's* fault is `folder_invalid`, matching DESIGN §5
    /// ("containment violation, missing dir, or `CUSTOM_DIRS=false`"); a broken configured base is
    /// the operator's fault and reports as `internal`.
    #[must_use]
    pub const fn code(&self) -> crate::error::ErrorCode {
        match self {
            Self::Base { .. } => crate::error::ErrorCode::Internal,
            _ => crate::error::ErrorCode::FolderInvalid,
        }
    }

    /// Path failures are never retryable.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rel_dir_accepts_and_normalises() {
        assert_eq!(RelDir::parse("Music").unwrap().as_str(), "Music");
        assert_eq!(RelDir::parse("a//b/").unwrap().as_str(), "a/b");
        assert_eq!(RelDir::parse("./a/./b").unwrap().as_str(), "a/b");
        assert_eq!(
            RelDir::parse("Shows/Season 1").unwrap().as_str(),
            "Shows/Season 1"
        );
    }

    #[test]
    fn rel_dir_rejects_traversal_and_absolutes() {
        assert!(matches!(
            RelDir::parse("../etc").unwrap_err(),
            PathError::ParentTraversal { .. }
        ));
        assert!(matches!(
            RelDir::parse("a/../../b").unwrap_err(),
            PathError::ParentTraversal { .. }
        ));
        assert!(matches!(
            RelDir::parse("/etc").unwrap_err(),
            PathError::NotRelative { .. }
        ));
        assert!(matches!(
            RelDir::parse("a\\b").unwrap_err(),
            PathError::Backslash { .. }
        ));
        assert!(matches!(
            RelDir::parse("").unwrap_err(),
            PathError::Empty { .. }
        ));
        assert!(matches!(
            RelDir::parse("//").unwrap_err(),
            PathError::NotRelative { .. }
        ));
    }

    #[test]
    fn rel_dir_serde_round_trips_and_validates() {
        let d = RelDir::parse("Music/Live").unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, "\"Music/Live\"");
        assert_eq!(serde_json::from_str::<RelDir>(&json).unwrap(), d);
        assert!(serde_json::from_str::<RelDir>("\"../x\"").is_err());
    }

    #[test]
    fn sanitize_replaces_exactly_the_legacy_class() {
        assert_eq!(
            sanitize_path_component(r#"a\b:c*d?e"f<g>h|i"#),
            "a_b_c_d_e_f_g_h_i"
        );
        // Forward slash is deliberately untouched, as in legacy.
        assert_eq!(sanitize_path_component("a/b"), "a/b");
        assert_eq!(sanitize_path_component("Nothing to do"), "Nothing to do");
    }
}
