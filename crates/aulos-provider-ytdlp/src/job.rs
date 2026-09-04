//! Rust → shim: the job object of DESIGN §9.2.
//!
//! Rust builds the *whole* thing — the merged option dict from [`crate::opts`], the selector from
//! [`crate::formats`], the templates from [`crate::outtmpl`] — and the shim only calls `yt_dlp`
//! with it. That asymmetry is the design: `YTDL_OPTIONS`, `YTDL_OPTIONS_FILE`, presets and
//! per-request overrides are Python **option dicts**, not CLI flags, and a dict cannot be
//! faithfully rendered as argv (`postprocessors`, `extractor_args`, `null`-clears-a-key). Sending
//! the dict as JSON is the only way to keep 100 % compatibility with them.
//!
//! # The two things the shim decides for itself
//!
//! [`Policy`] carries them, because they need the postprocessor `info_dict`, which never crosses
//! the boundary: which produced caption files count as artifacts (and the `.srt → .txt`
//! conversion), and the `.webm → .jpg` rewrite for a thumbnail-only download.
//!
//! # `coerce`
//!
//! A handful of yt-dlp options take Python objects rather than strings. `coerce` names them, so
//! the conversion is explicit and versioned with the shim instead of being a hard-coded surprise.
//! Today there is exactly one: `impersonate → ImpersonateTarget`. An unknown name is a
//! `bad_job` error naming the key — never a silent behaviour change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aulos_core::config::Config;
use aulos_core::selection::DownloadType;
use serde_json::{Map, Value, json};
use url::Url;

use crate::frames::PROTOCOL;
use crate::outtmpl::OutTmplJob;

/// The yt-dlp option key whose string value names an `ImpersonateTarget`.
pub const IMPERSONATE_KEY: &str = "impersonate";
/// The coercion name for [`IMPERSONATE_KEY`].
pub const IMPERSONATE_COERCION: &str = "ImpersonateTarget";

/// `policy.caption_exts` default — legacy's `allowed_caption_exts`.
pub const CAPTION_EXTS: [&str; 6] = [".vtt", ".srt", ".sbv", ".scc", ".ttml", ".dfxp"];

/// `policy.emit_progress_every_ms` default: ~10 frames/s **per stream**, at the source.
pub const EMIT_PROGRESS_EVERY_MS: u64 = 100;

/// `extract.max_entries` default — the hard cap on how many children one resolution may yield.
pub const MAX_ENTRIES: u32 = 5000;

/// Which of the four shim modes a job is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mode {
    /// Metadata only; no bytes are written.
    Extract,
    /// The real download.
    Download,
    /// Evaluate output templates through yt-dlp's own `evaluate_outtmpl`.
    OutTmpl,
    /// Prove the interpreter can import yt-dlp. Backs [`crate::YtdlpProvider::probe`].
    Selftest,
}

impl Mode {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extract => "extract",
            Self::Download => "download",
            Self::OutTmpl => "outtmpl",
            Self::Selftest => "selftest",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The local decisions the shim must make, and the knobs that bound its output (DESIGN §9.2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Policy {
    /// The request's download type, which selects the caption / thumbnail rules.
    pub download_type: DownloadType,
    /// The item's output directory.
    pub download_dir: PathBuf,
    /// The scratch directory.
    pub temp_dir: PathBuf,
    /// Extensions a produced caption file must have to be reported.
    pub caption_exts: Vec<String>,
    /// Whether the shim converts the produced `.srt` to `.txt` and reports the `.txt`.
    pub convert_srt_to_txt: bool,
    /// Whether a `.webm` thumbnail path is rewritten to `.jpg`.
    pub thumbnail_ext_rewrite: bool,
    /// The shim-side progress rate limit, per stream. `0` disables it.
    pub emit_progress_every_ms: u64,
    /// Ask yt-dlp for verbose output and forward its `debug`/`info` lines as `log` frames.
    pub debug: bool,
    /// The shim's own watchdog, milliseconds. `0` = off.
    ///
    /// Belt and braces with the parent's hard timer: the watchdog turns a wedged job into a clean
    /// `error{code:"timeout"}` transcript instead of a `SIGKILL` with no diagnosis.
    pub hard_timeout_ms: u64,
    /// The POT sidecar endpoint, reported back in `hello.pot.url`.
    pub pot_url: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            download_type: DownloadType::Video,
            download_dir: PathBuf::new(),
            temp_dir: PathBuf::new(),
            caption_exts: CAPTION_EXTS.iter().map(|s| (*s).to_owned()).collect(),
            convert_srt_to_txt: false,
            thumbnail_ext_rewrite: false,
            emit_progress_every_ms: EMIT_PROGRESS_EVERY_MS,
            debug: false,
            hard_timeout_ms: 0,
            pot_url: None,
        }
    }
}

