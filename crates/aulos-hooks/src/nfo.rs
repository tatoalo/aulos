//! NFO generation, now actually wired (DESIGN §13.2).
//!
//! A port of legacy `jellyfin_nfo_generator.py`, which the legacy image ran as an `Exec`
//! postprocessor on **every** finished download. Three things move:
//!
//! - The input is preferably the item's `entry_json`, read through
//!   [`aulos_core::ports::HookStore`], and only otherwise the on-disk `.info.json`. So for a
//!   StreamingCommunity item there is nothing to read back and no race with a user's own `Exec`
//!   postprocessor.
//! - The `.info.json` the NFO was rendered from is deleted once the `.nfo` is written, and only
//!   then — legacy `jellyfin_nfo_generator.py` removed it unconditionally after a successful
//!   write, and a sidecar left behind in the library is ~11 MB of noise per video that Jellyfin
//!   has no use for once the `.nfo` exists. A run that writes nothing deletes nothing, so the one
//!   readable source of metadata is never destroyed without a document to replace it.
//! - After a successful write the blob is dropped through the port, because DESIGN §7.5 keeps SC
//!   entries alive past the terminal transition **only** until this hook has run.
//!
//! # Every provider, not just StreamingCommunity
//!
//! DESIGN §13 originally scoped this hook to `provider == "streamingcommunity"`, on the reasoning
//! that YouTube NFOs came out of the user's own `Exec` postprocessor. In production they did not:
//! aulos downloads into a per-job temp dir and only `MoveFiles` at the end, so an `Exec` command
//! resolving `<file>.info.json` next to `%(filepath)q` looks in the temp dir while `writeinfojson`
//! has already written the sidecar to the final one, and the legacy generator exits having found
//! nothing. The result was a cutover with **no `.nfo` at all** — the built-in hook gated out and
//! the user hook broken. So the gate is now the outcome, the file and `AULOS_NFO_ENABLED`, with
//! `AULOS_NFO_PROVIDERS` as an opt-in allow-list for anyone who wants the old narrowing back.
//!
//! # Where the metadata comes from, per provider
//!
//! | Provider | Blob at hook time (DESIGN §7.5) | Source used |
//! |---|---|---|
//! | `streamingcommunity` | the whole `state` object, kept alive for this hook | the blob |
//! | `command:<name>` | nothing: `aulos-queue`'s `keeps_entry` is `provider == streamingcommunity`, so the blob is dropped at the terminal write like any other non-SC row | the `<file>.info.json` sidecar if the plugin wrote one, else nothing |
//! | `ytdlp` | nothing (a playlist child keeps only its `outtmpl` hints, which are not metadata) | the `<file>.info.json` sidecar |
//!
//! The sidecar is exactly what the legacy generator read, so a yt-dlp download renders the XML
//! legacy rendered for the same input — which is what `tests/nfo_legacy_parity.rs` pins against
//! output captured from `jellyfin_nfo_generator.py` itself.
//!
//! # No source, no file
//!
//! When neither source carries anything — no blob, no sidecar, or a sidecar that is not JSON —
//! **nothing is written**. That is legacy's own behaviour (`generate_nfo` returns before opening
//! the output file when the sidecar is missing) and it is the only safe one here: `writeinfojson`
//! is an operator's choice, `AULOS_NFO_ENABLED` defaults to `true`, and a document rendered from
//! the row alone would be a title and an empty plot — a stub Jellyfin would happily adopt as the
//! local metadata for the file, and a stub that would overwrite a real `.nfo` written by someone
//! else's tooling. A hook that has nothing to say says nothing.
//!
//! # The three blob shapes this must read
//!
//! `entry_json` is not one shape, and the hook reads all of them through one lookup chain
//! ([`Meta`]): the v2 `state` object of DESIGN §10.3 (a freshly resolved SC entry), the same object
//! with an imported legacy entry hanging off `state.legacy` (DESIGN §7.6.3a), and the flat legacy
//! `.info.json` shape a finished SC download reports as `Outcome::entry_final` (DESIGN §10.5). A
//! real v2 key always wins over a stale imported one, matching
//! `ScState::to_legacy_info_json`'s own precedence.
//!
//! # Byte-for-byte output
//!
//! Legacy serialised with `ElementTree` and then re-indented with
//! `minidom.toprettyxml(indent="  ", encoding=None)`, dropping blank lines. That has three
//! observable consequences this module reproduces exactly, all pinned by snapshots: the
//! `<?xml version="1.0" ?>` declaration, `<plot/>` for an empty element, and a one-line
//! `<title>text</title>` rather than an indented text node.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as Atomic};

