//! The wire contract of `ItemView`, `RequestView` and `WireError` (DESIGN §4.6, PROTOCOL §2).
//!
//! Three rules are pinned here, each with its own test:
//!
//! 1. **Every field is always serialised.** `Option::None` becomes JSON `null`; a key is never
//!    absent. This is what kills the client's "lazily created `filename`" problem.
//! 2. **The key set is exactly the struct's field set**, and equal to [`ItemView::FIELDS`] — the
//!    list the aggregator's delta diff (WP-13) must cover. `print-schema` is CUT for v1.0, so this
//!    assertion is what stands in for its snapshot.
//! 3. **`selection`, `folder` and `request` never change**, so a `delta` can never carry them.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::collections::BTreeSet;
use std::sync::Arc;

use aulos_core::{
    Codec, DownloadRequest, DownloadType, EntryBlob, ErrorCode, FileRef, FormatId, Item, ItemId,
    ItemView, Kind, Normalizer, PhaseTag, ProgressCell, QualityId, REDACTED, RawProgress, RelDir,
    RelPath, Selection, SourceKind, SourceRef, Status, SubtitleLang, SubtitleMode, ViewExtras,
    WireError,
};
use serde_json::{Value, json};
use url::Url;

/// A fixed id so the snapshot is stable.
fn id(s: &str) -> ItemId {
    s.parse().expect("valid ULID")
}

/// An item with **every** optional field populated, so the snapshot proves the whole key set.
fn fully_populated_item() -> Item {
    let mut request = DownloadRequest::new(
        Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").expect("url"),
        Selection::new(
            DownloadType::Video,
            Codec::H265,
            FormatId::parse("mp4").expect("format"),
            QualityId::parse("1080").expect("quality"),
        ),
    );
    request.folder = Some(RelDir::parse("Music/Live").expect("folder"));
    request.custom_name_prefix = "prefix - ".into();
    request.playlist_item_limit = 25;
    request.auto_start = true;
    request.split_by_chapters = true;
    request.chapter_template = "%(title)s - %(section_number)02d.%(ext)s".into();
    request.subtitle_language = SubtitleLang::parse("en-GB").expect("lang");
    request.subtitle_mode = SubtitleMode::PreferAuto;
    request.ytdl_options_presets = vec!["sponsorblock".into(), "archive".into()];
    request.ytdl_options_overrides = json!({
        "cookiefile": "/config/cookies.txt",
        "format_sort": ["res", "vcodec"],
    })
    .as_object()
    .expect("object")
    .clone();

    Item {
        id: id("01JBQ7Z5T9K3M2R8V4XW6Y0AAA"),
        kind: Kind::Item,
        group_id: Some(id("01JBQ8AA0000000000000000GG")),
        group_index: Some(12),
        ord: 981,
        url: request.url.clone(),
        canonical_key: "youtube:dQw4w9WgXcQ".into(),
        provider: Some("ytdlp".parse().expect("provider")),
        media_id: Some("dQw4w9WgXcQ".into()),
        title: "Rick Astley - Never Gonna Give You Up".into(),
        status: Status::Postprocessing,
        auto_start: false,
        msg: Some("Merging formats".into()),
        error: Some(
            WireError::new(ErrorCode::NotYetLive, "Live stream starts at 20:00")
                .with_provider("ytdlp", Some("ExtractorError".into())),
        ),
        request,
        entry: Some(EntryBlob::new(json!({ "playlist_index": 12 }))),
        filename: Some(RelPath::parse("Music/Live/prefix - Rick.mkv").expect("filename")),
        size: Some(104_857_600),
        chapter_files: vec![FileRef {
            filename: "Music/Live/prefix - Rick - 01 - Intro.mkv".into(),
            size: Some(1_048_576),
            download_url: Some(
                "download/Music/Live/prefix%20-%20Rick%20-%2001%20-%20Intro.mkv".into(),
            ),
            lang: None,
        }],
        subtitle_files: vec![FileRef {
            filename: "Music/Live/prefix - Rick.en-GB.srt".into(),
            size: Some(2048),
            download_url: Some("download/Music/Live/prefix%20-%20Rick.en-GB.srt".into()),
            lang: Some("en-GB".into()),
        }],
        created_at: 1_756_999_900_000,
        started_at: Some(1_756_999_901_000),
        finished_at: Some(1_756_999_999_000),
        attempt: 2,
        source: SourceRef::with_ref(SourceKind::Telegram, "12345"),
        children_total: Some(40),
        clear_after: Some(1_757_000_900_000),
    }
}

