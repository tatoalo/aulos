//! The legacy importer's acceptance suite (PLAN WP-05, DESIGN §7.6).
//!
//! Every test copies its fixture directory into a temporary one first, for two reasons: the
//! checked-in corpus must stay pristine, and the **T2** assertion — the importer never mutates a
//! legacy file — is then a byte-for-byte comparison against the original
//! ([`the_legacy_files_are_never_touched`]).
//!
//! The database deliberately lives **outside** the copied state directory, so "no new file appeared
//! in `STATE_DIR` besides `.aulos-imported`" is a real assertion and not an accounting exercise.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aulos_core::{DownloadType, Item, ProviderId, RelDir, RelPath, Status};
use aulos_store::import::{
    ImportErrorCode, ImportOpts, ImportReport, MARKER_FILE, OnError, WarningCode, import,
};
use aulos_store::{ItemFilter, Store};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A copied state directory plus a store over a database beside it (never inside it).
struct Rig {
    _tmp: TempDir,
    state: PathBuf,
    db: PathBuf,
    store: Store,
}

impl Rig {
    fn new(fixture: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        copy_dir(&fixtures().join(fixture), &state);
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db = db_dir.join("aulos.db");
        let store = Store::open(support::options(&db_dir)).unwrap();
        Self {
            _tmp: tmp,
            state,
            db,
            store,
        }
    }

    async fn run(&self, opts: ImportOpts) -> ImportReport {
        import(&self.state, &self.store, opts)
            .await
            .unwrap_or_else(|e| panic!("the import must succeed: {e}\n{}", e.report.render_table()))
    }

    async fn items(&self) -> Vec<Item> {
        self.store.items(ItemFilter::default()).await.unwrap().rows
    }

    fn marker(&self) -> PathBuf {
        self.state.join(MARKER_FILE)
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Every file in a directory, as `name → bytes`.
fn snapshot_dir(dir: &Path) -> HashMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap_or_default(),
            )
        })
        .collect()
}

fn opts() -> ImportOpts {
    ImportOpts::default()
}

fn by_url<'a>(items: &'a [Item], needle: &str) -> &'a Item {
    items
        .iter()
        .find(|i| i.url.as_str().contains(needle))
        .unwrap_or_else(|| panic!("no item for {needle} in {:?}", urls(items)))
}

fn urls(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.url.as_str()).collect()
}

