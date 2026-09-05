//! The format / quality catalog (DESIGN §6.6, PROTOCOL §4.5–§4.6).
//!
//! Validation is catalog-driven: a `DownloadRequest` is checked by looking its four fields up in
//! the selected provider's catalog, not against a hard-coded match arm. Adding a format to
//! `ytdlp` is one edit here; a plugin gets validation, a picker and a Telegram keyboard for free.
//!
//! There is exactly **one** catalogue in the codebase. The legacy Telegram keyboard list and the
//! flat `capabilities.formats` array are both documented *projections* of it
//! ([`FormatCatalog::bot_formats`], [`FormatCatalog::flat_formats`]).

use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::selection::{Codec, DownloadType, ProviderId};

/// Whether the file name comes from `OUTPUT_TEMPLATE*` or from the provider itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamingPolicy {
    /// `OUTPUT_TEMPLATE*` applies, so `custom_name_prefix` and `chapter_template` are meaningful.
    Template,
    /// The provider names the file and ignores those templates (StreamingCommunity does), so a
    /// picker should grey them out.
    Provider,
}

/// One provider's client-facing catalog.
///
/// `GET api/v2/catalog` wraps this with `etag`, `match` and `runner_up`; those three belong to the
/// *response*, not to the catalog, so they are added by `aulos-api` (WP-14).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FormatCatalog {
    /// Whose catalog this is.
    pub provider: ProviderId,
    /// Bumped on any change; part of the `ETag`.
    pub version: u32,
    /// How output files are named.
    pub naming: NamingPolicy,
    /// Never empty.
    pub download_types: Vec<DownloadTypeSpec>,
}

/// One download type and everything it offers.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct DownloadTypeSpec {
    /// One of `"video" | "audio" | "captions" | "thumbnail"`.
    pub id: Box<str>,
    /// Display label.
    pub label: Box<str>,
    /// Never empty.
    pub formats: Vec<FormatSpec>,
    /// Always an id present in `formats`.
    pub default_format: Box<str>,
    /// Extra request-body keys this download type accepts (PROTOCOL §4.6).
    pub options: Vec<OptionSpec>,
}

impl DownloadTypeSpec {
    /// The typed download type, if `id` is one of the four known values.
    #[must_use]
    pub fn download_type(&self) -> Option<DownloadType> {
        DownloadType::from_str_exact(&self.id)
    }

    /// Looks a format up by id.
    #[must_use]
    pub fn format(&self, id: &str) -> Option<&FormatSpec> {
        self.formats.iter().find(|f| &*f.id == id)
    }
}

/// One format and its qualities.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct FormatSpec {
    /// What the client sends as `format`.
    pub id: Box<str>,
    /// Display label.
    pub label: Box<str>,
    /// Never empty.
    pub qualities: Vec<QualitySpec>,
    /// Always an id present in `qualities`.
    pub default_quality: Box<str>,
    /// Empty means `codec` does not apply: send `"auto"` and hide the control.
    pub codecs: Vec<CodecSpec>,
    /// Display text about the format as a whole. Always safe to show verbatim.
    pub notice: Option<Box<str>>,
    /// Four booleans, all always present.
    pub flags: FormatFlags,
}

impl FormatSpec {
    /// Looks a quality up by id.
    #[must_use]
    pub fn quality(&self, id: &str) -> Option<&QualitySpec> {
        self.qualities.iter().find(|q| &*q.id == id)
    }
}

/// One quality option.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct QualitySpec {
    /// What the client sends as `quality`.
    pub id: Box<str>,
    /// Display label.
    pub label: Box<str>,
    /// Per-quality display text — where `worst` and `best_remux` tell the truth.
    pub notice: Option<Box<str>>,
}

/// One codec option.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct CodecSpec {
    /// What the client sends as `codec`.
    pub id: Box<str>,
    /// Display label.
    pub label: Box<str>,
}

/// UI-relevant properties of a format (PROTOCOL §4.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct FormatFlags {
    /// The server accepts the choice but may not honour it (a provider with one rendition).
    pub advisory: bool,
    /// Needs ffmpeg in the image.
    pub requires_ffmpeg: bool,
    /// The output is re-encoded, not stream-copied.
    pub lossy_remux: bool,
    /// Expect a `postprocessing` phase of minutes.
    pub slow: bool,
}

