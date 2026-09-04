//! The scripted, no-network provider the integration suite runs on (BRIEF §17, DESIGN §20).
//!
//! Behind the `fake` feature so it never ships in the image. A [`FakeProvider`] is a TOML
//! *timeline*: a list of [`Step`]s that it walks, emitting exactly the `Stage`, `Progress` and
//! `File` messages a real provider would and then finishing, failing or hanging.
//!
//! Two properties make it worth more than a hand-rolled mock per test:
//!
//! - **A ten-minute download runs in microseconds.** Every wait is a `tokio::time::sleep`, so
//!   under `tokio::time::pause()` the runtime auto-advances through it. A timeline can therefore
//!   describe a realistic 600-second download with 200 progress ticks and the test still finishes
//!   in a millisecond.
//! - **[`Step::Hang`] is a *real* hang.** It waits on the cancellation token and nothing else — no
//!   timer — so paused time does **not** advance past it and the caller's stall watchdog is the
//!   thing that has to fire. That is the only way to test a stall path without sleeping for real.
//!
//! ```toml
//! id     = "fake"
//! score  = 200
//! hosts  = ["fake.test"]
//!
//! [[timeline]]
//! url_regex = "playlist"
//! resolve   = [{ kind = "expand_playlist", count = 500 }]
//!
//! [[timeline]]
//! download = [
//!   { kind = "stage",    stage = "preparing" },
//!   { kind = "wait",     ms = 600000 },
//!   { kind = "stage",    stage = "downloading" },
//!   { kind = "progress", percent = 50.0, speed = 1048576.0, eta = 300 },
//!   { kind = "progress", percent = 99.9 },
//!   { kind = "stage",    stage = "postprocessing" },
//!   { kind = "finish",   filename = "Clip.mp4", size = 1024 },
//! ]
//! ```

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use aulos_core::catalog::{
    DownloadTypeSpec, FormatCatalog, FormatFlags, FormatSpec, NamingPolicy, QualitySpec,
};
use aulos_core::error::ErrorCode;
use aulos_core::item::{FileRef, FileSlot};
use aulos_core::paths::{RelPath, sanitize_path_component};
use aulos_core::progress::RawProgress;
use aulos_core::selection::ProviderId;
use regex::Regex;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::entry::{EntryHints, EntryKind, MediaEntry};
use crate::outcome::Outcome;
use crate::provider::{
    DownloadCtx, Match, Provider, ProviderError, ProviderHealth, ResolveCtx, SCORE_SC,
};
use crate::sink::{ProgressSink, Stage};

/// The synthetic total a timeline's progress percentages are computed against, when the timeline
/// has no `finish` step to take a size from.
pub const DEFAULT_TOTAL_BYTES: f64 = 1_000_000.0;

/// One scripted step (PLAN WP-03).
#[derive(Clone, PartialEq, Debug)]
pub enum Step {
    /// Sleep. Auto-advanced by paused time, so a ten-minute wait costs nothing in a test.
    Wait(Duration),
    /// Report a state transition.
    Stage(Stage),
    /// Report progress. `percent` drives the synthetic byte counts so the aggregator's
    /// `Normalizer` reproduces it exactly.
    Progress {
        /// `0.0..=100.0`.
        percent: f64,
        /// Bytes per second, or `None`.
        speed: Option<f64>,
        /// Seconds remaining, or `None`.
        eta: Option<i64>,
    },
    /// Report a produced auxiliary file, and create it on disk when `write_files` is on.
    File {
        /// Chapter or subtitle.
        slot: FileSlot,
        /// The file name, relative to the output directory.
        name: String,
        /// Its size in bytes.
        size: u64,
    },
    /// Finish successfully with this file. Terminal: later steps are not run.
    Finish {
        /// The file name, relative to the output directory.
        filename: String,
        /// Its size in bytes.
        size: u64,
    },
    /// Fail with this code. Terminal.
    Fail(ErrorCode),
    /// Block until cancelled, without arming a timer. Terminal (with `canceled`).
    Hang,
    /// Resolve into this many synthetic entries. Only meaningful in a `resolve` script.
    ExpandPlaylist(usize),
}