// ---------------------------------------------------------------------------
// v2
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_v2_state_dir_imports_with_the_documented_ord_and_status_mapping() {
    let rig = Rig::new("v2");
    let report = rig.run(opts()).await;

    // `ord` is chronological, globally, across completed → pending → queue (DESIGN §7.6.2).
    let items = rig.items().await;
    assert_eq!(items.len(), 6, "{:?}", urls(&items));
    assert_eq!(
        urls(&items)
            .iter()
            .map(|u| u.rsplit('=').next().unwrap())
            .collect::<Vec<_>>(),
        [
            "d0nEV1de0",   // completed, oldest
            "cAnc3ll3d",   // completed, +0.5 s
            "f41l3dV1d",   // completed, +1 s
            "pEnD1ngV1d",  // pending,   +2 s
            "dQw4w9WgXcQ", // queue,     +3 s
            "aBcDeF12345", // queue,     +4 s
        ]
    );
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item.ord, i as i64, "ord must be 0,1,2,… ({})", item.url);
    }

    // The DESIGN §7.6.3 status table.
    let finished = by_url(&items, "d0nEV1de0");
    assert_eq!(finished.status, Status::Finished);
    assert_eq!(finished.media_id.as_deref(), Some("d0nEV1de0"));
    assert_eq!(
        finished.filename.as_ref().map(RelPath::as_str),
        Some("A finished video.mp4")
    );
    assert_eq!(finished.size, Some(104_857_600));
    assert_eq!(finished.chapter_files.len(), 2);
    assert_eq!(
        &*finished.chapter_files[0].filename,
        "A finished video - 01.mp4"
    );
    assert_eq!(finished.chapter_files[0].size, Some(52_428_800));
    assert_eq!(finished.finished_at, Some(1_757_000_000_000));
    assert_eq!(finished.clear_after, None, "CLEAR_COMPLETED_AFTER is 0");

    let errored = by_url(&items, "f41l3dV1d");
    assert_eq!(errored.status, Status::Error);
    let wire = errored.error.as_ref().expect("the error must survive");
    assert_eq!(
        &*wire.message, "Video unavailable",
        "the ERROR: prefix is stripped"
    );
    assert_eq!(wire.code, aulos_core::ErrorCode::Unavailable);

    let unknown = by_url(&items, "cAnc3ll3d");
    assert_eq!(unknown.status, Status::Error, "never guesses finished");
    assert_eq!(
        unknown.msg.as_deref(),
        Some("Imported with unknown legacy status: cancelled")
    );

    let pending = by_url(&items, "pEnD1ngV1d");
    assert_eq!(pending.status, Status::Queued);
    assert!(!pending.auto_start, "pending.json waits for the user");
    assert_eq!(pending.request.selection.download_type, DownloadType::Audio);

    let queued = by_url(&items, "dQw4w9WgXcQ");
    assert_eq!(queued.status, Status::Queued);
    assert!(queued.auto_start, "queue.json is scheduled");
    assert_eq!(queued.attempt, 0);

    let restarted = by_url(&items, "aBcDeF12345");
    assert_eq!(restarted.status, Status::Queued);
    assert!(restarted.auto_start);
    assert_eq!(restarted.attempt, 1, "the upgrade interrupted it");
    assert_eq!(restarted.msg.as_deref(), Some("Restarted after upgrade"));
    assert_eq!(
        restarted.request.folder.as_ref().map(RelDir::as_str),
        Some("Shows")
    );
    assert_eq!(
        restarted.request.ytdl_options_presets,
        vec!["archive".into()]
    );
    // Entry compaction (DESIGN §7.5): the playlist keys survive, the format list does not.
    let entry = restarted
        .entry
        .as_ref()
        .expect("a playlist child keeps keys");
    let obj = entry.as_value().as_object().unwrap();
    assert_eq!(obj.len(), 4, "{obj:?}");
    assert_eq!(obj["playlist"], "Mix");
    assert_eq!(obj["n_entries"], 12);
    assert!(!obj.contains_key("formats"));

    // The report.
    assert_eq!(report.items.get(&Status::Queued), Some(&3));
    assert_eq!(report.items.get(&Status::Finished), Some(&1));
    assert_eq!(report.items.get(&Status::Error), Some(&2));
    assert_eq!(report.items.get(&Status::Canceled), Some(&0));
    assert_eq!(report.errors, vec![]);
    assert_eq!(report.seen_ids_imported, 3);
    let queue_file = report
        .files
        .iter()
        .find(|f| &*f.file == "queue.json")
        .unwrap();
    assert_eq!(
        (
            queue_file.schema_version,
            queue_file.records,
            queue_file.imported,
            queue_file.skipped
        ),
        (Some(2), 2, 2, 0)
    );
    let tg = report
        .files
        .iter()
        .find(|f| &*f.file == "telegram_bot_config.json")
        .unwrap();
    assert_eq!(tg.schema_version, None, "the bare object has no envelope");
    assert_eq!((tg.records, tg.imported), (2, 2));

    // Subscriptions, seen ids and chats.
    let subs = rig.store.subscriptions().await.unwrap();
    assert_eq!(subs.len(), 2);
    let veritasium = subs
        .iter()
        .find(|s| s.id.as_str() == "9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f")
        .expect("the legacy UUID must survive verbatim");
    assert_eq!(veritasium.check_interval_minutes, 120);
    assert_eq!(veritasium.last_checked, Some(1_757_000_100_500));
    assert_eq!(
        veritasium.next_due,
        Some(1_757_000_100_500 + 120 * 60_000),
        "last_checked + interval"
    );
    assert_eq!(veritasium.seen_count, 3);
    assert_eq!(veritasium.consecutive_failures, 0);
    let seen = rig.store.seen(&veritasium.id).await.unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen.contains(&Box::<str>::from("vid2")));

    let kurzgesagt = subs
        .iter()
        .find(|s| s.id.as_str() == "0b4c5d6e-7f80-4912-a3b4-c5d6e7f80912")
        .unwrap();
    assert!(!kurzgesagt.enabled);
    assert_eq!(kurzgesagt.check_interval_minutes, 1, "max(1, 0)");
    assert_eq!(kurzgesagt.error.as_deref(), Some("Could not resolve URL"));
    assert_eq!(
        kurzgesagt.ytdl_options_presets,
        vec!["archive".into()],
        "the singular legacy key is migrated"
    );
    assert_eq!(kurzgesagt.selection.download_type, DownloadType::Audio);

    let chats = rig.store.telegram_chats().await.unwrap();
    assert_eq!(chats.len(), 2);
    let group = chats.get(&-1_001_234_567_890).unwrap();
    assert_eq!(
        &*group.download_type, "audio",
        "the selection normaliser runs at import (DESIGN §7.6.5)"
    );
    assert_eq!(&*group.format, "m4a");
    let dm = chats.get(&42).unwrap();
    assert_eq!(&*dm.format, "ios", "best_ios normalises to the ios format");
    assert_eq!(&*dm.quality, "best");
    assert_eq!(dm.playlist_item_limit, 3);

    // Provenance.
    let meta = rig.store.meta().await.unwrap();
    assert_eq!(
        meta.get(aulos_store::IMPORTED_FROM).map(|v| &**v),
        Some(rig.state.display().to_string().as_str())
    );
    assert!(meta.contains_key(aulos_store::IMPORTED_AT));
    let stored: ImportReport =
        serde_json::from_str(meta.get(aulos_store::IMPORT_REPORT).unwrap()).unwrap();
    assert_eq!(stored, report);
    assert!(rig.marker().is_file(), "the marker file must be written");
}

