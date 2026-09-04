//! The persisted item and the one wire shape (DESIGN §4.5, §4.6).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::error::WireError;
use crate::id::{GroupId, ItemId, Ord0, UnixMs};
use crate::paths::RelPath;
use crate::progress::{PhaseTag, ProgressCell};
use crate::request::{DownloadRequest, RequestView};
use crate::selection::{ProviderId, SelectionView};
use crate::source::SourceRef;
use crate::status::Status;

/// Whether a row is a downloadable item or a playlist/channel/season container.
///
/// A group is an `items` row like any other, so there is one array, one decoder and one Swift
/// struct on the client (DESIGN thesis T5).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A downloadable item.
    Item,
    /// A container. It never downloads a file of its own.
    Group,
}

impl Kind {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Group => "group",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The compacted provider entry, as persisted in `items.entry_json` (DESIGN §7.5).
///
/// What is kept depends on the provider: nothing for a plain yt-dlp child, the
/// `playlist`/`channel` keys for a playlist child, the whole `state` object for a
/// StreamingCommunity item (the just-in-time m3u8 needs it), and `state` plus identity for a
/// `command` plugin. Over `AULOS_ENTRY_MAX_BYTES` the store writes [`EntryBlob::truncated`].
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryBlob(Arc<Value>);

impl EntryBlob {
    /// The key a truncated blob carries.
    pub const TRUNCATED_KEY: &'static str = "__truncated";

    /// Wraps a JSON object.
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self(Arc::new(value))
    }

    /// The `{"__truncated": true}` placeholder written when the entry exceeds the byte cap.
    #[must_use]
    pub fn truncated() -> Self {
        Self::new(serde_json::json!({ Self::TRUNCATED_KEY: true }))
    }

    /// Whether this is the truncation placeholder.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.0
            .get(Self::TRUNCATED_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// The underlying JSON.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

/// One produced auxiliary file: a chapter split or a subtitle track (DESIGN §4.6).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FileRef {
    /// Relative to the item's download root.
    pub filename: Arc<str>,
    /// Bytes on disk.
    pub size: Option<u64>,
    /// Ready-to-open URL. Absolute or relative; see [`ItemView::download_url`].
    pub download_url: Option<Arc<str>>,
    /// Subtitle language tag, for a subtitle file.
    pub lang: Option<Arc<str>>,
}

/// Which list a [`FileRef`] belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSlot {
    /// `chapter_files`.
    Chapter,
    /// `subtitle_files`.
    Subtitle,
}

/// The database row, minus transient progress (DESIGN §4.5).
///
/// `PartialEq` is derived so the store's round-trip tests (WP-04) and the importer's fixtures
/// (WP-05) can compare a written row with the one read back.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Immutable identity.
    pub id: ItemId,
    /// Item or group.
    pub kind: Kind,
    /// The group this item belongs to, if any.
    pub group_id: Option<GroupId>,
    /// 1-based position within the group.
    pub group_index: Option<u32>,
    /// The client sort key.
    pub ord: Ord0,
    /// The source page URL.
    pub url: Url,
    /// The dedupe key (DESIGN §8.5).
    pub canonical_key: Box<str>,
    /// The provider that resolved (or will download) this item. `None` before resolution.
    pub provider: Option<ProviderId>,
    /// The provider's own id — the legacy `id` field.
    pub media_id: Option<Box<str>>,
    /// Never empty: before resolution it is the URL.
    pub title: Box<str>,
    /// The closed eight-value status.
    pub status: Status,
    /// `Queued`'s scheduling flag.
    pub auto_start: bool,
    /// Human stage text, or the last provider message.
    pub msg: Option<Box<str>>,
    /// Terminal error, or a pre-download problem on a `queued` item (DESIGN §8.4).
    pub error: Option<WireError>,
    /// What was asked for. Written once at insert.
    pub request: DownloadRequest,
    /// The compacted provider entry.
    pub entry: Option<EntryBlob>,
    /// The produced file, relative to the item's download root.
    pub filename: Option<RelPath>,
    /// Bytes on disk.
    pub size: Option<u64>,
    /// Chapter splits.
    pub chapter_files: Vec<FileRef>,
    /// Subtitle tracks.
    pub subtitle_files: Vec<FileRef>,
    /// Insert time, unix ms.
    pub created_at: UnixMs,
    /// First `preparing`, unix ms. Survives a retry.
    pub started_at: Option<UnixMs>,
    /// Terminal transition, unix ms. Cleared by a retry.
    pub finished_at: Option<UnixMs>,
    /// `0` on the first try.
    pub attempt: u16,
    /// Attribution.
    pub source: SourceRef,
    /// Groups only: the declared child count.
    pub children_total: Option<u32>,
    /// When `CLEAR_COMPLETED_AFTER` should remove this row, unix ms.
    pub clear_after: Option<UnixMs>,
}

