//! Output-template pre-resolution: the playlist/channel template swap and the `mode = "outtmpl"`
//! shim job (DESIGN §9.2, §9.8; legacy `app/ytdl.py::_resolve_outtmpl_fields`).
//!
//! # Why a shim round trip at all
//!
//! Legacy resolved `%(playlist_title)s` and friends **before** the download by handing the single
//! field reference to yt-dlp's own `YoutubeDL.evaluate_outtmpl`. That is what makes the full
//! template syntax work — defaults (`%(playlist_title|Unknown)s`), math
//! (`%(playlist_index+100)03d`), conditionals (`%(playlist_index&{} - |)s`), date formatting. A
//! reimplementation of that grammar in Rust would be a permanent source of drift against the
//! nightly yt-dlp pin, so this module does exactly what legacy did: it finds the field references
//! whose root name starts with `playlist` or `channel`, sends **only those substrings** plus the
//! info dict to the shim, and splices the evaluated strings back in. Every other reference is left
//! untouched for yt-dlp to resolve during the real download.
//!
//! # Why it usually costs nothing
//!
//! A single video has no `playlist_index` and no `channel_index`, so [`build_outtmpl`] returns a
//! job that is already [`OutTmplJob::is_ready`] and no Python is spawned. The same is true for a
//! playlist child whose effective template happens to contain no `playlist*` reference.
//!
//! # The legacy algorithm, verbatim
//!
//! ```text
//! output = OUTPUT_TEMPLATE  (or  "<custom_name_prefix>.<OUTPUT_TEMPLATE>")
//! if playlist_index is not None:
//!     if OUTPUT_TEMPLATE_PLAYLIST: output = OUTPUT_TEMPLATE_PLAYLIST
//!     output = resolve(output, sanitize(info), prefixes=("playlist",))
//! if channel_index is not None:
//!     if OUTPUT_TEMPLATE_CHANNEL:  output = OUTPUT_TEMPLATE_CHANNEL
//!     output = resolve(output, sanitize(info), prefixes=("channel",))
//! ```
//!
//! Note what the second `if` does when it fires: it *discards* the playlist-resolved string by
//! reassigning `output`. So when both indices are present and `OUTPUT_TEMPLATE_CHANNEL` is
//! non-empty, only `channel*` references are pre-resolved; when it is empty, both prefixes apply
//! to the playlist template. [`build_outtmpl`] reproduces that, with one documented
//! simplification: legacy ran the two passes sequentially over the *substituted* string, so a
//! `playlist_title` whose value itself contained `%(channel)s` would have been evaluated a second
//! time. Here both prefixes are resolved in a single pass over the template, so a substituted
//! value is never re-scanned. That is a fix, not a regression — a title is data, not a template.
//!
//! # Sanitisation
//!
//! Legacy applied `_sanitize_path_component` to every **string** value of the info dict before
//! evaluation, so that a playlist title containing `:` or `?` could not produce a path that fails
//! on an NTFS mount. Numbers pass through untouched, which is what keeps `%(playlist_index)02d`
//! working. [`aulos_core::sanitize_path_component`] is that function.

use aulos_core::config::Config;
use aulos_core::paths::sanitize_path_component;
use aulos_core::request::DownloadRequest;
use aulos_provider::entry::{EntryHints, MediaEntry};
use serde_json::{Map, Value, json};

pub use aulos_provider::provider::OutTmpl;

/// The shim protocol version this job speaks (DESIGN §9.2).
pub const PROTOCOL: u32 = 1;

/// The template-field prefix that `playlist_index` activates.
const PLAYLIST_PREFIX: &str = "playlist";
/// The template-field prefix that `channel_index` activates.
const CHANNEL_PREFIX: &str = "channel";

/// A failure applying evaluated templates back to a job.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum OutTmplError {
    /// The shim returned a different number of strings than were asked for. A contract violation
    /// (DESIGN §9.3), not a user error.
    #[error("outtmpl job asked for {expected} template(s) but the shim returned {got}")]
    Arity {
        /// How many field references were sent.
        expected: usize,
        /// How many strings came back.
        got: usize,
    },
}

/// One field reference to pre-resolve: where it sits in the template, and its text.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Splice {
    /// Byte offset of the reference (including any leading `%%` escape run) in the template.
    start: usize,
    /// Byte offset one past the reference.
    end: usize,
    /// The reference text, exactly as it must be handed to `evaluate_outtmpl`.
    spec: String,
}