#[tokio::test]
async fn clear_completed_after_is_applied_to_terminal_rows() {
    let rig = Rig::new("v2");
    rig.run(ImportOpts {
        clear_completed_after_s: 3_600,
        ..opts()
    })
    .await;
    let items = rig.items().await;
    let finished = by_url(&items, "d0nEV1de0");
    assert_eq!(
        finished.clear_after,
        Some(1_757_000_000_000 + 3_600_000),
        "finished_at + CLEAR_COMPLETED_AFTER"
    );
    assert!(
        by_url(&items, "dQw4w9WgXcQ").clear_after.is_none(),
        "a queued row has no timer"
    );
}

// ---------------------------------------------------------------------------
// v1 and mixed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_row_of_the_v1_migration_table_imports() {
    let rig = Rig::new("v1");
    let report = rig.run(opts()).await;
    let items = rig.items().await;
    assert_eq!(items.len(), 9, "{:?}", urls(&items));

    let sel = |needle: &str| {
        let i = by_url(&items, needle);
        let s = &i.request.selection;
        (
            s.download_type,
            s.codec.as_str().to_owned(),
            s.format.as_str().to_owned(),
            s.quality.as_str().to_owned(),
        )
    };
    assert_eq!(
        sel("a1"),
        (
            DownloadType::Audio,
            "auto".into(),
            "m4a".into(),
            "192".into()
        )
    );
    assert_eq!(
        sel("a2"),
        (
            DownloadType::Audio,
            "auto".into(),
            "mp3".into(),
            "best".into()
        )
    );
    assert_eq!(
        sel("t1"),
        (
            DownloadType::Thumbnail,
            "auto".into(),
            "jpg".into(),
            "best".into()
        )
    );
    assert_eq!(
        sel("c1"),
        (
            DownloadType::Captions,
            "auto".into(),
            "vtt".into(),
            "best".into()
        )
    );
    assert_eq!(
        sel("c2"),
        (
            DownloadType::Captions,
            "auto".into(),
            "srt".into(),
            "best".into()
        )
    );
    assert_eq!(
        sel("i1"),
        (
            DownloadType::Video,
            "auto".into(),
            "ios".into(),
            "best".into()
        )
    );
    assert_eq!(
        sel("q1"),
        (
            DownloadType::Audio,
            "auto".into(),
            "m4a".into(),
            "best".into()
        )
    );
    assert_eq!(
        sel("v1"),
        (
            DownloadType::Video,
            "h265".into(),
            "mp4".into(),
            "720".into()
        )
    );

    // The record with no `status` at all becomes `queued`, and the singular preset is migrated.
    let v1 = by_url(&items, "youtu.be/v1");
    assert_eq!(v1.status, Status::Queued);
    assert!(v1.auto_start);
    assert_eq!(v1.request.ytdl_options_presets, vec!["archive".into()]);

    // The v1 `completed.json` still produces a finished row with its output.
    let old = by_url(&items, "old1");
    assert_eq!(old.status, Status::Finished);
    assert_eq!(old.size, Some(2_048));
    assert_eq!(old.ord, 0, "it is the oldest record");

    for f in &report.files {
        assert_eq!(f.skipped, 0, "{}", f.file);
    }
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(
        report
            .files
            .iter()
            .find(|f| &*f.file == "queue.json")
            .unwrap()
            .schema_version,
        Some(1)
    );
}