/// One extra request-body key a download type accepts.
///
/// This is the mechanism that lets `folder`, `custom_name_prefix`, `playlist_item_limit` and
/// friends grow without a client release: a client renders a control per entry and skips any
/// `kind` it does not understand, because every option has a server-side default.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct OptionSpec {
    /// The request body key to send, e.g. `"playlist_item_limit"`.
    pub id: Box<str>,
    /// Control label.
    pub label: Box<str>,
    /// What control to render.
    pub kind: OptionKind,
    /// The server-side default; matches `kind`.
    pub default: Value,
    /// Non-empty **only** for [`OptionKind::Enum`].
    pub choices: Vec<Choice>,
    /// One line of explanatory text.
    pub help: Option<Box<str>>,
}

/// The five control kinds, externally tagged on `type` (PROTOCOL §4.6).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OptionKind {
    /// A checkbox.
    Bool,
    /// A bounded integer. Both bounds are always present.
    Int {
        /// Inclusive lower bound.
        min: i64,
        /// Inclusive upper bound.
        max: i64,
    },
    /// A picker over `choices`.
    Enum,
    /// A text field. `pattern` may be `null`; when set it is a regex to validate against.
    Text {
        /// Validation regex, or `null`.
        pattern: Option<Box<str>>,
    },
    /// A server-relative directory. Offer `GET api/v2/custom-dirs` rather than free text.
    Path,
}

/// One entry of an [`OptionKind::Enum`] picker.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Choice {
    /// The value to send.
    pub id: Box<str>,
    /// Display label.
    pub label: Box<str>,
}

/// One entry of the legacy Telegram keyboard's flat nine-format list (DESIGN §12.2).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BotFormat {
    /// The button id — the legacy flat format name, e.g. `"thumbnail"` (not the catalog `"jpg"`).
    pub id: Box<str>,
    /// The quality ids this button offers, in legacy order.
    pub qualities: Vec<Box<str>>,
}

/// One entry of the flat `capabilities.formats` array (PROTOCOL §4.5).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FlatFormat {
    /// The catalog format id.
    pub id: Box<str>,
    /// Display label. Named `text` because that is the key the shipped iOS model decodes.
    pub text: Box<str>,
    /// Which download type this format belongs to.
    pub download_type: Box<str>,
    /// The qualities it offers.
    pub qualities: Vec<FlatQuality>,
}

/// One quality of a [`FlatFormat`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FlatQuality {
    /// The catalog quality id.
    pub id: Box<str>,
    /// Display label.
    pub text: Box<str>,
}

/// The union of every registered provider's catalog, served by `GET api/v2/catalog` with no
/// `?url=` (PROTOCOL §4.6).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct MergedCatalog {
    /// The contributing providers, in registration order.
    pub providers: Vec<ProviderId>,
    /// The sum of the contributing catalog versions, so the `ETag` moves when any of them does.
    pub version: u32,
    /// `Template` unless **every** contributing catalog says `Provider`, since a merged picker
    /// must not grey out controls that some providers honour.
    pub naming: NamingPolicy,
    /// The union of the contributing download types. Registration order wins on a collision: the
    /// first provider that declares a `download_type`/`format` id owns its spec — **unless** that
    /// spec is `advisory` and a later provider declares the same id for real. An advisory format
    /// (StreamingCommunity's single "Source" `mp4`) is a placeholder the server does not honour,
    /// so it must never hide a real provider's quality ladder from a merged picker; the real spec
    /// takes the advisory one's slot, so the position in the list is still the first declaration's.
    pub download_types: Vec<DownloadTypeSpec>,
}

