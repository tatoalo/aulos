//! The shim → Rust wire model: the `{"v","t","n","ts"}` envelope and the twelve frame bodies of
//! DESIGN §9.3.
//!
//! Every frame is one line of UTF-8 JSON on fd 3. The envelope carries a monotonic `n` starting
//! at 1, so a gap proves a line was lost — which [`crate::runner`] treats as a `contract`
//! failure rather than as a smaller download.
//!
//! # Why the model is this permissive
//!
//! The shim ships with the image and versions with the yt-dlp pin, so a nightly bump can add a
//! field or a frame type without a Rust release. Every optional field is therefore `Option` or
//! `#[serde(default)]`, and an unrecognised `t` deserialises to [`Body::Unknown`] and is skipped
//! with a `debug` log instead of failing the job. The three things that are *not* permissive are
//! the ones a correctness argument rests on: the `hello` `protocol` number, the `n` sequence, and
//! the "exactly one of `result` / `error`, then `bye`" ordering.
//!
//! # Why byte counts are `Value`
//!
//! `downloaded_bytes` and friends are parsed as raw [`Value`] and coerced through
//! [`aulos_core::progress::number`], which is legacy's `_number()`: it accepts a numeric
//! **string** and a bool, because that is what yt-dlp and the community plugins actually emit and
//! what the WP-00 golden corpus pins.

use serde::Deserialize;
use serde_json::{Map, Value};

/// The `v` field of every frame, and the protocol version this crate speaks (DESIGN §9.2).
pub const PROTOCOL: u32 = 1;

/// The maximum length of one fd-3 line before the child is killed (DESIGN §9.1).
///
/// Generous because an `info` frame for a large playlist genuinely is megabytes.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

const fn protocol_version() -> u32 {
    PROTOCOL
}

const fn yes() -> bool {
    true
}

/// One line of the fd-3 transcript.
#[derive(Clone, Debug, Deserialize)]
pub struct Frame {
    /// Envelope version. Defaults to [`PROTOCOL`] so a hand-written test fixture may omit it.
    #[serde(default = "protocol_version")]
    pub v: u32,
    /// Frame sequence number, 1-based and gap-free.
    #[serde(default)]
    pub n: u64,
    /// Wall-clock emission time, epoch seconds. Advisory only — never used for timing.
    #[serde(default)]
    pub ts: f64,
    /// The frame itself.
    #[serde(flatten)]
    pub body: Body,
}

/// The frame bodies of the DESIGN §9.3 table, discriminated by `t`.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Body {
    /// Always the first frame: the interpreter, the yt-dlp pin and the plugin inventory.
    Hello(Hello),
    /// The extraction root. A container root becomes a playlist shell.
    Resolved(Resolved),
    /// One resolved child, streamed as yt-dlp's lazy playlist yields it.
    Entry(EntryFrame),
    /// One `progress_hooks` call, already reduced to the §9.4 allow-list.
    Progress(Box<ProgressFrame>),
    /// One `postprocessor_hooks` call (§9.5).
    Pp(PpFrame),
    /// A produced auxiliary file.
    Artifact(ArtifactFrame),
    /// A human message for `ItemView.msg`.
    Phase(PhaseFrame),
    /// The full `sanitize_info`'d root dict, for the entry blob of DESIGN §7.5.
    Info(InfoFrame),
    /// A yt-dlp diagnostic.
    Log(LogFrame),
    /// The successful terminator.
    Result(Box<ResultFrame>),
    /// The very last frame.
    Bye(ByeFrame),
    /// The failing terminator.
    Error(ErrorFrame),
    /// A frame type this build does not know. Skipped, never fatal — see the module docs.
    #[serde(other)]
    Unknown,
}

impl Body {
    /// The `t` string, for logs and for the ordering diagnostics.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Hello(_) => "hello",
            Self::Resolved(_) => "resolved",
            Self::Entry(_) => "entry",
            Self::Progress(_) => "progress",
            Self::Pp(_) => "pp",
            Self::Artifact(_) => "artifact",
            Self::Phase(_) => "phase",
            Self::Info(_) => "info",
            Self::Log(_) => "log",
            Self::Result(_) => "result",
            Self::Bye(_) => "bye",
            Self::Error(_) => "error",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this frame terminates the job (`result` or `error`).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Result(_) | Self::Error(_))
    }
}