#[tokio::test]
async fn a_mixed_vintage_state_dir_imports_both_files() {
    let rig = Rig::new("mixed");
    let report = rig.run(opts()).await;
    let versions: HashMap<&str, Option<u32>> = report
        .files
        .iter()
        .map(|f| (&*f.file, f.schema_version))
        .collect();
    assert_eq!(versions["queue.json"], Some(2));
    assert_eq!(versions["completed.json"], Some(1));

    let items = rig.items().await;
    assert_eq!(items.len(), 2);
    assert_eq!(by_url(&items, "mix1").status, Status::Queued);
    let v1 = by_url(&items, "mix2");
    assert_eq!(v1.status, Status::Finished);
    assert_eq!(v1.request.selection.download_type, DownloadType::Audio);
}

// ---------------------------------------------------------------------------
// the failure taxonomy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_corrupt_file_under_the_default_policy_rolls_back_and_deletes_the_database() {
    let rig = Rig::new("corrupt");
    let fatal = import(&rig.state, &rig.store, opts())
        .await
        .expect_err("a file error under `fail` must abort");
    assert_eq!(fatal.code, ImportErrorCode::FileInvalid);
    assert_eq!(fatal.report.errors.len(), 1);
    assert_eq!(fatal.report.errors[0].code, ImportErrorCode::FileInvalid);
    assert!(
        fatal.report.errors[0].detail.contains("queue.json"),
        "{}",
        fatal.report.errors[0].detail
    );
    assert!(fatal.should_delete_db());

    // Nothing was written …
    assert_eq!(rig.items().await.len(), 0);
    assert!(rig.store.subscriptions().await.unwrap().is_empty());
    assert!(!rig.marker().exists(), "no marker on a failed import");

    // … and the caller's documented recovery removes the file entirely (DESIGN §7.6.6).
    rig.store.close().await.unwrap();
    aulos_store::import::delete_db_files(&rig.db).unwrap();
    assert!(!rig.db.exists(), "the DB file must not exist afterwards");
}

#[tokio::test]
async fn a_corrupt_file_under_skip_commits_everything_else_and_degrades() {
    let rig = Rig::new("corrupt");
    let report = rig
        .run(ImportOpts {
            on_error: OnError::Skip,
            ..opts()
        })
        .await;

    // The error became a warning …
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let skipped: Vec<_> = report
        .warnings
        .iter()
        .filter(|w| w.code == WarningCode::FileSkipped)
        .collect();
    assert_eq!(skipped.len(), 1);
    assert!(
        skipped[0].detail.starts_with("queue.json"),
        "{:?}",
        skipped[0]
    );
    // … and the importer component reports degraded, naming the file, for the life of the process.
    assert!(report.is_degraded());
    assert_eq!(report.skipped_files().len(), 1);
    assert!(report.skipped_files()[0].contains("queue.json"));

    // Every other file's records are present.
    let items = rig.items().await;
    assert_eq!(items.len(), 2, "{:?}", urls(&items));
    assert_eq!(by_url(&items, "ok1").status, Status::Queued);
    assert_eq!(by_url(&items, "ok2").status, Status::Finished);
    assert_eq!(rig.store.subscriptions().await.unwrap().len(), 1);
    assert!(rig.db.exists(), "the DB must exist");
    assert!(rig.marker().is_file());
}

