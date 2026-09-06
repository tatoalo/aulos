//! The headline acceptance test: the three **real** built-ins plus one community hook, driven by
//! the real dispatcher over the real event types, in the order DESIGN §13 specifies.
//!
//! `audio_sync` rewrites the file, so the NFO and the Jellyfin scan must come after it; the
//! community hook's default `ordering = 50` must land between `nfo` (20) and `jellyfin` (90). The
//! only fakes are the two media tools (scripted), the Jellyfin and Plex servers (`wiremock`) and
//! the store port (a `HashMap`) — there is no SQLite and no engine anywhere in this file.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use aulos_core::clock::FakeClock;
use aulos_core::item::EntryBlob;
use aulos_core::selection::DownloadType;
use aulos_core::status::Status;
use aulos_hooks::hook::Hook;
use aulos_hooks::{AudioSyncHook, HookDispatcher, JellyfinHook, ManifestHook, MediaTools, NfoHook};
use aulos_provider::command::load_manifest_with_env;
use common::{
    Call, FakeStore, ItemBuilder, LoggingFinalizer, RecordingHook, config_rooted, events, log,
    read_log, script, sink, until,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// PLAN WP-11: "with all three built-ins applicable, the observed run order is
/// audio_sync → nfo → jellyfin, and a community hook at `ordering = 50` lands between nfo and
/// jellyfin. audio_sync runs on `Finishing` and the other two on `Completed`."
#[tokio::test]
async fn the_three_builtins_and_a_community_hook_run_in_the_documented_order() {
    // --- a download root with the produced file and its sidecar ---
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir bin");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/two_seconds.mp4"),
        dir.path().join("Clip.mp4"),
    )
    .expect("copy the fixture");
    std::fs::write(dir.path().join("Clip.info.json"), b"{}").expect("sidecar");

    // --- scripted ffmpeg/ffprobe, so this test needs no media tooling ---
    let tools = MediaTools {
        ffprobe: script(
            &bin,
            "ffprobe",
            r#"case "$*" in
  *-select_streams*) echo '{"streams":[{"codec_type":"video"}]}' ;;
  *) echo '{"format":{"duration":"2.000000"}}' ;;
