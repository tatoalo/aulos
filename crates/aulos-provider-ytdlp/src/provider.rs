//! The `ytdlp` [`Provider`]: the catch-all fallback that ties the four pure modules to a running
//! shim process (DESIGN §6.1, §6.3, §9).
//!
//! # `matches` is unconditional
//!
//! [`YtdlpProvider::matches`] returns [`Match::Weak`]`(1)` for **every** URL. That is the whole
//! point of a fallback: because [`Match`]'s ordering puts every `Strong` above every `Weak`, a
//! real provider always wins, and a URL nobody claims still gets a downloader instead of a
//! `unsupported_url` from the registry. Scheme validation belongs to the request validator
//! (DESIGN §4.3), not here — a provider that second-guessed it would make the fallback property
//! unprovable.
//!
//! # Option assembly, in legacy order
//!
//! [`YtdlpProvider::download`] reproduces legacy `Download._download` exactly: a base dict
//! (`quiet`, `no_color`, `paths`, `outtmpl`, `format`, `socket_timeout`,
//! `ignore_no_formats_error`), then **the user options merged over it**, because legacy spread
//! `**self.ytdl_opts` last. [`crate::opts::get_opts`] has already layered env → file → presets →
//! per-request overrides and appended the derived postprocessors, so this module only adds the
//! base and the chapter-splitting step.
//!
//! The one thing applied *after* the user merge is [`pin_sidecars_to_the_scratch_dir`]: the
//! `.info.json` and `.description` have to follow the media into the per-job scratch directory,
//! and a `paths` key in `YTDL_OPTIONS` replaces the base dict's wholesale.
//!
//! [`YtdlpProvider::resolve`] reproduces legacy `__extract_info` with the opposite precedence:
//! MeTube's extraction keys (`extract_flat`, `noplaylist`, `ignore_no_formats_error`) are applied
//! **after** the user options, so a preset cannot break resolution. The shim applies them, so
//! there is one place that decides.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aulos_core::catalog::FormatCatalog;
use aulos_core::config::Config;
use aulos_core::id::ItemId;
use aulos_core::selection::{DownloadType, ProviderId};
use aulos_provider::entry::MediaEntry;
use aulos_provider::outcome::Outcome;
use aulos_provider::provider::{
    DownloadCtx, Match, Provider, ProviderError, ProviderHealth, ResolveCtx, SCORE_FALLBACK,
};
use aulos_provider::sink::{ProgressSink, ProgressSinkFactory, Stage};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::catalog::ytdlp_catalog;
use crate::formats::get_format;
use crate::job::{ExtractOpts, Job, Policy, debug_logging};
use crate::opts::get_opts;
use crate::outtmpl::OutTmplJob;
use crate::runner::{
    DEFAULT_PYTHON, DEFAULT_RUNNER_PATH, RunnerHandle, RunnerOutcome, ShimIdentity,
};

/// This provider's id on the wire and in the registry.
pub const ID: &str = "ytdlp";

/// Legacy's `socket_timeout` for every download and every extraction.
pub const SOCKET_TIMEOUT: u64 = 30;

/// How long [`YtdlpProvider::probe`] gives the interpreter to import yt-dlp.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The `ytdlp` provider.
pub struct YtdlpProvider {
    id: ProviderId,
    cfg: Arc<Config>,
    runner: RunnerHandle,
    catalog: Arc<FormatCatalog>,
}

/// [`ID`] as a [`ProviderId`].
///
/// Parsed rather than constructed because `ProviderId`'s invariants live in its own validator;
/// the literal is a compile-time constant, so the failure branch is genuinely unreachable.
#[must_use]
fn provider_id() -> ProviderId {
    ProviderId::parse(ID).unwrap_or_else(|_| unreachable!("`{ID}` is a valid provider id"))
}

impl YtdlpProvider {
    /// A provider that runs `python <runner>` for every job.
    ///
    /// The interpreter and the shim path are injected rather than read from the environment so
    /// that the tests can point at a stub and the image can move the file without a code change.
    #[must_use]
    pub fn new(cfg: Arc<Config>, python: PathBuf, runner: PathBuf) -> Self {
        Self {
            id: provider_id(),
            runner: RunnerHandle::from_config(&cfg, python, runner),
            cfg,
            catalog: ytdlp_catalog(),
        }
    }