#[tokio::test]
async fn a_bad_record_is_a_warning_under_both_policies() {
    for policy in [OnError::Fail, OnError::Skip] {
        let rig = Rig::new("bad-record");
        let report = rig
            .run(ImportOpts {
                on_error: policy,
                ..opts()
            })
            .await;
        assert!(report.errors.is_empty(), "{policy:?}: {:?}", report.errors);
        let skipped: Vec<_> = report
            .warnings
            .iter()
            .filter(|w| w.code == WarningCode::RecordSkipped)
            .collect();
        assert_eq!(skipped.len(), 2, "{policy:?}: {:?}", report.warnings);
        assert!(
            skipped[0].detail.starts_with("queue.json[1]"),
            "{skipped:?}"
        );

        let items = rig.items().await;
        assert_eq!(items.len(), 2, "{policy:?}: {:?}", urls(&items));
        let file = report
            .files
            .iter()
            .find(|f| &*f.file == "queue.json")
            .unwrap();
        assert_eq!((file.records, file.imported, file.skipped), (4, 2, 2));
        assert!(rig.marker().is_file(), "{policy:?}: the import must commit");
    }
}

#[tokio::test]
async fn a_legacy_shelf_with_no_json_counterpart_is_fatal_with_the_actionable_message() {
    let rig = Rig::new("shelf-present");
    let fatal = import(&rig.state, &rig.store, opts())
        .await
        .expect_err("a pickle shelf is out of scope");
    assert_eq!(fatal.code, ImportErrorCode::ShelfPresent);
    let detail = &fatal.report.errors[0].detail;
    assert!(detail.contains("legacy shelf found at"), "{detail}");
    assert!(
        detail.contains(
            "start the Python image once so it migrates to JSON, then re-run the import."
        ),
        "{detail}"
    );
    assert!(fatal.should_delete_db());
    assert_eq!(rig.items().await.len(), 0);
}

#[tokio::test]
async fn a_shelf_beside_a_readable_json_file_is_only_a_warning() {
    // This is the real-VPS shape: legacy never deleted the shelf after migrating it to JSON.
    let rig = Rig::new("shelf-present");
    std::fs::copy(
        rig.state.join("completed.json"),
        rig.state.join("queue.json"),
    )
    .unwrap();
    // The copy is a `completed` envelope, so it is *also* a kind mismatch — skip it and assert the
    // shelf itself did not abort the run.
    let report = rig
        .run(ImportOpts {
            on_error: OnError::Skip,
            ..opts()
        })
        .await;
    let ignored: Vec<_> = report
        .warnings
        .iter()
        .filter(|w| w.code == WarningCode::ShelfIgnored)
        .collect();
    assert_eq!(ignored.len(), 1, "{:?}", report.warnings);
    assert!(
        ignored[0].detail.contains("queue.json is present"),
        "{ignored:?}"
    );
    assert_eq!(rig.items().await.len(), 1);
}

