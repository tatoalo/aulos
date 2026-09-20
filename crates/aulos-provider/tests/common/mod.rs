//! Shared scaffolding for the `command` plugin tests (WP-10).
//!
//! Everything here exists so a test reads as "this manifest, that script, this expectation" with
//! no ceremony: [`Plugin`] writes a plugin directory into a `tempfile::TempDir`, and
//! [`resolve_ctx`] / [`download_ctx`] build the two borrowed contexts DESIGN §6.1 defines.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aulos_core::id::ItemId;
use aulos_core::paths::Paths;
use aulos_core::request::DownloadRequest;
use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
use aulos_core::ytdl_options::YtdlOptions;
use aulos_provider::command::{PluginEnv, PluginManifest, load_manifest};
use aulos_provider::provider::{DownloadCtx, OutTmpl, ResolveCtx};
use aulos_provider::{CommandProvider, MediaEntry};
use tokio_util::sync::CancellationToken;
use url::Url;

/// A plugin directory in a temporary tree.
pub struct Plugin {
    root: tempfile::TempDir,
    name: String,
}

impl Plugin {
    /// Creates `<tmp>/<name>/plugin.toml` with `manifest` as its contents.
    pub fn new(name: &str, manifest: &str) -> Self {
        let root = tempfile::tempdir().expect("a temp dir");
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).expect("the plugin dir");
        std::fs::write(dir.join("plugin.toml"), manifest).expect("the manifest");
        Self {
            root,
            name: name.to_owned(),
        }
    }

    /// The directory the scanner walks (the parent of the plugin directory).
    pub fn plugins_dir(&self) -> &Path {
        self.root.path()
    }

    /// The plugin's own directory.
    pub fn dir(&self) -> PathBuf {
        self.root.path().join(&self.name)
    }

    /// Adds an executable script.
    pub fn script(self, name: &str, body: &str) -> Self {
        let path = self.dir().join(name);
        std::fs::write(&path, body).expect("the script");
        chmod(&path, 0o755);
        self
    }

    /// Adds a non-executable file.
    pub fn file(self, name: &str, body: &str) -> Self {
        std::fs::write(self.dir().join(name), body).expect("the file");
        self
    }

    /// Changes the plugin directory's mode, for the world-writable refusal test.
    pub fn chmod_dir(self, mode: u32) -> Self {
        chmod(&self.dir(), mode);
        self
    }

    /// Changes a file's mode, for the setuid refusal test.
    pub fn chmod_file(self, name: &str, mode: u32) -> Self {
        chmod(&self.dir().join(name), mode);
        self
    }

    /// Loads and validates the manifest.
    pub fn load(&self) -> Result<PluginManifest, aulos_provider::ManifestError> {
        load_manifest(&self.dir())
    }

    /// The reason `healthz` would show for a rejected manifest.
    pub fn reason(&self) -> String {
        match self.load() {
            Ok(_) => panic!("the manifest was expected to be rejected, but it loaded"),
            Err(e) => e.reason().to_string(),
        }
    }

    /// A ready [`CommandProvider`] for a manifest that must load.
    pub fn provider(&self) -> CommandProvider {
        let manifest = self
            .load()
            .unwrap_or_else(|e| panic!("the manifest must load, but: {}", e.reason()));
        CommandProvider::new(Arc::new(manifest), PluginEnv::default())
    }
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A copy of the shipped `plugins/examples/<name>` directory, so a test never mutates the repo.
pub fn shipped_example(name: &str) -> Plugin {
    let src = repo_root().join("plugins/examples").join(name);
    let manifest = std::fs::read_to_string(src.join("plugin.toml"))
        .unwrap_or_else(|e| panic!("plugins/examples/{name}/plugin.toml: {e}"));
    let mut plugin = Plugin::new(name, &manifest);
    for entry in std::fs::read_dir(&src).expect("the example dir").flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name == "plugin.toml" || !entry.path().is_file() {
            continue;
        }
        let body = std::fs::read_to_string(entry.path()).expect("an example file");
        plugin = plugin.script(&file_name, &body);
    }
    plugin
}

/// The workspace root, from this crate's manifest directory.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate> has two parents")
        .to_path_buf()
}

/// Whether `python3` is on `PATH`. The example-plugin tests are skipped without it.
pub fn has_python3() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("python3").is_file()))
}

/// A request with the given selection and every other field at its default.
pub fn request(url: &Url, dt: DownloadType, format: &str, quality: &str) -> DownloadRequest {
    DownloadRequest::new(
        url.clone(),
        Selection::new(
            dt,
            Codec::Auto,
            FormatId::parse(format).expect("a format id"),
            QualityId::parse(quality).expect("a quality id"),
        ),
    )
}

/// The four filesystem roots, all inside `base`.
pub fn paths(base: &Path) -> Paths {
    for sub in ["downloads", "audio", "tmp", "state"] {
        std::fs::create_dir_all(base.join(sub)).expect("a root");
    }
    Paths {
        download: base.join("downloads"),
        audio_download: base.join("audio"),
        temp: base.join("tmp"),
        state: base.join("state"),
    }
}

/// A [`ResolveCtx`] with a generous deadline.
pub fn resolve_ctx<'a>(
    request: &'a DownloadRequest,
    paths: &'a Paths,
    cancel: CancellationToken,
) -> ResolveCtx<'a> {
    ResolveCtx {
        item_id: ItemId::new(),
        request,
        ytdl_options: Arc::new(YtdlOptions::empty()),
        paths,
        flat: false,
        playlist_end: None,
        cancel,
        deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(300),
    }
}

/// A [`DownloadCtx`] writing into `out_dir`.
pub fn download_ctx<'a>(
    entry: &'a MediaEntry,
    request: &'a DownloadRequest,
    out_dir: PathBuf,
    tmp_dir: PathBuf,
    cancel: CancellationToken,
) -> DownloadCtx<'a> {
    std::fs::create_dir_all(&out_dir).expect("out_dir");
    std::fs::create_dir_all(&tmp_dir).expect("tmp_dir");
    DownloadCtx {
        item_id: ItemId::new(),
        source: aulos_core::SourceKind::ApiV2,
        entry,
        request,
        ytdl_options: Arc::new(YtdlOptions::empty()),
        out_dir,
        tmp_dir,
        outtmpl: OutTmpl::default(),
        cancel,
    }
}

/// A single-video entry with no state.
pub fn entry(url: &Url, title: &str) -> MediaEntry {
    MediaEntry::video("m1", title, url.clone())
}