/// The derived, non-persisted parts of an [`ItemView`] that only the caller can supply.
///
/// `download_url` needs `PUBLIC_HOST_URL` and percent-encoding (`aulos-api`); the three child
/// counters come from the engine's group counters, not from the row (DESIGN §8.6).
#[derive(Clone, Debug, Default)]
pub struct ViewExtras {
    /// Ready-to-open URL for the primary file.
    pub download_url: Option<Arc<str>>,
    /// Groups only: children with `status == finished`.
    pub children_done: Option<u32>,
    /// Groups only: children with `status == error`.
    pub children_error: Option<u32>,
    /// Groups only: children in `preparing`/`downloading`/`postprocessing`.
    pub children_active: Option<u32>,
    /// Groups only: whether this payload also carries the children.
    pub children_inline: Option<bool>,
}

/// The one wire shape: what REST returns, what the WS `snapshot` contains, and what `added` and
/// `completed` carry (DESIGN §4.6, PROTOCOL §2).
///
/// **Every field is always serialised**; `None` becomes JSON `null` and a key is never absent.
/// There is no `skip_serializing_if` anywhere in this struct, and [`ItemView::FIELDS`] is the
/// authoritative key list that the aggregator's diff macro (WP-13) asserts against.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ItemView {
    /// Immutable identity.
    pub id: ItemId,
    /// `"item"` or `"group"`.
    pub kind: Kind,
    /// **The** sort key. `ORDER BY ord ASC, id ASC`.
    pub ord: i64,
    /// The group this item belongs to, or `null`.
    pub group_id: Option<ItemId>,
    /// 1-based position within the group.
    pub group_index: Option<u32>,
    /// The source page URL.
    pub url: Arc<str>,
    /// Never `null`; before resolution it is the URL.
    pub title: Arc<str>,
    /// The closed eight-value status, groups included.
    pub status: Status,
    /// With `queued`: `true` = waiting for a slot, `false` = waiting for the user.
    pub auto_start: bool,
    /// `null` until resolution picks one.
    pub provider: Option<Arc<str>>,

    /// `0.0..=100.0`. **Never null**, never a string.
    pub percent: f64,
    /// Bytes per second.
    pub speed: Option<f64>,
    /// Whole seconds remaining.
    pub eta: Option<i64>,
    /// Bytes fetched so far.
    pub downloaded_bytes: Option<u64>,
    /// Exact total, when known.
    pub total_bytes: Option<u64>,
    /// Estimated total. Use only when `total_bytes` is `null`.
    pub total_bytes_estimate: Option<u64>,
    /// HLS/DASH fragment index.
    pub fragment_index: Option<u32>,
    /// HLS/DASH fragment count.
    pub fragment_count: Option<u32>,
    /// A cosmetic label. Do not switch on it.
    pub phase: Option<PhaseTag>,
    /// Postprocessor progress, independent of `percent`.
    pub phase_percent: Option<f64>,

    /// A short human status line. Already cleaned.
    pub msg: Option<Arc<str>>,
    /// `{ code, message, field, provider, provider_code }`. May be non-null on a `queued` item.
    pub error: Option<WireError>,

    /// The produced file, relative to its download root.
    pub filename: Option<Arc<str>>,
    /// Bytes on disk.
    pub size: Option<u64>,
    /// A ready-to-open, percent-encoded URL.
    ///
    /// **Usually relative** to `<p>` (`"download/My%20Video.mp4"`) but **may be absolute** when
    /// the operator points `PUBLIC_HOST_URL` at a CDN. The client rule is one line: if it parses
    /// as an absolute URL, open it as-is; otherwise resolve it against the base URL plus `<p>`
    /// (DESIGN §4.6.2, PROTOCOL §2.3).
    pub download_url: Option<Arc<str>>,
    /// Always an array, possibly empty. Never `null`.
    pub chapter_files: Arc<[FileRef]>,
    /// Always an array, possibly empty. Never `null`.
    pub subtitle_files: Arc<[FileRef]>,

    /// Immutable for the record's life, therefore never present in a `delta`.
    pub selection: SelectionView,
    /// Immutable for the record's life, therefore never present in a `delta`.
    pub folder: Option<Arc<str>>,
    /// Immutable for the record's life, therefore never present in a `delta`.
    pub request: RequestView,

    /// Insert time, unix ms.
    pub created_at: i64,
    /// First `preparing`, unix ms.
    pub started_at: Option<i64>,
    /// Terminal transition, unix ms.
    pub finished_at: Option<i64>,
    /// `0` on the first try.
    pub attempt: u16,
    /// Attribution.
    pub source: SourceRef,

    /// Groups only: the declared child count.
    pub children_total: Option<u32>,
    /// Groups only: children with `status == finished`.
    pub children_done: Option<u32>,
    /// Groups only: children with `status == error`.
    pub children_error: Option<u32>,
    /// Groups only: children in `preparing`/`downloading`/`postprocessing`.
    pub children_active: Option<u32>,
    /// Groups only: `true` = the children are in this payload, `false` = fetch them.
    ///
    /// v1.0: not implemented, see BRIEF — `AULOS_SNAPSHOT_GROUP_INLINE` and the WS `watch` frame
    /// are CUT, so the snapshot always carries every non-terminal child and this is `true` on a
    /// group. The key stays on the wire because PROTOCOL §2.3 documents it.
    pub children_inline: Option<bool>,
}