#[tokio::test]
async fn a_duplicate_url_keeps_the_most_advanced_record() {
    let rig = Rig::new("dupes");
    let report = rig.run(opts()).await;
    let items = rig.items().await;
    assert_eq!(items.len(), 1, "{:?}", urls(&items));
    assert_eq!(
        items[0].status,
        Status::Finished,
        "the terminal record wins"
    );
    assert_eq!(&*items[0].title, "The finished copy");

    let dup: Vec<_> = report
        .warnings
        .iter()
        .filter(|w| w.code == WarningCode::DuplicateUrl)
        .collect();
    assert_eq!(dup.len(), 1, "{:?}", report.warnings);
    assert!(dup[0].detail.contains("https://youtu.be/dup"), "{dup:?}");
    assert!(dup[0].detail.contains("kept finished"), "{dup:?}");
    // The discarded record is accounted for in its own file's `skipped`.
    let queue = report
        .files
        .iter()
        .find(|f| &*f.file == "queue.json")
        .unwrap();
    assert_eq!((queue.records, queue.imported, queue.skipped), (1, 0, 1));
}

// ---------------------------------------------------------------------------
// StreamingCommunity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_streamingcommunity_row_is_translated_into_the_v2_state_shape() {
    let rig = Rig::new("sc-entry");
    let report = rig.run(opts()).await;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let items = rig.items().await;
    assert_eq!(items.len(), 2);

    let episode = by_url(&items, "watch/9");
    assert_eq!(
        episode.provider.as_ref().map(ProviderId::as_str),
        Some("streamingcommunity")
    );
    assert_eq!(episode.media_id.as_deref(), Some("sc_9_77"));
    assert_eq!(&*episode.canonical_key, "streamingcommunity\u{1f}sc_9_77");
    let state = episode.entry.as_ref().expect("state").as_value();
    // Derived, which legacy never stored separately.
    assert_eq!(state["title_id"], 9);
    assert_eq!(state["episode_id"], 77);
    // Renamed from the legacy `_sc_*` keys, which are consumed.
    assert_eq!(state["base_url"], "https://streamingcommunity.test");
    assert_eq!(state["needs_m3u8_extraction"], true);
    assert!(state["legacy"].get("_sc_base_url").is_none());
    // Carried through for the NFO hook.
    assert_eq!(state["legacy"]["plot"], "La trama dell'episodio.");
    assert_eq!(state["legacy"]["upload_date"], "20260101");
    assert_eq!(state["legacy"]["duration"], 2712);

    let movie = by_url(&items, "watch/5");
    let movie_state = movie.entry.as_ref().expect("state").as_value();
    assert_eq!(movie_state["title_id"], 5);
    assert_eq!(movie_state["episode_id"], serde_json::Value::Null);
    assert_eq!(movie_state["series"], serde_json::Value::Null);
    assert_eq!(movie_state["extractor"], "StreamingCommunity");

    insta::assert_snapshot!(
        "sc_state",
        format!(
            "episode:\n{}\n\nmovie:\n{}",
            serde_json::to_string_pretty(state).unwrap(),
            serde_json::to_string_pretty(movie_state).unwrap()
        )
    );
}

// ---------------------------------------------------------------------------
// T2, idempotence, dry run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_legacy_files_are_never_touched() {
    for (fixture, expect_ok) in [
        ("v2", true),
        ("v1", true),
        ("sc-entry", true),
        ("corrupt", false),
        ("shelf-present", false),
    ] {
        let rig = Rig::new(fixture);
        let before = snapshot_dir(&rig.state);
        let result = import(&rig.state, &rig.store, opts()).await;
        assert_eq!(result.is_ok(), expect_ok, "{fixture}");

        let after = snapshot_dir(&rig.state);
        for (name, bytes) in &before {
            assert_eq!(
                after.get(name),
                Some(bytes),
                "{fixture}/{name} must be byte-identical afterwards"
            );
        }
        // The only file the importer may add is the marker.
        let added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
        match expect_ok {
            true => assert_eq!(added, vec![&MARKER_FILE.to_owned()], "{fixture}"),
            false => assert!(added.is_empty(), "{fixture}: {added:?}"),
        }
    }
}

