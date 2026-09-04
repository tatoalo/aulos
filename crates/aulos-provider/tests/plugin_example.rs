//! The shipped `plugins/examples/bandcamp` plugin, end to end against a local HTTP stub
//! (DESIGN §6.5.4, PLAN WP-10).
//!
//! This test is also the plugin author's template: it is the shortest complete description of
//! what a `command` plugin has to do — resolve a URL into a group plus entries, then download one
//! of them, reporting progress in whatever shape its own `[progress]` table declares and
//! finishing with a `result` frame.
//!
//! It runs the **repository's own** `plugin.toml`, `resolve.py` and `download.py` (copied into a
//! temp directory so nothing in the repo is touched), so the example cannot rot without this test
//! going red.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use aulos_core::paths::RelPath;
use aulos_core::selection::DownloadType;
use aulos_provider::sink::{ProgressMsg, ProgressSinkFactory, Stage};
use aulos_provider::{Match, Provider};
use common::{download_ctx, has_python3, paths, request, resolve_ctx, shipped_example};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The page shape `resolve.py` reads: a `window.albumData = {…}` blob.
fn album_page(base: &str) -> String {
    format!(
        r#"<!doctype html><html><head><title>Album — Deluxe</title></head><body>
<script>
window.albumData = {{
  "id": 914,
  "title": "Album — Deluxe",
  "tracks": [
    {{ "id": 1, "title": "One",   "duration": 183.0, "url": "{base}/track/1", "stream_url": "{base}/stream/1" }},
    {{ "id": 2, "title": "Two",   "duration": 201.5, "url": "{base}/track/2", "stream_url": "{base}/stream/2" }},
    {{ "id": 3, "title": "Three", "duration": 178.0, "url": "{base}/track/3", "stream_url": "{base}/stream/3" }},
    {{ "id": 4, "title": "Bonus", "duration": 90.0,  "url": "{base}/track/4" }}
  ]
}};
</script>
</body></html>"#
    )
}

#[test]
fn the_example_manifest_claims_bandcamp_urls_at_the_top_score() {
    let example = shipped_example("bandcamp");
    let provider = example.provider();
    let m = |s: &str| provider.matches(&Url::parse(s).unwrap());

    // host_regex **and** path_regex ⇒ 250 (DESIGN §6.3).
    assert_eq!(m("https://bandcamp.com/album/914"), Match::Strong(250));
    assert_eq!(
        m("https://artist.bandcamp.com/track/one"),
        Match::Strong(250)
    );
    // The host matches but the path does not ⇒ the host_regex score.
    assert_eq!(m("https://bandcamp.com/"), Match::Strong(150));
    // `schemes = ["https"]`, and an unrelated host is not ours.
    assert_eq!(m("http://bandcamp.com/album/914"), Match::No);
    assert_eq!(m("https://example.test/album/914"), Match::No);

    // The `[catalog]` section of DESIGN §6.5.4 survives the round trip.
    let catalog = provider.catalog();
    assert_eq!(catalog.download_types.len(), 1);
    let audio = &catalog.download_types[0];
    assert_eq!(&*audio.id, "audio");
    assert_eq!(&*audio.label, "Audio");
    // `default_format` is not declared in the manifest, so the first format wins.
    assert_eq!(&*audio.default_format, "flac");
    let ids: Vec<&str> = audio.formats.iter().map(|f| &*f.id).collect();
    assert_eq!(ids, ["flac", "mp3"]);
    assert_eq!(&*audio.formats[0].default_quality, "best");
    assert_eq!(&*audio.formats[1].default_quality, "320");
    let mp3_qualities: Vec<&str> = audio.formats[1].qualities.iter().map(|q| &*q.id).collect();
    assert_eq!(mp3_qualities, ["320", "192"]);
    assert!(
        audio.formats[1]
            .notice
            .as_deref()
            .is_some_and(|n| n.contains("ffmpeg")),
        "the honest notice must survive"
    );

    // `own_slots()` is `None` because the example uses the global slot.
    assert_eq!(provider.own_slots(), None);
    // Every argv is auditable, unrendered (DESIGN §6.5.3).
    let audit = provider.audit();
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0][0], "python3");
    assert!(audit[1].contains(&"{out_path}".to_owned()), "{audit:?}");
}