use aulos_core::id::UnixMs;
use aulos_core::item::{EntryBlob, ItemView};
use aulos_core::status::TerminalStatus;
use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use serde_json::{Map, Value};

use crate::error::HookError;
use crate::hook::{Hook, HookCtx, HookHealth, SkipReason};

/// `ordering` — after `audio_sync` (which rewrites the file) and before the Jellyfin scan
/// (which reads the NFO) (DESIGN §13).
pub const ORDERING: i16 = 20;

/// The `healthz` component key and the hook id.
pub const ID: &str = "nfo";

/// The provider whose `uniqueid` carries its own `type` (DESIGN §13, legacy `create_nfo_xml`).
///
/// It is no longer a gate: the hook applies to every provider unless `AULOS_NFO_PROVIDERS` names
/// a narrower set.
pub const PROVIDER: &str = "streamingcommunity";

/// The largest `.info.json` the sidecar fallback will read.
///
/// yt-dlp sidecars are routinely half a megabyte (601 KB for the download in the report that
/// prompted this) and occasionally much larger on a channel entry, but a hook must not read an
/// arbitrary file into memory just because it sits next to the media.
pub const MAX_INFO_JSON_BYTES: u64 = 16 * 1024 * 1024;

/// The keys whose presence makes a blob worth rendering from.
///
/// A StreamingCommunity `state` has no `title` (DESIGN §10.3 splits the identity into
/// `title_id`/`episode_id`), while a plain yt-dlp row keeps nothing but `outtmpl` hints — so
/// "is there a blob?" is the wrong question and "does the blob carry any element?" is the right
/// one. `channel` is in this list because it renders `<studio>`, but it is also an `outtmpl` hint,
/// so [`Meta::carries_metadata`] discounts it; see [`is_outtmpl_hint`].
const METADATA_KEYS: [&str; 12] = [
    "title",
    "id",
    "title_id",
    "description",
    "plot",
    "upload_date",
    "uploader",
    "channel",
    "tags",
    "duration",
    "webpage_url",
    "original_url",
];

/// The maximum number of `<tag>` elements, from legacy's `tags[:20]`.
pub const MAX_TAGS: usize = 20;

/// The `dateadded` format, from legacy's `strftime("%Y-%m-%d %H:%M:%S")` in UTC.
const DATEADDED: &[time::format_description::FormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

/// The XML declaration `minidom` writes when `encoding=None`.
pub const XML_DECL: &str = "<?xml version=\"1.0\" ?>";

/// Legacy's `info.get("title", "Unknown Title")` default, used when a sidecar has no `title` key.
pub const UNKNOWN_TITLE: &str = "Unknown Title";

/// Where the metadata being rendered came from.
///
/// It decides one thing: whether the item row may fill a gap the metadata leaves. A `.info.json`
/// is rendered exactly as the legacy script rendered it, down to its odd fallbacks (a missing
/// `title` becomes `"Unknown Title"`, a missing url means no `<website>` at all), because the
/// promise made to anyone cutting over from `jellyfin_nfo_generator.py` is that the same sidecar
/// yields the same bytes. A stored `entry_json` had no legacy equivalent — legacy never saw one —
/// so there the row's own `title` and `url` are used, which is what makes a StreamingCommunity
/// `state` (DESIGN §10.3 keeps no title in it) render at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// The item's stored `entry_json`; the row may fill a gap.
    Entry,
    /// An on-disk `<file>.info.json`; legacy semantics exactly, no row fallbacks.
    Sidecar,
}

/// Whether `key` is one of the output-template hints DESIGN §7.5 keeps for a playlist or channel
/// child — `^(playlist|channel)` plus `n_entries`/`__last_playlist_index`, which is exactly
/// `aulos_provider::entry::outtmpl_info`'s key set.
///
/// Such a blob is not metadata: `channel` is a hint (it names the channel a playlist child came
/// from) *and* the source of `<studio>`, so a hint-only blob would otherwise look like metadata
/// and shadow the sidecar that carries the whole info dict.
#[must_use]
pub fn is_outtmpl_hint(key: &str) -> bool {
    key.starts_with("playlist")
        || key.starts_with("channel")
        || key == "n_entries"
        || key == "__last_playlist_index"
}