/// One scripted scenario, optionally scoped to the URLs matching `url_regex`.
#[derive(Debug, Default)]
pub struct Timeline {
    /// When set, this timeline applies only to URLs whose full text matches. The first matching
    /// timeline wins; a timeline with no regex is the fallback.
    pub url_regex: Option<Regex>,
    /// The title the synthetic entry gets. Defaults to the URL's last path segment.
    pub title: Option<String>,
    /// What [`Provider::resolve`] does. Empty means "one synthetic video entry".
    pub resolve: Vec<Step>,
    /// What [`Provider::download`] does. Empty means the built-in
    /// preparing → downloading → 100 % → finish script.
    pub download: Vec<Step>,
    /// Whether a `file`/`finish` step actually creates the file in `out_dir`. On by default, so
    /// the static file route and the size bookkeeping have something real to look at.
    pub write_files: bool,
}

impl Timeline {
    /// An empty timeline that writes its files.
    #[must_use]
    pub fn new() -> Self {
        Self {
            write_files: true,
            ..Self::default()
        }
    }

    /// The byte total progress percentages are computed against: the last `finish` size, or
    /// [`DEFAULT_TOTAL_BYTES`].
    #[must_use]
    pub fn total_bytes(&self) -> f64 {
        self.download
            .iter()
            .rev()
            .find_map(|s| match s {
                Step::Finish { size, .. } if *size > 0 => Some(*size as f64),
                _ => None,
            })
            .unwrap_or(DEFAULT_TOTAL_BYTES)
    }

    fn applies_to(&self, url: &Url) -> bool {
        self.url_regex
            .as_ref()
            .is_none_or(|re| re.is_match(url.as_str()))
    }
}

