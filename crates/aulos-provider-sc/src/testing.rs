//! A scripted in-process [`ScHttp`] for the unit tests.
//!
//! Every scrape step is tested against checked-in HTML/JSON captured from the real site rather
//! than against a live host, so the suite is deterministic, offline and fast. The mock also counts
//! requests per URL, which is what turns "a season resolves in 2 requests" and "the m3u8 is never
//! fetched" from prose into assertions.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use url::Url;

use crate::error::ScError;
use crate::http::{ScHttp, ScReq, ScRes};

/// One scripted URL: a queue of responses, the last of which repeats forever.
#[derive(Clone, Debug)]
struct Scripted {
    responses: Vec<(u16, String)>,
}

/// The recorded facts about one request.
#[derive(Clone, Debug)]
struct Recorded {
    headers: Vec<(String, String)>,
}

/// A scripted `ScHttp`.
#[derive(Debug, Default)]
pub struct MockHttp {
    routes: Mutex<HashMap<String, Scripted>>,
    locations: Mutex<HashMap<String, String>>,
    calls: Mutex<Vec<(String, Recorded)>>,
    cookies: String,
    /// What [`ScHttp::new_session`] hands out, if anything. `None` (the default) means "no
    /// separable session", so a caller that forks keeps using this mock and every existing test
    /// script still applies.
    session: Mutex<Option<std::sync::Arc<MockHttp>>>,
}

impl MockHttp {
    /// An empty mock: every request is a 404 with an empty body.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares the `Cookie:` header value the mock hands out.
    #[must_use]
    pub fn with_cookies(mut self, cookies: &str) -> Self {
        self.cookies = cookies.to_owned();
        self
    }

    /// Declares the client this mock hands out from [`ScHttp::new_session`].
    ///
    /// Lets a test prove that a caller which claims to run on a fresh cookie jar really does fork
    /// one: script the routes on the *session* and leave the parent empty.
    #[must_use]
    pub fn with_session(self, session: std::sync::Arc<MockHttp>) -> Self {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(session);
        self
    }

    /// Scripts one URL with a single response that repeats.
    #[must_use]
    pub fn on(self, url: &str, status: u16, body: &str) -> Self {
        self.on_sequence(url, vec![(status, body.to_owned())])
    }

    /// Scripts a redirect without following it inside the transport.
    #[must_use]
    pub fn on_redirect(self, url: &str, status: u16, location: &str) -> Self {
        self.locations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url.to_owned(), location.to_owned());
        self.on(url, status, "")
    }

    /// Scripts one URL with a sequence of responses; the last repeats.
    #[must_use]
    pub fn on_sequence(self, url: &str, responses: Vec<(u16, String)>) -> Self {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url.to_owned(), Scripted { responses });
        self
    }

    /// How many times `url` was requested.
    #[must_use]
    pub fn count(&self, url: &str) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(u, _)| u == url)
            .count()
    }

    /// Every URL requested, in order.
    #[must_use]
    pub fn urls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(u, _)| u.clone())
            .collect()
    }

    /// The total number of requests.
    #[must_use]
    pub fn total(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The headers sent with the first request to `url`.
    #[must_use]
    pub fn headers_for(&self, url: &str) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(u, _)| u == url)
            .map(|(_, r)| r.headers.clone())
            .unwrap_or_default()
    }

    /// Whether any request URL contains `needle`.
    #[must_use]
    pub fn requested_anything_containing(&self, needle: &str) -> bool {
        self.urls().iter().any(|u| u.contains(needle))
    }
}

#[async_trait]
impl ScHttp for MockHttp {
    async fn get(&self, req: ScReq) -> Result<ScRes, ScError> {
        let key = req.url.to_string();
        let seen = self.count(&key);
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                key.clone(),
                Recorded {
                    headers: req
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                },
            ));
        let scripted = self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        let (status, body) = match scripted {
            Some(s) => {
                let idx = seen.min(s.responses.len().saturating_sub(1));
                s.responses
                    .get(idx)
                    .cloned()
                    .unwrap_or((404, String::new()))
            }
            None => (404, String::new()),
        };
        Ok(ScRes {
            status,
            location: self
                .locations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
                .cloned(),
            url: Url::parse(&key).unwrap_or(req.url),
            body,
        })
    }

    fn impersonating(&self) -> bool {
        // The plain client is what the whole test matrix exercises (DESIGN §10.1: there is no
        // BoringSSL in CI), so the mock reports the same thing it does.
        false
    }

    fn cookie_header(&self) -> String {
        self.cookies.clone()
    }

    fn new_session(&self) -> Option<std::sync::Arc<dyn ScHttp>> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        Some(session as std::sync::Arc<dyn ScHttp>)
    }
}