/// A read-only view over an `entry_json` blob with the DESIGN §13.2 lookup chain.
///
/// Roots are consulted in order — the blob itself, a nested `state`, then their `legacy`
/// sub-objects — and the first non-null hit wins.
#[derive(Clone, Debug, Default)]
pub struct Meta<'a> {
    roots: Vec<&'a Map<String, Value>>,
}

impl<'a> Meta<'a> {
    /// Builds the chain for one blob. An absent or non-object blob yields an empty chain, which
    /// makes every lookup `None` rather than an error.
    #[must_use]
    pub fn new(entry: Option<&'a EntryBlob>) -> Self {
        let mut roots: Vec<&'a Map<String, Value>> = Vec::new();
        let Some(root) = entry.map(EntryBlob::as_value).and_then(Value::as_object) else {
            return Self { roots };
        };
        roots.push(root);
        if let Some(state) = root.get("state").and_then(Value::as_object) {
            roots.push(state);
        }
        // `legacy` last, so a real v2 key always wins over a stale imported one.
        if let Some(legacy) = root.get("legacy").and_then(Value::as_object) {
            roots.push(legacy);
        }
        if let Some(legacy) = root
            .get("state")
            .and_then(Value::as_object)
            .and_then(|s| s.get("legacy"))
            .and_then(Value::as_object)
        {
            roots.push(legacy);
        }
        Self { roots }
    }

    /// The first non-null value for `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.roots
            .iter()
            .filter_map(|r| r.get(key))
            .find(|v| !v.is_null())
    }

    /// `key` as the string Python's `info.get(key, "")` would have produced: a JSON string
    /// verbatim, a number or bool in its literal form, anything else empty.
    #[must_use]
    pub fn str(&self, key: &str) -> String {
        match self.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => {
                if *b {
                    "True".to_owned()
                } else {
                    "False".to_owned()
                }
            }
            _ => String::new(),
        }
    }

    /// `key` as an integer, accepting the numeric strings ffprobe-era metadata is full of.
    #[must_use]
    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Number(n) => n.as_i64().or_else(|| {
                #[allow(clippy::cast_possible_truncation)]
                n.as_f64().map(|f| f as i64)
            }),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        }
    }

    /// Whether `key` is present anywhere in the chain, **including** as an explicit `null`.
    ///
    /// This is Python's `key in info`, which is what legacy's nested `info.get(a, info.get(b, ""))`
    /// idiom actually tested: a present-but-null `uploader` selected `None`, not `channel`.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.roots.iter().any(|r| r.contains_key(key))
    }

    /// Whether any element-bearing key is reachable, i.e. whether rendering from this blob would
    /// produce more than the row alone already gives.
    ///
    /// Output-template hints do not count ([`is_outtmpl_hint`]): a yt-dlp playlist or channel
    /// child keeps `channel`, `playlist_index` and friends in its `state` past the terminal write,
    /// and rendering from those would produce a title-and-nothing document while the real info
    /// dict sat unread on disk next to the file.
    #[must_use]
    pub fn carries_metadata(&self) -> bool {
        METADATA_KEYS
            .iter()
            .any(|k| !is_outtmpl_hint(k) && self.get(k).is_some())
    }

    /// `key` as a float.
    #[must_use]
    pub fn float(&self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }
}

/// `(year, premiered)` from a yt-dlp `upload_date` (`YYYYMMDD`), or two empty strings.
#[must_use]
pub fn parse_upload_date(raw: &str) -> (String, String) {
    if raw.len() != 8 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return (String::new(), String::new());
    }
    let (year, rest) = raw.split_at(4);
    let (month, day) = rest.split_at(2);
    (year.to_owned(), format!("{year}-{month}-{day}"))
}

/// Legacy `seconds_to_minutes`: whole minutes, truncated toward zero, or `""`.
#[must_use]
pub fn seconds_to_minutes(secs: Option<f64>) -> String {
    match secs {
        Some(s) if s.is_finite() => {
            #[allow(clippy::cast_possible_truncation)]
            let minutes = (s / 60.0) as i64;
            minutes.to_string()
        }
        _ => String::new(),
    }
}

