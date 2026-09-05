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
use aulos_hooks::nfo::{self, NfoHook, Source};
use aulos_hooks::{HookRunner, HookStore};
use common::{Call, FakeStore, ItemBuilder, config, config_rooted, sink};

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
    let xml =
        nfo::render(&view, &fixture("sc_movie_state.json"), Source::Entry, NOW).expect("render");
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
    let xml =
        nfo::render(&view, &fixture("sc_episode_state.json"), Source::Entry, NOW).expect("render");
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
    let xml = nfo::render(
        &view,
        &fixture("sc_imported_episode.json"),
        Source::Entry,
        NOW,
    )
    .expect("render");
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
    let xml = nfo::render(
        &view,
        &fixture("sc_entry_final_movie.json"),
        Source::Entry,
        NOW,
    )
    .expect("render");
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
    let xml = nfo::render(&view, &blob, Source::Entry, NOW).expect("render");
    assert!(
        xml.contains("<uniqueid type=\"youtube\">dQw4w9WgXcQ</uniqueid>"),
        "{xml}"
    );
    assert!(
        xml.starts_with("<?xml version=\"1.0\" ?>\n<movie>"),
        "{xml}"
    );
}

/// A stored entry that carries almost nothing still renders from the row, because the row is the
/// only thing a StreamingCommunity `state` has to offer for a title and a url. This is the one
/// source that may do that; `tests/nfo_legacy_parity.rs` pins the sidecar's stricter rules.
#[test]
fn a_thin_entry_falls_back_to_the_row() {
    let view = ItemBuilder::finished("Titolo dalla riga").view();
    let blob = EntryBlob::new(serde_json::json!({ "title_id": 77 }));
    let xml = nfo::render(&view, &blob, Source::Entry, NOW).expect("render");
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
            "uniqueid",
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

/// The regression this hook exists for: it applies to a finished item of **any** provider, not
/// only StreamingCommunity, because legacy wrote an NFO for every finished download.
#[test]
fn applies_to_a_finished_item_of_every_provider() {
    let hook = NfoHook::new();
    let sc = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .view();
    assert!(hook.applies(&sc, TerminalStatus::Finished));
    assert!(!hook.applies(&sc, TerminalStatus::Error));
    assert!(!hook.applies(&sc, TerminalStatus::Canceled));

    let ytdlp = ItemBuilder::finished("Clip").provider("ytdlp").view();
    assert!(
        hook.applies(&ytdlp, TerminalStatus::Finished),
        "a YouTube download gets an NFO too"
    );

    let plugin = ItemBuilder::finished("Clip").provider("command:x").view();
    assert!(hook.applies(&plugin, TerminalStatus::Finished));

    let no_file = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .no_file()
        .view();
    assert!(!hook.applies(&no_file, TerminalStatus::Finished));
}

/// Every gate names itself, so a skipped hook is never mistaken for an idle one.
#[test]
fn every_gate_names_itself_as_a_skip_reason() {
    let hook = NfoHook::new();
    let view = ItemBuilder::finished("Clip").provider("ytdlp").view();
    assert_eq!(hook.skip_reason(&view, TerminalStatus::Finished), None);
    assert_eq!(
        hook.skip_reason(&view, TerminalStatus::Error)
            .map(|r| r.as_str().to_owned()),
        Some("the outcome is error, not finished".to_owned())
    );

    let no_file = ItemBuilder::finished("Clip").no_file().view();
    assert_eq!(
        hook.skip_reason(&no_file, TerminalStatus::Finished)
            .map(|r| r.as_str().to_owned()),
        Some("the item produced no file".to_owned())
    );

    let cfg = config(&[("AULOS_NFO_ENABLED", "false")]);
    assert_eq!(
        NfoHook::from_config(&cfg)
            .skip_reason(&view, TerminalStatus::Finished)
            .map(|r| r.as_str().to_owned()),
        Some("AULOS_NFO_ENABLED is false".to_owned())
    );
}

/// `AULOS_NFO_PROVIDERS` restores the old narrowing for anyone who wants it, and says so when it
/// gates an item out.
#[test]
fn the_provider_allow_list_is_opt_in_and_explains_itself() {
    let cfg = config(&[("AULOS_NFO_PROVIDERS", "streamingcommunity")]);
    let hook = NfoHook::from_config(&cfg);
    let sc = ItemBuilder::finished("Clip")
        .provider("streamingcommunity")
        .view();
    let ytdlp = ItemBuilder::finished("Clip").provider("ytdlp").view();
    assert!(hook.applies(&sc, TerminalStatus::Finished));
    assert!(!hook.applies(&ytdlp, TerminalStatus::Finished));
    assert_eq!(
        hook.skip_reason(&ytdlp, TerminalStatus::Finished)
            .map(|r| r.as_str().to_owned()),
        Some("AULOS_NFO_PROVIDERS does not list ytdlp".to_owned())
    );

    // Several ids, and whitespace around them, are accepted.
    let two = NfoHook::from_config(&config(&[(
        "AULOS_NFO_PROVIDERS",
        "streamingcommunity, ytdlp",
    )]));
    assert!(two.applies(&sc, TerminalStatus::Finished));
    assert!(two.applies(&ytdlp, TerminalStatus::Finished));

    // The default admits everything.
    assert!(NfoHook::from_config(&config(&[])).applies(&ytdlp, TerminalStatus::Finished));
}

/// A yt-dlp item has no entry blob by the time hooks run (DESIGN §7.5 drops it at the terminal
/// write), so the `.info.json` legacy read is the source, and the NFO lands next to the file.
#[tokio::test]
async fn a_ytdlp_item_renders_from_the_info_json_sidecar() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Le incredibili elezioni del 2000")
        .provider("ytdlp")
        .filename("Le incredibili elezioni del 2000 [8Xrcn5B04u4].mp4");
    let view = item.view();

    std::fs::write(
        dir.path()
            .join("Le incredibili elezioni del 2000 [8Xrcn5B04u4].mp4"),
        b"video",
    )
    .expect("media");
    std::fs::write(
        dir.path()
            .join("Le incredibili elezioni del 2000 [8Xrcn5B04u4].info.json"),
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/youtube_info.json"),
        )
        .expect("fixture"),
    )
    .expect("sidecar");

    // No blob at all, which is what the engine leaves behind for a plain yt-dlp row.
    let store = FakeStore::new();
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

    let written = std::fs::read_to_string(
        dir.path()
            .join("Le incredibili elezioni del 2000 [8Xrcn5B04u4].nfo"),
    )
    .expect("the nfo exists");
    assert!(
        written.contains("<uniqueid type=\"youtube\">8Xrcn5B04u4</uniqueid>"),
        "{written}"
    );
    assert!(
        written.contains("<studio>Il Post</studio>"),
        "the uploader becomes the studio: {written}"
    );
    assert!(
        written.contains("<premiered>2026-08-30</premiered>"),
        "{written}"
    );
    assert!(
        dir.path()
            .join("Le incredibili elezioni del 2000 [8Xrcn5B04u4].info.json")
            .exists(),
        "the sidecar survives by default"
    );
    assert!(
        store.writes().is_empty(),
        "a row with no blob is not charged an engine round trip: {:?}",
        store.writes()
    );
}