    /// A provider using the image's own layout: `python3 /app/python/ytdlp_runner.py`.
    #[must_use]
    pub fn with_defaults(cfg: Arc<Config>) -> Self {
        Self::new(
            cfg,
            PathBuf::from(DEFAULT_PYTHON),
            PathBuf::from(DEFAULT_RUNNER_PATH),
        )
    }

    /// The runner this provider drives, for a caller that needs the `mode = outtmpl` round trip
    /// of [`crate::outtmpl::build_outtmpl`].
    #[must_use]
    pub const fn runner(&self) -> &RunnerHandle {
        &self.runner
    }

    /// What the shim last reported about itself, for `healthz` and `GET <p>version`.
    #[must_use]
    pub fn identity(&self) -> Option<ShimIdentity> {
        self.runner.identity()
    }

    /// The output templates for one download, playlist/channel fields pre-resolved.
    ///
    /// [`DownloadCtx::outtmpl`] arrives built from the config templates alone: `aulos-queue` may
    /// not depend on this crate (DESIGN §3), so it reproduces legacy's prefix handling and the
    /// `OUTPUT_TEMPLATE_PLAYLIST`/`_CHANNEL` swap but *not* `_resolve_outtmpl_fields`. This is
    /// where that pre-resolution happens, because this is the only place that holds both the
    /// entry blob and the shim (the WP-12 request in `docs/INTEGRATION-NOTES.md`). Without it a
    /// template using `%(playlist_id)s` or `%(playlist_uploader)s` degrades to yt-dlp's `NA`.
    ///
    /// A single video, or a template with no `playlist*`/`channel*` reference, is
    /// [`OutTmplJob::is_ready`] and spawns nothing.
    ///
    /// # Errors
    /// Anything [`YtdlpProvider::resolve_outtmpl`] can fail with.
    async fn download_outtmpl(
        &self,
        ctx: &DownloadCtx<'_>,
        sink: &ProgressSink,
    ) -> Result<aulos_provider::provider::OutTmpl, ProviderError> {
        let job = crate::outtmpl::outtmpl_job(&self.cfg, ctx.request, ctx.entry);
        self.resolve_outtmpl(ctx.item_id, &job, sink, &ctx.cancel)
            .await
    }

    /// Pre-resolves the `playlist*` / `channel*` fields of an output template.
    ///
    /// A job that is already [`OutTmplJob::is_ready`] spawns nothing; otherwise this is the one
    /// place the `mode = outtmpl` round trip happens, so the full yt-dlp template grammar keeps
    /// working exactly as it did in legacy (DESIGN §9.8).
    ///
    /// # Errors
    /// [`ProviderError::Contract`] when the shim returns the wrong number of strings, plus
    /// anything [`RunnerHandle::run`] can fail with.
    pub async fn resolve_outtmpl(
        &self,
        item_id: ItemId,
        job: &OutTmplJob,
        sink: &ProgressSink,
        cancel: &CancellationToken,
    ) -> Result<aulos_provider::provider::OutTmpl, ProviderError> {
        if let Some(ready) = job.ready() {
            return Ok(ready);
        }
        let request = Job::from_outtmpl(item_id.to_string(), job);
        let RunnerOutcome::OutTmpl(evaluated) = self.runner.run(&request, sink, cancel).await?
        else {
            return Err(ProviderError::Contract(
                "the shim answered an outtmpl job with the wrong result shape".to_owned(),
            ));
        };
        job.apply(&evaluated)
            .map_err(|e| ProviderError::Contract(e.to_string()))
    }

    /// The user option dict for one request: env → file → presets → per-request overrides.
    fn user_options(
        &self,
        ctx_options: &aulos_core::ytdl_options::YtdlOptions,
        request: &aulos_core::request::DownloadRequest,
    ) -> Map<String, Value> {
        ctx_options.layer(
            &request.ytdl_options_presets,
            &request.ytdl_options_overrides,
        )
    }

    /// Whether `LOGLEVEL` asks for yt-dlp's verbose output.
    fn debug_logging(&self) -> bool {
        debug_logging(&self.cfg)
    }

    /// The shim policy for one extraction.
    ///
    /// `debug` is the DESIGN §9.2 switch: the shim derives the extraction's `quiet`/`verbose`
    /// from it *and* gates `FrameLogger`'s `debug`/`info` forwarding on it. Since the shim always
    /// installs that logger, leaving the flag false would swallow every yt-dlp diagnostic even
    /// with `LOGLEVEL=DEBUG` — legacy passed `verbose=True` in the same situation.
    fn extract_policy(&self, download: &Path, temp: &Path) -> Policy {
        Policy {
            download_dir: download.to_path_buf(),
            temp_dir: temp.to_path_buf(),
            debug: self.debug_logging(),
            ..Policy::default()
        }
    }