/// A pre-resolution unit: the templates a download will use, plus whatever still needs yt-dlp.
///
/// Build one with [`build_outtmpl`]. If [`OutTmplJob::is_ready`] there is nothing to do and
/// [`OutTmplJob::ready`] hands back the final [`OutTmpl`]; otherwise send
/// [`OutTmplJob::to_job`] to the shim and feed the strings it returns to
/// [`OutTmplJob::apply`].
#[derive(Clone, PartialEq, Debug)]
pub struct OutTmplJob {
    /// The effective `outtmpl.default`, still carrying the unresolved references.
    default: String,
    /// The effective `outtmpl.chapter`. Never pre-resolved: legacy did not touch it.
    chapter: String,
    /// The references to evaluate, in template order.
    splices: Vec<Splice>,
    /// The info dict, string values already sanitised.
    info: Map<String, Value>,
    /// The prefixes whose references are being resolved, in legacy pass order.
    prefixes: Vec<&'static str>,
}

impl OutTmplJob {
    /// Whether the templates are final and no shim call is needed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.splices.is_empty()
    }

    /// The final templates, when [`OutTmplJob::is_ready`].
    #[must_use]
    pub fn ready(&self) -> Option<OutTmpl> {
        self.is_ready().then(|| OutTmpl {
            default: self.default.clone(),
            chapter: self.chapter.clone(),
        })
    }

    /// The field references to evaluate, in the order [`OutTmplJob::apply`] expects them back.
    #[must_use]
    pub fn templates(&self) -> Vec<&str> {
        self.splices.iter().map(|s| s.spec.as_str()).collect()
    }

    /// The info dict the references are evaluated against.
    #[must_use]
    pub const fn info(&self) -> &Map<String, Value> {
        &self.info
    }

    /// The active field prefixes.
    #[must_use]
    pub fn prefixes(&self) -> &[&'static str] {
        &self.prefixes
    }

    /// Adds fields to the info dict, sanitising string values the way legacy did.
    ///
    /// [`build_outtmpl`] can only fill in what [`EntryHints`] carries. The engine holds the
    /// compacted entry blob of DESIGN §7.5 — every key matching `^(playlist|channel)` plus
    /// `n_entries` and `__last_playlist_index` — and this is how it contributes them, so a
    /// template using `%(playlist_id)s` or `%(playlist_uploader)s` resolves as it did in legacy
    /// instead of yielding `NA`.
    ///
    /// Existing keys are overwritten. Call it **before** looking at
    /// [`OutTmplJob::is_ready`]-adjacent state; it never changes which references were found,
    /// only what they evaluate to.
    pub fn merge_info(&mut self, extra: &Map<String, Value>) {
        for (k, v) in extra {
            self.info.insert(k.clone(), sanitize_value(v));
        }
    }

    /// The DESIGN §9.2 `mode = "outtmpl"` job object.
    #[must_use]
    pub fn to_job(&self, job_id: &str) -> Value {
        json!({
            "v": 1,
            "protocol": PROTOCOL,
            "job_id": job_id,
            "mode": "outtmpl",
            "templates": self.templates(),
            "info": Value::Object(self.info.clone()),
            "prefixes": self.prefixes,
        })
    }

    /// Splices the shim's evaluated strings back into the template.
    ///
    /// # Errors
    /// [`OutTmplError::Arity`] when `evaluated.len() != self.templates().len()`.
    pub fn apply(&self, evaluated: &[String]) -> Result<OutTmpl, OutTmplError> {
        if evaluated.len() != self.splices.len() {
            return Err(OutTmplError::Arity {
                expected: self.splices.len(),
                got: evaluated.len(),
            });
        }
        let mut default = self.default.clone();
        // Back to front, so an earlier splice's offsets stay valid.
        for (splice, value) in self.splices.iter().zip(evaluated).rev() {
            default.replace_range(splice.start..splice.end, value);
        }
        Ok(OutTmpl {
            default,
            chapter: self.chapter.clone(),
        })
    }
}