/// The shim's own watchdog, derived from `AULOS_JOB_TIMEOUT_SECS`.
///
/// Deliberately **half a second shorter** than the parent's hard timer. Both exist, and they must
/// not race: when the shim's alarm wins, a wedged job produces a clean
/// `error{code:"timeout"}` transcript with the extractor named; when the parent's timer wins, all
/// the operator gets is a `SIGKILL` and a contract failure. `0` (the default) disables both.
#[must_use]
pub const fn shim_watchdog_ms(job_timeout_secs: u64) -> u64 {
    if job_timeout_secs == 0 {
        return 0;
    }
    let budget = job_timeout_secs.saturating_mul(1000);
    if budget > 1500 { budget - 500 } else { 1000 }
}

impl Policy {
    /// The policy for one download, derived from the config and the selection.
    ///
    /// `convert_srt_to_txt` follows the legacy rule exactly: it is on only for
    /// `download_type == captions` with `format == "txt"`, because that is the one combination
    /// where legacy derived the `.txt` from the `.srt` yt-dlp had written.
    #[must_use]
    pub fn for_download(
        cfg: &Config,
        download_type: DownloadType,
        format: &str,
        out_dir: PathBuf,
    ) -> Self {
        let captions = download_type == DownloadType::Captions;
        Self {
            download_type,
            download_dir: out_dir,
            temp_dir: cfg.paths.temp.clone(),
            convert_srt_to_txt: captions && format.eq_ignore_ascii_case("txt"),
            thumbnail_ext_rewrite: download_type == DownloadType::Thumbnail,
            hard_timeout_ms: shim_watchdog_ms(cfg.job_timeout_secs),
            pot_url: (!cfg.pot_url.is_empty()).then(|| cfg.pot_url.to_string()),
            ..Self::default()
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "download_type": self.download_type.as_str(),
            "download_dir": self.download_dir,
            "temp_dir": self.temp_dir,
            "caption_exts": self.caption_exts,
            "convert_srt_to_txt": self.convert_srt_to_txt,
            "thumbnail_ext_rewrite": self.thumbnail_ext_rewrite,
            "emit_progress_every_ms": self.emit_progress_every_ms,
            "debug": self.debug,
            "hard_timeout_ms": self.hard_timeout_ms,
            "pot_url": self.pot_url,
        })
    }
}

/// The `extract` block of an `extract` job (DESIGN §9.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExtractOpts {
    /// `extract_flat`. Applied **after** the user options, so a preset cannot break it.
    pub flat: bool,
    /// `noplaylist`. Same ordering rule.
    pub noplaylist: bool,
    /// `playlistend`, when the request or a subscription scan caps the list.
    pub playlist_end: Option<u32>,
    /// Whether the legacy "empty `formats` ⇒ retry strictly" rule applies.
    pub strict_retry: bool,
    /// Whether children are streamed as yt-dlp yields them rather than collected first.
    pub stream_entries: bool,
    /// Hard cap on emitted children; the shim reports `truncated` when it bites.
    pub max_entries: u32,
}

impl Default for ExtractOpts {
    fn default() -> Self {
        Self {
            flat: true,
            noplaylist: true,
            playlist_end: None,
            strict_retry: true,
            stream_entries: true,
            max_entries: MAX_ENTRIES,
        }
    }
}

impl ExtractOpts {
    fn to_json(self) -> Value {
        json!({
            "flat": self.flat,
            "noplaylist": self.noplaylist,
            "playlist_end": self.playlist_end,
            "strict_retry": self.strict_retry,
            "stream_entries": self.stream_entries,
            "max_entries": self.max_entries,
        })
    }
}