/// The `.nfo` path for a produced file: the same base with the extension replaced.
#[must_use]
pub fn nfo_path(file: &Path) -> PathBuf {
    file.with_extension("nfo")
}

/// The `.info.json` sidecar path for a produced file.
#[must_use]
pub fn info_json_path(file: &Path) -> PathBuf {
    file.with_extension("info.json")
}

/// Escapes a text node the way `xml.dom.minidom` does: `&`, `<` and `>`, and nothing else.
///
/// `quick-xml`'s own escaper also rewrites `'` and `"`, which are legal raw in a text node and
/// which legacy left alone — an apostrophe in an Italian episode title would otherwise diff.
#[must_use]
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// One element to write, in order.
struct El {
    name: &'static str,
    text: String,
    attr: Option<(&'static str, String)>,
}

impl El {
    fn new(name: &'static str, text: impl Into<String>) -> Self {
        Self {
            name,
            text: text.into(),
            attr: None,
        }
    }

    fn with_attr(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.attr = Some((key, value.into()));
        self
    }
}

/// Renders the NFO document for one item from one metadata blob (DESIGN §13.2).
///
/// There is no "render from nothing": a caller with no metadata writes no file, the way legacy
/// did. `source` decides whether the row may fill a gap the metadata leaves; see [`Source`].
///
/// `now_ms` is the wall clock the `dateadded` element is stamped from, so the whole renderer is
/// deterministic under a [`aulos_core::clock::FakeClock`].
///
/// # Errors
/// [`HookError::Other`] only if the XML writer fails, which for an in-memory `Vec<u8>` it cannot.
pub fn render(
    item: &ItemView,
    entry: &EntryBlob,
    source: Source,
    now_ms: UnixMs,
) -> Result<String, HookError> {
    let meta = Meta::new(Some(entry));

    // Legacy's `bool(...)` truthiness: a `season_number` of 0 does **not** make it an episode,
    // but it is still written when present.
    let series = meta.str("series");
    let season = meta.int("season_number");
    let episode_number = meta.int("episode_number");
    let is_episode = !series.is_empty()
        || season.is_some_and(|n| n != 0)
        || episode_number.is_some_and(|n| n != 0);

    // Legacy: `info.get("title", "Unknown Title")` — the default fires on an absent key only, so
    // an explicitly empty title stayed empty. A stored entry gets the row's title instead, because
    // a StreamingCommunity `state` has no `title` key at all and "Unknown Title" would be a lie.
    let title = match source {
        Source::Sidecar if meta.contains("title") => meta.str("title"),
        Source::Sidecar => UNKNOWN_TITLE.to_owned(),
        Source::Entry => {
            let t = meta.str("title");
            if t.is_empty() {
                item.title.to_string()
            } else {
                t
            }
        }
    };

    let mut els: Vec<El> = Vec::with_capacity(16);
    els.push(El::new("title", title.clone()));
    els.push(El::new("originaltitle", title));

    if is_episode {
        if !series.is_empty() {
            els.push(El::new("showtitle", series));
        }
        if let Some(n) = season {
            els.push(El::new("season", n.to_string()));
        }
        if let Some(n) = episode_number {
            els.push(El::new("episode", n.to_string()));
        }
        let episode_title = meta.str("episode");
        if !episode_title.is_empty() {
            els.push(El::new("subtitle", episode_title));
        }
    }

    // Legacy read `description`; an imported StreamingCommunity row may carry `plot` instead,
    // which is the same field under the name the legacy extractor used.
    let plot = {
        let d = meta.str("description");
        if d.is_empty() { meta.str("plot") } else { d }
    };
    els.push(El::new("plot", plot));

    let (year, premiered) = parse_upload_date(&meta.str("upload_date"));
    if !year.is_empty() {
        els.push(El::new("year", year));
    }
    if !premiered.is_empty() {
        els.push(El::new("premiered", premiered));
    }
    els.push(El::new("dateadded", dateadded(now_ms)));

    // Legacy: `info.get("uploader", info.get("channel", ""))`. `channel` is the fallback for an
    // **absent** `uploader` only — a present-but-null one selected `None` and wrote no `<studio>`,
    // which is the shape a yt-dlp sidecar for a video without a named uploader actually has.
    let uploader = if meta.contains("uploader") {
        meta.str("uploader")
    } else {
        meta.str("channel")
    };
    if !uploader.is_empty() {
        els.push(El::new("studio", uploader.clone()));
        els.push(El::new("director", uploader));
    }

    if let Some(id) = unique_id(&meta) {
        let kind = if meta.str("extractor").to_lowercase().contains(PROVIDER) {
            PROVIDER
        } else {
            "youtube"
        };
        els.push(El::new("uniqueid", id).with_attr("type", kind));
    }

    // Legacy: `info.get("original_url", info.get("webpage_url", ""))`, with the same
    // absent-vs-null rule as `uploader` and no third fallback — a sidecar carrying neither url got
    // no `<website>`. A stored entry does fall back to the row's url: it is the url the item was
    // queued with, and for a StreamingCommunity `state` it is the only one there is.
    let website = {
        let w = if meta.contains("original_url") {
            meta.str("original_url")
        } else {
            meta.str("webpage_url")
        };
        if source == Source::Entry && w.is_empty() {
            item.url.to_string()
        } else {
            w
        }
    };
    if !website.is_empty() {
        els.push(El::new("website", website));
    }

    if let Some(tags) = meta.get("tags").and_then(Value::as_array) {
        for tag in tags.iter().take(MAX_TAGS) {
            let text = match tag {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            if !text.is_empty() {
                els.push(El::new("tag", text));
            }
        }
    }

    let runtime = seconds_to_minutes(meta.float("duration"));
    if !runtime.is_empty() {
        els.push(El::new("runtime", runtime));
    }

    let root = if is_episode {
        "episodedetails"
    } else {
        "movie"
    };
    write_xml(root, &els)
}

/// The legacy `id` field, or the one derivable from a v2 `state` (`sc_<title>[_<episode>]`).
fn unique_id(meta: &Meta<'_>) -> Option<String> {
    let id = meta.str("id");
    if !id.is_empty() {
        return Some(id);
    }
    // A freshly resolved or imported SC row has no `id` key; DESIGN §10.3 splits it into
    // `title_id`/`episode_id`, and legacy's spelling is recoverable from them exactly.
    let title_id = meta.int("title_id")?;
    Some(match meta.int("episode_id") {
        Some(ep) => format!("sc_{title_id}_{ep}"),
        None => format!("sc_{title_id}"),
    })
}

fn dateadded(now_ms: UnixMs) -> String {
    let nanos = i128::from(now_ms) * 1_000_000;
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|t| t.format(DATEADDED).ok())
        .unwrap_or_default()
}

/// Serialises the element list the way `minidom.toprettyxml(indent="  ")` did.
fn write_xml(root: &'static str, els: &[El]) -> Result<String, HookError> {
    let mut w = Writer::new_with_indent(Vec::new(), b' ', 2);
    let fail = |e: std::io::Error| HookError::other(format!("nfo: {e}"));
    w.write_event(Event::Start(BytesStart::new(root)))
        .map_err(fail)?;
    for el in els {
        let mut start = BytesStart::new(el.name);
        if let Some((k, v)) = &el.attr {
            start.push_attribute((*k, v.as_str()));
        }
        if el.text.is_empty() {
            w.write_event(Event::Empty(start)).map_err(fail)?;
        } else {
            w.write_event(Event::Start(start)).map_err(fail)?;
            w.write_event(Event::Text(BytesText::from_escaped(escape_text(&el.text))))
                .map_err(fail)?;
            w.write_event(Event::End(BytesEnd::new(el.name)))
                .map_err(fail)?;
        }
    }
    w.write_event(Event::End(BytesEnd::new(root)))
        .map_err(fail)?;

    let body = String::from_utf8(w.into_inner())
        .map_err(|e| HookError::other(format!("nfo: the writer produced invalid utf-8: {e}")))?;
    // Legacy dropped every blank line and joined with "\n", so there is no trailing newline.
    let mut out = String::with_capacity(body.len() + XML_DECL.len() + 1);
    out.push_str(XML_DECL);
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(line);
    }
    Ok(out)
}

