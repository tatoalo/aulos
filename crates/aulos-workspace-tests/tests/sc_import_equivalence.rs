//! An imported legacy StreamingCommunity row must be indistinguishable from a freshly resolved
//! one (PLAN WP-05, DESIGN §7.6.3a, §10.3 note 2, §13.2).
//!
//! This is the assertion PLAN WP-05 asked for and could not own. It needs the importer
//! (`aulos-store`), the SC provider's resolution and state shape (`aulos-provider-sc`) and the NFO
//! renderer (`aulos-hooks`) in one process, and arch rule A1 forbids a provider crate from
//! dev-depending on the store — so it lives here, in the dev-only crate that already owns the
//! workspace-wide gates.
//!
//! # Why it matters
//!
//! Legacy persisted a flat yt-dlp-shaped `entry` dict; v2 persists an [`ScState`] blob. The
//! importer translates the former into the latter (`_sc_base_url` → `base_url`, ids derived out of
//! `sc_<title>_<episode>`, everything else carried in `state.legacy`). If that translation drops a
//! field, an imported item's `.info.json` sidecar and its Jellyfin `.nfo` silently differ from the
//! ones a re-added item would get — and the file next to an existing library would be rewritten
//! with worse metadata. Nothing else in the workspace can see both sides.
//!
//! The fresh side is a real resolution against the SC crate's checked-in fixtures over a loopback
//! `wiremock` server; the imported side is a real `queue.json` run through the real importer. The
//! legacy `entry` fed to the importer is **derived from the fresh resolution** rather than
//! hand-written, so the two sides cannot drift apart by someone editing one fixture.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic is the reporting mechanism in a test"
)]

use std::path::Path;

use aulos_core::item::{Item, ItemView, ViewExtras};
use aulos_hooks::nfo;
use aulos_provider::entry::MediaEntry;
use aulos_provider_sc::http::{PlainClient, ScHttp};
use aulos_provider_sc::{ScState, SiteVersions, watch};
use aulos_store::import::{ImportOpts, import};
use aulos_store::{ItemFilter, Store, StoreOptions};
use serde_json::{Map, Value, json};
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The fixtures the fresh resolution needs, read from the SC crate's own corpus so this
/// test cannot drift from the suite that maintains them.
fn sc_fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../aulos-provider-sc/tests/fixtures/sc")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{} must be readable: {e}", p.display()))
}

/// The wall clock both NFO renders are stamped from, so `dateadded` cannot explain a difference.
const NOW_MS: i64 = 1_772_582_400_000;

/// Resolves `/it/watch/9?e=456` for real: S1 (site version), S2 (Inertia watch page), S3/S4 (the
/// embed and its `window.streams`, which `resolve_watch` performs as its validity probe).
async fn resolve_fresh() -> (MediaEntry, Url) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sc_fixture("it_page.html")))
        .mount(&server)
        .await;
    // The fixture's `embedUrl` is the real site's absolute URL; point it at the mock, so S3 stays
    // on loopback and the test needs no network.
    let watch_page = sc_fixture("watch_episode.json").replace("https://sc.test", &server.uri());
    Mock::given(method("GET"))
        .and(path("/it/watch/9"))
        .and(query_param("e", "456"))
        .respond_with(ResponseTemplate::new(200).set_body_string(watch_page))
        .mount(&server)
        .await;
    // S3: the embed page, whose `<iframe src>` is rewritten onto the mock for the same reason.
    let embed = sc_fixture("embed.html").replace("https://vixcloud.co", &server.uri());
    Mock::given(method("GET"))
        .and(path("/embed/456"))
        .respond_with(ResponseTemplate::new(200).set_body_string(embed))
        .mount(&server)
        .await;
    // S4: the vixcloud player page carrying `window.streams` and the token/expires pair.
    Mock::given(method("GET"))
        .and(path("/embed/98765"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sc_fixture("vixcloud_streams_active.html")),
        )
        .mount(&server)
        .await;

    let base = Url::parse(&server.uri()).unwrap();
    // The watch URL is echoed onto the entry verbatim, and it is what both sides key on.
    let watch_url = base.join("/it/watch/9?e=456").unwrap();
    let http: PlainClient = PlainClient::new().expect("the plain client must build");
    let entry = watch::resolve_watch(
        &http as &dyn ScHttp,
        &SiteVersions::new(),
        &base,
        &watch_url,
    )
    .await
    .expect("the fixture watch page must resolve");
    (entry, watch_url)
}