#[tokio::test]
async fn a_second_import_is_refused_unless_forced() {
    let rig = Rig::new("v2");
    rig.run(opts()).await;

    // The marker alone is enough …
    let fatal = import(&rig.state, &rig.store, opts())
        .await
        .expect_err("a second import must be refused");
    assert_eq!(fatal.code, ImportErrorCode::AlreadyImported);
    assert!(
        !fatal.should_delete_db(),
        "refusing must never delete the database that made us refuse"
    );
    assert!(fatal.reason.contains("--force"), "{}", fatal.reason);
    assert_eq!(rig.items().await.len(), 6, "the first import is intact");

    // … and so is `meta.imported_at` with the marker removed.
    std::fs::remove_file(rig.marker()).unwrap();
    let fatal = import(&rig.state, &rig.store, opts())
        .await
        .expect_err("meta.imported_at must also refuse");
    assert_eq!(fatal.code, ImportErrorCode::AlreadyImported);
    assert!(
        fatal.reason.contains("already imported"),
        "{}",
        fatal.reason
    );

    // `--force` re-imports. The rows are inserted again with fresh `ord`s, which is what an
    // operator asking for `--force` against a non-empty database is asking for.
    let report = rig
        .run(ImportOpts {
            force: true,
            ..opts()
        })
        .await;
    assert_eq!(report.items_total(), 6);
    assert_eq!(rig.items().await.len(), 12);
}

#[tokio::test]
async fn a_dry_run_writes_nothing_and_prints_the_same_report() {
    // The dry run gets its own throwaway database, exactly as the CLI arranges (DESIGN §7.6.6).
    let rig = Rig::new("v2");
    let dry = rig
        .run(ImportOpts {
            dry_run: true,
            ..opts()
        })
        .await;
    assert!(!rig.marker().exists(), "a dry run writes no marker");
    // The rows really were written to the throwaway database, which is what makes the rehearsal
    // meaningful: `STRICT` columns and the `ord` UNIQUE index are exercised for real.
    assert_eq!(rig.items().await.len(), 6);

    // A real run against the same state directory, into a second database, must produce an
    // identical table — that is what makes the rehearsal step of the runbook worth running.
    let db2 = rig._tmp.path().join("db2");
    std::fs::create_dir_all(&db2).unwrap();
    let real = Store::open(support::options(&db2)).unwrap();
    let wet = import(&rig.state, &real, opts())
        .await
        .expect("the real run must succeed");
    assert_eq!(dry.render_table(), wet.render_table());
    assert!(rig.marker().is_file(), "the real run writes the marker");
}

#[tokio::test]
async fn an_empty_state_dir_imports_cleanly() {
    let rig = Rig::new("empty");
    let report = rig.run(opts()).await;
    assert_eq!(report.items_total(), 0);
    assert!(report.errors.is_empty());
    assert!(report.warnings.is_empty());
    assert_eq!(report.files.len(), 5, "every file is accounted for");
    for f in &report.files {
        assert_eq!((f.records, f.imported, f.skipped), (0, 0, 0), "{}", f.file);
    }
    assert!(rig.marker().is_file());
    assert!(
        rig.store
            .meta()
            .await
            .unwrap()
            .contains_key(aulos_store::IMPORTED_AT)
    );
}

#[tokio::test]
async fn a_missing_state_dir_is_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(support::options(tmp.path())).unwrap();
    let fatal = import(&tmp.path().join("nope"), &store, opts())
        .await
        .expect_err("an unreadable STATE_DIR is fatal");
    assert_eq!(fatal.code, ImportErrorCode::StateDirUnreadable);
    assert!(fatal.should_delete_db());
}

#[tokio::test]
async fn a_cookie_file_becomes_the_runtime_override() {
    let rig = Rig::new("cookies");
    rig.run(opts()).await;
    let value = rig.store.kv_get("cookiefile").await.unwrap();
    assert_eq!(
        value.as_ref().and_then(|v| v.as_str()),
        Some(rig.state.join("cookies.txt").display().to_string().as_str())
    );
}

#[tokio::test]
async fn the_whole_import_is_one_transaction() {
    let rig = Rig::new("v2");
    let before = rig.store.commit_count();
    rig.run(opts()).await;
    let after = rig.store.commit_count();
    assert_eq!(
        after - before,
        1,
        "every row, subscription, seen id, chat and meta key must land in one commit"
    );
}