/// The `hello` frame (DESIGN §9.3).
#[derive(Clone, Debug, Deserialize)]
pub struct Hello {
    /// The protocol the shim speaks. Anything but [`PROTOCOL`] kills the job.
    pub protocol: u32,
    /// The installed yt-dlp version, reported by `/version` and `healthz`.
    #[serde(default)]
    pub yt_dlp: Option<String>,
    /// The interpreter version.
    #[serde(default)]
    pub python: Option<String>,
    /// The child pid, which is also its process-group leader.
    #[serde(default)]
    pub pid: Option<i32>,
    /// The loaded yt-dlp plugin packages.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// The POT sidecar as the shim sees it.
    #[serde(default)]
    pub pot: Option<Pot>,
}

/// The `hello.pot` object.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Pot {
    /// Whether a POT provider plugin is loaded.
    #[serde(default)]
    pub available: bool,
    /// The sidecar endpoint the plugin will use, when known.
    #[serde(default)]
    pub url: Option<String>,
}

/// The `resolved` frame.
#[derive(Clone, Debug, Deserialize)]
pub struct Resolved {
    /// The extraction root.
    pub root: Root,
}

/// The `resolved.root` object.
///
/// The shim emits it for a single video too, even though DESIGN §9.3 only requires it for a
/// container: it is where the extractor name and the container title come from, and the Rust
/// action stays conditional on [`Root::kind`].
#[derive(Clone, Debug, Deserialize)]
pub struct Root {
    /// `video` | `playlist` | `channel` | `url*`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The root's provider id.
    #[serde(default)]
    pub id: Option<String>,
    /// The root's title.
    #[serde(default)]
    pub title: Option<String>,
    /// The canonical page URL.
    #[serde(default)]
    pub webpage_url: Option<String>,
    /// The yt-dlp extractor that claimed the URL.
    #[serde(default)]
    pub extractor: Option<String>,
    /// The declared child count.
    #[serde(default)]
    pub playlist_count: Option<u32>,
    /// The uploader / channel name.
    #[serde(default)]
    pub uploader: Option<String>,
    /// The uploader handle.
    #[serde(default)]
    pub uploader_id: Option<String>,
}

impl Root {
    /// Whether this root is a container (a playlist, a channel or a season).
    #[must_use]
    pub fn is_container(&self) -> bool {
        matches!(self.kind.as_str(), "playlist" | "multi_video" | "channel")
    }

    /// Whether this root is a `url` / `url_transparent` pointer somewhere else.
    #[must_use]
    pub fn is_redirect(&self) -> bool {
        self.kind.starts_with("url")
    }
}

/// The `entry` frame: one child, as the flat-info subset.
#[derive(Clone, Debug, Deserialize)]
pub struct EntryFrame {
    /// 1-based position in the container.
    #[serde(default)]
    pub index: u32,
    /// The flat info subset. Kept as a map because [`crate::runner`] both reads named keys out of
    /// it and stores the whole thing as the entry's `state`.
    #[serde(default)]
    pub entry: Map<String, Value>,
    /// A non-fatal problem with this entry — legacy's `entry["msg"]`, which becomes the item's
    /// `pre_error` while the status stays `queued` (DESIGN §8.4).
    #[serde(default)]
    pub note: Option<String>,
}