#[tokio::test]
async fn the_example_plugin_resolves_and_downloads_end_to_end() {
    if !has_python3() {
        eprintln!("skipping: python3 is not on PATH, and the example plugin is written in Python");
        return;
    }

    let server = MockServer::start().await;
    let base = server.uri();
    let stream_body: Vec<u8> = (0..40_960u32).map(|i| (i % 251) as u8).collect();

    Mock::given(method("GET"))
        .and(path("/album/914"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(album_page(&base))
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/stream/1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(stream_body.clone())
                .insert_header("content-type", "audio/flac"),
        )
        .mount(&server)
        .await;

    let example = shipped_example("bandcamp");
    let provider = example.provider();

    let base_dir = tempfile::tempdir().unwrap();
    let p = paths(base_dir.path());
    let album_url = Url::parse(&format!("{base}/album/914")).unwrap();
    let req = request(&album_url, DownloadType::Audio, "flac", "best");

    // --- resolve: a group plus three entries (the fourth track has no stream and is skipped) ---
    let entries = provider
        .resolve(&album_url, resolve_ctx(&req, &p, CancellationToken::new()))
        .await
        .expect("the example plugin must resolve the album");

    assert_eq!(entries.len(), 1, "a group frame wraps the children");
    let group = &entries[0];
    assert!(group.is_playlist());
    assert_eq!(&*group.media_id, "bc:album:914");
    assert_eq!(&*group.title, "Album — Deluxe");
    // `expected` counts the four tracks the page declared, even though one was skipped — that is
    // what the plugin printed, and the note explains the difference.
    assert_eq!(group.hints.playlist_count, Some(4));

    let children = group.children();
    assert_eq!(children.len(), 3, "the streamless track is skipped");
    assert_eq!(&*children[0].media_id, "bc:track:1");
    assert_eq!(&*children[0].title, "One");
    assert_eq!(children[0].hints.duration, Some(183.0));
    assert_eq!(children[0].hints.playlist_index, Some(1));
    assert_eq!(children[0].hints.ext.as_deref(), Some("flac"));
    assert_eq!(
        children[0].state["stream_url"],
        format!("{base}/stream/1"),
        "the provider-native blob round-trips to download time"
    );

    // --- download the first child ---
    let ctx = download_ctx(
        &children[0],
        &req,
        p.audio_download.clone(),
        p.temp.clone(),
        CancellationToken::new(),
    );
    let item = ctx.item_id;
    let (factory, mut rx) = ProgressSinkFactory::channel();
    let outcome = provider
        .download(ctx, factory.for_item(item))
        .await
        .expect("the example plugin must download the track");

    assert_eq!(
        outcome.filename.as_ref().map(RelPath::as_str),
        Some("One.flac"),
        "`{{out_path}}` is `{{out_dir}}/{{out_name}}.{{output_ext}}`"
    );
    assert_eq!(outcome.size, Some(stream_body.len() as u64));
    let written = std::fs::read(p.audio_download.join("One.flac")).unwrap();
    assert_eq!(
        written, stream_body,
        "the bytes on disk are the bytes served"
    );
    // Nothing was left behind in the scratch directory.
    let leftovers: Vec<_> = std::fs::read_dir(&p.temp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // --- the progress its `[progress]` table declares reached the sink ---
    let mut stages = Vec::new();
    let mut frames = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            ProgressMsg::Stage { stage, .. } => stages.push(stage),
            ProgressMsg::Progress { raw, .. } => frames.push(raw),
            ProgressMsg::File { .. } => {}
        }
    }
    assert_eq!(stages.first(), Some(&Stage::Preparing));
    assert!(
        stages.contains(&Stage::Postprocessing),
        "`stage=mux` maps to postprocessing: {stages:?}"
    );
    // A 40 KiB download finishes inside one read, so `last_match_wins` collapses `stage=fetch`,
    // `stage=mux` and `stage=done` into the newest status the chunk carried — which is the point
    // of the option. The `fetch → downloading` and `done → finished` legs of the `status_map` are
    // asserted per-line in `command::progress`'s own tests.
    assert!(
        stages.iter().any(|s| *s != Stage::Preparing),
        "the item must leave `preparing`: {stages:?}"
    );
    let best = frames
        .iter()
        .filter_map(|f| f.downloaded_bytes)
        .fold(0.0_f64, f64::max);
    assert!(
        best >= 40.0 * 1024.0,
        "the 1024-based `units = \"auto\"` reading of `40.0KiB`: {frames:?}"
    );
}

#[tokio::test]
async fn the_example_plugin_reports_a_missing_album_as_unavailable() {
    if !has_python3() {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/album/404"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let example = shipped_example("bandcamp");
    let provider = example.provider();
    let base_dir = tempfile::tempdir().unwrap();
    let p = paths(base_dir.path());
    let url = Url::parse(&format!("{}/album/404", server.uri())).unwrap();
    let req = request(&url, DownloadType::Audio, "flac", "best");

    let e = provider
        .resolve(&url, resolve_ctx(&req, &p, CancellationToken::new()))
        .await
        .expect_err("a 404 album must fail");
    // The `{"t":"error","code":"unavailable"}` frame becomes the typed error it names, without the
    // plugin or the server owning a second copy of the code table (DESIGN §6.5.3).
    assert_eq!(e.code(), aulos_core::error::ErrorCode::Unavailable);
    assert!(e.message().contains("404"), "{}", e.message());
    assert!(!e.retryable());
}

#[tokio::test]
async fn the_example_plugin_falls_through_when_the_page_is_not_an_album() {
    if !has_python3() {
        eprintln!("skipping: python3 is not on PATH");
        return;
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/album/plain"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>nothing here</html>"))
        .mount(&server)
        .await;

    let example = shipped_example("bandcamp");
    let provider = example.provider();
    let base_dir = tempfile::tempdir().unwrap();
    let p = paths(base_dir.path());
    let url = Url::parse(&format!("{}/album/plain", server.uri())).unwrap();
    let req = request(&url, DownloadType::Audio, "flac", "best");

    let e = provider
        .resolve(&url, resolve_ctx(&req, &p, CancellationToken::new()))
        .await
        .expect_err("an unrecognisable page must not look like a success");
    // `unsupported_url` is the one code the engine retries through the runner-up (DESIGN §6.4),
    // which is exactly what a plugin that turned out not to understand the page wants.
    assert_eq!(
        e.code(),
        aulos_core::error::ErrorCode::UnsupportedUrl,
        "{}",
        e.message()
    );
}
