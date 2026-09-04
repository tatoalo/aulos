//! The NFO writer against captured StreamingCommunity entries (DESIGN §13.2).
//!
//! The fixtures in `tests/fixtures` are the three blob shapes `entry_json` actually holds — a
//! freshly resolved v2 `state`, an imported legacy row with its metadata under `state.legacy`, and
//! the flat `.info.json` shape a finished download reports as `Outcome::entry_final` — because a
//! renderer that only handles one of them writes an empty NFO for the other two.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use aulos_core::clock::FakeClock;
use aulos_core::item::EntryBlob;
use aulos_core::status::TerminalStatus;
use aulos_hooks::hook::{BatchEntry, Hook};
use aulos_hooks::nfo::{self, NfoHook};
use aulos_hooks::{HookRunner, HookStore};
use common::{Call, FakeStore, ItemBuilder, config_rooted, sink};

/// Loads a checked-in blob fixture.
fn fixture(name: &str) -> EntryBlob {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    EntryBlob::new(serde_json::from_str(&text).expect("the fixture must be JSON"))
}

/// The element names of a rendered document, in order.
fn elements(xml: &str) -> Vec<String> {
    xml.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix('<')?;
            if rest.starts_with('?') || rest.starts_with('/') {
                return None;
            }
            let name: String = rest
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

const NOW: i64 = 1_788_480_000_000; // 2026-09-04T00:00:00Z

#[test]
fn a_freshly_resolved_movie_renders() {
    let view = ItemBuilder::finished("Il Grande Film")
        .provider("streamingcommunity")
        .view();
    let xml = nfo::render(&view, Some(&fixture("sc_movie_state.json")), NOW).expect("render");
    insta::assert_snapshot!("movie_state", xml);
    assert_eq!(
        elements(&xml),
        [
            "movie",
            "title",
            "originaltitle",
            "plot",
            "dateadded",
            "uniqueid",
            "website"
        ]
    );
}

#[test]
fn a_freshly_resolved_episode_renders_as_episodedetails() {
    let view = ItemBuilder::finished("Mare Fuori S01E02 - Il segreto di Napoli")
        .provider("streamingcommunity")
        .view();
    let xml = nfo::render(&view, Some(&fixture("sc_episode_state.json")), NOW).expect("render");
    insta::assert_snapshot!("episode_state", xml);
    assert_eq!(
        elements(&xml),
        [
            "episodedetails",
            "title",
            "originaltitle",
            "showtitle",
            "season",
            "episode",
            "subtitle",
            "plot",
            "dateadded",
            "uniqueid",
            "website"
        ],
        "the legacy element order, episode block included"
    );
}

/// PLAN WP-11: "It must render correctly from an **imported** SC blob (the `state` shape of
/// DESIGN §10.3, with metadata under `state.legacy`) as well as from a freshly resolved one."
#[test]
fn an_imported_episode_keeps_every_legacy_element() {
    let view = ItemBuilder::finished("Mare Fuori S01E02 - Il segreto di Napoli")
        .provider("streamingcommunity")
        .view();
    let xml = nfo::render(&view, Some(&fixture("sc_imported_episode.json")), NOW).expect("render");
    insta::assert_snapshot!("episode_imported", xml);
    assert_eq!(
        elements(&xml),
        [
            "episodedetails",
            "title",
            "originaltitle",
            "showtitle",
            "season",
            "episode",
            "subtitle",
            "plot",
            "year",
            "premiered",
            "dateadded",
            "studio",
            "director",
            "uniqueid",
            "website",
            "tag",
            "tag",
            "tag",
            "runtime"
        ],
        "the full legacy order, with the empty tag skipped"
    );
    assert!(
        xml.contains("<plot>Scelte &amp; conseguenze &lt;difficili&gt;</plot>"),
        "text is escaped the way minidom escaped it: {xml}"
    );
    assert!(
        xml.contains("<website>https://streamingcommunity.test/it/watch/9?e=77</website>"),
        "original_url wins over webpage_url: {xml}"
    );
    assert!(xml.contains("<runtime>55</runtime>"), "{xml}");
}

/// The flat `Outcome::entry_final` shape a finished SC download reports (DESIGN §10.5).
#[test]
fn the_flat_entry_final_shape_renders_too() {
    let view = ItemBuilder::finished("Il Grande Film")
        .provider("streamingcommunity")
        .view();
    let xml = nfo::render(&view, Some(&fixture("sc_entry_final_movie.json")), NOW).expect("render");
    insta::assert_snapshot!("movie_entry_final", xml);
    assert!(
        xml.contains("<uniqueid type=\"streamingcommunity\">sc_1234</uniqueid>"),
        "{xml}"
    );
}

/// A yt-dlp entry gets the other `uniqueid` type, which is the only thing the extractor decides.
#[test]
fn a_youtube_entry_gets_the_youtube_uniqueid_type() {
    let view = ItemBuilder::finished("A clip").view();
    let blob = EntryBlob::new(serde_json::json!({
        "id": "dQw4w9WgXcQ",
        "extractor": "youtube",
        "title": "A clip",
        "webpage_url": "https://youtu.be/dQw4w9WgXcQ",
    }));
    let xml = nfo::render(&view, Some(&blob), NOW).expect("render");
    assert!(
        xml.contains("<uniqueid type=\"youtube\">dQw4w9WgXcQ</uniqueid>"),
        "{xml}"
    );
    assert!(
        xml.starts_with("<?xml version=\"1.0\" ?>\n<movie>"),
        "{xml}"
    );
}

/// With no blob at all the document is still valid, and the title comes from the row.
#[test]
fn an_absent_blob_still_produces_a_document() {
    let view = ItemBuilder::finished("Titolo dalla riga").view();
    let xml = nfo::render(&view, None, NOW).expect("render");
    assert!(xml.contains("<title>Titolo dalla riga</title>"), "{xml}");
    assert!(
        xml.contains("<plot/>"),
        "an empty plot is self-closing: {xml}"
    );
    assert_eq!(
        elements(&xml),
        [
            "movie",
            "title",
            "originaltitle",
            "plot",
            "dateadded",
            "website"
        ]
    );
}

/// The whole hook: the file is written next to the media file, the sidecar survives by default,
/// and the blob is dropped through the port afterwards.
#[tokio::test]
async fn the_hook_writes_the_file_keeps_the_sidecar_and_drops_the_blob() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Il Grande Film")
        .provider("streamingcommunity")
        .filename("Film/Il Grande Film.mp4");
    let id = item.id();
    let view = item.view();

    std::fs::create_dir_all(dir.path().join("Film")).expect("mkdir");
    std::fs::write(dir.path().join("Film/Il Grande Film.mp4"), b"video").expect("media");
    std::fs::write(dir.path().join("Film/Il Grande Film.info.json"), b"{}").expect("sidecar");

    let store = FakeStore::with_blob(id, fixture("sc_movie_state.json"));
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        Arc::clone(&cfg),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let hook = NfoHook::new();
    assert!(hook.applies(&view, TerminalStatus::Finished));
    assert!(hook.wants_entry());
    let batch = vec![BatchEntry::from_view(&view, TerminalStatus::Finished)];
    runner
        .run(&hook, &view, &batch)
        .await
        .expect("the hook writes");

    let nfo_file = dir.path().join("Film/Il Grande Film.nfo");
    let written = std::fs::read_to_string(&nfo_file).expect("the nfo exists");
    assert!(written.starts_with("<?xml version=\"1.0\" ?>"), "{written}");
    assert!(
        dir.path().join("Film/Il Grande Film.info.json").exists(),
        "the sidecar survives by default"
    );
    assert_eq!(
        store.writes(),
        [Call::DropEntryBlob(id)],
        "exactly the one write the hook is documented to make"
    );
    assert_eq!(store.calls().first(), Some(&Call::EntryBlob(id)));
}