esac"#,
        ),
        ffmpeg: script(
            &bin,
            "ffmpeg",
            r#"for a in "$@"; do out="$a"; done
echo "out_time_us=2000000"
printf 'RE-ENCODED' > "$out""#,
        ),
    };

    // --- a Jellyfin and a Plex to talk to ---
    let jellyfin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/Library/Refresh"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&jellyfin)
        .await;
    let plex = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections/3/refresh"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&plex)
        .await;

    let cfg = config_rooted(
        dir.path(),
        &[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", &jellyfin.uri()),
            ("JELLYFIN_API_KEY", "secret"),
            // No debounce, so all four hooks are observable in one ordered chain.
            ("AULOS_JELLYFIN_DEBOUNCE_SECS", "0"),
        ],
    );

    // --- the community hook, from a real manifest ---
    let plugins = dir.path().join("plugins");
    let plugin = plugins.join("media");
    std::fs::create_dir_all(&plugin).expect("mkdir");
    std::fs::write(
        plugin.join("plugin.toml"),
        format!(
            "manifest_version = 1\nname = \"t\"\nversion = \"1.0.0\"\n\n\
             [[hook]]\nid = \"plex\"\non = [\"finished\"]\n\
             http = {{ method = \"GET\", url = \"{}/library/sections/3/refresh\" }}\n",
            plex.uri()
        ),
    )
    .expect("write the manifest");
    let mut specs = load_manifest_with_env(&plugin, &|_| None)
        .expect("the manifest loads")
        .hooks;
    assert_eq!(specs.len(), 1);

    // --- the dispatcher, over the real hooks, each wrapped so its turn is recorded ---
    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        RecordingHook::new(
            Arc::new(AudioSyncHook::with_tools(tools)) as Arc<dyn Hook>,
            Arc::clone(&seq),
        ),
        RecordingHook::new(Arc::new(NfoHook::from_config(&cfg)), Arc::clone(&seq)),
        RecordingHook::new(Arc::new(JellyfinHook::new(&cfg)), Arc::clone(&seq)),
        RecordingHook::new(
            Arc::new(ManifestHook::new(specs.remove(0), &plugins)),
            Arc::clone(&seq),
        ),
    ];
    let finalizer = LoggingFinalizer::new(Arc::clone(&seq));
    let dispatcher = HookDispatcher::with_hooks(
        Arc::clone(&cfg),
        hooks,
        Arc::new(FakeClock::default()) as Arc<_>,
    )
    .with_finalizer(Arc::clone(&finalizer) as Arc<_>);
    assert_eq!(
        dispatcher
            .hook_ids()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["audio_sync", "nfo", "hook:media/plex", "jellyfin"],
        "10 / 20 / 50 / 90"
    );
    let health = dispatcher.health_handle();

    // --- the item: a StreamingCommunity best_remux download ---
    let item = ItemBuilder::finished("Il Grande Film")
        .provider("streamingcommunity")
        .selection(DownloadType::Video, "mp4", "best_remux")
        .filename("Clip.mp4");
    let id = item.id();
    let finishing = item.clone().status(Status::Postprocessing).view();
    let store = FakeStore::with_blob(
        id,
        EntryBlob::new(serde_json::json!({
            "base_url": "https://sc.test",
            "title_id": 1234,
            "needs_m3u8_extraction": true,
            "extractor": "streamingcommunity",
            "ext": "mp4",
        })),
    );

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, Arc::clone(&store) as Arc<_>);

    // 1. The engine finished the download and asks the pre-terminal phase to run.
    ev.finishing(&finishing).await;
    assert!(
        until(|| finalizer.ids() == vec![id]).await,
        "the pre-terminal phase must answer: {:?}",
        read_log(&seq)
    );
    assert_eq!(
        read_log(&seq),
        ["audio_sync", "HooksFinished"],
        "only audio_sync runs before the terminal write"
    );
    assert_eq!(
        std::fs::read(dir.path().join("Clip.mp4")).expect("read"),
        b"RE-ENCODED",
        "the file was rewritten before anything else looked at it"
    );

    // 2. The engine finalises and publishes `Completed`.
    let completed = item.view();
    assert_eq!(completed.id, id, "the same row, now terminal");
    ev.completed(&completed).await;
    assert!(
        until(|| read_log(&seq).len() == 5).await,
        "{:?}",
        read_log(&seq)
    );
    assert_eq!(
        read_log(&seq),
        [
            "audio_sync",
            "HooksFinished",
            "nfo",
            "hook:media/plex",
            "jellyfin"
        ],
        "a community hook at ordering 50 lands between nfo (20) and jellyfin (90)"
    );

    drop(ev.tx);
    task.await.expect("clean stop");

    // --- what the run produced ---
    let nfo = std::fs::read_to_string(dir.path().join("Clip.nfo")).expect("the NFO exists");
    assert!(
        nfo.contains("<uniqueid type=\"streamingcommunity\">sc_1234</uniqueid>"),
        "{nfo}"
    );
    assert!(
        !dir.path().join("Clip.info.json").exists(),
        "the NFO hook deletes the sidecar it replaces (DESIGN §13.2)"
    );
    assert_eq!(
        store.writes(),
        [Call::SetSize(id, 10), Call::DropEntryBlob(id)],
        "exactly the two writes the built-ins are documented to make, in phase order"
    );
    jellyfin.verify().await;
    plex.verify().await;

    let h = health.health();
    for id in ["audio_sync", "nfo", "hook:media/plex", "jellyfin"] {
        let stat = h.stat(id).unwrap_or_else(|| panic!("{id} is registered"));
        assert_eq!(stat.runs_total, 1, "{id}");
        assert_eq!(stat.failures_total, 0, "{id}");
        assert!(stat.last_success_at.is_some(), "{id}");
    }
    assert_eq!(h.failures_total(), 0);
    assert_eq!(h.events_dropped, 0);
}