/// The `progress` frame: the legacy `put_status` allow-list plus `elapsed` and `stream`
/// (DESIGN §9.4).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProgressFrame {
    /// `downloading` | `finished` | `error`. Per **stream**, not per item.
    #[serde(default)]
    pub status: Option<String>,
    /// The final path of this stream, once known.
    #[serde(default)]
    pub filename: Option<String>,
    /// The partial file. **Not sticky**: absent means "unchanged", not "cleared" (Δ C18).
    #[serde(default)]
    pub tmpfilename: Option<String>,
    /// Bytes fetched so far.
    #[serde(default)]
    pub downloaded_bytes: Option<Value>,
    /// The exact total.
    #[serde(default)]
    pub total_bytes: Option<Value>,
    /// An estimate; only trusted when `total_bytes` is absent.
    #[serde(default)]
    pub total_bytes_estimate: Option<Value>,
    /// 0-based fragment index.
    #[serde(default)]
    pub fragment_index: Option<Value>,
    /// Total fragment count.
    #[serde(default)]
    pub fragment_count: Option<Value>,
    /// Bytes per second.
    #[serde(default)]
    pub speed: Option<Value>,
    /// Whole seconds remaining.
    #[serde(default)]
    pub eta: Option<Value>,
    /// Text yt-dlp puts here on some errors.
    #[serde(default)]
    pub msg: Option<String>,
    /// Seconds since this stream started. Lets the parent detect a stall without a wall clock.
    #[serde(default)]
    pub elapsed: Option<f64>,
    /// `video` | `audio` | `fragment` | `unknown` — the merge leg this frame belongs to.
    #[serde(default)]
    pub stream: Option<String>,
}

/// The `pp` frame (DESIGN §9.5).
#[derive(Clone, Debug, Deserialize)]
pub struct PpFrame {
    /// The postprocessor class name, e.g. `MoveFiles`.
    #[serde(default)]
    pub postprocessor: String,
    /// `started` | `processing` | `finished` | `error`.
    #[serde(default)]
    pub status: String,
    /// The file the postprocessor is working on, `__finaldir` already applied.
    #[serde(default)]
    pub filepath: Option<String>,
    /// yt-dlp's `__finaldir`, for diagnostics.
    #[serde(default)]
    pub finaldir: Option<Value>,
    /// Subtitle tracks this step produced.
    #[serde(default)]
    pub subtitles: Vec<PpFile>,
    /// Chapter files this step produced.
    #[serde(default)]
    pub chapters: Vec<PpFile>,
}

/// One file named inside a `pp` frame.
#[derive(Clone, Debug, Deserialize)]
pub struct PpFile {
    /// The absolute path.
    pub path: String,
    /// Bytes on disk, when the shim could stat it.
    #[serde(default)]
    pub size: Option<u64>,
    /// The subtitle language tag.
    #[serde(default)]
    pub language: Option<String>,
    /// The chapter title.
    #[serde(default)]
    pub label: Option<String>,
}

/// The `artifact` frame: one produced auxiliary file.
#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactFrame {
    /// `media` | `chapter` | `subtitle`.
    #[serde(default)]
    pub role: String,
    /// The absolute path.
    #[serde(default)]
    pub path: String,
    /// Bytes on disk.
    #[serde(default)]
    pub size: Option<u64>,
    /// The subtitle language tag.
    #[serde(default)]
    pub language: Option<String>,
    /// A human label, e.g. a chapter title.
    #[serde(default)]
    pub label: Option<String>,
}

/// The `phase` frame: a message for `ItemView.msg`.
#[derive(Clone, Debug, Deserialize)]
pub struct PhaseFrame {
    /// The message.
    #[serde(default)]
    pub msg: String,
}

/// The `info` frame: the full `sanitize_info`'d root dict, minus `entries`.
#[derive(Clone, Debug, Deserialize)]
pub struct InfoFrame {
    /// The dict.
    #[serde(default)]
    pub entry: Value,
}

/// The `log` frame.
#[derive(Clone, Debug, Deserialize)]
pub struct LogFrame {
    /// `debug` | `info` | `warning` | `error`.
    #[serde(default)]
    pub level: String,
    /// The already-cleaned message.
    #[serde(default)]
    pub message: String,
    /// The extractor that produced it, when yt-dlp attributed one.
    #[serde(default)]
    pub extractor: Option<String>,
}