impl ItemView {
    /// The authoritative key list, in serialisation order.
    ///
    /// The aggregator's delta diff (WP-13) must cover exactly these, and `serde` must emit exactly
    /// these — both directions are asserted by tests, so a field added to the struct without being
    /// added to the diff cannot ship.
    pub const FIELDS: [&'static str; 40] = [
        "id",
        "kind",
        "ord",
        "group_id",
        "group_index",
        "url",
        "title",
        "status",
        "auto_start",
        "provider",
        "percent",
        "speed",
        "eta",
        "downloaded_bytes",
        "total_bytes",
        "total_bytes_estimate",
        "fragment_index",
        "fragment_count",
        "phase",
        "phase_percent",
        "msg",
        "error",
        "filename",
        "size",
        "download_url",
        "chapter_files",
        "subtitle_files",
        "selection",
        "folder",
        "request",
        "created_at",
        "started_at",
        "finished_at",
        "attempt",
        "source",
        "children_total",
        "children_done",
        "children_error",
        "children_active",
        "children_inline",
    ];

    /// The three fields written once at insert, which a `delta` can therefore never carry
    /// (DESIGN §4.6.1, PROTOCOL §5.4). The diff macro `debug_assert!`s they are equal.
    pub const IMMUTABLE_FIELDS: [&'static str; 3] = ["selection", "folder", "request"];

    /// Projects a persisted row plus its transient progress cell onto the wire shape.
    ///
    /// `percent` follows the DESIGN §4.6 rule: `Finished` is exactly `100.0`; `Error`/`Canceled`
    /// keep the last value; anything else takes the cell's value, or `0.0` when there is no cell.
    #[must_use]
    pub fn from_item(item: &Item, cell: Option<&ProgressCell>, extras: &ViewExtras) -> Self {
        let percent = match item.status {
            Status::Finished => 100.0,
            _ => cell.map_or(0.0, |c| c.percent),
        };

        Self {
            id: item.id,
            kind: item.kind,
            ord: item.ord,
            group_id: item.group_id,
            group_index: item.group_index,
            url: Arc::from(item.url.as_str()),
            title: Arc::from(&*item.title),
            status: item.status,
            auto_start: item.auto_start,
            provider: item.provider.as_ref().map(ProviderId::as_arc),

            percent,
            speed: cell.and_then(|c| c.speed),
            eta: cell.and_then(|c| c.eta),
            downloaded_bytes: cell.and_then(|c| c.downloaded_bytes),
            total_bytes: cell.and_then(|c| c.total_bytes),
            total_bytes_estimate: cell.and_then(|c| c.total_bytes_estimate),
            fragment_index: cell.and_then(|c| c.fragment_index),
            fragment_count: cell.and_then(|c| c.fragment_count),
            phase: cell.and_then(|c| c.phase),
            phase_percent: cell.and_then(|c| c.phase_percent),

            msg: item.msg.as_ref().map(|m| Arc::from(&**m)),
            error: item.error.clone(),

            filename: item.filename.as_ref().map(|f| Arc::from(f.as_str())),
            size: item.size,
            download_url: extras.download_url.clone(),
            chapter_files: Arc::from(item.chapter_files.clone()),
            subtitle_files: Arc::from(item.subtitle_files.clone()),

            selection: item.request.selection.to_view(),
            folder: item.request.folder.as_ref().map(|f| Arc::from(f.as_str())),
            request: item.request.to_view(),

            created_at: item.created_at,
            started_at: item.started_at,
            finished_at: item.finished_at,
            attempt: item.attempt,
            source: item.source.clone(),

            children_total: item.children_total,
            children_done: extras.children_done,
            children_error: extras.children_error,
            children_active: extras.children_active,
            children_inline: extras.children_inline,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn kind_serialises_lowercase() {
        assert_eq!(serde_json::to_string(&Kind::Item).unwrap(), "\"item\"");
        assert_eq!(serde_json::to_string(&Kind::Group).unwrap(), "\"group\"");
    }

    #[test]
    fn entry_blob_truncation_marker_round_trips() {
        let t = EntryBlob::truncated();
        assert!(t.is_truncated());
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#"{"__truncated":true}"#);
        assert!(
            serde_json::from_str::<EntryBlob>(&json)
                .unwrap()
                .is_truncated()
        );
        assert!(!EntryBlob::new(serde_json::json!({"id": "x"})).is_truncated());
    }

    #[test]
    fn file_ref_serialises_four_keys() {
        let f = FileRef {
            filename: "a.srt".into(),
            size: Some(12),
            download_url: None,
            lang: Some("en".into()),
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 4);
        assert!(v["download_url"].is_null());
        assert_eq!(serde_json::from_value::<FileRef>(v).unwrap(), f);
    }

    #[test]
    fn immutable_fields_are_a_subset_of_fields() {
        for f in ItemView::IMMUTABLE_FIELDS {
            assert!(ItemView::FIELDS.contains(&f), "{f} must be a real field");
        }
    }
}