/// A progress cell with every field populated too.
fn full_cell() -> ProgressCell {
    let mut n = Normalizer::new();
    let frame = RawProgress {
        downloaded_bytes: Some(44_040_192.0),
        total_bytes: Some(103_809_024.0),
        total_bytes_estimate: Some(103_800_000.0),
        fragment_index: Some(7),
        fragment_count: Some(19),
        speed: Some(3_145_728.0),
        eta: Some(63),
        phase: Some(PhaseTag::Remux),
        phase_percent: Some(41.5),
        source_tag: 7,
    };
    let percent = n.apply(&frame, Status::Postprocessing);
    let mut cell = ProgressCell::default();
    cell.apply(&frame, percent, cell.last_frame_at);
    cell
}

fn full_view() -> ItemView {
    let item = fully_populated_item();
    let cell = full_cell();
    let extras = ViewExtras {
        download_url: Some("download/Music/Live/prefix%20-%20Rick.mkv".into()),
        children_done: Some(30),
        children_error: Some(1),
        children_active: Some(2),
        children_inline: Some(true),
    };
    ItemView::from_item(&item, Some(&cell), &extras)
}

#[tokio::test]
async fn a_fully_populated_item_view_matches_its_snapshot() {
    insta::assert_json_snapshot!(full_view());
}

#[tokio::test]
async fn the_key_set_is_exactly_item_views_field_list() {
    let value = serde_json::to_value(full_view()).expect("serialise");
    let keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();

    let declared: BTreeSet<&str> = ItemView::FIELDS.into_iter().collect();
    let serialised: BTreeSet<&str> = keys.iter().copied().collect();
    assert_eq!(
        serialised, declared,
        "ItemView::FIELDS and the serializer disagree; the aggregator's delta diff (WP-13) is \
         asserted against FIELDS, so a field added to one and not the other would silently never \
         reach a client"
    );
    assert_eq!(
        keys.len(),
        ItemView::FIELDS.len(),
        "no duplicate keys, no missing keys"
    );
}

#[tokio::test]
async fn no_key_is_ever_absent_even_on_a_bare_item() {
    // The minimal item: nothing optional set, no progress cell at all.
    let mut item = fully_populated_item();
    item.group_id = None;
    item.group_index = None;
    item.provider = None;
    item.media_id = None;
    item.msg = None;
    item.error = None;
    item.entry = None;
    item.filename = None;
    item.size = None;
    item.chapter_files.clear();
    item.subtitle_files.clear();
    item.started_at = None;
    item.finished_at = None;
    item.children_total = None;
    item.request.folder = None;
    item.status = Status::Queued;

    let view = ItemView::from_item(&item, None, &ViewExtras::default());
    let value = serde_json::to_value(&view).expect("serialise");
    let obj = value.as_object().expect("object");

    for field in ItemView::FIELDS {
        assert!(obj.contains_key(field), "{field} is absent");
    }
    for nullable in [
        "group_id",
        "group_index",
        "provider",
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
        "folder",
        "started_at",
        "finished_at",
        "children_total",
        "children_done",
        "children_error",
        "children_active",
        "children_inline",
    ] {
        assert_eq!(obj[nullable], Value::Null, "{nullable} should be null");
    }

    // Non-nullable fields are still real values.
    assert_eq!(obj["percent"], json!(0.0), "percent is never null");
    assert!(obj["percent"].is_number());
    assert_eq!(obj["chapter_files"], json!([]), "arrays are never null");
    assert_eq!(obj["subtitle_files"], json!([]));
    assert_eq!(obj["status"], "queued");
    assert_eq!(obj["kind"], "item");
    assert!(obj["title"].is_string());
}

#[tokio::test]
async fn percent_is_exactly_one_hundred_when_finished_and_frozen_otherwise() {
    let mut item = fully_populated_item();
    let cell = full_cell();

    item.status = Status::Finished;
    let finished = ItemView::from_item(&item, Some(&cell), &ViewExtras::default());
    assert_eq!(finished.percent, 100.0);

    for status in [Status::Error, Status::Canceled] {
        item.status = status;
        let view = ItemView::from_item(&item, Some(&cell), &ViewExtras::default());
        assert_eq!(
            view.percent, cell.percent,
            "{status} keeps the last progress value"
        );
        assert!(view.percent < 100.0);
    }
}

#[tokio::test]
async fn the_immutable_fields_are_the_three_a_delta_may_never_carry() {
    assert_eq!(
        ItemView::IMMUTABLE_FIELDS,
        ["selection", "folder", "request"]
    );

    // They are derived only from `Item.request`, which is written once at insert, so two views of
    // the same row are always equal on all three however much progress moved.
    let item = fully_populated_item();
    let a = ItemView::from_item(&item, None, &ViewExtras::default());
    let b = ItemView::from_item(&item, Some(&full_cell()), &ViewExtras::default());
    assert_eq!(a.selection, b.selection);
    assert_eq!(a.folder, b.folder);
    assert_eq!(a.request, b.request);
    assert_ne!(a.percent, b.percent, "the mutable half did move");
}