/// Reads a `.info.json` sidecar, the way the legacy generator did.
///
/// A missing sidecar is `None`, not an error: legacy logged a warning and exited 0, because
/// `writeinfojson` is optional. So is an oversized or malformed one — an NFO rendered from the row
/// alone is better than a failed hook, and the reason is logged either way.
async fn read_info_json(path: &Path) -> Option<EntryBlob> {
    match tokio::fs::metadata(path).await {
        Ok(m) if m.len() > MAX_INFO_JSON_BYTES => {
            tracing::warn!(
                path = %path.display(), bytes = m.len(), cap = MAX_INFO_JSON_BYTES,
                "info.json is too large to render an NFO from"
            );
            return None;
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "no info.json sidecar next to the file");
            return None;
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not stat info.json");
            return None;
        }
    }
    let text = match tokio::fs::read_to_string(path).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not read info.json");
            return None;
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Some(EntryBlob::new(v)),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "info.json is not valid JSON");
            None
        }
    }
}

/// The NFO writer (DESIGN §13.2).
#[derive(Clone, Debug)]
pub struct NfoHook {
    enabled: bool,
    /// `AULOS_NFO_PROVIDERS`. Empty means every provider.
    providers: Arc<[Box<str>]>,
    /// How many applicable items had nothing to render from.
    ///
    /// "The hook ran and wrote no file" is the other half of the shape that hid this bug: a skip
    /// is counted by the dispatcher, but a run that finds neither a stored entry nor a readable
    /// `.info.json` would otherwise look exactly like a run that wrote an NFO. It is published as
    /// `wrote_nothing_total` (§16.3), and a non-zero count on a yt-dlp install means the sidecar
    /// is missing — almost always `writeinfojson` being off.
    wrote_nothing: Arc<AtomicU64>,
}