/// A [`FakeProvider`] could not be built from its TOML.
#[derive(Debug, thiserror::Error)]
pub enum FakeError {
    /// The TOML did not parse, or a field had the wrong type.
    #[error("invalid fake timeline: {0}")]
    Toml(#[from] toml::de::Error),
    /// A `url_regex` did not compile.
    #[error("invalid url_regex {pattern:?}: {source}")]
    Regex {
        /// The offending pattern.
        pattern: String,
        /// Why it did not compile.
        source: regex::Error,
    },
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// An id or a file name was not usable.
    #[error("{0}")]
    Invalid(String),
}

/// The scripted provider.
pub struct FakeProvider {
    id: ProviderId,
    catalog: Arc<FormatCatalog>,
    hosts: Vec<String>,
    answer: Match,
    own_slots: Option<usize>,
    health: ProviderHealth,
    timelines: Vec<Timeline>,
    resolves: AtomicUsize,
    downloads: AtomicUsize,
}

impl std::fmt::Debug for FakeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeProvider")
            .field("id", &self.id)
            .field("hosts", &self.hosts)
            .field("answer", &self.answer)
            .field("timelines", &self.timelines.len())
            .finish_non_exhaustive()
    }
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    /// A provider called `fake` that matches every URL at `Strong(200)` and runs the built-in
    /// script: one synthetic entry, then preparing → downloading → 100 % → a 1 KiB file.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: fake_id(),
            catalog: fake_catalog(),
            hosts: Vec::new(),
            answer: Match::Strong(SCORE_SC),
            own_slots: None,
            health: ProviderHealth::Ok,
            timelines: vec![Timeline::new()],
            resolves: AtomicUsize::new(0),
            downloads: AtomicUsize::new(0),
        }
    }

    /// Parses a TOML timeline document.
    ///
    /// # Errors
    /// [`FakeError`] when the document does not parse, a regex does not compile or an id is not a
    /// valid [`ProviderId`].
    pub fn from_toml(src: &str) -> Result<Self, FakeError> {
        let doc: Document = toml::from_str(src)?;
        let mut p = Self::new();
        if let Some(id) = doc.id {
            p.id = ProviderId::parse(&id)
                .map_err(|e| FakeError::Invalid(format!("id {id:?}: {e}")))?;
            p.catalog = Arc::new(FormatCatalog {
                provider: p.id.clone(),
                ..(*p.catalog).clone()
            });
        }
        p.hosts = doc.hosts.into_iter().map(|h| h.to_lowercase()).collect();
        p.answer = if doc.strong {
            Match::Strong(doc.score)
        } else {
            Match::Weak(doc.score)
        };
        p.own_slots = doc.own_slots;
        p.health = match doc.health.as_deref() {
            None | Some("ok") => ProviderHealth::Ok,
            Some("degraded") => {
                ProviderHealth::Degraded(doc.health_reason.unwrap_or_default().into())
            }
            Some("down") => ProviderHealth::Down(doc.health_reason.unwrap_or_default().into()),
            Some(other) => {
                return Err(FakeError::Invalid(format!(
                    "health must be ok, degraded or down, not {other:?}"
                )));
            }
        };
        if !doc.timeline.is_empty() {
            p.timelines = doc
                .timeline
                .into_iter()
                .map(TimelineSpec::build)
                .collect::<Result<_, _>>()?;
        }
        Ok(p)
    }

    /// Parses a TOML timeline file.
    ///
    /// # Errors
    /// [`FakeError::Io`] when the file cannot be read, plus everything [`Self::from_toml`] can
    /// fail with.
    pub fn from_toml_file(path: &Path) -> Result<Self, FakeError> {
        let src = std::fs::read_to_string(path).map_err(|source| FakeError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&src)
    }

    /// Replaces the timelines with one built in Rust — the terse path for a unit test that does
    /// not want a TOML fixture.
    #[must_use]
    pub fn with_timeline(mut self, t: Timeline) -> Self {
        self.timelines = vec![t];
        self
    }

    /// Overrides the id.
    ///
    /// # Errors
    /// [`FakeError::Invalid`] when `id` is not a valid [`ProviderId`].
    pub fn with_id(mut self, id: &str) -> Result<Self, FakeError> {
        self.id =
            ProviderId::parse(id).map_err(|e| FakeError::Invalid(format!("id {id:?}: {e}")))?;
        self.catalog = Arc::new(FormatCatalog {
            provider: self.id.clone(),
            ..(*self.catalog).clone()
        });
        Ok(self)
    }

    /// Overrides what [`Provider::matches`] answers.
    #[must_use]
    pub const fn with_match(mut self, m: Match) -> Self {
        self.answer = m;
        self
    }

    /// Overrides [`Provider::own_slots`].
    #[must_use]
    pub const fn with_own_slots(mut self, slots: Option<usize>) -> Self {
        self.own_slots = slots;
        self
    }

    /// Overrides [`Provider::probe`].
    #[must_use]
    pub fn with_health(mut self, health: ProviderHealth) -> Self {
        self.health = health;
        self
    }

    /// How many times [`Provider::resolve`] has been called.
    #[must_use]
    pub fn resolve_count(&self) -> usize {
        self.resolves.load(Ordering::SeqCst)
    }

    /// How many times [`Provider::download`] has been called.
    #[must_use]
    pub fn download_count(&self) -> usize {
        self.downloads.load(Ordering::SeqCst)
    }

    fn timeline_for(&self, url: &Url) -> &Timeline {
        self.timelines
            .iter()
            .find(|t| {
                t.url_regex
                    .as_ref()
                    .is_some_and(|re| re.is_match(url.as_str()))
            })
            .or_else(|| self.timelines.iter().find(|t| t.applies_to(url)))
            .unwrap_or(&EMPTY_TIMELINE)
    }
}

/// The fallback used when a document declared no timelines at all.
static EMPTY_TIMELINE: Timeline = Timeline {
    url_regex: None,
    title: None,
    resolve: Vec::new(),
    download: Vec::new(),
    write_files: false,
};

