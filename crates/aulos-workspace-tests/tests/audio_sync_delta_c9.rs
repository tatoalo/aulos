//! Δ C9: the audio-sync fix moved out of yt-dlp and into a hook — both halves, in one place.
//!
//! DESIGN §9.8's deliberate behaviour change is that `get_opts` no longer emits legacy's
//! `Exec` postprocessor (the `ffmpeg -c:v copy -c:a aac …` re-encode) for a
//! `{video, mp4, best_remux}` selection. WP-06 removed it; WP-11's `audio_sync` hook is the
//! replacement. Each package tested its own half:
//!
//! - `aulos-provider-ytdlp`'s `tests/golden_opts.rs` asserts the `Exec` entry is **gone** and that
//!   it is the only difference from the captured Python output;
//! - `aulos-hooks`' `tests/audio_sync.rs` asserts the hook runs and what it execs.
//!
//! Nothing asserted the two halves describe the **same selection** — and if they ever disagree,
//! that selection silently loses the A/V-desync fix it had in legacy, with no test going red.
//! Neither crate can own this: `aulos-hooks` may not depend on a provider crate, and
//! `aulos-provider-ytdlp` must not depend on `aulos-hooks`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic is the reporting mechanism in a test"
)]

use std::sync::Arc;

use aulos_core::clock::DEFAULT_FAKE_EPOCH_MS;
use aulos_core::id::ItemId;
use aulos_core::item::{Item, ItemView, Kind, ViewExtras};
use aulos_core::paths::RelPath;
use aulos_core::request::{DownloadRequest, SubtitleLang, SubtitleMode};
use aulos_core::selection::{Codec, DownloadType, FormatId, ProviderId, QualityId, Selection};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::status::{Status, TerminalStatus};
use aulos_hooks::{AudioSyncHook, Hook};
use aulos_provider_ytdlp::opts::{get_opts, legacy_audio_sync_exec};
use serde_json::{Map, Value};
use url::Url;

/// The one selection Δ C9 is about.
const DELTA_C9: (DownloadType, &str, &str) = (DownloadType::Video, "mp4", "best_remux");

/// A finished item with `selection` and a produced file.
fn view(dt: DownloadType, format: &str, quality: &str, filename: Option<&str>) -> Arc<ItemView> {
    let url = Url::parse("https://example.test/v").unwrap();
    let selection = Selection::new(
        dt,
        Codec::Auto,
        FormatId::parse(format).unwrap(),
        QualityId::parse(quality).unwrap(),
    );
    let item = Item {
        id: ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord: 1,
        url: url.clone(),
        canonical_key: "k".into(),
        provider: ProviderId::parse("ytdlp").ok(),
        media_id: None,
        title: "A video".into(),
        status: Status::Postprocessing,
        auto_start: true,
        msg: None,
        error: None,
        request: DownloadRequest::new(url, selection),
        entry: None,
        filename: filename.map(|f| RelPath::parse(f).unwrap()),
        size: None,
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: DEFAULT_FAKE_EPOCH_MS,
        started_at: None,
        finished_at: None,
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV2),
        children_total: None,
        clear_after: None,
    };
    Arc::new(ItemView::from_item(&item, None, &ViewExtras::default()))
}

/// `get_opts` for one selection, with legacy's defaults for everything else.
fn opts(dt: DownloadType, format: &str, quality: &str) -> Map<String, Value> {
    get_opts(
        dt,
        format,
        quality,
        Map::new(),
        &SubtitleLang::english(),
        SubtitleMode::PreferManual,
    )
}

/// Whether `opts` carries legacy's audio-sync `Exec` postprocessor.
fn emits_legacy_exec(opts: &Map<String, Value>) -> bool {
    opts.get("postprocessors")
        .and_then(Value::as_array)
        .is_some_and(|list| list.contains(&legacy_audio_sync_exec()))
}

#[test]
fn the_selection_ytdlp_stopped_re_encoding_is_exactly_the_one_the_hook_picks_up() {
    let (dt, format, quality) = DELTA_C9;
    let hook = AudioSyncHook::new();

    // Half one: yt-dlp no longer does it.
    assert!(
        !emits_legacy_exec(&opts(dt, format, quality)),
        "Δ C9: get_opts must not emit the legacy audio-sync Exec for {dt:?}/{format}/{quality}"
    );
    // Half two: the hook does, for that same selection.
    assert!(
        hook.applies(
            &view(dt, format, quality, Some("A video.mp4")),
            TerminalStatus::Finished
        ),
        "Δ C9: the audio_sync hook must claim {dt:?}/{format}/{quality} — otherwise that \
         selection loses the A/V-desync fix it had in legacy"
    );
}

#[test]
fn no_other_selection_is_left_without_a_re_encoder() {
    // The invariant, over **every** `(download_type, format, quality)` the `ytdlp` catalog admits:
    // the postprocessor is gone everywhere, and exactly one selection routes through the hook.
    // Both sides false is the normal case (nobody re-encodes); the regression this file exists to
    // catch is `best_remux` ending up false on both.
    let hook = AudioSyncHook::new();
    let mut claimed = Vec::new();
    let mut seen = 0_usize;
    for flat in aulos_core::YTDLP_CATALOG.flat_formats() {
        let dt =
            DownloadType::from_str_exact(&flat.download_type).expect("a catalog download type");
        for q in &flat.qualities {
            seen += 1;
            assert!(
                !emits_legacy_exec(&opts(dt, &flat.id, &q.id)),
                "Δ C9 is unconditional: {dt:?}/{}/{} still emits the Exec",
                flat.id,
                q.id
            );
            if hook.applies(
                &view(dt, &flat.id, &q.id, Some("A video.mp4")),
                TerminalStatus::Finished,
            ) {
                claimed.push((dt, flat.id.to_string(), q.id.to_string()));
            }
        }
    }
    assert!(
        seen > 20,
        "the catalog sweep must be a real sweep, saw {seen}"
    );
    let (dt, format, quality) = DELTA_C9;
    assert_eq!(
        claimed,
        vec![(dt, format.to_owned(), quality.to_owned())],
        "exactly one selection may route through the audio_sync hook"
    );
}

#[test]
fn a_failed_or_fileless_job_is_never_re_encoded() {
    // The hook reads the *prospective* outcome, and a failed download can leave a partial `.mp4`
    // on disk. Re-encoding that would publish a broken file as finished.
    let (dt, format, quality) = DELTA_C9;
    let hook = AudioSyncHook::new();
    for outcome in [TerminalStatus::Error, TerminalStatus::Canceled] {
        assert!(
            !hook.applies(&view(dt, format, quality, Some("A video.mp4")), outcome),
            "{outcome:?} must not re-encode"
        );
    }
    assert!(
        !hook.applies(&view(dt, format, quality, None), TerminalStatus::Finished),
        "an item with no produced file has nothing to re-encode"
    );
}