/// A blob that carries no element — a yt-dlp playlist child keeps only its `outtmpl` hints
/// (DESIGN §7.5) — must not shadow a sidecar that carries the whole info dict.
#[tokio::test]
async fn a_hints_only_blob_does_not_shadow_the_sidecar() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Clip")
        .provider("ytdlp")
        .filename("Clip.mp4");
    let id = item.id();
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");
    std::fs::write(
        dir.path().join("Clip.info.json"),
        br#"{"id":"abc","title":"Dal sidecar","extractor":"youtube"}"#,
    )
    .expect("sidecar");

    // Every key DESIGN §7.5 keeps for a channel download, `channel` included — it renders
    // `<studio>`, so it looks like metadata unless the source rule knows it is a hint.
    let hints = EntryBlob::new(serde_json::json!({
        "hints": { "playlist_index": 3 },
        "state": {
            "playlist": "Uploads",
            "playlist_id": "UU1",
            "playlist_index": 3,
            "channel": "Il Post",
            "channel_id": "UC1",
            "n_entries": 9,
            "__last_playlist_index": 9,
        },
    }));
    let store = FakeStore::with_blob(id, hints);
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
    let written = std::fs::read_to_string(dir.path().join("Clip.nfo")).expect("the nfo exists");
    assert!(written.contains("<title>Dal sidecar</title>"), "{written}");
    assert!(
        written.contains("<uniqueid type=\"youtube\">abc</uniqueid>"),
        "{written}"
    );
    assert!(
        !written.contains("Il Post"),
        "a hint is not metadata, so it does not render either: {written}"
    );
}