/// The provider id the default constructor uses.
#[must_use]
// `"fake"` matches the `ProviderId` grammar by inspection, so this cannot fail; a panic here
// would mean `aulos_core::ProviderId::parse` had regressed, which is worth a loud failure in the
// one test-only crate that calls it.
#[allow(clippy::expect_used)]
pub fn fake_id() -> ProviderId {
    ProviderId::parse("fake").expect("\"fake\" is a valid provider id")
}

/// The catalog the fake provider advertises: one `video`/`mp4`/`best` entry, honestly labelled.
#[must_use]
pub fn fake_catalog() -> Arc<FormatCatalog> {
    Arc::new(FormatCatalog {
        provider: fake_id(),
        version: 1,
        naming: NamingPolicy::Template,
        download_types: vec![DownloadTypeSpec {
            id: "video".into(),
            label: "Video".into(),
            default_format: "mp4".into(),
            options: Vec::new(),
            formats: vec![FormatSpec {
                id: "mp4".into(),
                label: "MP4".into(),
                default_quality: "best".into(),
                qualities: vec![QualitySpec {
                    id: "best".into(),
                    label: "Best".into(),
                    notice: None,
                }],
                codecs: Vec::new(),
                notice: Some("The fake provider ignores every selection.".into()),
                flags: FormatFlags {
                    advisory: true,
                    ..FormatFlags::default()
                },
            }],
        }],
    })
}

fn title_from(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .map_or_else(|| url.as_str().to_owned(), str::to_owned)
}

fn media_id_from(url: &Url) -> String {
    format!("fake:{}", title_from(url))
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn matches(&self, url: &Url) -> Match {
        if self.hosts.is_empty() {
            return self.answer;
        }
        let host = url.host_str().unwrap_or_default().to_lowercase();
        if self.hosts.iter().any(|h| host.contains(h.as_str())) {
            self.answer
        } else {
            Match::No
        }
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        url: &Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        self.resolves.fetch_add(1, Ordering::SeqCst);
        let timeline = self.timeline_for(url);
        let title = timeline.title.clone().unwrap_or_else(|| title_from(url));

        for step in &timeline.resolve {
            match step {
                Step::Wait(d) => wait(*d, &ctx.cancel).await?,
                Step::Hang => return hang(&ctx.cancel).await,
                Step::Fail(code) => return Err(fail(*code)),
                Step::ExpandPlaylist(n) => {
                    let count = u32::try_from(*n).unwrap_or(u32::MAX);
                    let mut out = Vec::with_capacity(*n);
                    for i in 1..=*n {
                        let index = u32::try_from(i).unwrap_or(u32::MAX);
                        let mut child_url = url.clone();
                        child_url.set_query(Some(&format!("fake_index={i}")));
                        let mut child = MediaEntry::video(
                            format!("fake:{title}:{i}"),
                            format!("{title} #{i}"),
                            child_url,
                        );
                        child.hints = EntryHints {
                            playlist_index: Some(index),
                            playlist_count: Some(count),
                            playlist_title: Some(title.clone().into()),
                            ext: Some("mp4".into()),
                            ..EntryHints::default()
                        };
                        out.push(child);
                    }
                    return Ok(out);
                }
                Step::Stage(_)
                | Step::Progress { .. }
                | Step::File { .. }
                | Step::Finish { .. } => {
                    tracing::debug!(?step, "step ignored in a resolve script");
                }
            }
        }

        let mut entry = MediaEntry::video(media_id_from(url), title, url.clone());
        entry.hints.ext = Some("mp4".into());
        entry.kind = EntryKind::Video;
        Ok(vec![entry])
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        let timeline = self.timeline_for(&ctx.entry.url);
        let total = timeline.total_bytes();

        if timeline.download.is_empty() {
            return default_download(&ctx, &sink, timeline.write_files).await;
        }

        let mut outcome = Outcome::default();
        for step in &timeline.download {
            if ctx.cancel.is_cancelled() {
                return Err(ProviderError::Canceled);
            }
            match step {
                Step::Wait(d) => wait(*d, &ctx.cancel).await?,
                Step::Stage(s) => sink.stage(*s, None).await,
                Step::Progress {
                    percent,
                    speed,
                    eta,
                } => {
                    sink.progress(RawProgress {
                        downloaded_bytes: Some(percent.clamp(0.0, 100.0) / 100.0 * total),
                        total_bytes: Some(total),
                        speed: *speed,
                        eta: *eta,
                        ..RawProgress::default()
                    });
                }
                Step::File { slot, name, size } => {
                    if timeline.write_files {
                        write_file(&ctx.out_dir.join(name), *size)?;
                    }
                    let file = FileRef {
                        filename: name.as_str().into(),
                        size: Some(*size),
                        download_url: None,
                        lang: if *slot == FileSlot::Subtitle {
                            Some("en".into())
                        } else {
                            None
                        },
                    };
                    sink.file(*slot, file.clone()).await;
                    outcome = match slot {
                        FileSlot::Chapter => outcome.with_chapter(file),
                        FileSlot::Subtitle => outcome.with_subtitle(file),
                    };
                }
                Step::Finish { filename, size } => {
                    if timeline.write_files {
                        write_file(&ctx.out_dir.join(filename), *size)?;
                    }
                    let rel = RelPath::parse(filename).map_err(|e| {
                        ProviderError::Contract(format!("timeline filename {filename:?}: {e}"))
                    })?;
                    outcome.filename = Some(rel);
                    outcome.size = Some(*size);
                    outcome.entry_final = Some(ctx.entry.state.clone());
                    return Ok(outcome);
                }
                Step::Fail(code) => return Err(fail(*code)),
                Step::Hang => return hang(&ctx.cancel).await,
                Step::ExpandPlaylist(_) => {
                    tracing::debug!(?step, "step ignored in a download script");
                }
            }
        }

        // A script that ran off the end still has to produce a file, or the engine would have to
        // special-case the fake provider.
        let (rel, size) = synth_file(&ctx, timeline.write_files)?;
        outcome.filename = Some(rel);
        outcome.size = Some(size);
        outcome.entry_final = Some(ctx.entry.state.clone());
        Ok(outcome)
    }

    fn own_slots(&self) -> Option<usize> {
        self.own_slots
    }

    async fn probe(&self) -> ProviderHealth {
        self.health.clone()
    }
}