/// The production scenario of the bug report, end to end over the real hooks: a **yt-dlp**
/// download with a `.info.json` sidecar and no entry blob. The NFO must be written, and it must be
/// written *before* the Jellyfin scan, or the library refresh reads a directory with no `.nfo` in
/// it and the metadata is missed until the next scan.
#[tokio::test]
async fn a_ytdlp_download_gets_its_nfo_before_the_jellyfin_scan() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Clip.mp4"), b"video").expect("media");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/youtube_info.json"),
        dir.path().join("Clip.info.json"),
    )
    .expect("sidecar");

    let jellyfin = MockServer::start().await;
    // The refresh only counts as correct if the NFO is already on disk when it arrives.
    let seen_nfo = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let seen = Arc::clone(&seen_nfo);
        let nfo_path = dir.path().join("Clip.nfo");
        Mock::given(method("POST"))
            .and(path("/Library/Refresh"))
            .respond_with(move |_: &wiremock::Request| {
                seen.store(nfo_path.exists(), std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(204)
            })
            .expect(1)
            .mount(&jellyfin)
            .await;
    }

    let cfg = config_rooted(
        dir.path(),
        &[
            ("JELLYFIN_SYNC_ENABLED", "true"),
            ("JELLYFIN_URL", &jellyfin.uri()),
            ("JELLYFIN_API_KEY", "secret"),
            ("AULOS_JELLYFIN_DEBOUNCE_SECS", "0"),
        ],
    );

    let seq = log();
    let hooks: Vec<Arc<dyn Hook>> = vec![
        RecordingHook::new(Arc::new(NfoHook::from_config(&cfg)), Arc::clone(&seq)),
        RecordingHook::new(Arc::new(JellyfinHook::new(&cfg)), Arc::clone(&seq)),
    ];
    let dispatcher = HookDispatcher::with_hooks(
        Arc::clone(&cfg),
        hooks,
        Arc::new(FakeClock::default()) as Arc<_>,
    );
    let health = dispatcher.health_handle();

    let item = ItemBuilder::finished("Le incredibili elezioni del 2000")
        .provider("ytdlp")
        .filename("Clip.mp4");
    // No blob: DESIGN §7.5 drops a plain yt-dlp entry at the terminal write.
    let store = FakeStore::new();

    let mut ev = events();
    let (factory, _rx) = sink();
    let task = dispatcher.spawn(ev.inbox(), factory, Arc::clone(&store) as Arc<_>);
    ev.completed(&item.view()).await;
    assert!(
        until(|| read_log(&seq).len() == 2).await,
        "{:?}",
        read_log(&seq)
    );
    assert_eq!(
        read_log(&seq),
        ["nfo", "jellyfin"],
        "nfo (20) before jellyfin (90)"
    );

    drop(ev.tx);
    task.await.expect("clean stop");

    let nfo = std::fs::read_to_string(dir.path().join("Clip.nfo")).expect("the NFO exists");
    assert!(
        nfo.contains("<uniqueid type=\"youtube\">8Xrcn5B04u4</uniqueid>"),
        "rendered from the sidecar: {nfo}"
    );
    assert!(
        seen_nfo.load(std::sync::atomic::Ordering::SeqCst),
        "the library scan must see the .nfo"
    );
    assert!(
        store.writes().is_empty(),
        "a row with no blob makes no engine write: {:?}",
        store.writes()
    );
    jellyfin.verify().await;

    let h = health.health();
    let stat = h.stat("nfo").expect("nfo");
    assert_eq!(stat.runs_total, 1);
    assert_eq!(stat.skipped_total, 0, "nothing was declined");
}