/// One unit of work for the shim.
///
/// Build one with [`Job::extract`], [`Job::download`], [`Job::outtmpl`] or [`Job::selftest`] and
/// hand it to [`crate::RunnerHandle::run`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Job {
    /// The item id, echoed in the child's log span. Not interpreted by the shim.
    pub job_id: String,
    /// Which of the four things to do.
    pub mode: Mode,
    /// The target URL, for `extract` and `download`.
    pub url: Option<Url>,
    /// The fully merged yt-dlp option dict.
    pub options: Map<String, Value>,
    /// Option keys whose string values the shim converts to Python objects.
    pub coerce: BTreeMap<String, String>,
    /// The shim's local decisions.
    pub policy: Policy,
    /// Present exactly for `mode == Extract`.
    pub extract: Option<ExtractOpts>,
    /// Present exactly for `mode == OutTmpl`: the field references to evaluate.
    pub templates: Vec<String>,
    /// Present exactly for `mode == OutTmpl`: the info dict to evaluate them against.
    pub info: Map<String, Value>,
    /// Present exactly for `mode == OutTmpl`: which field prefixes are being resolved.
    pub prefixes: Vec<String>,
    /// **Not serialised.** The root the produced absolute paths are made relative to, so
    /// `Outcome::filename` is what `download_url` can be built from (DESIGN §4.5).
    pub download_root: Option<PathBuf>,
}

impl Job {
    fn bare(job_id: impl Into<String>, mode: Mode) -> Self {
        Self {
            job_id: job_id.into(),
            mode,
            url: None,
            options: Map::new(),
            coerce: BTreeMap::new(),
            policy: Policy::default(),
            extract: None,
            templates: Vec::new(),
            info: Map::new(),
            prefixes: Vec::new(),
            download_root: None,
        }
    }

    /// A metadata-only job.
    #[must_use]
    pub fn extract(job_id: impl Into<String>, url: Url) -> Self {
        Self {
            url: Some(url),
            extract: Some(ExtractOpts::default()),
            ..Self::bare(job_id, Mode::Extract)
        }
    }

    /// A real download.
    #[must_use]
    pub fn download(job_id: impl Into<String>, url: Url) -> Self {
        Self {
            url: Some(url),
            ..Self::bare(job_id, Mode::Download)
        }
    }

    /// An output-template evaluation.
    #[must_use]
    pub fn outtmpl(
        job_id: impl Into<String>,
        templates: Vec<String>,
        info: Map<String, Value>,
        prefixes: Vec<String>,
    ) -> Self {
        Self {
            templates,
            info,
            prefixes,
            ..Self::bare(job_id, Mode::OutTmpl)
        }
    }

    /// The output-template evaluation an [`OutTmplJob`] needs.
    ///
    /// This is the other half of [`crate::outtmpl::build_outtmpl`]: that function finds the
    /// `playlist*` / `channel*` field references, this one sends them, and
    /// [`OutTmplJob::apply`] splices the answers back in.
    #[must_use]
    pub fn from_outtmpl(job_id: impl Into<String>, job: &OutTmplJob) -> Self {
        Self::outtmpl(
            job_id,
            job.templates().into_iter().map(str::to_owned).collect(),
            job.info().clone(),
            job.prefixes().iter().map(|p| (*p).to_owned()).collect(),
        )
    }

    /// A readiness probe.
    #[must_use]
    pub fn selftest(job_id: impl Into<String>) -> Self {
        Self::bare(job_id, Mode::Selftest)
    }

    /// Sets the merged option dict and derives the `coerce` map from it.
    #[must_use]
    pub fn with_options(mut self, options: Map<String, Value>) -> Self {
        self.options = options;
        self.derive_coercions();
        self
    }

    /// Sets the shim policy.
    #[must_use]
    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Replaces the `extract` block. Ignored by every other mode.
    #[must_use]
    pub fn with_extract(mut self, extract: ExtractOpts) -> Self {
        self.extract = Some(extract);
        self
    }

