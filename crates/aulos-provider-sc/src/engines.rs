//! The download engines (DESIGN §10.5): output naming, the legacy `.info.json` sidecar, engine
//! selection, the automatic ffmpeg retry and partial cleanup.
//!
//! ```text
//! download()
//!   ├─ jit::fresh_stream        S1–S4 again, fresh cookie jar, tokens live for minutes
//!   ├─ OutputNames::plan        <out_dir>/<sanitised title>.mp4  (+ .info.json)
//!   ├─ sidecar                  the LEGACY FLAT shape (DESIGN §10.3 note 2)
//!   └─ SC_USE_FFMPEG ? ffmpeg::download
//!                     : nm3u8dl::download ─(non-zero exit)→ cleanup → ffmpeg::download
//! ```
//!
//! # The output path is deliberately not the output template
//!
//! Every other provider honours `OUTPUT_TEMPLATE`. This one writes
//! `<download_dir>/<sanitised title>.mp4` and ignores the template entirely, because existing
//! Jellyfin libraries — and the `.nfo` files next to them — are keyed on those paths. Opting in is
//! `AULOS_SC_USE_OUTPUT_TEMPLATE=true` (DESIGN §10.5).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use aulos_core::config::Config;
use aulos_core::paths::RelPath;
use aulos_provider::outcome::Outcome;
use aulos_provider::proc::Child;
use aulos_provider::provider::{DownloadCtx, ProviderError};
use aulos_provider::sink::{ProgressSink, Stage};
use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::provider::ScProvider;
use crate::state::ScState;
use crate::{ffmpeg, jit, nm3u8dl};

/// The message legacy put on the item before the `N_m3u8DL-RE` run (`app/ytdl.py:592`).
pub const MSG_START_NM3U8: &str = "Starting N_m3u8DL-RE download...";
/// The message legacy put on the item when `SC_USE_FFMPEG` forced the ffmpeg path
/// (`app/ytdl.py:586`).
pub const MSG_START_FFMPEG: &str = "Starting ffmpeg download...";
/// The message legacy put on the item before the automatic retry (`app/ytdl.py:605`).
pub const MSG_RETRY_FFMPEG: &str = "N_m3u8DL-RE failed, retrying with ffmpeg...";
/// Legacy's message for "the tool exited 0 and produced nothing" (`app/ytdl.py:796`).
pub const MSG_NO_OUTPUT: &str = "Download finished but output file not found";
/// Legacy's message for an empty segment directory (`app/ytdl.py:818`).
pub const MSG_NO_SEGMENTS: &str = "Download finished but no segments to mux";
/// Legacy's message for a failed fallback mux (`app/ytdl.py:840`).
pub const MSG_MUX_FAILED: &str = "Download finished but muxing failed";

/// The extension every StreamingCommunity download produces.
pub const OUTPUT_EXT: &str = "mp4";

/// How many trailing output lines an error message quotes (legacy `lines[-20:]`).
pub const TAIL_REPORTED: usize = 20;
/// The character cap on a quoted tail (legacy `[-500:]`).
pub const TAIL_CHARS: usize = 500;

/// Legacy's error tail: `"\n".join(lines[-20:]).strip()[-500:]`.
///
/// The cap is what keeps a provider message inside the ≤ 512-character budget of DESIGN §9.6, and
/// taking the *end* rather than the start is deliberate: ffmpeg and `N_m3u8DL-RE` both print the
/// reason last.
#[must_use]
pub fn error_tail(lines: &[String]) -> String {
    let skip = lines.len().saturating_sub(TAIL_REPORTED);
    let joined = lines[skip..].join("\n");
    let joined = joined.trim();
    let n = joined.chars().count();
    if n <= TAIL_CHARS {
        return joined.to_owned();
    }
    joined.chars().skip(n - TAIL_CHARS).collect()
}

/// [`error_tail`] of a child's stderr ring, once the drain has reached end of stream.
///
/// `proc::Child`'s stderr drain is a separate task (it has to be: an undrained pipe deadlocks the
/// child), so a child's last lines may still be in flight at the instant `wait()` returns.
/// Quoting the ring immediately therefore *sometimes* produced `FFmpeg failed with code 3` with no
/// reason attached — the error message a user sees would depend on task scheduling.
/// [`Child::drained`] joins the drain instead of polling for it, so the tail is deterministic.
pub async fn settled_tail(child: &mut Child) -> String {
    child.drained().await;
    error_tail(&child.stderr().lines())
}