/// The size the built-in script and a `finish` step with no explicit size produce.
pub const DEFAULT_FILE_SIZE: u64 = 1024;

/// The built-in script: preparing → downloading → 100 % → a small file named after the entry.
async fn default_download(
    ctx: &DownloadCtx<'_>,
    sink: &ProgressSink,
    write_files: bool,
) -> Result<Outcome, ProviderError> {
    sink.stage(Stage::Preparing, None).await;
    sink.stage(Stage::Downloading, None).await;
    sink.progress(RawProgress {
        downloaded_bytes: Some(f64::from(
            u32::try_from(DEFAULT_FILE_SIZE).unwrap_or(u32::MAX),
        )),
        total_bytes: Some(f64::from(
            u32::try_from(DEFAULT_FILE_SIZE).unwrap_or(u32::MAX),
        )),
        ..RawProgress::default()
    });
    let (rel, size) = synth_file(ctx, write_files)?;
    let mut outcome = Outcome::file(rel, size);
    outcome.entry_final = Some(ctx.entry.state.clone());
    Ok(outcome)
}

/// Names, and optionally writes, the default output file: the entry title, sanitised, `.mp4`.
fn synth_file(ctx: &DownloadCtx<'_>, write_files: bool) -> Result<(RelPath, u64), ProviderError> {
    let mut stem = sanitize_path_component(&ctx.entry.title);
    if stem.is_empty() {
        stem = "fake".to_owned();
    }
    let name = format!("{stem}.mp4");
    if write_files {
        write_file(&ctx.out_dir.join(&name), DEFAULT_FILE_SIZE)?;
    }
    let rel = RelPath::parse(&name)
        .map_err(|e| ProviderError::Contract(format!("derived filename {name:?}: {e}")))?;
    Ok((rel, DEFAULT_FILE_SIZE))
}