    /// A sink for an operation the engine did not give one for (`resolve` and `probe`).
    ///
    /// [`Provider::resolve`] and [`Provider::probe`] are both handed no sink by the engine — a
    /// resolution has no percent to report and a probe has no item — but the runner needs one to
    /// forward `log` frames through. The receiver is dropped immediately: [`ProgressSink`]
    /// documents a closed channel as a no-op rather than an error, precisely so a provider does
    /// not need two code paths.
    fn detached_sink() -> ProgressSink {
        let (factory, rx) = ProgressSinkFactory::channel();
        drop(rx);
        factory.for_item(ItemId::new())
    }
}

#[async_trait]
impl Provider for YtdlpProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn matches(&self, _url: &Url) -> Match {
        Match::Weak(SCORE_FALLBACK)
    }

    fn catalog(&self) -> Arc<FormatCatalog> {
        Arc::clone(&self.catalog)
    }

    async fn resolve(
        &self,
        url: &Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        let options = extract_options(
            self.user_options(&ctx.ytdl_options, ctx.request),
            &ctx.paths.download,
            &ctx.paths.temp,
        );

        let job = Job::extract(ctx.item_id.to_string(), url.clone())
            .with_options(options)
            .with_extract(ExtractOpts {
                // Legacy always extracted flat and always set `noplaylist`; `ctx.flat` is the
                // subscription-scan hint, which does not change either flag but does cap the
                // list through `playlist_end`.
                flat: true,
                noplaylist: true,
                playlist_end: ctx.playlist_end,
                ..ExtractOpts::default()
            })
            .with_policy(self.extract_policy(&ctx.paths.download, &ctx.paths.temp));

        let remaining = ctx
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        let runner = self
            .runner
            .clone()
            .with_timeout((!remaining.is_zero()).then_some(remaining));
        let sink = Self::detached_sink();

        match runner.run(&job, &sink, &ctx.cancel).await? {
            RunnerOutcome::Extracted { entries, truncated } => {
                if truncated {
                    tracing::info!(url = %url, "the resolution hit the entry cap and was truncated");
                }
                Ok(entries)
            }
            other => Err(ProviderError::Contract(format!(
                "the shim answered an extract job with {other:?}"
            ))),
        }
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        let selection = &ctx.request.selection;
        let download_type = selection.download_type;
        let format_id = selection.format.as_str();
        let quality = selection.quality.as_str();

        let selector = get_format(download_type, selection.codec, format_id, quality)
            .map_err(|e| ProviderError::NoFormat(e.to_string()))?;

        // Legacy read `logging.getLogger().isEnabledFor(DEBUG)` here; `LOGLEVEL` is the same
        // switch, now read from the typed config instead of from a global logger.
        let debug_logging = self.debug_logging();

        // Legacy's `_resolve_outtmpl_fields`, which the engine structurally cannot do.
        let outtmpl = self.download_outtmpl(&ctx, &sink).await?;

        let user = get_opts(
            download_type,
            format_id,
            quality,
            self.user_options(&ctx.ytdl_options, ctx.request),
            &ctx.request.subtitle_language,
            ctx.request.subtitle_mode,
        );
        let options = download_options(DownloadOptions {
            out_dir: &ctx.out_dir,
            tmp_dir: &ctx.tmp_dir,
            outtmpl: &outtmpl,
            selector: &selector,
            debug_logging,
            split_by_chapters: ctx.request.split_by_chapters,
            user,
        });

        let job = Job::download(ctx.item_id.to_string(), ctx.entry.url.clone())
            .with_options(options)
            .with_policy(Policy {
                subscription_download: ctx.source == aulos_core::SourceKind::Subscription,
                ..Policy::for_download(&self.cfg, download_type, format_id, ctx.out_dir.clone())
            })
            .with_download_root(self.cfg.paths.root_for(download_type).to_path_buf());

        sink.stage(Stage::Preparing, None).await;
        match self.runner.run(&job, &sink, &ctx.cancel).await? {
            RunnerOutcome::Downloaded(outcome) => Ok(outcome),
            other => Err(ProviderError::Contract(format!(
                "the shim answered a download job with {other:?}"
            ))),
        }
    }

    async fn probe(&self) -> ProviderHealth {
        let job = Job::selftest("probe");
        let runner = self.runner.clone().with_timeout(Some(PROBE_TIMEOUT));
        let sink = Self::detached_sink();
        match runner.run(&job, &sink, &CancellationToken::new()).await {
            Ok(RunnerOutcome::Selftest(identity)) => match identity.yt_dlp {
                Some(_) => ProviderHealth::Ok,
                None => ProviderHealth::Degraded(
                    "the shim ran but could not report a yt-dlp version".into(),
                ),
            },
            Ok(other) => ProviderHealth::Degraded(
                format!("the shim answered a selftest with {other:?}").into(),
            ),
            Err(ProviderError::ToolMissing(tool)) => {
                ProviderHealth::Down(format!("{tool} is not installed").into())
            }
            Err(e) => ProviderHealth::Down(e.message().into()),
        }
    }
}