/// Builds the output templates for one item, pre-resolving playlist/channel fields
/// (DESIGN §9.8).
///
/// `hints` decides which of the two swaps applies: `playlist_index` activates
/// `OUTPUT_TEMPLATE_PLAYLIST`, `channel_index` activates `OUTPUT_TEMPLATE_CHANNEL`, and an empty
/// value for either keeps whatever template was already selected. `custom_name_prefix` is
/// prepended to `OUTPUT_TEMPLATE` only — a playlist or channel template replaces the whole string,
/// prefix included, exactly as legacy did.
#[must_use]
pub fn build_outtmpl(cfg: &Config, req: &DownloadRequest, hints: &EntryHints) -> OutTmplJob {
    let mut default = if req.custom_name_prefix.is_empty() {
        cfg.output_template.to_string()
    } else {
        format!("{}.{}", req.custom_name_prefix, cfg.output_template)
    };

    // Legacy set `outtmpl.chapter` from `OUTPUT_TEMPLATE_CHAPTER` and overrode it with the
    // request's own template only when `split_by_chapters` was set. A `chapter_template` sent
    // without that flag was silently ignored, and still is.
    let chapter = if req.split_by_chapters && !req.chapter_template.is_empty() {
        req.chapter_template.to_string()
    } else {
        cfg.output_template_chapter.to_string()
    };

    let mut prefixes: Vec<&'static str> = Vec::new();
    if hints.playlist_index.is_some() {
        if !cfg.output_template_playlist.is_empty() {
            default = cfg.output_template_playlist.to_string();
        }
        prefixes.push(PLAYLIST_PREFIX);
    }
    if hints.channel_index.is_some() {
        if cfg.output_template_channel.is_empty() {
            // The playlist-resolved template survives, so both prefixes apply to it.
            prefixes.push(CHANNEL_PREFIX);
        } else {
            // The reassignment discards the playlist template and any pass over it.
            default = cfg.output_template_channel.to_string();
            prefixes = vec![CHANNEL_PREFIX];
        }
    }

    let splices = if prefixes.is_empty() {
        Vec::new()
    } else {
        scan_fields(&default, &prefixes)
    };
    let info = if splices.is_empty() {
        Map::new()
    } else {
        info_from_hints(hints)
    };

    OutTmplJob {
        default,
        chapter,
        splices,
        info,
        prefixes,
    }
}

/// [`build_outtmpl`] for one resolved entry, with the entry blob's info fields merged in.
///
/// This is the whole of legacy's `_resolve_outtmpl_fields` input: `build_outtmpl` derives what
/// [`EntryHints`] carries, and the DESIGN §7.5 subset of the provider's raw info dict
/// (`^(playlist|channel)`, `n_entries`, `__last_playlist_index`) supplies the rest — so a template
/// using `%(playlist_id)s` or `%(playlist_uploader)s` resolves instead of yielding yt-dlp's `NA`.
///
/// The result still has to be evaluated: [`OutTmplJob::ready`] when nothing needs the shim,
/// otherwise the `mode = "outtmpl"` round trip
/// ([`crate::YtdlpProvider::resolve_outtmpl`] does both).
#[must_use]
pub fn outtmpl_job(cfg: &Config, req: &DownloadRequest, entry: &MediaEntry) -> OutTmplJob {
    let mut job = build_outtmpl(cfg, req, &entry.hints);
    if job.is_ready() {
        // Nothing to splice, so no info field can change the answer.
        return job;
    }
    let info = aulos_provider::outtmpl_info(&entry.state);
    if !info.is_empty() {
        job.merge_info(&info);
    }
    job
}