// ---------------------------------------------------------------------------
// WP-09: the download-engine fixtures.
// ---------------------------------------------------------------------------

/// The directory the stand-in binaries and captured outputs live in.
///
/// An absolute path built from `CARGO_MANIFEST_DIR`, so a test does not depend on the working
/// directory the harness happens to use.
#[must_use]
pub fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sc")
}

/// One of the stand-in binaries in `tests/fixtures/sc/bin`.
///
/// These are checked-in `sh` scripts with the executable bit set. They are what makes the engine
/// suite runnable on a machine with no `N_m3u8DL-RE`: `EngineCfg` names its binaries, so a test
/// points `argv[0]` at a script that behaves like the tool — writes the output file, prints
/// captured Spectre.Console frames or an ffmpeg progress stream, exits non-zero, or hangs.
#[must_use]
pub fn fixture_bin(name: &str) -> std::path::PathBuf {
    fixture_dir().join("bin").join(name)
}

/// A real ffmpeg, for the one gated test that needs one. `None` when there is none installed.
#[must_use]
pub fn real_ffmpeg() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = vec![
        "/opt/homebrew/bin/ffmpeg".into(),
        "/usr/local/bin/ffmpeg".into(),
        "/usr/bin/ffmpeg".into(),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join("ffmpeg")));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Writes a file, creating its parent directory.
pub fn write(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, bytes).expect("write");
}

/// Everything one engine test needs: the temp directories, the entry, the request and the
/// [`EngineCfg`] whose binaries the test overrides.
pub struct EngineFixture {
    /// Keeps the temp tree alive.
    _tmp: tempfile::TempDir,
    /// The download root (`DOWNLOAD_DIR`).
    root: std::path::PathBuf,
    /// The scratch root (`TEMP_DIR`).
    temp: std::path::PathBuf,
    /// The engine configuration under test.
    pub cfg: crate::engines::EngineCfg,
    /// The resolved entry being downloaded.
    pub entry: aulos_provider::entry::MediaEntry,
    /// Its state blob, already inside `entry.state`.
    pub state: crate::state::ScState,
    /// The request.
    pub request: aulos_core::request::DownloadRequest,
    /// The output template, used only when `cfg.use_output_template` is on.
    pub outtmpl: String,
    /// The job's cancellation token.
    pub cancel: tokio_util::sync::CancellationToken,
    /// The item id.
    pub item_id: aulos_core::id::ItemId,
}