/// The characters legacy replaced with `_` in a title (`app/ytdl.py:574`).
///
/// Note the forward slash: unlike
/// [`sanitize_path_component`](aulos_core::paths::sanitize_path_component), which leaves `/` alone
/// so an output template can nest, this collapses it — the SC name is one file name, never a path.
const TITLE_INVALID: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Which binaries the engines run and how they are configured (DESIGN §10.5, §17.3).
///
/// The three program names are fields rather than literals for one reason: it is the seam the unit
/// tests inject shell-script stand-ins through, so the suite proves the argv, the progress
/// plumbing, the retry and the cleanup **without** `N_m3u8DL-RE` installed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EngineCfg {
    /// `argv[0]` of the HLS downloader.
    pub nm3u8dl: OsString,
    /// `argv[0]` of ffmpeg.
    pub ffmpeg: OsString,
    /// `argv[0]` of ffprobe.
    pub ffprobe: OsString,
    /// `SC_THREAD_COUNT` — `--thread-count`.
    pub thread_count: u32,
    /// `SC_USE_FFMPEG` — skip `N_m3u8DL-RE` entirely.
    pub use_ffmpeg: bool,
    /// `AULOS_SC_USE_OUTPUT_TEMPLATE` — opt out of the legacy `<title>.mp4` naming.
    pub use_output_template: bool,
    /// `AULOS_KILL_GRACE_MS` — the `SIGTERM` → `SIGKILL` grace on cancel.
    pub kill_grace: Duration,
}

impl Default for EngineCfg {
    fn default() -> Self {
        Self {
            nm3u8dl: OsString::from("N_m3u8DL-RE"),
            ffmpeg: OsString::from("ffmpeg"),
            ffprobe: OsString::from("ffprobe"),
            thread_count: 16,
            use_ffmpeg: false,
            use_output_template: false,
            kill_grace: aulos_provider::proc::DEFAULT_KILL_GRACE,
        }
    }
}

impl EngineCfg {
    /// The engine configuration the process was started with.
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            thread_count: cfg.sc_thread_count,
            use_ffmpeg: cfg.sc_use_ffmpeg,
            use_output_template: cfg.sc_use_output_template,
            kill_grace: Duration::from_millis(cfg.kill_grace_ms),
            ..Self::default()
        }
    }
}

/// Legacy's `safe_title`: `[<>:"/\|?*]` → `_`, then `.` and space trimmed from both ends.
///
/// DESIGN §10.5 says "trailing `. ` trimmed"; legacy's `.strip(". ")` trimmed **both** ends, and
/// this keeps the legacy behaviour because the produced path is the compatibility surface. An
/// all-punctuation title would collapse to nothing and write a hidden `.mp4`, so an empty result
/// falls back to the caller's replacement.
#[must_use]
pub fn sanitize_title(title: &str) -> String {
    let replaced: String = title
        .chars()
        .map(|c| if TITLE_INVALID.contains(&c) { '_' } else { c })
        .collect();
    replaced.trim_matches(|c| c == '.' || c == ' ').to_owned()
}

/// Where the produced files go.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutputNames {
    /// The file stem: `--save-name` for `N_m3u8DL-RE`, and the segment directory's name.
    pub stem: String,
    /// The absolute directory the `.mp4` lands in. Equal to `ctx.out_dir` unless a template nested
    /// it.
    pub save_dir: PathBuf,
    /// The absolute `.mp4` path.
    pub out_path: PathBuf,
    /// The absolute `.info.json` path.
    pub info_path: PathBuf,
    /// The segment directory `N_m3u8DL-RE` leaves behind when its own mux did not run.
    pub seg_dir: PathBuf,
    /// The produced file relative to the item's download root, for [`Outcome::filename`].
    pub rel: RelPath,
}