#[tokio::test]
async fn request_view_redacts_secret_override_values_and_keeps_the_key_set() {
    let view = full_view();
    let overrides = &view.request.ytdl_options_overrides;
    assert_eq!(overrides.len(), 2, "the key set is preserved");
    assert_eq!(
        overrides["cookiefile"],
        Value::String(REDACTED.to_owned()),
        "a secret-looking key hides its value"
    );
    assert_eq!(overrides["format_sort"], json!(["res", "vcodec"]));

    // The serialised form says the same thing.
    let value = serde_json::to_value(&view).expect("serialise");
    let text = value.to_string();
    assert!(
        !text.contains("/config/cookies.txt"),
        "the cookie path leaked: {text}"
    );
    assert!(text.contains(REDACTED));
}

#[tokio::test]
async fn wire_error_is_one_struct_for_both_surfaces() {
    // `Item.error` is `WireError` verbatim...
    let view = full_view();
    let item_error = serde_json::to_value(view.error.as_ref().expect("error")).expect("serialise");
    assert_eq!(item_error.as_object().expect("object").len(), 5);
    assert_eq!(item_error["code"], "not_yet_live");
    assert!(
        item_error["field"].is_null(),
        "an item error names no request field"
    );
    assert_eq!(item_error["provider"], "ytdlp");

    // ...and the HTTP envelope is the same struct plus `request_id`, so stripping that key leaves
    // something this type decodes.
    let envelope = json!({
        "code": "validation_failed",
        "message": "quality must be one of ['best'] for format opus",
        "field": "quality",
        "provider": Value::Null,
        "provider_code": Value::Null,
        "request_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
    });
    let mut body = envelope.as_object().expect("object").clone();
    body.remove("request_id");
    let decoded: WireError = serde_json::from_value(Value::Object(body)).expect("decode");
    assert_eq!(decoded.code, ErrorCode::ValidationFailed);
    assert_eq!(decoded.field.as_deref(), Some("quality"));
    assert_eq!(decoded.provider, None);
}

#[tokio::test]
async fn a_group_carries_its_aggregates_and_an_item_does_not() {
    let mut item = fully_populated_item();
    item.kind = Kind::Group;
    item.status = Status::Downloading;
    let group = ItemView::from_item(
        &item,
        None,
        &ViewExtras {
            children_done: Some(30),
            children_error: Some(1),
            children_active: Some(2),
            children_inline: Some(true),
            download_url: None,
        },
    );
    let value = serde_json::to_value(&group).expect("serialise");
    assert_eq!(value["kind"], "group");
    assert_eq!(value["children_total"], 40);
    assert_eq!(value["children_done"], 30);
    assert_eq!(value["children_error"], 1);
    assert_eq!(value["children_active"], 2);
    assert_eq!(value["children_inline"], true);
    assert_eq!(
        value["status"], "downloading",
        "a group uses the same closed status vocabulary"
    );

    // A plain item leaves all five null.
    item.kind = Kind::Item;
    item.children_total = None;
    let plain = serde_json::to_value(ItemView::from_item(&item, None, &ViewExtras::default()))
        .expect("serialise");
    for k in [
        "children_total",
        "children_done",
        "children_error",
        "children_active",
        "children_inline",
    ] {
        assert_eq!(plain[k], Value::Null, "{k} on a plain item");
    }
}

#[tokio::test]
async fn the_view_round_trips_through_serde() {
    let view = full_view();
    let json = serde_json::to_string(&view).expect("serialise");
    let back: ItemView = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(view, back);
}

#[tokio::test]
async fn a_source_ref_needs_no_custom_decoder() {
    let value = serde_json::to_value(full_view()).expect("serialise");
    assert_eq!(
        value["source"],
        json!({ "kind": "telegram", "ref": "12345" })
    );
}

#[tokio::test]
async fn arc_str_fields_are_plain_json_strings() {
    // `Arc<str>` and `Arc<[FileRef]>` must not serialise as anything exotic.
    let value = serde_json::to_value(full_view()).expect("serialise");
    assert!(value["url"].is_string());
    assert!(value["title"].is_string());
    assert!(value["chapter_files"].is_array());
    assert!(value["subtitle_files"].is_array());
    assert_eq!(value["chapter_files"][0]["lang"], Value::Null);
    assert_eq!(value["subtitle_files"][0]["lang"], "en-GB");
    let file = value["chapter_files"][0].as_object().expect("object");
    assert_eq!(file.len(), 4);
}

/// `Arc<ItemView>` is what every `DomainEvent` payload carries, so it must serialise identically.
#[tokio::test]
async fn an_arc_wrapped_view_serialises_identically() {
    let view = full_view();
    let direct = serde_json::to_value(&view).expect("serialise");
    let shared = serde_json::to_value(Arc::new(view)).expect("serialise");
    assert_eq!(direct, shared);
}