impl MergedCatalog {
    /// Merges catalogs in registration order.
    #[must_use]
    pub fn merge(catalogs: &[Arc<FormatCatalog>]) -> Self {
        let mut download_types: Vec<DownloadTypeSpec> = Vec::new();

        for cat in catalogs {
            for dt in &cat.download_types {
                match download_types.iter_mut().find(|d| d.id == dt.id) {
                    None => download_types.push(dt.clone()),
                    Some(existing) => {
                        for f in &dt.formats {
                            match existing.formats.iter_mut().find(|e| e.id == f.id) {
                                None => existing.formats.push(f.clone()),
                                // A real ladder replaces an advisory placeholder in place; every
                                // other collision keeps the first declaration.
                                Some(shadowed) if shadowed.flags.advisory && !f.flags.advisory => {
                                    *shadowed = f.clone();
                                }
                                Some(_) => {}
                            }
                        }
                        for o in &dt.options {
                            if !existing.options.iter().any(|e| e.id == o.id) {
                                existing.options.push(o.clone());
                            }
                        }
                    }
                }
            }
        }

        Self {
            providers: catalogs.iter().map(|c| c.provider.clone()).collect(),
            version: catalogs.iter().map(|c| c.version).sum(),
            naming: if !catalogs.is_empty()
                && catalogs.iter().all(|c| c.naming == NamingPolicy::Provider)
            {
                NamingPolicy::Provider
            } else {
                NamingPolicy::Template
            },
            download_types,
        }
    }
}

impl FormatCatalog {
    /// Looks a download type up by id.
    #[must_use]
    pub fn download_type(&self, id: &str) -> Option<&DownloadTypeSpec> {
        self.download_types.iter().find(|d| &*d.id == id)
    }

    /// Looks a download type up by its typed value.
    #[must_use]
    pub fn spec_for(&self, dt: DownloadType) -> Option<&DownloadTypeSpec> {
        self.download_type(dt.as_str())
    }

    /// The legacy Telegram keyboard list, **derived** from this catalog (DESIGN §6.6, §12.2).
    ///
    /// The projection is not the identity: the keyboard is a *flat* nine-entry list with two
    /// quirks the `cfg:` callback grammar depends on, while the catalog is keyed by download type
    /// and carries sixteen format ids.
    ///
    /// - `any` gains the `audio` pseudo-quality, which the bot maps to `(audio, m4a, best)`.
    /// - `ios` offers only `best`, even though the catalog now offers it all nine heights — the
    ///   bot is the parity surface, the API is the honest one.
    /// - the thumbnail button keeps the legacy id `thumbnail`; the catalog id is `jpg`.
    /// - caption formats are **not** reachable, because the legacy list had no caption entry.
    ///
    /// Entries whose format is absent from this catalog are skipped, so a plugin catalog yields a
    /// shorter keyboard rather than a keyboard with dead buttons.
    #[must_use]
    pub fn bot_formats(&self) -> Vec<BotFormat> {
        /// `(button id, download type, catalog format id, only offer "best")`
        const LAYOUT: [(&str, DownloadType, &str, bool); 9] = [
            ("any", DownloadType::Video, "any", false),
            ("mp4", DownloadType::Video, "mp4", false),
            ("ios", DownloadType::Video, "ios", true),
            ("m4a", DownloadType::Audio, "m4a", false),
            ("mp3", DownloadType::Audio, "mp3", false),
            ("opus", DownloadType::Audio, "opus", false),
            ("wav", DownloadType::Audio, "wav", false),
            ("flac", DownloadType::Audio, "flac", false),
            ("thumbnail", DownloadType::Thumbnail, "jpg", false),
        ];

        let mut out = Vec::with_capacity(LAYOUT.len());
        for (button, dt, format_id, only_best) in LAYOUT {
            let Some(fmt) = self.spec_for(dt).and_then(|d| d.format(format_id)) else {
                continue;
            };
            let mut qualities: Vec<Box<str>> = if only_best {
                vec!["best".into()]
            } else {
                fmt.qualities.iter().map(|q| q.id.clone()).collect()
            };
            if button == "any" {
                qualities.push("audio".into());
            }
            out.push(BotFormat {
                id: button.into(),
                qualities,
            });
        }
        out
    }