/// `AULOS_NFO_DELETE_INFO_JSON=true` restores the legacy CLI's behaviour.
#[tokio::test]
async fn the_sidecar_is_deleted_when_the_knob_is_on() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[("AULOS_NFO_DELETE_INFO_JSON", "true")]);
    let item = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .filename("Clip.mp4");
    let id = item.id();
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");
    std::fs::write(dir.path().join("Clip.info.json"), b"{}").expect("sidecar");

    let store = FakeStore::with_blob(id, fixture("sc_movie_state.json"));
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        Arc::clone(&cfg),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    runner
        .run(
            &NfoHook::new(),
            &view,
            &[BatchEntry::from_view(&view, TerminalStatus::Finished)],
        )
        .await
        .expect("the hook writes");
    assert!(dir.path().join("Clip.nfo").exists());
    assert!(
        !dir.path().join("Clip.info.json").exists(),
        "and it is gone"
    );
}

/// `AULOS_NFO_ENABLED=false` makes it a no-op, and reports `disabled` rather than `ok`.
#[tokio::test]
async fn the_hook_can_be_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[("AULOS_NFO_ENABLED", "false")]);
    let item = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .filename("Clip.mp4");
    let id = item.id();
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");

    let hook = NfoHook::from_config(&cfg);
    assert!(!hook.applies(&view, TerminalStatus::Finished));
    assert_eq!(
        hook.health().status,
        aulos_core::health::ComponentStatus::Disabled
    );

    let store = FakeStore::with_blob(id, fixture("sc_movie_state.json"));
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        Arc::clone(&cfg),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    runner
        .run(
            &hook,
            &view,
            &[BatchEntry::from_view(&view, TerminalStatus::Finished)],
        )
        .await
        .expect("a disabled hook is a no-op, not an error");
    assert!(!dir.path().join("Clip.nfo").exists());
    assert!(store.writes().is_empty(), "and it wrote nothing");
}

/// `applies` is narrow on purpose: only a finished StreamingCommunity item with a file.
#[test]
fn applies_is_limited_to_finished_streamingcommunity_items() {
    let hook = NfoHook::new();
    let sc = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .view();
    assert!(hook.applies(&sc, TerminalStatus::Finished));
    assert!(!hook.applies(&sc, TerminalStatus::Error));
    assert!(!hook.applies(&sc, TerminalStatus::Canceled));

    let ytdlp = ItemBuilder::finished("Clip").view();
    assert!(!hook.applies(&ytdlp, TerminalStatus::Finished));

    let no_file = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .no_file()
        .view();
    assert!(!hook.applies(&no_file, TerminalStatus::Finished));
}

/// A store that refuses the blob drop is a hook failure, not a lost NFO: the file is already on
/// disk when the port is called.
#[tokio::test]
async fn a_failing_port_does_not_lose_the_written_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .filename("Clip.mp4");
    let id = item.id();
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");

    let store = FakeStore::with_blob(id, fixture("sc_movie_state.json"));
    store.fail_with(aulos_core::ports::PortError::Unavailable);
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        Arc::clone(&cfg),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let e = runner
        .run(
            &NfoHook::new(),
            &view,
            &[BatchEntry::from_view(&view, TerminalStatus::Finished)],
        )
        .await
        .expect_err("the port failure surfaces");
    assert!(e.retryable(), "an unavailable engine is worth another try");
    assert!(
        dir.path().join("Clip.nfo").exists(),
        "and the NFO is on disk regardless"
    );
}
