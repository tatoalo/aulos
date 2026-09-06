//! `YTDL_OPTIONS` loading and layering (DESIGN §17.2).
//!
//! The option dicts are yt-dlp **Python API** option dictionaries, not CLI flags, which is why
//! they survive verbatim into the shim's job (DESIGN §9.2) and why `serde_json::Map` is the right
//! representation all the way through.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::error::ErrorCode;

/// The layered yt-dlp option state. Held in an `Arc<ArcSwap<_>>`; a job snapshots it at spawn, so
/// a reload never mutates an in-flight job's options.
#[derive(Clone, Debug)]
pub struct YtdlOptions {
    /// `YTDL_OPTIONS` from the environment, with `YTDL_OPTIONS_FILE` merged **over** it.
    pub base: Map<String, Value>,
    /// `YTDL_OPTIONS_PRESETS`, with `YTDL_OPTIONS_PRESETS_FILE` merged over it.
    pub presets: BTreeMap<String, Map<String, Value>>,
    /// Runtime overrides — today only `cookiefile`. Re-applied after every reload, as in legacy.
    pub overrides: Map<String, Value>,
    /// The options file's mtime as fractional epoch seconds, or `None`.
    ///
    /// This is the exact legacy payload of the `ytdl_options` frame's `update_time`.
    pub file_mtime: Option<f64>,
    /// When this snapshot was built.
    pub loaded_at: Instant,
}

impl YtdlOptions {
    /// Empty options — the `YTDL_OPTIONS={}` default.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            base: Map::new(),
            presets: BTreeMap::new(),
            overrides: Map::new(),
            file_mtime: None,
            loaded_at: Instant::now(),
        }
    }

    /// Loads and merges everything, preserving the legacy error strings exactly.
    ///
    /// `env_options` / `env_presets` are the raw environment values; the two paths are the already
    /// absolutised `YTDL_OPTIONS_FILE` / `YTDL_OPTIONS_PRESETS_FILE`, or `None` when unset.
    ///
    /// # Errors
    /// The first failure, as a [`YtdlOptionsError`] whose `Display` is the legacy message. Loading
    /// stops there because legacy exited on it — and because a half-merged option dict is worse
    /// than none.
    pub fn load(
        env_options: &str,
        options_file: Option<&Path>,
        env_presets: &str,
        presets_file: Option<&Path>,
    ) -> Result<Self, YtdlOptionsError> {
        let base = load_options(env_options, options_file)?;
        let presets = load_presets(env_presets, presets_file)?;
        let file_mtime = options_file.and_then(mtime_epoch_secs);
        Ok(Self {
            base,
            presets,
            overrides: Map::new(),
            file_mtime,
            loaded_at: Instant::now(),
        })
    }

    /// Merges `base` → runtime `overrides` → the named `presets` in request order → per-request
    /// `overrides`.
    ///
    /// `null` values are **kept** rather than removed, so a preset can clear a global
    /// `download_archive` by setting it to `null`. An unknown preset name is skipped: the request
    /// validator rejects it up front with `unknown_preset`, so reaching here means the caller has
    /// already decided to proceed.
    #[must_use]
    pub fn layer(
        &self,
        presets: &[Box<str>],
        overrides: &Map<String, Value>,
    ) -> Map<String, Value> {
        let mut out = self.base.clone();
        merge_over(&mut out, &self.overrides);
        for name in presets {
            if let Some(preset) = self.presets.get(&**name) {
                merge_over(&mut out, preset);
            } else {
                tracing::warn!(preset = %name, "unknown yt-dlp option preset; skipped");
            }
        }
        merge_over(&mut out, overrides);
        out
    }

    /// Registers a runtime override, e.g. `cookiefile` after a cookie upload.
    pub fn set_runtime_override(&mut self, key: impl Into<String>, value: Value) {
        self.overrides.insert(key.into(), value);
    }

    /// Removes a runtime override.
    pub fn remove_runtime_override(&mut self, key: &str) {
        self.overrides.remove(key);
    }

    /// Copies the runtime overrides from a previous snapshot onto this one.
    ///
    /// Called after every successful reload, matching legacy's `_apply_runtime_overrides`.
    pub fn inherit_overrides(&mut self, previous: &Self) {
        for (k, v) in &previous.overrides {
            self.overrides.insert(k.clone(), v.clone());
        }
    }
}

impl Default for YtdlOptions {
    fn default() -> Self {
        Self::empty()
    }
}

/// Shallow merge: `src` wins, and a `null` value is stored rather than deleting the key.
fn merge_over(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        dst.insert(k.clone(), v.clone());
    }
}