    /// Sets the root that produced paths are reported relative to.
    #[must_use]
    pub fn with_download_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.download_root = Some(root.into());
        self
    }

    /// Adds a `coerce` entry for every option key that needs one.
    ///
    /// Called by [`Job::with_options`]; public because a caller that mutates `options` directly
    /// must be able to re-run it.
    pub fn derive_coercions(&mut self) {
        if self
            .options
            .get(IMPERSONATE_KEY)
            .is_some_and(Value::is_string)
        {
            self.coerce
                .insert(IMPERSONATE_KEY.to_owned(), IMPERSONATE_COERCION.to_owned());
        } else {
            self.coerce.remove(IMPERSONATE_KEY);
        }
    }

    /// Makes an absolute path produced by the shim relative to [`Job::download_root`].
    ///
    /// Falls back to `policy.download_dir`, then to the basename: a provider must be able to
    /// report *something* for a file it just wrote, and a path outside every known root is a
    /// misconfiguration the operator needs to see rather than a lost download.
    #[must_use]
    pub fn rebase<'a>(&self, path: &'a str) -> &'a str {
        let p = Path::new(path);
        for root in [
            self.download_root.as_deref(),
            Some(&self.policy.download_dir),
        ]
        .into_iter()
        .flatten()
        .filter(|r| !r.as_os_str().is_empty())
        {
            if let Ok(rel) = p.strip_prefix(root)
                && let Some(s) = rel.to_str()
                && !s.is_empty()
            {
                return s;
            }
        }
        p.file_name().and_then(|n| n.to_str()).unwrap_or(path)
    }

    /// The DESIGN §9.2 JSON object, ready for the child's stdin.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("v".to_owned(), json!(1));
        map.insert("protocol".to_owned(), json!(PROTOCOL));
        map.insert("job_id".to_owned(), json!(self.job_id));
        map.insert("mode".to_owned(), json!(self.mode.as_str()));
        map.insert("options".to_owned(), Value::Object(self.options.clone()));
        map.insert("coerce".to_owned(), json!(self.coerce));
        map.insert("policy".to_owned(), self.policy.to_json());
        if let Some(url) = &self.url {
            map.insert("url".to_owned(), Value::String(url.to_string()));
        }
        match self.mode {
            Mode::Extract => {
                if let Some(extract) = self.extract {
                    map.insert("extract".to_owned(), extract.to_json());
                }
            }
            Mode::OutTmpl => {
                map.insert("templates".to_owned(), json!(self.templates));
                map.insert("info".to_owned(), Value::Object(self.info.clone()));
                map.insert("prefixes".to_owned(), json!(self.prefixes));
            }
            Mode::Download | Mode::Selftest => {}
        }
        Value::Object(map)
    }

    /// The single stdin line: the job, then `\n`.
    #[must_use]
    pub fn to_line(&self) -> String {
        let mut line = self.to_json().to_string();
        line.push('\n');
        line
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap()
    }

    #[test]
    fn a_download_job_matches_the_design_shape() {
        let mut options = Map::new();
        options.insert("format".to_owned(), json!("bestvideo+bestaudio"));
        options.insert("impersonate".to_owned(), json!("chrome"));
        let job = Job::download("01JBQ7Z5T9K3M2R8V4XW6Y0AAA", url()).with_options(options);
        let v = job.to_json();

        assert_eq!(v["v"], 1);
        assert_eq!(v["protocol"], 1);
        assert_eq!(v["job_id"], "01JBQ7Z5T9K3M2R8V4XW6Y0AAA");
        assert_eq!(v["mode"], "download");
        assert_eq!(v["url"], url().as_str());
        assert_eq!(v["options"]["format"], "bestvideo+bestaudio");
        assert_eq!(v["coerce"]["impersonate"], "ImpersonateTarget");
        assert_eq!(v["policy"]["download_type"], "video");
        assert!(
            v.get("extract").is_none(),
            "a download job has no extract block"
        );
        assert!(job.to_line().ends_with('\n'));
        // One line, always: a newline inside the payload would break the shim's `readline`.
        assert_eq!(job.to_line().matches('\n').count(), 1);
    }

    #[test]
    fn the_coerce_map_tracks_the_option_dict() {
        let job = Job::download("j", url()).with_options(Map::new());
        assert!(job.coerce.is_empty(), "no impersonate, no coercion");

        let mut options = Map::new();
        // A non-string `impersonate` is already an object as far as yt-dlp is concerned.
        options.insert("impersonate".to_owned(), json!(null));
        let job = Job::download("j", url()).with_options(options);
        assert!(job.coerce.is_empty());
    }

    #[test]
    fn an_extract_job_carries_the_extract_block() {
        let job = Job::extract("j", url()).with_extract(ExtractOpts {
            playlist_end: Some(50),
            max_entries: 100,
            ..ExtractOpts::default()
        });
        let v = job.to_json();
        assert_eq!(v["mode"], "extract");
        assert_eq!(v["extract"]["flat"], true);
        assert_eq!(v["extract"]["noplaylist"], true);
        assert_eq!(v["extract"]["playlist_end"], 50);
        assert_eq!(v["extract"]["strict_retry"], true);
        assert_eq!(v["extract"]["max_entries"], 100);
    }

    #[test]
    fn an_outtmpl_job_carries_the_templates_and_the_info() {
        let mut info = Map::new();
        info.insert("playlist_title".to_owned(), json!("Mix"));
        let job = Job::outtmpl(
            "j",
            vec!["%(playlist_title)s".to_owned()],
            info,
            vec!["playlist".to_owned()],
        );
        let v = job.to_json();
        assert_eq!(v["mode"], "outtmpl");
        assert_eq!(v["templates"][0], "%(playlist_title)s");
        assert_eq!(v["info"]["playlist_title"], "Mix");
        assert_eq!(v["prefixes"][0], "playlist");
        assert!(v.get("url").is_none());
    }

    #[test]
    fn a_selftest_job_needs_nothing() {
        let v = Job::selftest("probe").to_json();
        assert_eq!(v["mode"], "selftest");
        assert!(v.get("url").is_none());
        assert!(v.get("extract").is_none());
    }

    #[test]
    fn rebase_prefers_the_download_root_then_the_out_dir_then_the_basename() {
        let job = Job::download("j", url())
            .with_download_root("/downloads")
            .with_policy(Policy {
                download_dir: PathBuf::from("/downloads/Show"),
                ..Policy::default()
            });
        assert_eq!(job.rebase("/downloads/Show/S01E01.mkv"), "Show/S01E01.mkv");
        assert_eq!(job.rebase("/elsewhere/x.mkv"), "x.mkv");

        let no_root = Job::download("j", url()).with_policy(Policy {
            download_dir: PathBuf::from("/downloads"),
            ..Policy::default()
        });
        assert_eq!(no_root.rebase("/downloads/a/b.mp4"), "a/b.mp4");

        let nothing = Job::download("j", url());
        assert_eq!(nothing.rebase("/downloads/a/b.mp4"), "b.mp4");
    }

    #[test]
    fn the_shim_watchdog_is_shorter_than_the_parent_timer() {
        assert_eq!(shim_watchdog_ms(0), 0, "0 disables both timers");
        assert_eq!(shim_watchdog_ms(3600), 3_599_500);
        // A tiny budget still leaves the shim a full second, so it can write a transcript.
        assert_eq!(shim_watchdog_ms(1), 1000);
        assert_eq!(shim_watchdog_ms(2), 1500);
        for secs in [1_u64, 2, 30, 900, 7200] {
            assert!(
                shim_watchdog_ms(secs) <= secs * 1000,
                "the shim must never outlast the parent at {secs} s"
            );
        }
    }

    #[test]
    fn the_captions_policy_follows_the_legacy_rule() {
        let cfg = aulos_core::config::load(&aulos_core::config::RawEnv::from_pairs([
            ("DOWNLOAD_DIR", "/downloads"),
            ("TEMP_DIR", "/tmp/x"),
        ]))
        .expect("config");
        let txt = Policy::for_download(
            &cfg,
            DownloadType::Captions,
            "txt",
            PathBuf::from("/downloads"),
        );
        assert!(txt.convert_srt_to_txt);
        assert!(!txt.thumbnail_ext_rewrite);
        let vtt = Policy::for_download(
            &cfg,
            DownloadType::Captions,
            "vtt",
            PathBuf::from("/downloads"),
        );
        assert!(!vtt.convert_srt_to_txt);
        let thumb = Policy::for_download(
            &cfg,
            DownloadType::Thumbnail,
            "jpg",
            PathBuf::from("/downloads"),
        );
        assert!(thumb.thumbnail_ext_rewrite);
        assert_eq!(thumb.temp_dir, PathBuf::from("/tmp/x"));
    }
}