impl OutputNames {
    /// Plans the output paths for one item.
    ///
    /// # Errors
    /// [`ProviderError::Other`] when the resolved name escapes the output directory or is not a
    /// usable relative path.
    pub fn plan(
        cfg: &EngineCfg,
        ctx: &DownloadCtx<'_>,
        state: Option<&ScState>,
    ) -> Result<Self, ProviderError> {
        let title = display_title(ctx);
        let fallback = ctx.entry.media_id.to_string();
        let relative = if cfg.use_output_template {
            template_name(&ctx.outtmpl.default, ctx, state, &title, &fallback)
        } else {
            let mut stem = sanitize_title(&title);
            if stem.is_empty() {
                stem = sanitize_title(&fallback);
            }
            format!("{stem}.{OUTPUT_EXT}")
        };

        let out_path = aulos_core::paths::contain(&ctx.out_dir, Path::new(&relative))
            .map_err(|e| ProviderError::Other(format!("the output path is not usable: {e}")))?;
        let save_dir = out_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| ctx.out_dir.clone());
        let stem = out_path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or(&fallback)
            .to_owned();

        let rel = match &ctx.request.folder {
            Some(folder) => format!("{}/{relative}", folder.as_str()),
            None => relative.clone(),
        };
        let rel = RelPath::parse(&rel)
            .map_err(|e| ProviderError::Other(format!("the output path is not usable: {e}")))?;

        Ok(Self {
            info_path: save_dir.join(format!("{stem}.info.json")),
            seg_dir: save_dir.join(&stem),
            stem,
            save_dir,
            out_path,
            rel,
        })
    }
}

/// The title the produced file is named after — legacy's `DownloadInfo.title`, which already
/// carries `custom_name_prefix` when one was given (`app/ytdl.py:321`).
fn display_title(ctx: &DownloadCtx<'_>) -> String {
    let title = &*ctx.entry.title;
    let prefix = &*ctx.request.custom_name_prefix;
    if prefix.is_empty() {
        title.to_owned()
    } else {
        format!("{prefix}.{title}")
    }
}

/// `%(field)s` / `%(field)02d` references, the subset of the yt-dlp template grammar the opt-in
/// naming path supports.
static FIELD: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"%\((\w+)\)(0?\d*)([sd])").ok());

/// Resolves `tmpl` against the entry, one path component at a time.
///
/// This is **not** yt-dlp's template engine — this provider never runs yt-dlp, so there is nothing
/// to delegate to. It supports plain `%(field)s` and zero-padded `%(field)02d` over the fields an
/// SC entry actually has; an unknown or absent field becomes `NA`, exactly as yt-dlp renders one.
/// Each `/`-separated component is sanitised separately, so a template may nest
/// (`%(series)s/Season %(season_number)02d/…`) without a title being able to escape the directory.
fn template_name(
    tmpl: &str,
    ctx: &DownloadCtx<'_>,
    state: Option<&ScState>,
    title: &str,
    fallback: &str,
) -> String {
    let mut fields: BTreeMap<&str, Value> = BTreeMap::new();
    fields.insert("title", Value::String(title.to_owned()));
    fields.insert("id", Value::String(ctx.entry.media_id.to_string()));
    fields.insert("ext", Value::String(OUTPUT_EXT.to_owned()));
    fields.insert("url", Value::String(ctx.entry.url.to_string()));
    if let Some(s) = state {
        fields.insert("extractor", Value::String(s.extractor.clone()));
        fields.insert("extractor_key", Value::String(s.extractor_key.clone()));
        fields.insert("episode", Value::String(s.episode.clone()));
        if let Some(series) = &s.series {
            fields.insert("series", Value::String(series.clone()));
        }
        if let Some(n) = s.season_number {
            fields.insert("season_number", Value::from(n));
        }
        if let Some(n) = s.episode_number {
            fields.insert("episode_number", Value::from(n));
        }
    }

    let rendered = match FIELD.as_ref() {
        Some(re) => re
            .replace_all(tmpl, |c: &regex::Captures<'_>| {
                render_field(&fields, &c[1], &c[2], &c[3])
            })
            .into_owned(),
        None => tmpl.to_owned(),
    };

    let mut parts: Vec<String> = rendered
        .split('/')
        .map(sanitize_title)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        parts.push(sanitize_title(fallback));
    }
    let mut out = parts.join("/");
    if !out.ends_with(&format!(".{OUTPUT_EXT}")) {
        out.push('.');
        out.push_str(OUTPUT_EXT);
    }
    out
}