/// Whether `download_type` writes into `AUDIO_DOWNLOAD_DIR` rather than `DOWNLOAD_DIR`.
///
/// Re-exported as a named predicate because three call sites need the same answer and
/// [`aulos_core::paths::Paths::root_for`] is the only other place that knows it.
#[must_use]
pub const fn uses_audio_root(download_type: DownloadType) -> bool {
    matches!(download_type, DownloadType::Audio)
}

/// Everything [`download_options`] needs, so the one function that assembles the download dict
/// is also the one the tests drive.
struct DownloadOptions<'a> {
    out_dir: &'a Path,
    tmp_dir: &'a Path,
    outtmpl: &'a aulos_provider::provider::OutTmpl,
    selector: &'a str,
    debug_logging: bool,
    split_by_chapters: bool,
    user: Map<String, Value>,
}

/// The whole download option dict, in legacy order (DESIGN §9.2).
///
/// Legacy's base dict first, then **the user options merged over it** (legacy spread
/// `**self.ytdl_opts` last), then [`pin_sidecars_to_the_scratch_dir`] — which has to run *after*
/// the merge, because a `paths` key in `YTDL_OPTIONS` replaces the base dict's wholesale and its
/// scratch directory needs the same treatment as ours. Chapter splitting is last, as it was.
///
/// This is a function rather than three statements inside
/// [`YtdlpProvider::download`](Provider::download) so that the ordering the sidecar fix depends
/// on is covered by a test that runs the real code, not a copy of it.
fn download_options(job: DownloadOptions<'_>) -> Map<String, Value> {
    let DownloadOptions {
        out_dir,
        tmp_dir,
        outtmpl,
        selector,
        debug_logging,
        split_by_chapters,
        user,
    } = job;

    let mut options: Map<String, Value> = serde_json::from_value(json!({
        "quiet": !debug_logging,
        "verbose": debug_logging,
        "no_color": true,
        "paths": { "home": out_dir, "temp": tmp_dir },
        "outtmpl": { "default": outtmpl.default, "chapter": outtmpl.chapter },
        "format": selector,
        "socket_timeout": SOCKET_TIMEOUT,
        "ignore_no_formats_error": true,
    }))
    .unwrap_or_default();

    for (key, value) in user {
        options.insert(key, value);
    }
    pin_sidecars_to_the_scratch_dir(&mut options);

    if split_by_chapters {
        // Legacy appended this after everything else and pinned the chapter template.
        if let Some(templates) = options.get_mut("outtmpl").and_then(Value::as_object_mut) {
            templates.insert("chapter".to_owned(), Value::String(outtmpl.chapter.clone()));
        }
        let list = options
            .entry("postprocessors".to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(array) = list.as_array_mut() {
            array.push(json!({ "key": "FFmpegSplitChapters", "force_keyframes": false }));
        }
    }
    options
}

/// The sidecar output types yt-dlp resolves through `paths[<type>]` but writes straight to
/// `paths.home` and then never moves (DESIGN §9.2).
///
/// Subtitles and thumbnails are already written next to the *temp* file and handed to
/// `MoveFilesAfterDownloadPP`; these two are not. Link shortcut files (`writeurllink` and
/// friends) share the flaw but are deliberately left alone — see the shim's
/// `HOME_SIDECAR_TYPES`.
const HOME_SIDECAR_TYPES: [&str; 2] = ["description", "infojson"];

/// Points the sidecars of [`HOME_SIDECAR_TYPES`] at the job's scratch directory, so the whole
/// file set lives in one directory at every stage of the download (DESIGN §9.2).
///
/// Legacy MeTube had `temp == home`, so nothing ever noticed that yt-dlp writes the
/// `.info.json` and `.description` at the destination while the media is still in the scratch
/// directory. With the per-job `/downloads/<ULID>/` of DESIGN §8.7 the split is real, and every
/// `Exec` postprocessor that resolves a sidecar relative to `%(filepath)q` — MeTube's
/// `jellyfin_nfo_generator.py` is the one nearly every migrating user carries — fails on it,
/// because a postprocessor's default `when` is `post_process`, which runs *before* the move.
///
/// The shim's `SidecarsTravelWithTheMedia` is the other half: yt-dlp does not register these two
/// for the move, so without it they would stay in the scratch directory — and neither does it
/// register whatever a user `Exec` hook *writes* there, which is why that postprocessor sweeps
/// the directory rather than just naming these two types.
///
/// Nothing is done unless `paths` carries a `temp` that actually differs from `home`, and a
/// per-type entry the operator set themselves is never overwritten.
fn pin_sidecars_to_the_scratch_dir(options: &mut Map<String, Value>) {
    let Some(paths) = options.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(temp) = paths.get("temp").and_then(Value::as_str) else {
        return;
    };
    if temp.is_empty() || Some(temp) == paths.get("home").and_then(Value::as_str) {
        return;
    }
    let temp = temp.to_owned();
    for kind in HOME_SIDECAR_TYPES {
        paths
            .entry(kind)
            .or_insert_with(|| Value::String(temp.clone()));
    }
}

/// The extraction option dict: the layered user options plus legacy's `__extract_info` keys.
///
/// Legacy built `{**user_opts, 'quiet':…, 'no_color':…, 'extract_flat':…,
/// 'ignore_no_formats_error':…, 'noplaylist':…, 'paths':…}` — the MeTube keys last, so a preset
/// cannot break resolution. `socket_timeout` was **not** among them, so it is only a default
/// here: an operator who raises it in `YTDL_OPTIONS` for a slow upstream keeps it for extraction
/// too, exactly as they already do for the download (where the user dict is merged last).
fn extract_options(
    mut options: Map<String, Value>,
    download: &Path,
    temp: &Path,
) -> Map<String, Value> {
    options.insert(
        "paths".to_owned(),
        json!({ "home": download, "temp": temp }),
    );
    options.insert("no_color".to_owned(), Value::Bool(true));
    options
        .entry("socket_timeout".to_owned())
        .or_insert_with(|| json!(SOCKET_TIMEOUT));
    options
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use aulos_core::config::RawEnv;

    use super::*;

    fn config() -> Arc<Config> {
        Arc::new(
            aulos_core::config::load(&RawEnv::from_pairs([
                ("DOWNLOAD_DIR", "/downloads"),
                ("AUDIO_DOWNLOAD_DIR", "/audio"),
                ("TEMP_DIR", "/tmp/aulos"),
            ]))
            .expect("config"),
        )
    }

    fn provider() -> YtdlpProvider {
        YtdlpProvider::with_defaults(config())
    }

    #[test]
    fn the_id_is_stable() {
        assert_eq!(provider().id().as_str(), "ytdlp");
    }

    #[test]
    fn it_matches_everything_weakly_so_it_can_never_outrank_a_real_provider() {
        let p = provider();
        for raw in [
            "https://www.youtube.com/watch?v=x",
            "https://streamingcommunity.test/watch/1",
            "http://example.test/",
            "file:///downloads/a.mp4",
        ] {
            let url = Url::parse(raw).unwrap();
            assert_eq!(p.matches(&url), Match::Weak(SCORE_FALLBACK), "{raw}");
            assert!(p.matches(&url) < Match::Strong(0));
            assert!(p.matches(&url).is_match());
        }
    }

    #[test]
    fn it_advertises_the_shared_catalog_by_arc() {
        let p = provider();
        assert!(Arc::ptr_eq(&p.catalog(), &p.catalog()));
        assert!(!p.catalog().download_types.is_empty());
    }

    #[test]
    fn it_takes_no_provider_slots_of_its_own() {
        assert_eq!(provider().own_slots(), None);
    }

    #[test]
    fn the_audio_root_predicate_matches_paths_root_for() {
        let cfg = config();
        for dt in DownloadType::ALL {
            let expected = cfg.paths.root_for(dt) == cfg.paths.audio_download.as_path();
            assert_eq!(uses_audio_root(dt), expected, "{dt:?}");
        }
    }

    #[test]
    fn extraction_keeps_a_user_socket_timeout_but_still_pins_paths_and_no_color() {
        let mut user = Map::new();
        user.insert("socket_timeout".to_owned(), json!(120));
        user.insert("no_color".to_owned(), json!(false));
        user.insert("paths".to_owned(), json!({ "home": "/wrong" }));

        let options = extract_options(user, Path::new("/downloads"), Path::new("/tmp/aulos"));
        // Legacy set no `socket_timeout` for extraction, so the user's value survives.
        assert_eq!(options["socket_timeout"], json!(120));
        // These two legacy *did* apply after the user options.
        assert_eq!(options["no_color"], json!(true));
        assert_eq!(options["paths"]["home"], json!("/downloads"));
        assert_eq!(options["paths"]["temp"], json!("/tmp/aulos"));

        let defaults = extract_options(Map::new(), Path::new("/downloads"), Path::new("/tmp"));
        assert_eq!(defaults["socket_timeout"], json!(SOCKET_TIMEOUT));
    }

    #[test]
    fn loglevel_debug_reaches_the_shim_policy_for_extraction_and_download() {
        let quiet = provider();
        assert!(
            !quiet
                .extract_policy(Path::new("/downloads"), Path::new("/tmp"))
                .debug
        );

        for level in ["DEBUG", "debug", "TRACE"] {
            let cfg = Arc::new(
                aulos_core::config::load(&RawEnv::from_pairs([
                    ("DOWNLOAD_DIR", "/downloads"),
                    ("TEMP_DIR", "/tmp/aulos"),
                    ("LOGLEVEL", level),
                ]))
                .expect("config"),
            );
            let p = YtdlpProvider::with_defaults(Arc::clone(&cfg));
            let policy = p.extract_policy(Path::new("/downloads"), Path::new("/tmp/aulos"));
            assert!(policy.debug, "extract policy at LOGLEVEL={level}");
            // The shim reads `policy.debug` for the download's log forwarding too.
            let download = Policy::for_download(
                &cfg,
                DownloadType::Video,
                "any",
                PathBuf::from("/downloads"),
            );
            assert!(download.debug, "download policy at LOGLEVEL={level}");
        }
    }

    /// One run of the real assembly [`YtdlpProvider::download`] uses, with `user` standing in
    /// for what [`get_opts`] returned.
    ///
    /// Nothing here re-implements `download`: the layering, the pin and the chapter step are all
    /// inside [`download_options`], so a test that moved the pin ahead of the user merge would
    /// fail here rather than pass on a copy.
    fn built(out: &str, tmp: &str, user: &Value) -> Map<String, Value> {
        download_options(DownloadOptions {
            out_dir: Path::new(out),
            tmp_dir: Path::new(tmp),
            outtmpl: &aulos_provider::provider::OutTmpl {
                default: "%(title)s.%(ext)s".to_owned(),
                chapter: "%(title)s - %(section_number)03d.%(ext)s".to_owned(),
            },
            selector: "bv+ba/b",
            debug_logging: false,
            split_by_chapters: false,
            user: user.as_object().cloned().unwrap_or_default(),
        })
    }

    /// The `paths` dict that assembly produced.
    fn download_paths(out: &str, tmp: &str, user: &Value) -> Value {
        built(out, tmp, user)["paths"].clone()
    }

    #[test]
    fn the_download_dict_is_the_base_then_the_user_options_then_the_pin() {
        // The order is the fix: a `paths` key in `YTDL_OPTIONS` replaces the base dict's
        // wholesale, so pinning before the merge would silently lose the sidecar entries. This
        // is the assertion that fails if the pin ever moves ahead of the merge.
        let options = built(
            "/downloads",
            "/downloads/01ABC",
            &json!({ "paths": { "home": "/library", "temp": "/library/01ABC" }, "quiet": false }),
        );
        assert_eq!(options["paths"]["home"], json!("/library"));
        assert_eq!(options["paths"]["infojson"], json!("/library/01ABC"));
        assert_eq!(options["paths"]["description"], json!("/library/01ABC"));
        // …and the rest of legacy's base dict is still underneath the user's.
        assert_eq!(options["quiet"], json!(false));
        assert_eq!(options["format"], json!("bv+ba/b"));
        assert_eq!(options["ignore_no_formats_error"], json!(true));
        assert_eq!(options["socket_timeout"], json!(SOCKET_TIMEOUT));
        assert!(options.get("postprocessors").is_none());
    }

    #[test]
    fn splitting_by_chapters_still_lands_after_everything_else() {
        let options = download_options(DownloadOptions {
            out_dir: Path::new("/downloads"),
            tmp_dir: Path::new("/downloads/01ABC"),
            outtmpl: &aulos_provider::provider::OutTmpl {
                default: "%(title)s.%(ext)s".to_owned(),
                chapter: "chapters/%(title)s.%(ext)s".to_owned(),
            },
            selector: "b",
            debug_logging: false,
            split_by_chapters: true,
            user: serde_json::from_value(json!({ "outtmpl": { "default": "%(id)s.%(ext)s" } }))
                .unwrap(),
        });
        assert_eq!(options["outtmpl"]["default"], json!("%(id)s.%(ext)s"));
        assert_eq!(
            options["outtmpl"]["chapter"],
            json!("chapters/%(title)s.%(ext)s")
        );
        assert_eq!(
            options["postprocessors"],
            json!([{ "key": "FFmpegSplitChapters", "force_keyframes": false }])
        );
        // Even a request that splits keeps its sidecars with the media.
        assert_eq!(options["paths"]["infojson"], json!("/downloads/01ABC"));
    }

    #[test]
    fn the_sidecars_are_written_into_the_scratch_directory_beside_the_media() {
        let paths = download_paths("/downloads", "/downloads/01ABC", &json!({}));
        assert_eq!(paths["home"], json!("/downloads"));
        assert_eq!(paths["temp"], json!("/downloads/01ABC"));
        // Bug 2b: without these two, yt-dlp writes the `.info.json`/`.description` straight to
        // `home` while the media is still in `temp`, and `%(filepath)q` Exec hooks break.
        for kind in HOME_SIDECAR_TYPES {
            assert_eq!(paths[kind], json!("/downloads/01ABC"), "paths.{kind}");
        }
        // The two yt-dlp already writes next to the temp file and moves itself stay unpinned:
        // pinning them would make `MoveFiles` see source == destination and strand them.
        for kind in ["subtitle", "thumbnail"] {
            assert!(paths.get(kind).is_none(), "paths.{kind} must stay unset");
        }
    }

    #[test]
    fn a_scratch_directory_that_is_the_download_directory_pins_nothing() {
        // Legacy MeTube's layout, and the `TEMP_DIR == DOWNLOAD_DIR` default: there is no split
        // to fix, and a pinned entry would only add noise to the job dict.
        let paths = download_paths("/downloads", "/downloads", &json!({}));
        for kind in HOME_SIDECAR_TYPES {
            assert!(paths.get(kind).is_none(), "paths.{kind}");
        }
    }

    #[test]
    fn a_user_paths_dict_is_pinned_to_its_own_scratch_directory_but_never_overridden() {
        let paths = download_paths(
            "/downloads",
            "/downloads/01ABC",
            &json!({ "paths": { "home": "/elsewhere", "temp": "/scratch", "infojson": "/meta" } }),
        );
        // Their `paths` replaces ours wholesale (legacy spread `**self.ytdl_opts` last)…
        assert_eq!(paths["home"], json!("/elsewhere"));
        assert_eq!(paths["temp"], json!("/scratch"));
        // …their explicit per-type entry is theirs to keep…
        assert_eq!(paths["infojson"], json!("/meta"));
        // …and the type they said nothing about still follows the media.
        assert_eq!(paths["description"], json!("/scratch"));
    }

    #[test]
    fn a_paths_dict_with_no_temp_is_left_exactly_as_it_is() {
        let paths = download_paths(
            "/downloads",
            "/downloads/01ABC",
            &json!({ "paths": { "home": "/elsewhere" } }),
        );
        assert_eq!(paths, json!({ "home": "/elsewhere" }));
    }

    #[test]
    fn the_runner_path_defaults_to_the_image_layout() {
        assert_eq!(
            provider().runner().runner_path(),
            std::path::Path::new(DEFAULT_RUNNER_PATH)
        );
    }
}
