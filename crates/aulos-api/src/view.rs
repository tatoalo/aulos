//! `download_url`: the one derived field `aulos-api` owns (DESIGN §4.6.2, PROTOCOL §2.3).
//!
//! The engine builds every [`ItemView`] with `download_url: None`, because turning a relative
//! `filename` into a ready-to-open URL needs `PUBLIC_HOST_URL` / `PUBLIC_HOST_AUDIO_URL` and
//! percent-encoding — configuration and an encoder that live on this side of the process
//! (`aulos_core::ViewExtras`'s own documentation assigns the field here, and
//! `docs/INTEGRATION-NOTES.md` records it for WP-13). So this module fills it in on **every**
//! surface, and does so from one function so the surfaces cannot disagree:
//!
//! | Surface | How |
//! |---|---|
//! | `GET api/v2/state`, `items`, `items/{id}`, the WS `snapshot` | [`project`] over the published view |
//! | the `added` / `completed` frames | [`patch_frame`] over the serialised frame |
//! | a `delta` that carries a new `filename` | [`patch_frame`], with the download type looked up in the published snapshot |
//!
//! Patching the serialised frame rather than the view costs one parse per frame per socket, which
//! is why it is confined to the three kinds that can carry a file name: `added`, `completed` and
//! `delta`. A `delta` frame is the only case where the object itself does not carry the item's
//! `selection`, so the download type is read from [`Published`].
//!
//! **On filling the field in the engine instead** (the WP-13/WP-14 proposal in
//! `docs/INTEGRATION-NOTES.md`): an additive formatter passed to `Engine::new` would fill
//! `ViewExtras.download_url` once, at the source, and this module's frame patching could go. The
//! wave-2 integration pass **kept the patching**, because it is unconditionally correct — this
//! crate always holds the `Config` — whereas an engine-side formatter is wiring that a build can
//! forget, and forgetting it is silent: every `download_url` on every surface goes `null`. The
//! [`Published`] lookup below is the only part that could have been a real gap, and it is not:
//! the aggregator emits a `delta` only for an id it has already sent a full object for, and
//! publishing a new item always marks the state dirty, so the id is in `Published` by then. The
//! `DownloadType::Video` fallbacks are therefore unreachable defensive defaults, in the same
//! sense as `SkipReason::NotCancelable`.

use std::sync::Arc;

use aulos_core::{Config, DownloadType, FileRef, ItemView};
use aulos_queue::{FrameKind, Published};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::Value;

