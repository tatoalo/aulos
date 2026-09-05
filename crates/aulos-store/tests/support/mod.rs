//! Shared fixtures for the store's integration tests.
//!
//! Everything is a temporary file on the local disk, the flush window is five milliseconds so a
//! test that *does* want a batch to fill does not wait 200 ms for it, and nothing touches the
//! network. (A lone batched write no longer waits out the window anywhere — the writer commits on
//! the idle grace, DESIGN §7.1 — so the short window is a convenience, not a workaround.)
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use aulos_core::{
    Codec, DownloadRequest, DownloadType, FileRef, FormatId, Item, ItemId, Kind, Ord0, QualityId,
    Selection, SourceKind, SourceRef, Status,
};
use aulos_store::{Store, StoreOptions};
use tempfile::TempDir;

/// A temporary directory plus the store opened inside it.
pub struct Harness {
    pub dir: TempDir,
    pub store: Store,
}

/// Options over `<dir>/aulos.db` with a test-sized flush window.
pub fn options(dir: &Path) -> StoreOptions {
    StoreOptions::new(dir.join("aulos.db"))
        .with_flush_ms(5)
        .with_readers(2)
        .with_busy_timeout_ms(200)
}

/// A fresh store over a fresh temporary directory.
pub fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(options(dir.path())).unwrap();
    Harness { dir, store }
}

/// The database file inside a harness directory.
pub fn db_path(dir: &Path) -> PathBuf {
    dir.join("aulos.db")
}

/// The selection every fixture uses: `video / auto / mp4 / best`.
pub fn selection() -> Selection {
    Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").unwrap(),
        QualityId::parse("best").unwrap(),
    )
}

/// A fully-populated item, so a round-trip test actually exercises every column.
pub fn item(ord: Ord0) -> Item {
    let url = url::Url::parse(&format!("https://example.com/watch?v={ord}")).unwrap();
    let mut request = DownloadRequest::new(url.clone(), selection());
    request.custom_name_prefix = "prefix".into();
    request.playlist_item_limit = 7;
    request.chapter_template = "chapter-%(section_number)s".into();
    request
        .ytdl_options_overrides
        .insert("retries".to_owned(), serde_json::json!(3));
    request.ytdl_options_presets = vec!["fast".into()];

    Item {
        id: ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord,
        url,
        canonical_key: format!("ytdlp:v{ord}").into(),
        provider: None,
        media_id: None,
        title: format!("Item {ord}").into(),
        status: Status::Queued,
        auto_start: true,
        msg: None,
        error: None,
        request,
        entry: None,
        filename: None,
        size: None,
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 1_700_000_000_000 + ord,
        started_at: None,
        finished_at: None,
        attempt: 0,
        source: SourceRef::with_ref(SourceKind::ApiV2, "req-1"),
        children_total: None,
        clear_after: None,
    }
}

/// An auxiliary produced file.
pub fn file_ref(name: &str, lang: Option<&str>) -> FileRef {
    FileRef {
        filename: name.into(),
        size: Some(1_234),
        download_url: Some(format!("download/{name}").into()),
        lang: lang.map(std::convert::Into::into),
    }
}