impl Default for NfoHook {
    fn default() -> Self {
        Self::new()
    }
}

impl NfoHook {
    /// The hook, enabled for every provider.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: true,
            providers: Arc::from([]),
            wrote_nothing: Arc::default(),
        }
    }

    /// The hook, armed from `AULOS_NFO_ENABLED` and `AULOS_NFO_PROVIDERS`.
    #[must_use]
    pub fn from_config(cfg: &aulos_core::config::Config) -> Self {
        Self {
            enabled: cfg.nfo_enabled,
            providers: cfg.nfo_providers.iter().cloned().collect(),
            wrote_nothing: Arc::default(),
        }
    }

    /// How many applicable items this hook found no metadata for, and so wrote no file for.
    #[must_use]
    pub fn wrote_nothing_total(&self) -> u64 {
        self.wrote_nothing.load(Atomic::Relaxed)
    }

    /// Whether `provider` is in the allow-list. An empty list admits everything, which is the
    /// default and the legacy behaviour.
    #[must_use]
    pub fn allows(&self, provider: Option<&str>) -> bool {
        if self.providers.is_empty() {
            return true;
        }
        let provider = provider.unwrap_or_default();
        self.providers.iter().any(|p| &**p == provider)
    }
}

#[async_trait::async_trait]
impl Hook for NfoHook {
    fn id(&self) -> Arc<str> {
        Arc::from(ID)
    }

    fn ordering(&self) -> i16 {
        ORDERING
    }

    fn wants_entry(&self) -> bool {
        true
    }

    /// `finished && AULOS_NFO_ENABLED && the item produced a file` (DESIGN §13.2).
    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool {
        self.skip_reason(item, outcome).is_none()
    }

    fn skip_reason(&self, item: &ItemView, outcome: TerminalStatus) -> Option<SkipReason> {
        if !self.enabled {
            return Some(SkipReason::new("AULOS_NFO_ENABLED is false"));
        }
        if outcome != TerminalStatus::Finished {
            return Some(SkipReason::owned(format!(
                "the outcome is {outcome}, not finished"
            )));
        }
        if item.filename.is_none() {
            return Some(SkipReason::new("the item produced no file"));
        }
        if !self.allows(item.provider.as_deref()) {
            return Some(SkipReason::owned(format!(
                "AULOS_NFO_PROVIDERS does not list {}",
                item.provider.as_deref().unwrap_or("(no provider)")
            )));
        }
        None
    }