impl EngineFixture {
    /// An episode of `Una Serie`, ready to download.
    pub async fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("downloads");
        let temp = tmp.path().join("temp");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&temp).expect("mkdir");
        // `DownloadCtx::out_dir` is documented as already containment-checked, and
        // `paths::contain` canonicalises — on macOS `/var` is a symlink to `/private/var`, so a
        // fixture that skipped this would compare a canonical path against a symlinked one.
        let root = std::fs::canonicalize(&root).expect("canonicalize");
        let temp = std::fs::canonicalize(&temp).expect("canonicalize");

        let state = crate::state::ScState::episode(
            "https://sc.test",
            Some(9),
            Some(77),
            1,
            2,
            "Pilota",
            "Una Serie",
        );
        let url = Url::parse("https://sc.test/it/watch/9?e=77").expect("url");
        let mut entry = aulos_provider::entry::MediaEntry::video(
            "sc_9_77",
            "Una Serie S01E02 - Pilota",
            url.clone(),
        );
        entry.state = state.to_json();

        use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
        let request = aulos_core::request::DownloadRequest::new(
            url,
            Selection::new(
                DownloadType::Video,
                Codec::Auto,
                FormatId::parse("mp4").expect("format"),
                QualityId::parse("best").expect("quality"),
            ),
        );

        Self {
            _tmp: tmp,
            root,
            temp,
            cfg: crate::engines::EngineCfg::default(),
            entry,
            state,
            request,
            outtmpl: "%(title)s.%(ext)s".to_owned(),
            cancel: tokio_util::sync::CancellationToken::new(),
            item_id: aulos_core::id::ItemId::new(),
        }
    }

    /// Puts the item in a custom folder, as `POST add` with `folder` does.
    pub async fn with_folder(&mut self, folder: &str) {
        std::fs::create_dir_all(self.root.join(folder)).expect("mkdir");
        self.request.folder = Some(aulos_core::paths::RelDir::parse(folder).expect("folder"));
    }

    /// The absolute output directory: the root plus the request's folder.
    #[must_use]
    pub fn out_dir(&self) -> std::path::PathBuf {
        match &self.request.folder {
            Some(f) => self.root.join(f.as_str()),
            None => self.root.clone(),
        }
    }

    /// The absolute scratch directory.
    #[must_use]
    pub fn tmp_dir(&self) -> std::path::PathBuf {
        self.temp.clone()
    }

    /// The borrowed download context.
    #[must_use]
    pub fn ctx(&self) -> aulos_provider::provider::DownloadCtx<'_> {
        aulos_provider::provider::DownloadCtx {
            item_id: self.item_id,
            source: aulos_core::SourceKind::ApiV2,
            entry: &self.entry,
            request: &self.request,
            ytdl_options: std::sync::Arc::new(aulos_core::ytdl_options::YtdlOptions::default()),
            out_dir: self.out_dir(),
            tmp_dir: self.tmp_dir(),
            outtmpl: aulos_provider::provider::OutTmpl {
                default: self.outtmpl.clone(),
                chapter: String::new(),
            },
            cancel: self.cancel.clone(),
        }
    }

    /// A sink and its receiver.
    #[must_use]
    pub fn sink(
        &self,
    ) -> (
        aulos_provider::sink::ProgressSink,
        tokio::sync::mpsc::Receiver<aulos_provider::sink::ProgressMsg>,
    ) {
        let (factory, rx) = aulos_provider::sink::ProgressSinkFactory::channel();
        (factory.for_item(self.item_id), rx)
    }

    /// A provider whose HTTP client serves the just-in-time scrape from the checked-in fixtures,
    /// and whose engine configuration is this fixture's.
    #[must_use]
    pub fn provider(
        &self,
    ) -> (
        crate::provider::ScProvider,
        aulos_provider::sink::ProgressSink,
        tokio::sync::mpsc::Receiver<aulos_provider::sink::ProgressMsg>,
    ) {
        let http = MockHttp::new()
            .with_cookies("sid=abc")
            .on(
                "https://sc.test/",
                200,
                include_str!("../tests/fixtures/sc/it_page.html"),
            )
            .on(
                "https://sc.test/it/watch/9?e=77",
                200,
                include_str!("../tests/fixtures/sc/watch_episode.json"),
            )
            .on(
                "https://sc.test/embed/456",
                200,
                include_str!("../tests/fixtures/sc/embed.html"),
            )
            .on(
                "https://vixcloud.co/embed/98765?token=abc&referer=1",
                200,
                include_str!("../tests/fixtures/sc/vixcloud_streams_active.html"),
            );
        let cfg =
            aulos_core::config::load(&aulos_core::config::RawEnv::from_pairs(
                Vec::<(&str, &str)>::new(),
            ))
            .expect("config");
        let provider = crate::provider::ScProvider::with_http(&cfg, std::sync::Arc::new(http))
            .with_engine(self.cfg.clone());
        let (sink, rx) = self.sink();
        (provider, sink, rx)
    }
}

/// Drains the sink: every `msg` and every progress frame it saw, in order.
///
/// One function rather than two, because draining twice would throw away whichever half was asked
/// for second.
#[must_use]
pub fn drain(
    rx: &mut tokio::sync::mpsc::Receiver<aulos_provider::sink::ProgressMsg>,
) -> (Vec<String>, Vec<aulos_core::progress::RawProgress>) {
    let (mut msgs, mut frames) = (Vec::new(), Vec::new());
    while let Ok(m) = rx.try_recv() {
        match m {
            aulos_provider::sink::ProgressMsg::Stage { msg: Some(m), .. } => {
                msgs.push(m.to_string());
            }
            aulos_provider::sink::ProgressMsg::Progress { raw, .. } => frames.push(raw),
            aulos_provider::sink::ProgressMsg::Stage { .. }
            | aulos_provider::sink::ProgressMsg::File { .. } => {}
        }
    }
    (msgs, frames)
}

/// The legacy status-message sequence the sink saw.
#[must_use]
pub fn drain_messages(
    rx: &mut tokio::sync::mpsc::Receiver<aulos_provider::sink::ProgressMsg>,
) -> Vec<String> {
    drain(rx).0
}

/// Every progress frame the sink saw, in order.
#[must_use]
pub fn drain_frames(
    rx: &mut tokio::sync::mpsc::Receiver<aulos_provider::sink::ProgressMsg>,
) -> Vec<aulos_core::progress::RawProgress> {
    drain(rx).1
}