/// A legacy `STATE_DIR/queue.json` holding one row whose `entry` is `info`.
///
/// This is the shape the Python server wrote (`schema_version 2`, `persistent_queue:queue`), and
/// `info` is the flat dict it kept — i.e. exactly what [`ScState::to_legacy_info_json`] renders.
fn write_legacy_queue(state_dir: &Path, url: &str, title: &str, info: &Value) {
    std::fs::create_dir_all(state_dir).unwrap();
    let queue = json!({
        "schema_version": 2,
        "kind": "persistent_queue:queue",
        "items": [{
            "key": url,
            "info": {
                "id": info["id"],
                "title": title,
                "url": url,
                "quality": "best",
                "download_type": "video",
                "codec": "auto",
                "format": "any",
                "folder": "",
                "custom_name_prefix": "",
                "playlist_item_limit": 0,
                "split_by_chapters": false,
                "chapter_template": "",
                "subtitle_language": "en",
                "subtitle_mode": "prefer_manual",
                "ytdl_options_presets": [],
                "ytdl_options_overrides": {},
                "status": "pending",
                "timestamp": 1_757_000_000_000_000_000_i64,
                "entry": info,
            }
        }]
    });
    std::fs::write(
        state_dir.join("queue.json"),
        serde_json::to_vec(&queue).unwrap(),
    )
    .unwrap();
}

/// Imports `state_dir` into a throwaway store and hands back the single row it produced.
async fn import_one(tmp: &Path, state_dir: &Path) -> Item {
    let db_dir = tmp.join("db");
    std::fs::create_dir_all(&db_dir).unwrap();
    let store = Store::open(
        StoreOptions::new(db_dir.join("aulos.db"))
            .with_flush_ms(5)
            .with_readers(2),
    )
    .unwrap();
    let report = import(state_dir, &store, ImportOpts::default())
        .await
        .unwrap_or_else(|e| panic!("the import must succeed: {e}\n{}", e.report.render_table()));
    assert!(
        report.warnings.is_empty(),
        "an unexpected import warning: {:#?}",
        report.warnings
    );
    let mut rows = store.items(ItemFilter::default()).await.unwrap().rows;
    store.close().await.unwrap();
    assert_eq!(rows.len(), 1, "the fixture holds exactly one row");
    rows.remove(0)
}