/// `YTDL_OPTIONS` + `YTDL_OPTIONS_FILE`, merged the legacy way.
///
/// # Errors
/// [`YtdlOptionsError::EnvInvalid`], [`YtdlOptionsError::FileNotFound`] or
/// [`YtdlOptionsError::FileInvalid`].
pub fn load_options(
    env_value: &str,
    file: Option<&Path>,
) -> Result<Map<String, Value>, YtdlOptionsError> {
    let mut base = parse_object(env_value).ok_or(YtdlOptionsError::EnvInvalid {
        var: "YTDL_OPTIONS",
    })?;

    let Some(path) = file else {
        warn_about_picker_overrides(&base);
        return Ok(base);
    };
    let text = read_existing(path, "YTDL_OPTIONS_FILE")?;
    let from_file = parse_object(&text).ok_or(YtdlOptionsError::FileInvalid {
        var: "YTDL_OPTIONS_FILE",
    })?;
    merge_over(&mut base, &from_file);
    warn_about_picker_overrides(&base);
    Ok(base)
}

/// The operator-layer keys that quietly outrank the per-download format picker, and what to say
/// about each (DESIGN §17.2).
///
/// - `format` is merged **over** the base dict's computed selector for every job (DESIGN §9.2),
///   so it replaces whatever `formats::get_format` produced. The one exception is
///   `{video, mp4, best_remux}`, where `opts::get_opts` pops it back out.
/// - `merge_output_format` is the mirror image: it is one of the type-derived keys `get_opts`
///   writes **on top** of the operator layer, so for `{video, mp4}` the operator's value is the
///   one that loses (DESIGN §9.8 Δ C47).
///
/// Array order is report order, so the log is stable across runs.
const PICKER_OVERRIDES: [(&str, &str); 2] = [
    (
        "format",
        "it replaces the per-download format selector for every job, so the format and quality \
         chosen in the UI, the bot and the API are ignored — except video/mp4/best_remux, which \
         pops the key. Remove it unless you mean to pin one selector globally",
    ),
    (
        "merge_output_format",
        "it names the output container for every job, except video/mp4 downloads, where Aulos \
         sets \"mp4\" over it (DESIGN §9.8)",
    ),
];

/// One human-readable warning per [`PICKER_OVERRIDES`] key present in the merged operator layer.
///
/// A pure function rather than a `warn!` at the call site because the wording is the part worth
/// testing and this crate has no tracing capture: the caller logs, the test asserts the text.
#[must_use]
pub fn override_warnings(base: &Map<String, Value>) -> Vec<String> {
    PICKER_OVERRIDES
        .iter()
        .filter(|(key, _)| base.contains_key(*key))
        .map(|(key, why)| format!("YTDL_OPTIONS/YTDL_OPTIONS_FILE sets `{key}`: {why}."))
        .collect()
}

/// Logs [`override_warnings`] once per load — at boot and on every reload, never per job.
fn warn_about_picker_overrides(base: &Map<String, Value>) {
    for message in override_warnings(base) {
        tracing::warn!("{message}");
    }
}

/// `YTDL_OPTIONS_PRESETS` + `YTDL_OPTIONS_PRESETS_FILE`. Every value must itself be an object.
///
/// # Errors
/// [`YtdlOptionsError::EnvInvalid`], [`YtdlOptionsError::FileNotFound`] or
/// [`YtdlOptionsError::FileInvalid`].
pub fn load_presets(
    env_value: &str,
    file: Option<&Path>,
) -> Result<BTreeMap<String, Map<String, Value>>, YtdlOptionsError> {
    let mut presets = parse_preset_map(env_value).ok_or(YtdlOptionsError::EnvInvalid {
        var: "YTDL_OPTIONS_PRESETS",
    })?;

    let Some(path) = file else { return Ok(presets) };
    let text = read_existing(path, "YTDL_OPTIONS_PRESETS_FILE")?;
    let from_file = parse_preset_map(&text).ok_or(YtdlOptionsError::FileInvalid {
        var: "YTDL_OPTIONS_PRESETS_FILE",
    })?;
    presets.extend(from_file);
    Ok(presets)
}

fn read_existing(path: &Path, var: &'static str) -> Result<String, YtdlOptionsError> {
    if !path.exists() {
        return Err(YtdlOptionsError::FileNotFound {
            path: path.display().to_string().into_boxed_str(),
        });
    }
    fs::read_to_string(path).map_err(|_| YtdlOptionsError::FileInvalid { var })
}