    /// The flat `capabilities.formats` array (PROTOCOL §4.5).
    ///
    /// Deliberately carries no `notice`, `flags`, `codecs` or `options` — those live on the richer
    /// per-URL catalog of PROTOCOL §4.6.
    #[must_use]
    pub fn flat_formats(&self) -> Vec<FlatFormat> {
        self.download_types
            .iter()
            .flat_map(|dt| {
                dt.formats.iter().map(move |f| FlatFormat {
                    id: f.id.clone(),
                    text: f.label.clone(),
                    download_type: dt.id.clone(),
                    qualities: f
                        .qualities
                        .iter()
                        .map(|q| FlatQuality {
                            id: q.id.clone(),
                            text: q.label.clone(),
                        })
                        .collect(),
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The `ytdlp` catalog — exactly the legacy matrix (DESIGN §6.6, Appendix A §6).
// ---------------------------------------------------------------------------

/// The pattern `subtitle_language` must match, advertised as an [`OptionKind::Text`] pattern and
/// enforced by `DownloadRequest::validate`.
pub const SUBTITLE_LANGUAGE_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9-]{0,34}$";

/// The `worst` honesty notice. The legacy quirk is kept (DESIGN Appendix B K1) but no longer lies.
const WORST_NOTICE: &str = "This selector currently resolves to the best available stream";

/// The `best_remux` notice.
const BEST_REMUX_NOTICE: &str =
    "Re-encodes audio after download (slower; fixes SponsorBlock drift)";

fn quality(id: &str, label: &str, notice: Option<&str>) -> QualitySpec {
    QualitySpec {
        id: id.into(),
        label: label.into(),
        notice: notice.map(Into::into),
    }
}

/// `best, 2160, 1440, 1080, 720, 480, 360, 240, worst` — the nine video heights.
fn video_heights() -> Vec<QualitySpec> {
    vec![
        quality("best", "Best", None),
        quality("2160", "2160p", None),
        quality("1440", "1440p", None),
        quality("1080", "1080p", None),
        quality("720", "720p", None),
        quality("480", "480p", None),
        quality("360", "360p", None),
        quality("240", "240p", None),
        quality("worst", "Worst", Some(WORST_NOTICE)),
    ]
}

fn codec_specs() -> Vec<CodecSpec> {
    Codec::ALL
        .into_iter()
        .map(|c| CodecSpec {
            id: c.as_str().into(),
            label: match c {
                Codec::Auto => "Auto",
                Codec::H264 => "H.264",
                Codec::H265 => "H.265",
                Codec::Av1 => "AV1",
                Codec::Vp9 => "VP9",
            }
            .into(),
        })
        .collect()
}

fn format(
    id: &str,
    label: &str,
    qualities: Vec<QualitySpec>,
    codecs: Vec<CodecSpec>,
    notice: Option<&str>,
    flags: FormatFlags,
) -> FormatSpec {
    FormatSpec {
        id: id.into(),
        label: label.into(),
        default_quality: "best".into(),
        qualities,
        codecs,
        notice: notice.map(Into::into),
        flags,
    }
}

fn option(
    id: &str,
    label: &str,
    kind: OptionKind,
    default: Value,
    choices: Vec<Choice>,
    help: Option<&str>,
) -> OptionSpec {
    OptionSpec {
        id: id.into(),
        label: label.into(),
        kind,
        default,
        choices,
        help: help.map(Into::into),
    }
}

/// The option set every `ytdlp` download type advertises (PROTOCOL §4.6).
///
/// `ytdl_options_presets` is left with an empty `choices` here: the real preset names come from
/// `YTDL_OPTIONS_PRESETS` and are filled in by `aulos-api` when it serves the catalog, because a
/// static constant cannot know the operator's configuration.
fn common_options() -> Vec<OptionSpec> {
    vec![
        option(
            "folder",
            "Folder",
            OptionKind::Path,
            Value::Null,
            vec![],
            Some("Subdirectory of the download root."),
        ),
        option(
            "custom_name_prefix",
            "Filename prefix",
            OptionKind::Text {
                pattern: Some(r"^[^/\\]*$".into()),
            },
            Value::String(String::new()),
            vec![],
            None,
        ),
        option(
            "playlist_item_limit",
            "Playlist limit",
            OptionKind::Int {
                min: 0,
                max: 10_000,
            },
            Value::from(0),
            vec![],
            Some("0 downloads the whole playlist."),
        ),
        option(
            "auto_start",
            "Start immediately",
            OptionKind::Bool,
            Value::Bool(true),
            vec![],
            None,
        ),
        option(
            "split_by_chapters",
            "Split by chapters",
            OptionKind::Bool,
            Value::Bool(false),
            vec![],
            Some("Writes one file per chapter."),
        ),
        option(
            "ytdl_options_presets",
            "Presets",
            OptionKind::Enum,
            Value::Array(vec![]),
            vec![],
            Some("Named yt-dlp option bundles the operator configured."),
        ),
    ]
}

/// The extra options the `captions` download type advertises.
fn caption_options() -> Vec<OptionSpec> {
    let mut opts = common_options();
    opts.push(option(
        "subtitle_language",
        "Language",
        OptionKind::Text {
            pattern: Some(SUBTITLE_LANGUAGE_PATTERN.into()),
        },
        Value::String("en".to_owned()),
        vec![],
        None,
    ));
    opts.push(option(
        "subtitle_mode",
        "Subtitle source",
        OptionKind::Enum,
        Value::String("prefer_manual".to_owned()),
        vec![
            Choice {
                id: "auto_only".into(),
                label: "Automatic only".into(),
            },
            Choice {
                id: "manual_only".into(),
                label: "Manual only".into(),
            },
            Choice {
                id: "prefer_manual".into(),
                label: "Prefer manual".into(),
            },
            Choice {
                id: "prefer_auto".into(),
                label: "Prefer automatic".into(),
            },
        ],
        None,
    ));
    opts
}

/// Builds the `ytdlp` catalog: exactly the legacy matrix, with labels, flags and honest notices.
///
/// `video`/`ios` carries **all nine heights** and `captions` carries **all seven formats**: legacy
/// accepted `{video, ios, 1080}` and composed a real `[height<=1080]` selector for it
/// (`app/main.py:610-620`, `dl_formats.get_format`), so narrowing either would reject a request
/// the legacy server accepted — and, worse, would make the v1 shim pass legacy validation and then
/// fail catalog validation.
#[must_use]
pub fn ytdlp_catalog() -> FormatCatalog {
    let video = DownloadTypeSpec {
        id: "video".into(),
        label: "Video".into(),
        default_format: "mp4".into(),
        options: common_options(),
        formats: vec![
            format(
                "any",
                "Any",
                video_heights(),
                codec_specs(),
                None,
                FormatFlags::default(),
            ),
            format(
                "mp4",
                "MP4",
                {
                    let mut q = video_heights();
                    // `best_remux` sits immediately after `best`, as in the legacy bot list.
                    q.insert(
                        1,
                        quality("best_remux", "Best (remux)", Some(BEST_REMUX_NOTICE)),
                    );
                    q
                },
                codec_specs(),
                None,
                // DESIGN §6.6 puts `slow` on `best_remux`, but `flags` is a property of a
                // `FormatSpec` and `mp4` is the only format that offers that quality, so the flag
                // lands here. The per-quality `notice` is what tells the user which choice is slow.
                FormatFlags {
                    requires_ffmpeg: true,
                    slow: true,
                    ..FormatFlags::default()
                },
            ),
            format(
                "ios",
                "iOS",
                video_heights(),
                codec_specs(),
                Some("Uses the iOS player client's format list."),
                FormatFlags::default(),
            ),
        ],
    };

    let audio = DownloadTypeSpec {
        id: "audio".into(),
        label: "Audio".into(),
        default_format: "m4a".into(),
        options: common_options(),
        formats: vec![
            format(
                "m4a",
                "M4A",
                vec![
                    quality("best", "Best", None),
                    quality("192", "192 kbps", None),
                    quality("128", "128 kbps", None),
                ],
                vec![],
                None,
                FormatFlags {
                    requires_ffmpeg: true,
                    ..FormatFlags::default()
                },
            ),
            format(
                "mp3",
                "MP3",
                vec![
                    quality("best", "Best", None),
                    quality("320", "320 kbps", None),
                    quality("192", "192 kbps", None),
                    quality("128", "128 kbps", None),
                ],
                vec![],
                None,
                FormatFlags {
                    requires_ffmpeg: true,
                    lossy_remux: true,
                    ..FormatFlags::default()
                },
            ),
            format(
                "opus",
                "Opus",
                vec![quality("best", "Best", None)],
                vec![],
                None,
                FormatFlags {
                    requires_ffmpeg: true,
                    lossy_remux: true,
                    ..FormatFlags::default()
                },
            ),
            format(
                "wav",
                "WAV",
                vec![quality("best", "Best", None)],
                vec![],
                None,
                FormatFlags {
                    requires_ffmpeg: true,
                    ..FormatFlags::default()
                },
            ),
            format(
                "flac",
                "FLAC",
                vec![quality("best", "Best", None)],
                vec![],
                None,
                FormatFlags {
                    requires_ffmpeg: true,
                    ..FormatFlags::default()
                },
            ),
        ],
    };

    /// The seven legacy `VALID_SUBTITLE_FORMATS`, in `app/main.py:255` order.
    const CAPTIONS: [(&str, &str); 7] = [
        ("srt", "SRT"),
        ("txt", "Text"),
        ("vtt", "VTT"),
        ("ttml", "TTML"),
        ("sbv", "SBV"),
        ("scc", "SCC"),
        ("dfxp", "DFXP"),
    ];

    let captions = DownloadTypeSpec {
        id: "captions".into(),
        label: "Subtitles".into(),
        default_format: "srt".into(),
        options: caption_options(),
        formats: CAPTIONS
            .into_iter()
            .map(|(id, label)| {
                format(
                    id,
                    label,
                    vec![quality("best", "Best", None)],
                    vec![],
                    None,
                    FormatFlags {
                        requires_ffmpeg: id != "vtt" && id != "srt",
                        ..FormatFlags::default()
                    },
                )
            })
            .collect(),
    };

    let thumbnail = DownloadTypeSpec {
        id: "thumbnail".into(),
        label: "Thumbnail".into(),
        default_format: "jpg".into(),
        options: common_options(),
        formats: vec![format(
            "jpg",
            "Thumbnail",
            vec![quality("best", "Best", None)],
            vec![],
            None,
            FormatFlags::default(),
        )],
    };

    FormatCatalog {
        provider: ytdlp_provider_id(),
        version: 1,
        naming: NamingPolicy::Template,
        download_types: vec![video, audio, captions, thumbnail],
    }
}

/// `ProviderId("ytdlp")`. The literal is valid, so the parse cannot fail.
fn ytdlp_provider_id() -> ProviderId {
    ProviderId::parse("ytdlp").unwrap_or_else(|_| unreachable!("\"ytdlp\" is a valid provider id"))
}

/// The process-wide `ytdlp` catalog. Built once; every consumer clones the `Arc`.
pub static YTDLP_CATALOG: LazyLock<Arc<FormatCatalog>> =
    LazyLock::new(|| Arc::new(ytdlp_catalog()));

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ids(f: &DownloadTypeSpec) -> Vec<&str> {
        f.formats.iter().map(|x| &*x.id).collect()
    }

    fn qids(f: &FormatSpec) -> Vec<&str> {
        f.qualities.iter().map(|x| &*x.id).collect()
    }

    #[test]
    fn the_catalog_is_the_legacy_matrix() {
        let c = ytdlp_catalog();
        assert_eq!(
            c.download_types.iter().map(|d| &*d.id).collect::<Vec<_>>(),
            ["video", "audio", "captions", "thumbnail"]
        );
        let video = c.download_type("video").unwrap();
        assert_eq!(ids(video), ["any", "mp4", "ios"]);
        let audio = c.download_type("audio").unwrap();
        assert_eq!(ids(audio), ["m4a", "mp3", "opus", "wav", "flac"]);
        let thumb = c.download_type("thumbnail").unwrap();
        assert_eq!(ids(thumb), ["jpg"]);
    }

    #[test]
    fn video_and_ios_carry_all_nine_heights() {
        let c = ytdlp_catalog();
        let video = c.download_type("video").unwrap();
        let nine = [
            "best", "2160", "1440", "1080", "720", "480", "360", "240", "worst",
        ];
        assert_eq!(qids(video.format("any").unwrap()), nine);
        assert_eq!(
            qids(video.format("ios").unwrap()),
            nine,
            "legacy accepted {{video, ios, 1080}} (app/main.py:610-620)"
        );
        assert_eq!(
            qids(video.format("mp4").unwrap()),
            [
                "best",
                "best_remux",
                "2160",
                "1440",
                "1080",
                "720",
                "480",
                "360",
                "240",
                "worst"
            ]
        );
    }

    #[test]
    fn captions_carry_all_seven_legacy_formats() {
        let c = ytdlp_catalog();
        let captions = c.download_type("captions").unwrap();
        assert_eq!(
            ids(captions),
            ["srt", "txt", "vtt", "ttml", "sbv", "scc", "dfxp"],
            "legacy VALID_SUBTITLE_FORMATS (app/main.py:255)"
        );
        for f in &captions.formats {
            assert_eq!(qids(f), ["best"]);
        }
    }

    #[test]
    fn notices_and_flags_are_honest() {
        let c = ytdlp_catalog();
        let video = c.download_type("video").unwrap();
        for id in ["any", "mp4", "ios"] {
            let worst = video.format(id).unwrap().quality("worst").unwrap();
            assert_eq!(worst.notice.as_deref(), Some(WORST_NOTICE));
        }
        let remux = video.format("mp4").unwrap().quality("best_remux").unwrap();
        assert_eq!(remux.notice.as_deref(), Some(BEST_REMUX_NOTICE));
    }

    #[test]
    fn best_remux_is_marked_slow_on_the_quality_and_the_format_requires_ffmpeg() {
        // `slow` is a *format* flag in DESIGN §6.6, and `mp4` is the only format carrying
        // `best_remux`, so it is the format that must advertise it.
        let c = ytdlp_catalog();
        let mp4 = c.download_type("video").unwrap().format("mp4").unwrap();
        assert!(mp4.flags.requires_ffmpeg);
        assert!(
            mp4.flags.slow,
            "the best_remux quality makes mp4 potentially slow"
        );
    }

    #[test]
    fn bot_formats_are_exactly_the_legacy_nine() {
        let bots = ytdlp_catalog().bot_formats();
        let expected: [(&str, &[&str]); 9] = [
            (
                "any",
                &[
                    "best", "2160", "1440", "1080", "720", "480", "360", "240", "worst", "audio",
                ],
            ),
            (
                "mp4",
                &[
                    "best",
                    "best_remux",
                    "2160",
                    "1440",
                    "1080",
                    "720",
                    "480",
                    "360",
                    "240",
                    "worst",
                ],
            ),
            ("ios", &["best"]),
            ("m4a", &["best", "192", "128"]),
            ("mp3", &["best", "320", "192", "128"]),
            ("opus", &["best"]),
            ("wav", &["best"]),
            ("flac", &["best"]),
            ("thumbnail", &["best"]),
        ];
        assert_eq!(bots.len(), 9);
        for (got, (id, qs)) in bots.iter().zip(expected) {
            assert_eq!(&*got.id, id);
            assert_eq!(
                got.qualities.iter().map(|q| &**q).collect::<Vec<_>>(),
                qs,
                "qualities for {id}"
            );
        }
    }

    #[test]
    fn flat_formats_is_the_sixteen_entry_capabilities_array() {
        let flat = ytdlp_catalog().flat_formats();
        assert_eq!(flat.len(), 16);
        assert_eq!(
            flat.iter().map(|f| &*f.id).collect::<Vec<_>>(),
            [
                "any", "mp4", "ios", "m4a", "mp3", "opus", "wav", "flac", "srt", "txt", "vtt",
                "ttml", "sbv", "scc", "dfxp", "jpg"
            ]
        );
        assert_eq!(&*flat[15].text, "Thumbnail");
        assert_eq!(&*flat[15].download_type, "thumbnail");
    }

    #[test]
    fn defaults_always_exist_in_their_lists() {
        let c = ytdlp_catalog();
        for dt in &c.download_types {
            assert!(
                dt.format(&dt.default_format).is_some(),
                "{}: default_format {} missing",
                dt.id,
                dt.default_format
            );
            for f in &dt.formats {
                assert!(
                    f.quality(&f.default_quality).is_some(),
                    "{}/{}: default_quality {} missing",
                    dt.id,
                    f.id,
                    f.default_quality
                );
            }
        }
    }

    #[test]
    fn only_video_formats_offer_codecs() {
        let c = ytdlp_catalog();
        for dt in &c.download_types {
            for f in &dt.formats {
                assert_eq!(
                    f.codecs.is_empty(),
                    &*dt.id != "video",
                    "{}/{} codecs",
                    dt.id,
                    f.id
                );
            }
        }
    }

    #[test]
    fn option_kind_is_externally_tagged_on_type() {
        assert_eq!(
            serde_json::to_string(&OptionKind::Bool).unwrap(),
            r#"{"type":"bool"}"#
        );
        assert_eq!(
            serde_json::to_string(&OptionKind::Int { min: 0, max: 10 }).unwrap(),
            r#"{"type":"int","min":0,"max":10}"#
        );
        assert_eq!(
            serde_json::to_string(&OptionKind::Text { pattern: None }).unwrap(),
            r#"{"type":"text","pattern":null}"#
        );
        assert_eq!(
            serde_json::to_string(&OptionKind::Path).unwrap(),
            r#"{"type":"path"}"#
        );
        assert_eq!(
            serde_json::to_string(&OptionKind::Enum).unwrap(),
            r#"{"type":"enum"}"#
        );
    }

    #[test]
    fn choices_are_non_empty_only_for_enums() {
        for dt in &ytdlp_catalog().download_types {
            for o in &dt.options {
                if matches!(o.kind, OptionKind::Enum) {
                    // `ytdl_options_presets` is filled in at serve time from the operator's config.
                    assert!(!o.choices.is_empty() || &*o.id == "ytdl_options_presets");
                } else {
                    assert!(o.choices.is_empty(), "{} must have no choices", o.id);
                }
            }
        }
    }

    #[test]
    fn naming_policy_serialises_lowercase() {
        assert_eq!(
            serde_json::to_string(&NamingPolicy::Template).unwrap(),
            "\"template\""
        );
        assert_eq!(
            serde_json::to_string(&NamingPolicy::Provider).unwrap(),
            "\"provider\""
        );
    }

    #[test]
    fn merging_keeps_registration_order_and_sums_versions() {
        let a = Arc::new(ytdlp_catalog());
        let mut sc = ytdlp_catalog();
        sc.provider = ProviderId::parse("streamingcommunity").unwrap();
        sc.naming = NamingPolicy::Provider;
        sc.version = 4;
        sc.download_types.retain(|d| &*d.id == "video");
        let merged = MergedCatalog::merge(&[a, Arc::new(sc)]);
        assert_eq!(merged.version, 5);
        assert_eq!(merged.naming, NamingPolicy::Template);
        assert_eq!(merged.providers.len(), 2);
        assert_eq!(
            merged
                .download_types
                .iter()
                .map(|d| &*d.id)
                .collect::<Vec<_>>(),
            ["video", "audio", "captions", "thumbnail"]
        );
    }

    /// The production registration order is `command:*`, then `streamingcommunity`, then `ytdlp`
    /// (bootstrap), so without this rule the merged `video/mp4` was SC's advisory "Source" entry
    /// with a single quality, and the web UI's picker offered nothing else until a URL was typed.
    #[test]
    fn an_advisory_format_never_shadows_a_real_one_whatever_the_registration_order() {
        let real = Arc::new(ytdlp_catalog());
        let mut sc = FormatCatalog {
            provider: ProviderId::parse("streamingcommunity").unwrap(),
            version: 1,
            naming: NamingPolicy::Provider,
            download_types: Vec::new(),
        };
        let mut video = ytdlp_catalog()
            .download_type("video")
            .cloned()
            .expect("video");
        video.formats = vec![FormatSpec {
            id: "mp4".into(),
            label: "Source".into(),
            qualities: vec![QualitySpec {
                id: "best".into(),
                label: "Source".into(),
                notice: None,
            }],
            default_quality: "best".into(),
            codecs: vec![],
            notice: None,
            flags: FormatFlags {
                advisory: true,
                requires_ffmpeg: true,
                lossy_remux: false,
                slow: false,
            },
        }];
        sc.download_types.push(video);
        let sc = Arc::new(sc);

        for order in [
            vec![Arc::clone(&sc), Arc::clone(&real)],
            vec![real.clone(), sc.clone()],
        ] {
            let merged = MergedCatalog::merge(&order);
            let mp4 = merged
                .download_types
                .iter()
                .find(|d| &*d.id == "video")
                .and_then(|d| d.format("mp4"))
                .expect("video/mp4");
            assert!(!mp4.flags.advisory, "the advisory placeholder won");
            assert_eq!(&*mp4.label, "MP4");
            assert!(mp4.qualities.len() > 1, "{:?}", mp4.qualities);
            assert_eq!(&*mp4.quality("best").unwrap().label, "Best");
        }
    }

    #[test]
    fn the_static_catalog_matches_the_builder() {
        assert_eq!(**YTDLP_CATALOG, ytdlp_catalog());
    }
}