/// A StreamingCommunity blob still wins over any sidecar: it is the richer source and the sidecar
/// is not written for that provider at all.
#[tokio::test]
async fn a_streamingcommunity_blob_wins_over_a_sidecar() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Il Grande Film")
        .provider("streamingcommunity")
        .filename("Clip.mp4");
    let id = item.id();
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");
    std::fs::write(
        dir.path().join("Clip.info.json"),
        br#"{"id":"wrong","title":"Non usare questo","extractor":"youtube"}"#,
    )
    .expect("sidecar");

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
    let written = std::fs::read_to_string(dir.path().join("Clip.nfo")).expect("the nfo exists");
    assert!(
        written.contains("<uniqueid type=\"streamingcommunity\">sc_1234</uniqueid>"),
        "{written}"
    );
    assert!(!written.contains("Non usare questo"), "{written}");
}

/// A sidecar that is not JSON is not a hook failure — and not a file either. Legacy logged the
/// `JSONDecodeError` and wrote nothing; writing a stub instead would hand Jellyfin a document
/// whose only true statement is the title.
#[tokio::test]
async fn a_broken_sidecar_is_not_a_hook_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Titolo dalla riga")
        .provider("ytdlp")
        .filename("Clip.mp4");
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");
    std::fs::write(dir.path().join("Clip.info.json"), b"{not json").expect("sidecar");

    let store = FakeStore::new();
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
        .expect("a broken sidecar is not an error");
    assert!(
        !dir.path().join("Clip.nfo").exists(),
        "unreadable metadata writes no file"
    );
}

/// The stock install the bug report describes: `AULOS_NFO_ENABLED` defaults to `true`, but nothing
/// forces `writeinfojson`, so most downloads finish with no blob and no sidecar. Legacy wrote no
/// file in that case (`generate_nfo` returns before opening the output), and neither does this —
/// a `movie/title/plot` stub next to every media file is metadata Jellyfin would adopt.
#[tokio::test]
async fn nothing_to_render_from_writes_no_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Titolo dalla riga")
        .provider("ytdlp")
        .filename("Clip.mp4");
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");

    let store = FakeStore::new();
    let (factory, _rx) = sink();
    let runner = HookRunner::new(
        Arc::clone(&cfg),
        Arc::new(FakeClock::default()),
        Arc::clone(&store) as Arc<dyn HookStore>,
        factory,
    );
    let hook = NfoHook::new();
    assert!(
        !hook.health().detail.contains_key("wrote_nothing_total"),
        "a counter at zero stays off the payload"
    );
    runner
        .run(
            &hook,
            &view,
            &[BatchEntry::from_view(&view, TerminalStatus::Finished)],
        )
        .await
        .expect("having nothing to write is not a failure");
    // A run that writes nothing is not a skip — the hook did apply — so it needs its own line in
    // `healthz`, or `runs_total: 1` says a file was written when none was.
    assert_eq!(hook.wrote_nothing_total(), 1);
    assert_eq!(hook.health().detail["wrote_nothing_total"], 1);
    assert!(
        !dir.path().join("Clip.nfo").exists(),
        "no metadata, no file: {:?}",
        std::fs::read_dir(dir.path()).map(|d| d
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect::<Vec<_>>())
    );
    assert!(store.writes().is_empty(), "{:?}", store.writes());
}

/// The migration case the same report describes: a user whose `YTDL_OPTIONS` still runs
/// `jellyfin_nfo_generator.py` as an `Exec` postprocessor with `"when": "after_move"`. The script
/// writes its `.nfo` and then deletes the sidecar it read, so the hook arrives with no metadata at
/// all — and must leave the file that is already there alone rather than truncating it with a stub.
#[tokio::test]
async fn an_nfo_written_by_someone_else_is_not_overwritten_with_a_stub() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config_rooted(dir.path(), &[]);
    let item = ItemBuilder::finished("Titolo dalla riga")
        .provider("ytdlp")
        .filename("Clip.mp4");
    let view = item.view();
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");
    let theirs =
        "<?xml version=\"1.0\" ?>\n<movie>\n  <title>Scritto dallo script</title>\n</movie>";
    std::fs::write(dir.path().join("Clip.nfo"), theirs).expect("their nfo");

    let store = FakeStore::new();
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
        .expect("the hook is not a failure");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("Clip.nfo")).expect("still there"),
        theirs,
        "someone else's NFO survives a hook that has nothing to say"
    );
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
    // The engine is down for every call, the blob read included, so the sidecar is what the
    // document is rendered from — and it is on disk before `drop_entry_blob` is ever attempted.
    std::fs::write(
        dir.path().join("Clip.info.json"),
        br#"{"id":"sc_1234","title":"Clip","extractor":"streamingcommunity"}"#,
    )
    .expect("sidecar");

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
