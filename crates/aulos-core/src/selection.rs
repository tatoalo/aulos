//! What the user asked for: download type, codec, format and quality (DESIGN §4.3, §4.6).

use std::fmt;
use std::sync::Arc;

use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};

/// The four kinds of thing this server downloads.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadType {
    /// A video file.
    Video,
    /// An audio-only file. Lands under `AUDIO_DOWNLOAD_DIR`.
    Audio,
    /// A subtitle file.
    Captions,
    /// A thumbnail image.
    Thumbnail,
}

impl DownloadType {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Captions => "captions",
            Self::Thumbnail => "thumbnail",
        }
    }

    /// Every value, in the order legacy's `VALID_DOWNLOAD_TYPES` sorts to.
    pub const ALL: [Self; 4] = [Self::Audio, Self::Captions, Self::Thumbnail, Self::Video];

    /// Parses a wire string.
    #[must_use]
    pub fn from_str_exact(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == s)
    }
}

impl fmt::Display for DownloadType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The video codec preference. `Auto` for every non-video download type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// Let the provider decide.
    #[default]
    Auto,
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    H265,
    /// AV1.
    Av1,
    /// VP9.
    Vp9,
}

impl Codec {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::H264 => "h264",
            Self::H265 => "h265",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
        }
    }

    /// Every value, in the order legacy's `VALID_VIDEO_CODECS` sorts to.
    pub const ALL: [Self; 5] = [Self::Auto, Self::Av1, Self::H264, Self::H265, Self::Vp9];

    /// Parses a wire string.
    #[must_use]
    pub fn from_str_exact(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A catalog format id, e.g. `"mp4"` or `"srt"` (DESIGN §6.6). Validated against the selected
/// provider's catalog, never against a hard-coded match arm.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct FormatId(Arc<str>);

/// A catalog quality id, e.g. `"best"`, `"1080"` or `"best_remux"` (DESIGN §6.6).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct QualityId(Arc<str>);

/// A provider id: `"ytdlp"`, `"streamingcommunity"`, `"command:<name>"` or `"fake"`
/// (DESIGN §6.1).
///
/// Declared here rather than in `aulos-provider` because `Item.provider`, `ItemView.provider` and
/// `DownloadRequest.provider_hint` all name it, and `aulos-core` is downstream of nothing.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ProviderId(Arc<str>);

macro_rules! id_newtype {
    ($t:ty, $what:literal, $max:literal) => {
        impl $t {
            /// Validates and wraps a catalog id.
            ///
            /// Accepted shape: 1..=$max characters of `[A-Za-z0-9._:-]`. Deliberately permissive
            /// (a plugin picks its own ids) but tight enough that an id is safe in a log line, a
            /// URL query and a `cfg:` callback payload.
            ///
            /// # Errors
            /// [`SelectionError::BadId`] when the shape does not match.
            pub fn parse(s: &str) -> Result<Self, SelectionError> {
                let ok = !s.is_empty()
                    && s.len() <= $max
                    && s.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
                    });
                if ok {
                    Ok(Self(s.into()))
                } else {
                    Err(SelectionError::BadId {
                        what: $what,
                        value: s.into(),
                    })
                }
            }

            /// The id as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The id as a shared string, for cheap cloning into an `ItemView`.
            #[must_use]
            pub fn as_arc(&self) -> Arc<str> {
                Arc::clone(&self.0)
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($t), "({:?})"), &*self.0)
            }
        }

        impl std::str::FromStr for $t {
            type Err = SelectionError;
            fn from_str(s: &str) -> Result<Self, SelectionError> {
                Self::parse(s)
            }
        }

        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                Self::parse(&raw).map_err(|_| {
                    DeError::invalid_value(Unexpected::Str(&raw), &concat!("a ", $what, " id"))
                })
            }
        }

        impl PartialEq<str> for $t {
            fn eq(&self, other: &str) -> bool {
                &*self.0 == other
            }
        }

        impl PartialEq<&str> for $t {
            fn eq(&self, other: &&str) -> bool {
                &*self.0 == *other
            }
        }
    };
}

id_newtype!(FormatId, "format", 32);
id_newtype!(QualityId, "quality", 32);
id_newtype!(ProviderId, "provider", 64);

/// The four-field selection: what to download and in what shape (DESIGN §4.3).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Selection {
    /// Video, audio, captions or thumbnail.
    pub download_type: DownloadType,
    /// The video codec preference. Forced to `auto` for non-video types, as legacy did.
    pub codec: Codec,
    /// A catalog format id.
    pub format: FormatId,
    /// A catalog quality id.
    pub quality: QualityId,
}

impl Selection {
    /// Constructs a selection from already-validated parts.
    #[must_use]
    pub const fn new(
        download_type: DownloadType,
        codec: Codec,
        format: FormatId,
        quality: QualityId,
    ) -> Self {
        Self {
            download_type,
            codec,
            format,
            quality,
        }
    }

    /// The wire projection. Structurally identical; a separate type so the diff macro of
    /// DESIGN §15.1 can assert it never appears in a `delta`.
    #[must_use]
    pub fn to_view(&self) -> SelectionView {
        SelectionView {
            download_type: self.download_type,
            codec: self.codec,
            format: self.format.as_arc(),
            quality: self.quality.as_arc(),
        }
    }
}

/// `Item.selection` on the wire: four keys, all four always strings (PROTOCOL §2.3).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SelectionView {
    /// `"video" | "audio" | "captions" | "thumbnail"`.
    pub download_type: DownloadType,
    /// `"auto" | "h264" | "h265" | "av1" | "vp9"`.
    pub codec: Codec,
    /// A catalog format id.
    pub format: Arc<str>,
    /// A catalog quality id.
    pub quality: Arc<str>,
}

/// Failures constructing a selection.
#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    /// An id did not match the accepted shape.
    #[error("{what} id {value:?} is not a valid catalog id")]
    BadId {
        /// Which id was being parsed.
        what: &'static str,
        /// The offending value.
        value: Box<str>,
    },
}

impl SelectionError {
    /// The wire error code.
    #[must_use]
    pub const fn code(&self) -> crate::error::ErrorCode {
        crate::error::ErrorCode::ValidationFailed
    }

    /// Never retryable.
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
    fn download_type_and_codec_serialise_lowercase() {
        for d in DownloadType::ALL {
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                format!("\"{}\"", d.as_str())
            );
            assert_eq!(DownloadType::from_str_exact(d.as_str()), Some(d));
        }
        for c in Codec::ALL {
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.as_str())
            );
            assert_eq!(Codec::from_str_exact(c.as_str()), Some(c));
        }
        assert_eq!(DownloadType::from_str_exact("Video"), None);
    }

    #[test]
    fn format_and_quality_ids_validate() {
        assert_eq!(FormatId::parse("mp4").unwrap().as_str(), "mp4");
        assert_eq!(
            QualityId::parse("best_remux").unwrap().as_str(),
            "best_remux"
        );
        assert_eq!(
            ProviderId::parse("command:bandcamp").unwrap().as_str(),
            "command:bandcamp"
        );
        for bad in ["", "a b", "a/b", "a\"b", &"x".repeat(33)] {
            assert!(FormatId::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn selection_view_round_trips() {
        let s = Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("1080").unwrap(),
        );
        let v = s.to_view();
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["download_type"], "video");
        assert_eq!(json["codec"], "auto");
        assert_eq!(json["format"], "mp4");
        assert_eq!(json["quality"], "1080");
        assert_eq!(json.as_object().unwrap().len(), 4);
    }
}