/// The info dict [`build_outtmpl`] can derive from [`EntryHints`], string values sanitised.
///
/// Only `playlist*` and `channel*` keys are worth emitting: those are the only prefixes legacy
/// pre-resolved. Two aliases are deliberate, because yt-dlp's own child info dicts carry them and
/// the shipped default templates use them:
///
/// - `playlist` — yt-dlp sets it to the playlist *title* in every child info dict.
/// - `channel` — the default `OUTPUT_TEMPLATE_CHANNEL` is `%(channel)s/%(title)s.%(ext)s`, and
///   [`EntryHints`] calls the same thing `channel_title`.
///
/// `playlist_autonumber` and `n_entries` are approximated from the index and the count. Anything
/// else a template asks for (`playlist_id`, `playlist_uploader`, …) is absent and evaluates to
/// yt-dlp's `NA` unless the caller supplies it via [`OutTmplJob::merge_info`].
fn info_from_hints(h: &EntryHints) -> Map<String, Value> {
    let mut info = Map::new();
    let mut put_num = |k: &str, v: Option<u32>| {
        if let Some(v) = v {
            info.insert(k.to_owned(), Value::from(v));
        }
    };
    put_num("playlist_index", h.playlist_index);
    put_num("playlist_autonumber", h.playlist_index);
    put_num("playlist_count", h.playlist_count);
    put_num("n_entries", h.playlist_count);
    put_num("channel_index", h.channel_index);
    put_num("channel_count", h.channel_count);

    let mut put_str = |keys: &[&str], v: Option<&str>| {
        if let Some(v) = v {
            let clean = Value::String(sanitize_path_component(v));
            for k in keys {
                info.insert((*k).to_owned(), clean.clone());
            }
        }
    };
    put_str(&["playlist_title", "playlist"], h.playlist_title.as_deref());
    put_str(&["channel_title", "channel"], h.channel_title.as_deref());
    info
}

/// Legacy `_sanitize_path_component`, lifted to a JSON value: strings are sanitised, everything
/// else passes through so numeric format specs keep working.
fn sanitize_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(sanitize_path_component(s)),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), sanitize_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// The field scanner.
//
// A hand-written port of yt-dlp's `STR_FORMAT_RE_TMPL.format('[^)]+', f'[{STR_FORMAT_TYPES}
// ljhqBUDS]')`, which legacy compiled as `_OUTTMPL_FIELD_RE`. It is hand-written rather than a
// `regex` because the pattern opens with the lookbehind `(?<!%)`, which the `regex` crate does not
// support — and because `aulos-provider-ytdlp`'s DESIGN §3 row does not budget for `regex`.
// ---------------------------------------------------------------------------

/// yt-dlp's `STR_FORMAT_TYPES` plus the extra conversions its own templates accept.
const CONVERSION_TYPES: &[u8] = b"diouxXeEfFgGcrsaljhqBUDS";
/// The `[#0\-+ ]` conversion-flag class.
const FLAG_CHARS: &[u8] = b"#0-+ ";
/// The `[hlL]` length-modifier class, kept for parity even though Python ignores it.
const LEN_MODS: &[u8] = b"hlL";

/// Finds every field reference in `template` whose key root starts with one of `prefixes`.
///
/// The escape rule is the regex's: a run of `%` characters of even length is fully escaped
/// (`%%` → a literal `%`), and only the last `%` of an odd-length run can open a specifier. The
/// returned range covers the whole match, escape run included, because that is the substring
/// legacy handed to `evaluate_outtmpl`.
fn scan_fields(template: &str, prefixes: &[&str]) -> Vec<Splice> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        let run_start = i;
        let mut run_end = i;
        while run_end < bytes.len() && bytes[run_end] == b'%' {
            run_end += 1;
        }
        if (run_end - run_start) % 2 == 0 {
            i = run_end;
            continue;
        }
        let Some((end, key)) = parse_spec(bytes, run_end) else {
            i = run_end;
            continue;
        };
        if key
            .as_ref()
            .is_some_and(|k| root_matches(&template[k.clone()], prefixes))
        {
            out.push(Splice {
                start: run_start,
                end,
                spec: template[run_start..end].to_owned(),
            });
        }
        i = end;
    }
    out
}

/// Parses one specifier body starting at `at` (just past its `%`).
///
/// Returns the byte offset one past the specifier and the byte range of its `(key)`, if it had
/// one. `None` when the bytes at `at` are not a specifier at all, which is what makes a stray `%`
/// harmless.
fn parse_spec(bytes: &[u8], at: usize) -> Option<(usize, Option<std::ops::Range<usize>>)> {
    let mut i = at;
    let mut key = None;
    if bytes.get(i) == Some(&b'(') {
        // `\((?P<key>[^)]+)\)` — at least one character, and it must be closed.
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b')' {
            end += 1;
        }
        if end >= bytes.len() || end == start {
            return None;
        }
        key = Some(start..end);
        i = end + 1;
    }
    // `[#0\-+ ]+` then `\d+` then `\.\d+`, each optional and greedy.
    while bytes.get(i).is_some_and(|b| FLAG_CHARS.contains(b)) {
        i += 1;
    }
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        let mut j = i + 1;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j > i + 1 {
            i = j;
        }
    }
    // `[hlL]?` is greedy, so a length modifier followed by a conversion type wins even when the
    // modifier would itself have been a valid conversion type (`l` and `h` are in both classes).
    if bytes.get(i).is_some_and(|b| LEN_MODS.contains(b))
        && bytes
            .get(i + 1)
            .is_some_and(|b| CONVERSION_TYPES.contains(b))
    {
        return Some((i + 2, key));
    }
    if bytes.get(i).is_some_and(|b| CONVERSION_TYPES.contains(b)) {
        return Some((i + 1, key));
    }
    None
}