/// The `result` frame. Its shape depends on the mode (DESIGN §9.3).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ResultFrame {
    /// Whether the job succeeded.
    #[serde(default)]
    pub ok: bool,
    /// download: yt-dlp's own return code.
    #[serde(default)]
    pub retcode: Option<i32>,
    /// download: the primary produced file, absolute.
    #[serde(default)]
    pub filename: Option<String>,
    /// download: its size.
    #[serde(default)]
    pub size: Option<u64>,
    /// download: every produced file.
    #[serde(default)]
    pub artifacts: Vec<ArtifactFrame>,
    /// extract: how many entries were emitted.
    #[serde(default)]
    pub count: Option<u32>,
    /// extract: whether `max_entries` cut the list short.
    #[serde(default)]
    pub truncated: bool,
    /// outtmpl: the evaluated templates, in request order.
    #[serde(default)]
    pub templates: Option<Vec<String>>,
    /// selftest: the yt-dlp version that imported successfully.
    #[serde(default)]
    pub yt_dlp: Option<String>,
}

/// The `bye` frame.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ByeFrame {
    /// Wall time the shim spent, milliseconds.
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    /// Total frames written, including this one.
    #[serde(default)]
    pub frames: Option<u64>,
    /// Peak RSS in KiB, where the platform reports it.
    #[serde(default)]
    pub peak_rss_kb: Option<u64>,
}

/// The `error` frame (DESIGN §9.6).
#[derive(Clone, Debug, Deserialize)]
pub struct ErrorFrame {
    /// One of the §9.6 codes. An unknown code becomes `contract` (see [`crate::errmap`]).
    #[serde(default)]
    pub code: String,
    /// The already-cleaned, already-capped message.
    #[serde(default)]
    pub message: String,
    /// The shim's opinion on retryability. Advisory: the queue uses
    /// [`aulos_provider::ProviderError::retryable`], so there is one retry table.
    #[serde(default)]
    pub retryable: bool,
    /// The extractor that failed.
    #[serde(default)]
    pub extractor: Option<String>,
    /// Whether the failure is terminal for the item. Defaults to `true`.
    #[serde(default = "yes")]
    pub fatal: bool,
    /// The Python exception class, reported as `WireError::provider_code`.
    #[serde(default)]
    pub provider_code: Option<String>,
    /// A traceback, when the shim was asked for one. Never shown to a client.
    #[serde(default)]
    pub traceback: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Frame {
        serde_json::from_str(line).expect("frame must parse")
    }

    #[test]
    fn the_design_example_transcript_parses() {
        // The abridged download transcript of DESIGN §9.3, verbatim.
        let lines = [
            r#"{"v":1,"t":"hello","n":1,"ts":1772668800.11,"protocol":1,"yt_dlp":"2026.8.30.232658.dev0","python":"3.13.2","pid":4821,"plugins":["bgutil_ytdlp_pot_provider"],"pot":{"available":true,"url":"http://127.0.0.1:4416"}}"#,
            r#"{"v":1,"t":"progress","n":3,"ts":1772668801.90,"status":"downloading","filename":null,"tmpfilename":"/downloads/Rick.f616.mp4.part","downloaded_bytes":262144,"total_bytes":null,"total_bytes_estimate":58720256,"fragment_index":null,"fragment_count":null,"speed":1310720.0,"eta":44,"elapsed":0.3,"stream":"video"}"#,
            r#"{"v":1,"t":"pp","n":10,"ts":1772668812.75,"postprocessor":"MoveFiles","status":"finished","filepath":"/downloads/Rick.mp4","finaldir":null,"subtitles":[],"chapters":[]}"#,
            r#"{"v":1,"t":"result","n":12,"ts":1772668812.80,"ok":true,"retcode":0,"filename":"/downloads/Rick.mp4","size":62390272,"artifacts":[{"role":"media","path":"/downloads/Rick.mp4","size":62390272}]}"#,
            r#"{"v":1,"t":"bye","n":13,"ts":1772668812.81,"elapsed_ms":12700,"frames":13,"peak_rss_kb":91240}"#,
        ];
        let kinds: Vec<_> = lines.iter().map(|l| parse(l).body.kind()).collect();
        assert_eq!(kinds, ["hello", "progress", "pp", "result", "bye"]);

        let Body::Hello(hello) = parse(lines[0]).body else {
            panic!("expected hello")
        };
        assert_eq!(hello.protocol, PROTOCOL);
        assert_eq!(hello.plugins, ["bgutil_ytdlp_pot_provider"]);
        assert!(hello.pot.unwrap().available);

        let Body::Progress(p) = parse(lines[1]).body else {
            panic!("expected progress")
        };
        assert_eq!(p.stream.as_deref(), Some("video"));
        // An explicit `null` and an absent key are the same thing, which is what makes the
        // "not sticky" rule of DESIGN §9.4 expressible: `"tmpfilename": null` means "unchanged".
        assert_eq!(p.total_bytes, None);
        assert_eq!(p.filename, None);
        assert_eq!(
            p.tmpfilename.as_deref(),
            Some("/downloads/Rick.f616.mp4.part")
        );

        let Body::Result(r) = parse(lines[3]).body else {
            panic!("expected result")
        };
        assert!(r.ok && !r.truncated);
        assert_eq!(r.artifacts.len(), 1);
        assert_eq!(r.artifacts[0].role, "media");
    }