/// One `%(field)…` reference.
fn render_field(fields: &BTreeMap<&str, Value>, key: &str, flags: &str, kind: &str) -> String {
    let Some(v) = fields.get(key) else {
        return "NA".to_owned();
    };
    match (v, kind) {
        (Value::Number(n), "d") => {
            let width = flags.trim_start_matches('0').parse::<usize>().unwrap_or(0);
            if flags.starts_with('0') {
                format!("{:0width$}", n.as_u64().unwrap_or(0), width = width)
            } else {
                format!("{:width$}", n.as_u64().unwrap_or(0), width = width)
            }
        }
        (Value::String(s), _) => s.clone(),
        (Value::Number(n), _) => n.to_string(),
        _ => "NA".to_owned(),
    }
}

/// The temp directory the engines use: `TEMP_DIR` when set, else `<out_dir>/.tmp`
/// (legacy `app/ytdl.py:701`, DESIGN §10.5).
///
/// Created if missing, because `N_m3u8DL-RE` will not create it itself.
pub async fn temp_dir(ctx: &DownloadCtx<'_>) -> PathBuf {
    let dir = if ctx.tmp_dir.as_os_str().is_empty() {
        ctx.out_dir.join(".tmp")
    } else {
        ctx.tmp_dir.clone()
    };
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(dir = %dir.display(), error = %e, "could not create the temp directory");
    }
    dir
}