/// Legacy's `re.match(r'\w+', key)` followed by `root.startswith(prefixes)`.
///
/// A key that does not *start* with a word character has no root and is skipped, which is how
/// `%(+playlist)s` stays unresolved.
fn root_matches(key: &str, prefixes: &[&str]) -> bool {
    let root: String = key
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    !root.is_empty() && prefixes.iter().any(|p| root.starts_with(p))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn specs(t: &str, prefixes: &[&str]) -> Vec<String> {
        scan_fields(t, prefixes)
            .into_iter()
            .map(|s| s.spec)
            .collect()
    }

    #[test]
    fn the_scanner_finds_only_prefixed_roots() {
        assert_eq!(
            specs("%(playlist_title)s/%(title)s.%(ext)s", &["playlist"]),
            ["%(playlist_title)s"]
        );
        assert!(specs("%(title)s.%(ext)s", &["playlist", "channel"]).is_empty());
        assert_eq!(
            specs("%(channel)s/%(playlist_index)02d", &["playlist", "channel"]),
            ["%(channel)s", "%(playlist_index)02d"]
        );
    }

    #[test]
    fn the_scanner_honours_the_full_specifier_grammar() {
        // Defaults, math, conditionals, precision, flags, and the `q`/`j`/`B` conversions.
        for t in [
            "%(playlist_title|Unknown)s",
            "%(playlist_index+100)03d",
            "%(playlist_index&{} - |)s",
            "%(playlist_title).20s",
            "%(playlist_title)-30q",
            "%(playlist_count)lj",
        ] {
            assert_eq!(specs(t, &["playlist"]), [t], "{t} must be one match");
        }
    }

    #[test]
    fn an_escaped_percent_is_not_a_field() {
        // Even-length runs are fully escaped.
        assert!(specs("%%(playlist_title)s", &["playlist"]).is_empty());
        assert!(specs("100%%%%", &["playlist"]).is_empty());
        // An odd run opens a specifier with the whole run as the match, as the regex did.
        assert_eq!(
            specs("%%%(playlist_title)s", &["playlist"]),
            ["%%%(playlist_title)s"]
        );
        // A stray `%` is harmless.
        assert!(specs("50% off %(playlist_x", &["playlist"]).is_empty());
    }

    #[test]
    fn a_keyless_or_unclosed_specifier_is_skipped() {
        assert!(specs("%s %d %(unclosed", &["playlist"]).is_empty());
        assert!(specs("%()s", &["playlist"]).is_empty());
        assert!(specs("%(+playlist)s", &["playlist"]).is_empty());
    }

    #[test]
    fn only_string_values_are_sanitised() {
        let v = json!({ "playlist_title": "A: B?", "playlist_index": 3, "x": [1, "a|b"] });
        let clean = sanitize_value(&v);
        assert_eq!(clean["playlist_title"], json!("A_ B_"));
        assert_eq!(clean["playlist_index"], json!(3));
        assert_eq!(clean["x"], json!([1, "a_b"]));
    }

    #[test]
    fn apply_rejects_the_wrong_arity() {
        let job = OutTmplJob {
            default: "%(playlist_title)s/x".to_owned(),
            chapter: "c".to_owned(),
            splices: vec![Splice {
                start: 0,
                end: 18,
                spec: "%(playlist_title)s".to_owned(),
            }],
            info: Map::new(),
            prefixes: vec!["playlist"],
        };
        assert_eq!(
            job.apply(&[]).unwrap_err(),
            OutTmplError::Arity {
                expected: 1,
                got: 0
            }
        );
        assert_eq!(job.apply(&["Mix".to_owned()]).unwrap().default, "Mix/x");
    }
}