    #[test]
    fn the_extract_example_transcript_parses() {
        let resolved = parse(
            r#"{"v":1,"t":"resolved","n":2,"root":{"type":"playlist","id":"PL9","title":"Mix - lofi","webpage_url":"https://x/y","extractor":"youtube:tab","playlist_count":500,"uploader":"Chillhop","uploader_id":"@chillhop"}}"#,
        );
        let Body::Resolved(r) = resolved.body else {
            panic!("expected resolved")
        };
        assert!(r.root.is_container() && !r.root.is_redirect());
        assert_eq!(r.root.playlist_count, Some(500));

        let entry = parse(
            r#"{"v":1,"t":"entry","n":3,"index":1,"entry":{"id":"aXbZ1","title":"Track 1","url":"https://x/1","duration":183.0},"note":null}"#,
        );
        let Body::Entry(e) = entry.body else {
            panic!("expected entry")
        };
        assert_eq!(e.index, 1);
        assert_eq!(e.entry["title"], "Track 1");
        assert!(e.note.is_none());
    }

    #[test]
    fn an_unknown_frame_type_is_skipped_not_fatal() {
        let f = parse(r#"{"v":1,"t":"telemetry","n":7,"whatever":true}"#);
        assert!(matches!(f.body, Body::Unknown));
        assert_eq!(f.body.kind(), "unknown");
        assert!(!f.body.is_terminal());
    }

    #[test]
    fn a_hand_written_fixture_may_omit_the_envelope_defaults() {
        let f = parse(r#"{"t":"phase","n":2,"msg":"Merging"}"#);
        assert_eq!(f.v, PROTOCOL);
        assert!((f.ts - 0.0).abs() < f64::EPSILON);
        let Body::Phase(p) = f.body else {
            panic!("expected phase")
        };
        assert_eq!(p.msg, "Merging");
    }

    #[test]
    fn an_error_frame_defaults_to_fatal() {
        let f = parse(r#"{"v":1,"t":"error","n":4,"code":"network","message":"HTTP Error 503"}"#);
        assert!(f.body.is_terminal());
        let Body::Error(e) = f.body else {
            panic!("expected error")
        };
        assert!(
            e.fatal,
            "a frame without `fatal` must be treated as terminal"
        );
        assert!(
            !e.retryable,
            "`retryable` is advisory and defaults to false"
        );
        assert_eq!(e.code, "network");
    }

    #[test]
    fn byte_counts_accept_the_legacy_string_form() {
        let f = parse(
            r#"{"v":1,"t":"progress","n":2,"status":"downloading","downloaded_bytes":"250.5","total_bytes":"1000.0"}"#,
        );
        let Body::Progress(p) = f.body else {
            panic!("expected progress")
        };
        assert_eq!(
            aulos_core::progress::number(p.downloaded_bytes.as_ref().unwrap()),
            Some(250.5)
        );
        assert_eq!(
            aulos_core::progress::number(p.total_bytes.as_ref().unwrap()),
            Some(1000.0)
        );
    }
}