/// Sleeps, or returns `canceled` if the token fires first. The sleep is a real timer, so paused
/// time auto-advances through it.
async fn wait(d: Duration, cancel: &CancellationToken) -> Result<(), ProviderError> {
    tokio::select! {
        () = tokio::time::sleep(d) => Ok(()),
        () = cancel.cancelled() => Err(ProviderError::Canceled),
    }
}

/// Blocks on cancellation and nothing else — deliberately **not** a timer, so paused time cannot
/// advance past it and the caller's stall watchdog is what has to notice.
async fn hang<T>(cancel: &CancellationToken) -> Result<T, ProviderError> {
    cancel.cancelled().await;
    Err(ProviderError::Canceled)
}

fn fail(code: ErrorCode) -> ProviderError {
    ProviderError::from_code(code, format!("scripted failure: {code}"))
}

fn write_file(path: &Path, size: u64) -> Result<(), ProviderError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ProviderError::Disk(format!("{}: {e}", parent.display())))?;
    }
    let capped = usize::try_from(size.min(4 * 1024 * 1024)).unwrap_or(0);
    std::fs::write(path, vec![0u8; capped])
        .map_err(|e| ProviderError::Disk(format!("{}: {e}", path.display())))
}

// ---------------------------------------------------------------------------
// The TOML model. Private, because [`Step`] is the public shape (PLAN WP-03) and internally
// tagged serde cannot express its newtype variants.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default)]
    id: Option<String>,
    #[serde(default = "default_score")]
    score: u8,
    #[serde(default = "default_true")]
    strong: bool,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    own_slots: Option<usize>,
    #[serde(default)]
    health: Option<String>,
    #[serde(default)]
    health_reason: Option<String>,
    #[serde(default)]
    timeline: Vec<TimelineSpec>,
}

const fn default_score() -> u8 {
    SCORE_SC
}

const fn default_true() -> bool {
    true
}

const fn default_size() -> u64 {
    DEFAULT_FILE_SIZE
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineSpec {
    #[serde(default)]
    url_regex: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    resolve: Vec<StepSpec>,
    #[serde(default)]
    download: Vec<StepSpec>,
    #[serde(default = "default_true")]
    write_files: bool,
}

impl TimelineSpec {
    fn build(self) -> Result<Timeline, FakeError> {
        let url_regex = match self.url_regex {
            None => None,
            Some(p) => {
                Some(Regex::new(&p).map_err(|source| FakeError::Regex { pattern: p, source })?)
            }
        };
        Ok(Timeline {
            url_regex,
            title: self.title,
            resolve: self.resolve.into_iter().map(StepSpec::build).collect(),
            download: self.download.into_iter().map(StepSpec::build).collect(),
            write_files: self.write_files,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StepSpec {
    Wait {
        ms: u64,
    },
    Stage {
        stage: Stage,
    },
    Progress {
        percent: f64,
        #[serde(default)]
        speed: Option<f64>,
        #[serde(default)]
        eta: Option<i64>,
    },
    File {
        slot: FileSlot,
        name: String,
        #[serde(default = "default_size")]
        size: u64,
    },
    Finish {
        filename: String,
        #[serde(default = "default_size")]
        size: u64,
    },
    Fail {
        code: ErrorCode,
    },
    Hang,
    ExpandPlaylist {
        count: usize,
    },
}

impl StepSpec {
    fn build(self) -> Step {
        match self {
            Self::Wait { ms } => Step::Wait(Duration::from_millis(ms)),
            Self::Stage { stage } => Step::Stage(stage),
            Self::Progress {
                percent,
                speed,
                eta,
            } => Step::Progress {
                percent,
                speed,
                eta,
            },
            Self::File { slot, name, size } => Step::File { slot, name, size },
            Self::Finish { filename, size } => Step::Finish { filename, size },
            Self::Fail { code } => Step::Fail(code),
            Self::Hang => Step::Hang,
            Self::ExpandPlaylist { count } => Step::ExpandPlaylist(count),
        }
    }
}