/// Writes the `.info.json` sidecar in the **legacy flat shape** (DESIGN §10.3 note 2).
///
/// A failure is a warning, not an error: legacy tolerated it, and the media file is what the user
/// asked for. Returns the JSON that was written (or would have been), which the outcome carries
/// back as `entry_final` for the NFO hook (DESIGN §13.2).
async fn write_sidecar(names: &OutputNames, ctx: &DownloadCtx<'_>, state: &ScState) -> Value {
    let json = state.to_legacy_info_json(
        &ctx.entry.media_id,
        &display_title(ctx),
        ctx.entry.url.as_str(),
    );
    // Legacy wrote `json.dump(entry, indent=2, ensure_ascii=False)`; `to_string_pretty` is the
    // same two-space indentation and serde_json never escapes non-ASCII.
    match serde_json::to_string_pretty(&json) {
        Ok(text) => {
            if let Err(e) = tokio::fs::write(&names.info_path, text).await {
                tracing::warn!(path = %names.info_path.display(), error = %e, "failed to write info.json");
            } else {
                tracing::info!(path = %names.info_path.display(), "wrote StreamingCommunity info.json");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not serialise info.json"),
    }
    json
}

/// Removes everything a failed or cancelled attempt may have left behind (DESIGN §10.5).
///
/// Every removal is best-effort: this runs on the cancel path, where the only thing worse than a
/// leftover file is a cancel that fails.
pub async fn cleanup_partial(names: &OutputNames, tmp: &Path) {
    if let Err(e) = tokio::fs::remove_file(&names.out_path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %names.out_path.display(), error = %e, "could not remove the partial output");
    }
    for dir in [
        tmp.join(&names.stem),
        tmp.join(format!("{}.tmp", names.stem)),
        names.seg_dir.clone(),
    ] {
        if dir.is_dir()
            && let Err(e) = tokio::fs::remove_dir_all(&dir).await
        {
            tracing::warn!(dir = %dir.display(), error = %e, "could not remove a temp directory");
        }
    }
}

/// Runs the download (DESIGN §10.5).
///
/// # Errors
/// [`ProviderError::Canceled`] on cancel, whatever [`jit::fresh_stream`] maps its failures to, and
/// [`ProviderError::Other`] / [`ProviderError::Postprocessing`] for an engine that failed.
pub async fn download(
    provider: &ScProvider,
    ctx: DownloadCtx<'_>,
    sink: ProgressSink,
) -> Result<Outcome, ProviderError> {
    let cfg = provider.engine();
    sink.stage(Stage::Preparing, None).await;
    if ctx.cancel.is_cancelled() {
        return Err(ProviderError::Canceled);
    }

    let state = ScState::from_json(&ctx.entry.state);
    let base = base_url(&ctx, state.as_ref())?;

    // Legacy re-extracted here on every download because vixcloud tokens expire in minutes
    // (`app/ytdl.py:558-570`).
    let target = tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => return Err(ProviderError::Canceled),
        r = jit::fresh_stream(provider.http().as_ref(), &base, &ctx.entry.url) => {
            r.map_err(crate::error::ScError::into_provider_error)?
        }
    };
    tracing::info!(item = %ctx.item_id, host = target.m3u8.host_str().unwrap_or("?"), "got a fresh m3u8 URL");

    let names = OutputNames::plan(cfg, &ctx, state.as_ref())?;
    if let Err(e) = tokio::fs::create_dir_all(&names.save_dir).await {
        return Err(ProviderError::Other(format!(
            "could not create {}: {e}",
            names.save_dir.display()
        )));
    }
    let tmp = temp_dir(&ctx).await;
    let entry_final = match &state {
        Some(s) => Some(write_sidecar(&names, &ctx, s).await),
        None => {
            tracing::warn!(
                item = %ctx.item_id,
                "the entry carries no StreamingCommunity state blob; skipping the .info.json sidecar"
            );
            None
        }
    };

    let outcome = if cfg.use_ffmpeg {
        sink.stage(Stage::Downloading, Some(MSG_START_FFMPEG.into()))
            .await;
        ffmpeg::download_ffmpeg(cfg, &ctx, &target, &names, &tmp, &sink).await
    } else {
        sink.stage(Stage::Downloading, Some(MSG_START_NM3U8.into()))
            .await;
        match nm3u8dl::download_nm3u8(cfg, &ctx, &target, &names, &tmp, &sink).await {
            Ok(o) => Ok(o),
            // A cancel is not a failure to retry: legacy's cancel killed the process and the job.
            Err(ProviderError::Canceled) => Err(ProviderError::Canceled),
            Err(e) => {
                tracing::warn!(
                    item = %ctx.item_id,
                    error = %e,
                    "N_m3u8DL-RE failed; retrying StreamingCommunity download with ffmpeg"
                );
                sink.stage(Stage::Downloading, Some(MSG_RETRY_FFMPEG.into()))
                    .await;
                cleanup_partial(&names, &tmp).await;
                ffmpeg::download_ffmpeg(cfg, &ctx, &target, &names, &tmp, &sink).await
            }
        }
    };

    match outcome {
        Ok(mut o) => {
            if let Some(entry) = entry_final {
                o = o.with_entry_final(entry);
            }
            Ok(o)
        }
        Err(e) => {
            // DESIGN §10.5: partial cleanup on cancel *and* on failure. Legacy left the truncated
            // mp4 on disk after a final failure, where it looked like a finished download to
            // anything scanning the directory.
            cleanup_partial(&names, &tmp).await;
            Err(e)
        }
    }
}