/// Parses a JSON object, or `None` for anything else — legacy's
/// `json.loads(...); assert isinstance(..., dict)`.
fn parse_object(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Parses a `dict[str, dict]`, or `None` — legacy's second `assert all(...)`.
fn parse_preset_map(text: &str) -> Option<BTreeMap<String, Map<String, Value>>> {
    let obj = parse_object(text)?;
    let mut out = BTreeMap::new();
    for (name, value) in obj {
        match value {
            Value::Object(m) => {
                out.insert(name, m);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// A file's mtime as fractional epoch seconds, matching legacy's `os.path.getmtime`.
#[must_use]
pub fn mtime_epoch_secs(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs_f64()),
        Err(_) => {
            // A pre-1970 mtime. Report it honestly rather than clamping to 0.
            let d = SystemTime::now().duration_since(modified).ok()?;
            Some(-d.as_secs_f64())
        }
    }
}

/// Option-loading failures. Every `Display` is a legacy message, byte-identical (DESIGN §17.2).
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum YtdlOptionsError {
    /// `Environment variable YTDL_OPTIONS is invalid` (or the presets analogue).
    #[error("Environment variable {var} is invalid")]
    EnvInvalid {
        /// `YTDL_OPTIONS` or `YTDL_OPTIONS_PRESETS`.
        var: &'static str,
    },
    /// `File "<path>" not found`
    #[error("File \"{path}\" not found")]
    FileNotFound {
        /// The configured path.
        path: Box<str>,
    },
    /// `YTDL_OPTIONS_FILE contents is invalid` (or the presets analogue).
    #[error("{var} contents is invalid")]
    FileInvalid {
        /// `YTDL_OPTIONS_FILE` or `YTDL_OPTIONS_PRESETS_FILE`.
        var: &'static str,
    },
}

impl YtdlOptionsError {
    /// The wire error code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        ErrorCode::ValidationFailed
    }

    /// A reload can be retried once the operator fixes the file, but not automatically.
    #[must_use]
    pub const fn retryable(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn opts() -> YtdlOptions {
        let mut o = YtdlOptions::empty();
        o.base = json!({ "format": "bv+ba", "download_archive": "/a.txt" })
            .as_object()
            .unwrap()
            .clone();
        o.presets.insert(
            "sponsorblock".to_owned(),
            json!({ "sponsorblock_remove": ["sponsor"] })
                .as_object()
                .unwrap()
                .clone(),
        );
        o.presets.insert(
            "no_archive".to_owned(),
            json!({ "download_archive": null })
                .as_object()
                .unwrap()
                .clone(),
        );
        o
    }

    #[test]
    fn env_then_file_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        fs::write(&file, r#"{"format":"from-file","extra":1}"#).unwrap();
        let merged = load_options(r#"{"format":"from-env","only_env":true}"#, Some(&file)).unwrap();
        assert_eq!(
            merged["format"], "from-file",
            "the file merges OVER the env"
        );
        assert_eq!(merged["only_env"], json!(true));
        assert_eq!(merged["extra"], json!(1));
    }

    #[test]
    fn an_operator_format_key_is_reported_as_overriding_the_picker() {
        // The production incident this exists for: an avc1-only `format` in
        // `YTDL_OPTIONS_FILE` turned every add into 1080p H.264 while the UI still offered
        // "best", with nothing in the log to say so (DESIGN §17.2).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("opts.json");
        fs::write(&file, r#"{"format":"bv[vcodec^=avc1]+ba"}"#).unwrap();
        let merged = load_options("{}", Some(&file)).unwrap();

        let warnings = override_warnings(&merged);
        assert_eq!(warnings.len(), 1, "got {warnings:?}");
        assert!(warnings[0].contains("`format`"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("video/mp4/best_remux"),
            "the one selection that survives must be named: {}",
            warnings[0]
        );
    }

    #[test]
    fn both_picker_override_keys_are_reported_and_nothing_else_is() {
        let both = json!({ "merge_output_format": "mkv", "format": "worst", "quiet": true })
            .as_object()
            .unwrap()
            .clone();
        let warnings = override_warnings(&both);
        assert_eq!(warnings.len(), 2, "got {warnings:?}");
        assert!(
            warnings[0].contains("`format`"),
            "`format` is reported first"
        );
        assert!(warnings[1].contains("`merge_output_format`"));

        // A value of `null` still counts: the key is present, and `layer` keeps nulls.
        let nulled = json!({ "format": null }).as_object().unwrap().clone();
        assert_eq!(override_warnings(&nulled).len(), 1);

        // Everything else is silent — this must not become a general linter for yt-dlp options.
        let innocent = json!({ "quiet": true, "postprocessors": [] })
            .as_object()
            .unwrap()
            .clone();
        assert!(override_warnings(&innocent).is_empty());
    }

    #[test]
    fn the_legacy_error_strings_are_byte_identical() {
        assert_eq!(
            load_options("not json", None).unwrap_err().to_string(),
            "Environment variable YTDL_OPTIONS is invalid"
        );
        assert_eq!(
            load_options("[1,2]", None).unwrap_err().to_string(),
            "Environment variable YTDL_OPTIONS is invalid",
            "a JSON array is not a dict"
        );
        assert_eq!(
            load_options("{}", Some(Path::new("/nope/x.json")))
                .unwrap_err()
                .to_string(),
            "File \"/nope/x.json\" not found"
        );
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.json");
        fs::write(&bad, "[]").unwrap();
        assert_eq!(
            load_options("{}", Some(&bad)).unwrap_err().to_string(),
            "YTDL_OPTIONS_FILE contents is invalid"
        );
        assert_eq!(
            load_presets("{\"a\": 1}", None).unwrap_err().to_string(),
            "Environment variable YTDL_OPTIONS_PRESETS is invalid",
            "presets must be dict[str, dict]"
        );
        fs::write(&bad, "{\"a\": 1}").unwrap();
        assert_eq!(
            load_presets("{}", Some(&bad)).unwrap_err().to_string(),
            "YTDL_OPTIONS_PRESETS_FILE contents is invalid"
        );
        assert_eq!(
            load_presets("{}", Some(Path::new("/nope/p.json")))
                .unwrap_err()
                .to_string(),
            "File \"/nope/p.json\" not found"
        );
    }

    #[tokio::test]
    async fn layer_applies_presets_in_request_order_then_overrides() {
        let o = opts();
        let mut request = Map::new();
        request.insert("format".to_owned(), json!("from-request"));
        let merged = o.layer(&["sponsorblock".into()], &request);
        assert_eq!(merged["format"], "from-request");
        assert_eq!(merged["sponsorblock_remove"], json!(["sponsor"]));

        // Later presets win over earlier ones.
        let mut two = opts();
        two.presets
            .insert("a".to_owned(), json!({"k": 1}).as_object().unwrap().clone());
        two.presets
            .insert("b".to_owned(), json!({"k": 2}).as_object().unwrap().clone());
        assert_eq!(two.layer(&["a".into(), "b".into()], &Map::new())["k"], 2);
        assert_eq!(two.layer(&["b".into(), "a".into()], &Map::new())["k"], 1);
    }

    #[tokio::test]
    async fn a_null_preset_value_is_kept_as_a_key_clearing_value() {
        let merged = opts().layer(&["no_archive".into()], &Map::new());
        assert!(
            merged.contains_key("download_archive"),
            "the key must survive"
        );
        assert_eq!(merged["download_archive"], Value::Null);
    }

    #[tokio::test]
    async fn runtime_overrides_sit_above_base_and_below_presets() {
        let mut o = opts();
        o.set_runtime_override("cookiefile", json!("/state/cookies.txt"));
        let merged = o.layer(&[], &Map::new());
        assert_eq!(merged["cookiefile"], "/state/cookies.txt");

        o.presets.insert(
            "nocookies".to_owned(),
            json!({"cookiefile": null}).as_object().unwrap().clone(),
        );
        let merged = o.layer(&["nocookies".into()], &Map::new());
        assert_eq!(merged["cookiefile"], Value::Null);

        o.remove_runtime_override("cookiefile");
        assert!(!o.layer(&[], &Map::new()).contains_key("cookiefile"));
    }

    #[tokio::test]
    async fn overrides_are_inherited_across_a_reload() {
        let mut before = YtdlOptions::empty();
        before.set_runtime_override("cookiefile", json!("/x"));
        let mut after = YtdlOptions::empty();
        after.inherit_overrides(&before);
        assert_eq!(after.overrides["cookiefile"], "/x");
    }

    #[tokio::test]
    async fn an_unknown_preset_is_skipped_rather_than_panicking() {
        let merged = opts().layer(&["nope".into()], &Map::new());
        assert_eq!(merged["format"], "bv+ba");
    }

    #[test]
    fn file_mtime_is_fractional_epoch_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("o.json");
        fs::write(&file, "{}").unwrap();
        let m = mtime_epoch_secs(&file).unwrap();
        assert!(m > 1_577_836_800.0, "got {m}");
        assert_eq!(mtime_epoch_secs(Path::new("/nope/x")), None);
    }
}