/// Everything that must be escaped inside one path segment.
///
/// `/` is **not** in the set: it is handled by splitting on it, so `"Lo-fi beats/Episode 12.mp4"`
/// becomes `"Lo-fi%20beats/Episode%2012.mp4"` and stays a path rather than one escaped blob.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Percent-encodes a relative path, segment by segment.
#[must_use]
pub fn encode_path(relative: &str) -> String {
    relative
        .split('/')
        .map(|segment| utf8_percent_encode(segment, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// The ready-to-open URL for one produced file.
///
/// The result is relative to `<p>` for the stock configuration (`"download/My%20Video.mp4"`) and
/// absolute when the operator points `PUBLIC_HOST_URL` at a CDN — the two cases PROTOCOL §2.3
/// tells a client to distinguish by looking for a scheme.
#[must_use]
pub fn public_url(cfg: &Config, filename: &str, download_type: DownloadType) -> Arc<str> {
    let prefix = cfg.public_host_prefix(download_type);
    Arc::from(format!("{prefix}{}", encode_path(filename)).as_str())
}

/// One [`FileRef`] with its `download_url` filled in.
fn project_file(cfg: &Config, file: &FileRef, download_type: DownloadType) -> FileRef {
    FileRef {
        filename: Arc::clone(&file.filename),
        size: file.size,
        download_url: Some(public_url(cfg, &file.filename, download_type)),
        lang: file.lang.clone(),
    }
}

/// The view a client sees: the engine's view plus `download_url`, on the item and on both artifact
/// lists.
///
/// Returns the input `Arc` untouched when there is nothing to fill, so serving 500 queued items
/// allocates nothing.
#[must_use]
pub fn project(cfg: &Config, view: &Arc<ItemView>) -> Arc<ItemView> {
    let has_files = !view.chapter_files.is_empty() || !view.subtitle_files.is_empty();
    if view.filename.is_none() && !has_files {
        return Arc::clone(view);
    }
    let dt = view.selection.download_type;
    let mut out = ItemView::clone(view);
    out.download_url = view.filename.as_ref().map(|name| public_url(cfg, name, dt));
    if has_files {
        out.chapter_files = view
            .chapter_files
            .iter()
            .map(|f| project_file(cfg, f, dt))
            .collect();
        out.subtitle_files = view
            .subtitle_files
            .iter()
            .map(|f| project_file(cfg, f, dt))
            .collect();
    }
    Arc::new(out)
}

/// [`project`] over a sequence.
pub fn project_all<'a>(
    cfg: &Config,
    views: impl IntoIterator<Item = &'a Arc<ItemView>>,
) -> Vec<Arc<ItemView>> {
    views.into_iter().map(|v| project(cfg, v)).collect()
}

/// Fills `download_url` on one **full** item object, in place.
///
/// The object carries its own `selection.download_type`, so no lookup is needed.
pub fn patch_item_json(cfg: &Config, item: &mut Value) {
    let dt = item
        .get("selection")
        .and_then(|s| s.get("download_type"))
        .and_then(Value::as_str)
        .and_then(DownloadType::from_str_exact)
        .unwrap_or(DownloadType::Video);
    patch_item_json_with(cfg, item, dt);
}

/// Fills `download_url` on one item object whose download type the caller has resolved.
fn patch_item_json_with(cfg: &Config, item: &mut Value, download_type: DownloadType) {
    let Some(object) = item.as_object_mut() else {
        return;
    };
    if let Some(name) = object.get("filename").and_then(Value::as_str) {
        let url = public_url(cfg, name, download_type);
        object.insert("download_url".to_owned(), Value::String(url.to_string()));
    }
    for list in ["chapter_files", "subtitle_files"] {
        if let Some(files) = object.get_mut(list).and_then(Value::as_array_mut) {
            for file in files.iter_mut() {
                let Some(entry) = file.as_object_mut() else {
                    continue;
                };
                if let Some(name) = entry.get("filename").and_then(Value::as_str) {
                    let url = public_url(cfg, name, download_type);
                    entry.insert("download_url".to_owned(), Value::String(url.to_string()));
                }
            }
        }
    }
}

/// Fills `download_url` on one `delta` patch, looking the download type up by id.
fn patch_delta_json(cfg: &Config, published: &Published, patch: &mut Value) {
    let carries_file = patch.get("filename").is_some_and(Value::is_string)
        || patch.get("chapter_files").is_some()
        || patch.get("subtitle_files").is_some();
    if !carries_file {
        return;
    }
    // Unreachable in practice: a `delta` is only emitted for an id whose full object has already
    // gone out, and that publish marked the state dirty, so `published` holds it. See the module
    // docs for why this is a defensive default rather than a fallback.
    let download_type = patch
        .get("id")
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse().ok())
        .and_then(|id| published.get(id).map(|v| v.selection.download_type))
        .unwrap_or(DownloadType::Video);
    patch_item_json_with(cfg, patch, download_type);
}

/// The frame a socket should send: the hub's serialised text, with `download_url` filled in.
///
/// `None` means "send the frame unchanged" — the common case, since only three of the fourteen
/// frame kinds can carry a file name and most of those carry none.
#[must_use]
pub fn patch_frame(
    cfg: &Config,
    published: &Published,
    kind: FrameKind,
    text: &str,
) -> Option<String> {
    if !matches!(
        kind,
        FrameKind::Added | FrameKind::Completed | FrameKind::Delta
    ) {
        return None;
    }
    if !text.contains("\"filename\":\"") {
        return None; // nothing in the frame has a file, so nothing to fill.
    }
    let mut value: Value = serde_json::from_str(text).ok()?;
    let items = value.get_mut("items")?.as_array_mut()?;
    for item in items.iter_mut() {
        if kind == FrameKind::Delta {
            patch_delta_json(cfg, published, item);
        } else {
            patch_item_json(cfg, item);
        }
    }
    serde_json::to_string(&value).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use aulos_core::config::{RawEnv, load};

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        load(&RawEnv::from_pairs(pairs.to_vec())).unwrap()
    }

    #[test]
    fn a_path_is_encoded_segment_by_segment() {
        assert_eq!(
            encode_path("Lo-fi beats/Episode 12.mp4"),
            "Lo-fi%20beats/Episode%2012.mp4"
        );
        assert_eq!(encode_path("100% done.mp4"), "100%25%20done.mp4");
        assert_eq!(encode_path("Ω.mp4"), "%CE%A9.mp4");
    }

    #[test]
    fn the_two_roots_come_from_the_two_settings() {
        let c = cfg(&[]);
        assert_eq!(
            &*public_url(&c, "a b.mp4", DownloadType::Video),
            "download/a%20b.mp4"
        );
        assert_eq!(
            &*public_url(&c, "a b.m4a", DownloadType::Audio),
            "audio_download/a%20b.m4a"
        );
        assert_eq!(
            &*public_url(&c, "a.srt", DownloadType::Captions),
            "download/a.srt"
        );
    }

    #[test]
    fn a_cdn_prefix_produces_an_absolute_url() {
        let c = cfg(&[("PUBLIC_HOST_URL", "https://cdn.example")]);
        let url = public_url(&c, "My Video.mp4", DownloadType::Video);
        assert_eq!(&*url, "https://cdn.example/My%20Video.mp4");
        assert!(url::Url::parse(&url).is_ok(), "absolute, so opened as-is");
    }

    #[test]
    fn patching_a_frame_with_no_file_is_a_no_op() {
        let c = cfg(&[]);
        let published = Published::empty(aulos_core::BootId::new());
        assert!(
            patch_frame(
                &c,
                &published,
                FrameKind::Delta,
                r#"{"t":"delta","seq":1,"items":[{"id":"x","percent":1.0}]}"#
            )
            .is_none()
        );
        assert!(
            patch_frame(&c, &published, FrameKind::Notice, r#"{"filename":"a"}"#).is_none(),
            "a kind that cannot carry a file is never parsed"
        );
    }

    #[test]
    fn patching_an_added_frame_fills_every_file() {
        let c = cfg(&[]);
        let published = Published::empty(aulos_core::BootId::new());
        let text = r#"{"t":"completed","seq":9,"items":[{
            "id":"01JBQ7Z5T9K3M2R8V4XW6Y0AAA","filename":"A b.mp4","download_url":null,
            "selection":{"download_type":"video","codec":"auto","format":"mp4","quality":"best"},
            "chapter_files":[],
            "subtitle_files":[{"filename":"A b.en.srt","size":1,"download_url":null,"lang":"en"}]
        }]}"#;
        let patched = patch_frame(&c, &published, FrameKind::Completed, text).unwrap();
        let value: Value = serde_json::from_str(&patched).unwrap();
        let item = &value["items"][0];
        assert_eq!(item["download_url"], "download/A%20b.mp4");
        assert_eq!(
            item["subtitle_files"][0]["download_url"],
            "download/A%20b.en.srt"
        );
    }
}