    fn health(&self) -> HookHealth {
        let mut health = if self.enabled {
            HookHealth::ok()
        } else {
            HookHealth::disabled()
        };
        // Same rule as `skipped_total` (§16.3): published once it has happened, absent otherwise,
        // so a healthy install's payload does not grow a counter that is always zero.
        let wrote_nothing = self.wrote_nothing_total();
        if wrote_nothing > 0 {
            health
                .detail
                .insert("wrote_nothing_total".to_owned(), wrote_nothing.into());
        }
        health
    }

    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError> {
        if !ctx.cfg.nfo_enabled {
            return Ok(());
        }
        let Some(file) = ctx.file else {
            return Ok(());
        };

        // The stored blob first, the sidecar second. For a StreamingCommunity item the blob is the
        // whole `state` object; for every other provider `aulos-queue` drops the blob at the
        // terminal write (DESIGN §7.5) and a playlist child's surviving `outtmpl` hints are not
        // metadata, so the `.info.json` legacy read is the only source of title, plot and tags.
        let sidecar_path = info_json_path(file);
        let from_entry = ctx.entry.filter(|e| Meta::new(Some(e)).carries_metadata());
        let from_disk: Option<EntryBlob> = if from_entry.is_some() {
            None
        } else {
            read_info_json(&sidecar_path).await
        };
        let source = from_entry
            .map(|e| (e, Source::Entry))
            .or_else(|| from_disk.as_ref().map(|e| (e, Source::Sidecar)));

        match source {
            Some((entry, source)) => {
                let xml = render(ctx.item, entry, source, ctx.clock.now_ms())?;
                let path = nfo_path(file);
                tokio::fs::write(&path, xml.as_bytes())
                    .await
                    .map_err(|e| HookError::io(format!("write {}", path.display()), e))?;
                tracing::info!(path = %path.display(), "created NFO");

                // Legacy parity (DESIGN §13.2): the sidecar goes as soon as the `.nfo` exists,
                // and only from this arm — the arm that writes nothing must leave the only
                // readable metadata where it is. A missing sidecar is a no-op (the metadata came
                // from the stored blob), and a failed unlink is not worth failing the hook over:
                // the NFO, which is the point, is already on disk.
                match tokio::fs::remove_file(&sidecar_path).await {
                    Ok(()) => {
                        tracing::info!(path = %sidecar_path.display(), "deleted info.json");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(path = %sidecar_path.display(), error = %e, "could not delete info.json");
                    }
                }
            }
            // Legacy warned and exited 0 **without writing**, and so does this: a document
            // rendered from the row alone is a stub, and a stub is worse than no file at all —
            // Jellyfin adopts it as the local metadata, and writing one would truncate a `.nfo`
            // another tool (an `Exec` postprocessor still running the legacy script, say) had
            // already written correctly.
            None => {
                self.wrote_nothing.fetch_add(1, Atomic::Relaxed);
                tracing::info!(
                    item = %ctx.item.id,
                    sidecar = %sidecar_path.display(),
                    "no metadata for an NFO: no stored entry and no readable .info.json, so nothing was written"
                );
            }
        }

        // DESIGN §7.5: the SC blob is kept past the terminal transition only until this ran, so a
        // StreamingCommunity row is always told to drop it — including when the blob failed to
        // load, since a failed read is not proof there is nothing to drop, and the retryable error
        // it raises is how that gets another try. No other provider reaches this with a blob:
        // `aulos-queue`'s `keeps_entry` is `provider == streamingcommunity`, so a `command` plugin
        // row is dropped at the terminal write exactly like a yt-dlp one. The second arm is a
        // guard for a future provider that keeps one, not a path anything takes today — and it is
        // what keeps a plain download from being charged an engine round trip.
        if ctx.item.provider.as_deref() == Some(PROVIDER) || ctx.entry.is_some() {
            ctx.store.drop_entry_blob(ctx.item.id).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_core::clock::Clock;

    #[test]
    fn upload_dates_parse_or_yield_nothing() {
        assert_eq!(
            parse_upload_date("20260904"),
            ("2026".to_owned(), "2026-09-04".to_owned())
        );
        assert_eq!(parse_upload_date(""), (String::new(), String::new()));
        assert_eq!(parse_upload_date("2026-09"), (String::new(), String::new()));
        assert_eq!(
            parse_upload_date("2026090x"),
            (String::new(), String::new())
        );
    }

    #[test]
    fn runtime_is_whole_minutes_truncated_toward_zero() {
        assert_eq!(seconds_to_minutes(Some(3599.0)), "59");
        assert_eq!(seconds_to_minutes(Some(30.0)), "0");
        assert_eq!(seconds_to_minutes(None), "");
        assert_eq!(seconds_to_minutes(Some(f64::NAN)), "");
    }

    #[test]
    fn the_lookup_chain_prefers_v2_over_legacy() {
        let blob = EntryBlob::new(serde_json::json!({
            "series": "V2 Serie",
            "legacy": { "series": "Old Serie", "plot": "Trama" },
        }));
        let meta = Meta::new(Some(&blob));
        assert_eq!(meta.str("series"), "V2 Serie");
        assert_eq!(meta.str("plot"), "Trama");
        assert_eq!(meta.str("absent"), "");
        assert_eq!(Meta::new(None).str("series"), "");
    }

    #[test]
    fn the_lookup_chain_reaches_into_a_nested_state() {
        let blob = EntryBlob::new(serde_json::json!({
            "media_id": "sc_9_77",
            "state": { "series": "Serie", "legacy": { "upload_date": "20260101" } },
        }));
        let meta = Meta::new(Some(&blob));
        assert_eq!(meta.str("series"), "Serie");
        assert_eq!(meta.str("upload_date"), "20260101");
    }

    #[test]
    fn numeric_strings_are_accepted_for_numbers() {
        let blob = EntryBlob::new(serde_json::json!({
            "season_number": "1", "episode_number": 2, "duration": "183.5",
        }));
        let meta = Meta::new(Some(&blob));
        assert_eq!(meta.int("season_number"), Some(1));
        assert_eq!(meta.int("episode_number"), Some(2));
        assert_eq!(meta.float("duration"), Some(183.5));
        assert_eq!(meta.int("nope"), None);
    }

    #[test]
    fn a_unique_id_is_derived_from_the_v2_state_when_there_is_no_legacy_id() {
        let blob = EntryBlob::new(serde_json::json!({"title_id": 9, "episode_id": 77}));
        assert_eq!(
            unique_id(&Meta::new(Some(&blob))),
            Some("sc_9_77".to_owned())
        );
        let movie = EntryBlob::new(serde_json::json!({"title_id": 9}));
        assert_eq!(unique_id(&Meta::new(Some(&movie))), Some("sc_9".to_owned()));
        let legacy = EntryBlob::new(serde_json::json!({"id": "sc_1_2", "title_id": 9}));
        assert_eq!(
            unique_id(&Meta::new(Some(&legacy))),
            Some("sc_1_2".to_owned()),
            "an explicit id wins"
        );
        assert_eq!(unique_id(&Meta::new(None)), None);
    }

    #[test]
    fn text_is_escaped_the_way_minidom_escaped_it() {
        assert_eq!(escape_text("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(
            escape_text("L'ultimo \"caso\""),
            "L'ultimo \"caso\"",
            "quotes are legal raw in a text node and legacy left them alone"
        );
    }

    #[test]
    fn dateadded_is_utc_seconds() {
        assert_eq!(dateadded(1_788_480_000_000), "2026-09-04 00:00:00");
        // `FakeClock::default()`, which every snapshot in this crate is stamped from. (Its own
        // doc comment in `aulos-core` says 2026-09-04; the epoch value it carries is 2026-03-04.)
        assert_eq!(
            dateadded(aulos_core::clock::FakeClock::default().now_ms()),
            "2026-03-04 00:00:00"
        );
    }

    #[test]
    fn an_empty_element_is_self_closing_and_text_stays_on_one_line() {
        let xml = write_xml("movie", &[El::new("title", "X"), El::new("plot", "")]).unwrap();
        assert_eq!(
            xml,
            "<?xml version=\"1.0\" ?>\n<movie>\n  <title>X</title>\n  <plot/>\n</movie>"
        );
    }

    #[test]
    fn nfo_and_sidecar_paths_replace_the_extension() {
        let f = Path::new("/downloads/Show/Clip.mp4");
        assert_eq!(nfo_path(f), Path::new("/downloads/Show/Clip.nfo"));
        assert_eq!(
            info_json_path(f),
            Path::new("/downloads/Show/Clip.info.json")
        );
    }
}