/// `{scheme}://{host}` of the site: the persisted `base_url` when the entry has one, else derived
/// from the watch URL (DESIGN §7.6.3a — an imported row may have neither).
fn base_url(ctx: &DownloadCtx<'_>, state: Option<&ScState>) -> Result<Url, ProviderError> {
    if let Some(s) = state
        && let Ok(u) = Url::parse(&s.base_url)
    {
        return Ok(u);
    }
    ScProvider::base_of(&ctx.entry.url).ok_or_else(|| {
        ProviderError::Other(format!(
            "cannot derive the site base url from {}",
            ctx.entry.url
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{EngineFixture, fixture_bin};

    #[test]
    fn the_title_sanitiser_is_legacys() {
        assert_eq!(sanitize_title("Una Serie S01E02"), "Una Serie S01E02");
        assert_eq!(
            sanitize_title(r#"a<b>c:d"e/f\g|h?i*j"#),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_title("  Trailing dots... "), "Trailing dots");
        assert_eq!(sanitize_title("...."), "");
        // Legacy kept accents, apostrophes and dashes.
        assert_eq!(sanitize_title("L'ultimo — Città"), "L'ultimo — Città");
    }

    #[tokio::test]
    async fn the_default_naming_is_title_mp4_plus_info_json() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert_eq!(names.stem, "Una Serie S01E02 - Pilota");
        assert_eq!(
            names.out_path,
            f.out_dir().join("Una Serie S01E02 - Pilota.mp4")
        );
        assert_eq!(
            names.info_path,
            f.out_dir().join("Una Serie S01E02 - Pilota.info.json")
        );
        assert_eq!(names.rel.as_str(), "Una Serie S01E02 - Pilota.mp4");
        assert_eq!(names.seg_dir, f.out_dir().join("Una Serie S01E02 - Pilota"));
    }

    #[tokio::test]
    async fn a_custom_name_prefix_is_carried_into_the_file_name_as_legacy_did() {
        let mut f = EngineFixture::new().await;
        f.request.custom_name_prefix = "01".into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert_eq!(names.stem, "01.Una Serie S01E02 - Pilota");
    }

    #[tokio::test]
    async fn a_folder_lands_in_the_relative_outcome_path() {
        let mut f = EngineFixture::new().await;
        f.with_folder("Serie").await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert_eq!(
            names.rel.as_str(),
            "Serie/Una Serie S01E02 - Pilota.mp4",
            "the wire filename is relative to the download root, not to out_dir"
        );
    }

    #[tokio::test]
    async fn a_title_that_sanitises_to_nothing_falls_back_to_the_media_id() {
        let mut f = EngineFixture::new().await;
        f.entry.title = "...".into();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert_eq!(
            names.stem, "sc_9_77",
            "a hidden file called `.mp4` is not an output path"
        );
    }

    #[tokio::test]
    async fn the_opt_in_template_path_uses_the_template() {
        let mut f = EngineFixture::new().await;
        f.cfg.use_output_template = true;
        f.outtmpl = "%(series)s/Season %(season_number)02d/%(title)s.%(ext)s".to_owned();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert_eq!(
            names.rel.as_str(),
            "Una Serie/Season 01/Una Serie S01E02 - Pilota.mp4"
        );
        assert_eq!(names.save_dir, f.out_dir().join("Una Serie/Season 01"));
        assert_eq!(names.stem, "Una Serie S01E02 - Pilota");
    }

    #[tokio::test]
    async fn the_template_renders_an_unknown_field_as_na_and_forces_the_extension() {
        let mut f = EngineFixture::new().await;
        f.cfg.use_output_template = true;
        f.outtmpl = "%(uploader)s - %(title)s".to_owned();
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert_eq!(names.rel.as_str(), "NA - Una Serie S01E02 - Pilota.mp4");
    }

    #[tokio::test]
    async fn a_template_cannot_escape_the_output_directory() {
        let mut f = EngineFixture::new().await;
        f.cfg.use_output_template = true;
        f.outtmpl = "../../etc/%(title)s.%(ext)s".to_owned();
        let ctx = f.ctx();
        // `..` is collapsed by the component sanitiser, so containment holds by construction.
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        assert!(
            names.out_path.starts_with(f.out_dir()),
            "{}",
            names.out_path.display()
        );
    }

    #[tokio::test]
    async fn the_sidecar_is_written_in_the_legacy_flat_shape() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let written = write_sidecar(&names, &ctx, &f.state).await;
        let text = std::fs::read_to_string(&names.info_path).expect("the sidecar must exist");
        let json: Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(json, written);
        assert_eq!(json["_sc_base_url"], "https://sc.test");
        assert_eq!(json["_sc_needs_m3u8_extraction"], true);
        assert_eq!(json["id"], "sc_9_77");
        assert_eq!(json["title"], "Una Serie S01E02 - Pilota");
        assert!(json.get("base_url").is_none(), "the v2 keys stay in the DB");
        assert!(text.contains("\n  \"id\""), "indent=2, like legacy");
    }

    #[tokio::test]
    async fn cleanup_removes_the_partial_mp4_the_segment_dir_and_both_temp_dirs() {
        let f = EngineFixture::new().await;
        let ctx = f.ctx();
        let names = OutputNames::plan(&f.cfg, &ctx, Some(&f.state)).expect("names");
        let tmp = temp_dir(&ctx).await;
        crate::testing::write(&names.out_path, b"partial");
        for dir in [
            tmp.join(&names.stem),
            tmp.join(format!("{}.tmp", names.stem)),
            names.seg_dir.clone(),
        ] {
            std::fs::create_dir_all(&dir).expect("mkdir");
            crate::testing::write(&dir.join("seg1.ts"), b"x");
        }

        cleanup_partial(&names, &tmp).await;

        assert!(!names.out_path.exists());
        assert!(!names.seg_dir.exists());
        assert!(!tmp.join(&names.stem).exists());
        assert!(!tmp.join(format!("{}.tmp", names.stem)).exists());
        // Idempotent: the cancel path may run it twice.
        cleanup_partial(&names, &tmp).await;
    }

    #[tokio::test]
    async fn the_base_url_comes_from_the_state_and_falls_back_to_the_watch_url() {
        let mut f = EngineFixture::new().await;
        let ctx = f.ctx();
        assert_eq!(
            base_url(&ctx, Some(&f.state)).expect("base").as_str(),
            "https://sc.test/"
        );
        drop(ctx);
        f.entry.state = Value::Null;
        let ctx = f.ctx();
        assert_eq!(
            base_url(&ctx, None).expect("base").as_str(),
            "https://sc.test/",
            "an imported row with no state blob still resolves"
        );
    }

    #[tokio::test]
    async fn the_engine_config_reads_the_legacy_env_vars() {
        use aulos_core::config::{self, RawEnv};
        let cfg = config::load(&RawEnv::from_pairs(
            [
                ("SC_THREAD_COUNT", "4"),
                ("SC_USE_FFMPEG", "true"),
                ("AULOS_SC_USE_OUTPUT_TEMPLATE", "true"),
                ("AULOS_KILL_GRACE_MS", "1500"),
            ]
            .into_iter(),
        ))
        .expect("config");
        let e = EngineCfg::from_config(&cfg);
        assert_eq!(e.thread_count, 4);
        assert!(e.use_ffmpeg);
        assert!(e.use_output_template);
        assert_eq!(e.kill_grace, Duration::from_millis(1500));
        assert_eq!(e.nm3u8dl, OsString::from("N_m3u8DL-RE"));
        assert_eq!(e.ffmpeg, OsString::from("ffmpeg"));
        assert_eq!(e.ffprobe, OsString::from("ffprobe"));
    }

    // -- the whole download, end to end, with stand-in binaries --------------------------------

    #[tokio::test]
    async fn a_successful_nm3u8_run_produces_the_outcome_and_the_sidecar() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_ok.sh").into();
        let (provider, sink, mut rx) = f.provider();
        let outcome = download(&provider, f.ctx(), sink)
            .await
            .expect("a download");

        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some("Una Serie S01E02 - Pilota.mp4")
        );
        assert_eq!(outcome.size, Some(14));
        assert_eq!(
            outcome.entry_final.as_ref().map(|v| v["id"].clone()),
            Some(Value::String("sc_9_77".to_owned()))
        );
        assert!(
            f.out_dir()
                .join("Una Serie S01E02 - Pilota.info.json")
                .is_file()
        );

        let (msgs, frames) = crate::testing::drain(&mut rx);
        assert_eq!(msgs, [MSG_START_NM3U8], "the legacy message sequence");
        assert!(
            frames.iter().any(|p| p.fragment_index == Some(325)),
            "the parsed repaint frames must reach the sink: {frames:?}"
        );
    }

    #[tokio::test]
    async fn sc_use_ffmpeg_skips_nm3u8_entirely() {
        let mut f = EngineFixture::new().await;
        f.cfg.use_ffmpeg = true;
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_dl.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_must_not_run.sh").into();
        let (provider, sink, mut rx) = f.provider();
        let outcome = download(&provider, f.ctx(), sink)
            .await
            .expect("a download");
        assert!(outcome.filename.is_some());
        assert_eq!(
            crate::testing::drain_messages(&mut rx),
            [MSG_START_FFMPEG],
            "no N_m3u8DL-RE message, and no retry message"
        );
    }

    #[tokio::test]
    async fn a_failing_nm3u8_cleans_up_and_retries_with_ffmpeg_in_the_legacy_message_order() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_fail.sh").into();
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_dl.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        let (provider, sink, mut rx) = f.provider();
        let outcome = download(&provider, f.ctx(), sink)
            .await
            .expect("the ffmpeg retry must succeed");

        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some("Una Serie S01E02 - Pilota.mp4")
        );
        assert_eq!(
            crate::testing::drain_messages(&mut rx),
            [MSG_START_NM3U8, MSG_RETRY_FFMPEG]
        );
        // The partial the failing run left behind is gone, replaced by ffmpeg's output.
        let text =
            std::fs::read(f.out_dir().join("Una Serie S01E02 - Pilota.mp4")).expect("the mp4");
        assert_eq!(text, b"fake-ffmpeg-mp4");
        assert!(
            !f.out_dir().join("Una Serie S01E02 - Pilota").exists(),
            "the segment directory the failing run left is cleaned up"
        );
    }

    #[tokio::test]
    async fn a_missing_nm3u8_binary_falls_through_to_ffmpeg_rather_than_failing_the_item() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = "aulos-no-such-binary".into();
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_dl.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        let (provider, sink, mut rx) = f.provider();
        download(&provider, f.ctx(), sink)
            .await
            .expect("ffmpeg must take over");
        assert_eq!(
            crate::testing::drain_messages(&mut rx),
            [MSG_START_NM3U8, MSG_RETRY_FFMPEG]
        );
    }

    #[tokio::test]
    async fn both_engines_failing_reports_the_error_and_leaves_nothing_behind() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_fail.sh").into();
        f.cfg.ffmpeg = fixture_bin("fake_ffmpeg_fail.sh").into();
        f.cfg.ffprobe = fixture_bin("fake_ffprobe.sh").into();
        let (provider, sink, _rx) = f.provider();
        let err = download(&provider, f.ctx(), sink)
            .await
            .expect_err("both engines failed");
        assert!(
            err.to_string().starts_with("FFmpeg failed with code"),
            "{err}"
        );
        assert!(!f.out_dir().join("Una Serie S01E02 - Pilota.mp4").exists());
    }

    #[tokio::test]
    async fn a_cancel_mid_download_removes_the_partial_mp4_the_segments_and_the_temp_dirs() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_hang.sh").into();
        f.cfg.kill_grace = Duration::from_millis(200);
        let (provider, sink, _rx) = f.provider();
        let cancel = f.cancel.clone();
        let out_dir = f.out_dir();
        let stem = "Una Serie S01E02 - Pilota";

        let waiter = tokio::spawn(async move {
            // The stand-in creates the partial output and the segment directory, then hangs.
            for _ in 0..200 {
                if out_dir.join(stem).join("seg1.ts").exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel();
        });

        let err = download(&provider, f.ctx(), sink)
            .await
            .expect_err("a cancelled download never succeeds");
        waiter.await.expect("the canceller");
        assert!(matches!(err, ProviderError::Canceled), "{err:?}");
        assert!(!f.out_dir().join(format!("{stem}.mp4")).exists());
        assert!(!f.out_dir().join(stem).exists());
        assert!(!f.tmp_dir().join(stem).exists());
        assert!(!f.tmp_dir().join(format!("{stem}.tmp")).exists());
    }

    #[tokio::test]
    async fn the_provider_trait_delegates_to_the_engines() {
        // `ScProvider::download` is a one-line delegation to this module; this is what proves the
        // whole public path — trait method, engine selection, stand-in binary, outcome — is wired.
        use aulos_provider::provider::Provider as _;
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_ok.sh").into();
        let (provider, sink, _rx) = f.provider();
        let outcome = provider
            .download(f.ctx(), sink)
            .await
            .expect("a download through the trait");
        assert_eq!(
            outcome.filename.as_ref().map(RelPath::as_str),
            Some("Una Serie S01E02 - Pilota.mp4")
        );
    }

    #[tokio::test]
    async fn an_already_cancelled_download_spawns_nothing() {
        let mut f = EngineFixture::new().await;
        f.cfg.nm3u8dl = fixture_bin("fake_nm3u8dl_must_not_run.sh").into();
        f.cancel.cancel();
        let (provider, sink, _rx) = f.provider();
        let err = download(&provider, f.ctx(), sink)
            .await
            .expect_err("cancelled");
        assert!(matches!(err, ProviderError::Canceled));
    }
}