#[tokio::test]
async fn an_imported_sc_row_and_a_freshly_resolved_one_agree_on_the_sidecar_and_the_nfo() {
    let (fresh, watch_url) = resolve_fresh().await;

    // --- the fresh side -------------------------------------------------------------------
    let fresh_state = ScState::from_json(&fresh.state)
        .expect("a freshly resolved entry's state must be an ScState");
    assert_eq!(fresh_state.title_id, Some(9));
    assert_eq!(fresh_state.episode_id, Some(456));
    assert_eq!(fresh_state.season_number, Some(2));
    assert_eq!(fresh_state.episode_number, Some(3));

    // The legacy flat dict for this entry: what the Python server would have persisted, and what
    // it would have written next to the file as `<title>.info.json`.
    let mut legacy_info = fresh_state
        .to_legacy_info_json(&fresh.media_id, &fresh.title, watch_url.as_str())
        .as_object()
        .cloned()
        .expect("the sidecar is an object");
    // The three metadata keys legacy carried that v2 keeps only inside `state.legacy` — they are
    // what the NFO hook reads, so leaving them out would make this test pass vacuously.
    legacy_info.insert("plot".to_owned(), json!("Qualcosa succede."));
    legacy_info.insert("upload_date".to_owned(), json!("20260101"));
    legacy_info.insert("duration".to_owned(), json!(2712));
    let legacy_info = Value::Object(legacy_info);

    // --- the imported side ----------------------------------------------------------------
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    write_legacy_queue(&state_dir, watch_url.as_str(), &fresh.title, &legacy_info);
    let item = import_one(tmp.path(), &state_dir).await;

    assert_eq!(
        item.provider.as_ref().map(aulos_core::ProviderId::as_str),
        Some("streamingcommunity"),
        "an SC row must be attributed to the SC provider, not left unresolved"
    );
    let blob = item
        .entry
        .as_ref()
        .expect("the imported row keeps a state blob");

    // WP-05's first assertion: the translated blob is a readable `ScState`.
    let imported_state =
        ScState::from_json(blob.as_value()).expect("the imported blob must parse as an ScState");
    assert_eq!(imported_state.title_id, fresh_state.title_id);
    assert_eq!(imported_state.episode_id, fresh_state.episode_id);
    assert_eq!(imported_state.season_number, fresh_state.season_number);
    assert_eq!(imported_state.episode_number, fresh_state.episode_number);
    assert_eq!(imported_state.series, fresh_state.series);
    assert_eq!(imported_state.base_url, fresh_state.base_url);
    assert!(imported_state.needs_m3u8_extraction);

    // WP-05's second assertion, half one: the sidecar. The imported row's `state.legacy` carries
    // the three extra keys, so the fresh side is compared with them folded in — anything *else*
    // differing is the translation losing a field.
    let imported_sidecar =
        imported_state.to_legacy_info_json(&fresh.media_id, &fresh.title, watch_url.as_str());
    assert_eq!(
        sorted(&imported_sidecar),
        sorted(&legacy_info),
        "the imported row's .info.json must be byte-equivalent to the freshly resolved one"
    );

    // WP-05's second assertion, half two: the NFO. Same item, same clock — only the blob differs,
    // which is precisely the thing under test.
    let view = ItemView::from_item(&item, None, &ViewExtras::default());
    let fresh_blob = aulos_core::item::EntryBlob::new(
        ScState {
            legacy: legacy_map(&legacy_info),
            ..fresh_state.clone()
        }
        .to_json(),
    );
    let from_imported =
        nfo::render(&view, blob, nfo::Source::Entry, NOW_MS).expect("the imported NFO renders");
    let from_fresh =
        nfo::render(&view, &fresh_blob, nfo::Source::Entry, NOW_MS).expect("the fresh NFO renders");
    assert_eq!(
        from_imported, from_fresh,
        "an imported item's .nfo must match the one a freshly resolved item would get"
    );
    // Not vacuous: the document really does carry the episode metadata.
    assert!(
        from_imported.contains("<season>2</season>"),
        "{from_imported}"
    );
    assert!(
        from_imported.contains("<episode>3</episode>"),
        "{from_imported}"
    );
    assert!(
        from_imported.contains("Qualcosa succede."),
        "{from_imported}"
    );
}

/// `state.legacy` as the importer builds it: the flat dict minus the keys `ScState` promotes to
/// real fields and minus the `_sc_*` keys it renames.
fn legacy_map(info: &Value) -> Map<String, Value> {
    const PROMOTED: [&str; 12] = [
        "id",
        "title",
        "url",
        "webpage_url",
        "ext",
        "_type",
        "extractor",
        "extractor_key",
        "season_number",
        "episode_number",
        "episode",
        "series",
    ];
    let mut out = Map::new();
    for (k, v) in info.as_object().expect("an object") {
        if PROMOTED.contains(&k.as_str()) || k.starts_with("_sc_") {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

/// A JSON object as a sorted key/value list, so key *order* is not part of the comparison — the
/// order assertion belongs to the SC crate's own `to_legacy_info_json` test.
fn sorted(v: &Value) -> Vec<(String, Value)> {
    let mut pairs: Vec<(String, Value)> = v
        .as_object()
        .expect("an object")
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}
