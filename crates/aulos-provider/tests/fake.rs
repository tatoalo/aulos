//! The `fake` provider acceptance suite (PLAN WP-03).
//!
//! Everything here runs under `start_paused = true`, which is the point: the fixture describes a
//! ten-minute download and the test finishes in microseconds of wall-clock time.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aulos_core::clock::{Clock, SystemClock};
use aulos_core::error::ErrorCode;
use aulos_core::id::ItemId;
use aulos_core::item::FileSlot;
use aulos_core::paths::Paths;
use aulos_core::progress::{Normalizer, RawProgress};
use aulos_core::request::DownloadRequest;
use aulos_core::selection::{Codec, DownloadType, FormatId, ProviderId, QualityId, Selection};
use aulos_core::status::Status;
use aulos_provider::fake::{FakeError, FakeProvider, Step, Timeline};
use aulos_provider::{
    DownloadCtx, Match, MediaEntry, ProgressMsg, ProgressSinkFactory, Provider, ProviderError,
    ProviderHealth, ResolveCtx, Stage,
};
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use url::Url;

fn fixture() -> FakeProvider {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake/timelines.toml");
    FakeProvider::from_toml_file(&path).expect("the checked-in fixture must load")
}

fn url(s: &str) -> Url {
    Url::parse(s).expect("a test url")
}

fn request(u: &Url) -> DownloadRequest {
    DownloadRequest::new(
        u.clone(),
        Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").expect("format"),
            QualityId::parse("best").expect("quality"),
        ),
    )
}

fn paths(root: &Path) -> Paths {
    Paths {
        download: root.to_path_buf(),
        audio_download: root.to_path_buf(),
        temp: root.to_path_buf(),
        state: root.to_path_buf(),
    }
}

/// Everything a download needs, owned by the caller so the borrowed context can be rebuilt.
struct Harness {
    dir: tempfile::TempDir,
    request: DownloadRequest,
    entry: MediaEntry,
    cancel: CancellationToken,
    factory: ProgressSinkFactory,
    rx: Receiver<ProgressMsg>,
    id: ItemId,
}

impl Harness {
    fn new(u: &Url, title: &str) -> Self {
        let (factory, rx) = ProgressSinkFactory::channel();
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            request: request(u),
            entry: MediaEntry::video("fake:1", title, u.clone()),
            cancel: CancellationToken::new(),
            factory,
            rx,
            id: ItemId::new(),
        }
    }

    fn out_dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn download_ctx(&self) -> DownloadCtx<'_> {
        DownloadCtx {
            item_id: self.id,
            source: aulos_core::SourceKind::ApiV2,
            entry: &self.entry,
            request: &self.request,
            ytdl_options: Arc::new(aulos_core::ytdl_options::YtdlOptions::empty()),
            out_dir: self.out_dir(),
            tmp_dir: self.out_dir(),
            outtmpl: aulos_provider::OutTmpl::default(),
            cancel: self.cancel.clone(),
        }
    }

    fn resolve_ctx<'a>(&'a self, p: &'a Paths) -> ResolveCtx<'a> {
        ResolveCtx {
            item_id: self.id,
            request: &self.request,
            ytdl_options: Arc::new(aulos_core::ytdl_options::YtdlOptions::empty()),
            paths: p,
            flat: false,
            playlist_end: None,
            cancel: self.cancel.clone(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(60),
        }
    }

    /// Everything the sink received, in order.
    fn drain(&mut self) -> Vec<ProgressMsg> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            out.push(m);
        }
        out
    }
}

/// The percent the aggregator's `Normalizer` would derive from a message sequence.
fn percents(msgs: &[ProgressMsg]) -> Vec<f64> {
    let mut norm = Normalizer::new();
    msgs.iter()
        .filter_map(|m| match m {
            ProgressMsg::Progress { raw, .. } => Some(norm.apply(raw, Status::Downloading)),
            _ => None,
        })
        .collect()
}

fn stages(msgs: &[ProgressMsg]) -> Vec<Stage> {
    msgs.iter()
        .filter_map(|m| match m {
            ProgressMsg::Stage { stage, .. } => Some(*stage),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_ten_minute_download_runs_in_microseconds_and_emits_the_expected_sequence() {
    let provider = fixture();
    let u = url("https://fake.test/watch/slow-1");
    let mut h = Harness::new(&u, "A Ten Minute Clip");

    let virtual_start = tokio::time::Instant::now();
    let wall_start = SystemClock.now_ms();
    let outcome = provider
        .download(h.download_ctx(), h.factory.for_item(h.id))
        .await
        .expect("the scripted download must succeed");
    let virtual_elapsed = virtual_start.elapsed();
    let wall_elapsed = SystemClock.now_ms() - wall_start;

    assert_eq!(
        virtual_elapsed,
        Duration::from_millis(602_000),
        "the timeline's ten minutes must actually elapse on the paused clock"
    );
    assert!(
        wall_elapsed < 2_000,
        "…but not in wall-clock time, took {wall_elapsed} ms"
    );

    let msgs = h.drain();
    assert_eq!(
        stages(&msgs),
        [Stage::Preparing, Stage::Downloading, Stage::Postprocessing]
    );
    let p = percents(&msgs);
    assert_eq!(p.len(), 3);
    assert!((p[0] - 0.0).abs() < 1e-9, "{p:?}");
    assert!((p[1] - 50.0).abs() < 1e-9, "{p:?}");
    assert!((p[2] - 99.9).abs() < 1e-9, "{p:?}");

    // Both artifacts reached the sink, in script order, and the outcome carries them.
    let files: Vec<(FileSlot, String)> = msgs
        .iter()
        .filter_map(|m| match m {
            ProgressMsg::File { slot, file, .. } => Some((*slot, file.filename.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        files,
        [
            (FileSlot::Subtitle, "clip.en.srt".to_owned()),
            (FileSlot::Chapter, "clip - 01.mp4".to_owned())
        ]
    );
    assert_eq!(outcome.subtitle_files.len(), 1);
    assert_eq!(outcome.chapter_files.len(), 1);
    assert_eq!(outcome.subtitle_files[0].lang.as_deref(), Some("en"));

    assert_eq!(
        outcome
            .filename
            .as_ref()
            .map(aulos_core::paths::RelPath::as_str),
        Some("clip.mp4")
    );
    assert_eq!(outcome.size, Some(4096));

    // And the files really exist, so the static file route and the size bookkeeping have
    // something to look at.
    for name in ["clip.mp4", "clip.en.srt", "clip - 01.mp4"] {
        let p = h.out_dir().join(name);
        assert!(p.exists(), "{} must have been written", p.display());
    }
    assert_eq!(
        std::fs::metadata(h.out_dir().join("clip.mp4"))
            .expect("stat")
            .len(),
        4096
    );
    assert_eq!(provider.download_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_hang_never_returns_and_lets_the_callers_stall_timer_fire() {
    let provider = fixture();
    let u = url("https://fake.test/watch/stall-1");
    let mut h = Harness::new(&u, "Stalled");

    // Exactly what the engine does: race the download against the stall deadline. `Hang` arms no
    // timer, so paused time reaches the deadline and the *timeout* wins.
    let result = tokio::time::timeout(
        Duration::from_secs(600),
        provider.download(h.download_ctx(), h.factory.for_item(h.id)),
    )
    .await;
    assert!(result.is_err(), "the stall timer must be what fires");

    let msgs = h.drain();
    assert_eq!(stages(&msgs), [Stage::Downloading]);
    let p = percents(&msgs);
    assert!((p[0] - 43.2).abs() < 1e-9, "{p:?}");
}

#[tokio::test(start_paused = true)]
async fn a_hang_still_answers_a_cancel() {
    let provider = Arc::new(fixture());
    let u = url("https://fake.test/watch/stall-2");
    let h = Harness::new(&u, "Stalled");
    let cancel = h.cancel.clone();

    let ctx_provider = Arc::clone(&provider);
    let sink = h.factory.for_item(h.id);
    let task = tokio::spawn(async move { ctx_provider.download(h.download_ctx(), sink).await });
    tokio::task::yield_now().await;
    cancel.cancel();
    let err = task
        .await
        .expect("the task must not panic")
        .expect_err("a cancelled download must fail");
    assert!(matches!(err, ProviderError::Canceled));
    assert_eq!(err.code(), ErrorCode::Canceled);
}

#[tokio::test(start_paused = true)]
async fn expand_playlist_returns_five_hundred_entries_with_playlist_hints() {
    let provider = fixture();
    let u = url("https://fake.test/playlist/1");
    let h = Harness::new(&u, "Big Playlist");
    let p = paths(h.dir.path());

    let entries = provider
        .resolve(&u, h.resolve_ctx(&p))
        .await
        .expect("resolve must succeed");
    assert_eq!(entries.len(), 500);
    assert_eq!(&*entries[0].title, "Big Playlist #1");
    assert_eq!(entries[0].hints.playlist_index, Some(1));
    assert_eq!(entries[0].hints.playlist_count, Some(500));
    assert_eq!(
        entries[0].hints.playlist_title.as_deref(),
        Some("Big Playlist")
    );
    assert_eq!(entries[499].hints.playlist_index, Some(500));
    // Every child has a distinct URL, so the engine's dedupe key is distinct too.
    let unique: std::collections::HashSet<_> = entries.iter().map(|e| e.url.as_str()).collect();
    assert_eq!(unique.len(), 500);
    assert_eq!(provider.resolve_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_scripted_failure_produces_the_mapped_error_code() {
    let provider = fixture();

    let u = url("https://fake.test/watch/geo-1");
    let h = Harness::new(&u, "Blocked");
    let p = paths(h.dir.path());
    let err = provider
        .resolve(&u, h.resolve_ctx(&p))
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), ErrorCode::GeoRestricted);
    assert!(!err.retryable());

    let u = url("https://fake.test/watch/flaky-1");
    let mut h = Harness::new(&u, "Flaky");
    let err = provider
        .download(h.download_ctx(), h.factory.for_item(h.id))
        .await
        .expect_err("must fail");
    assert_eq!(err.code(), ErrorCode::Network);
    assert!(err.retryable(), "network is the retryable one");
    // The progress it did report before failing still reached the sink.
    let msgs = h.drain();
    assert_eq!(stages(&msgs), [Stage::Downloading]);
    assert!((percents(&msgs)[0] - 12.5).abs() < 1e-9);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_during_a_wait_returns_canceled_promptly() {
    let provider = Arc::new(fixture());
    let u = url("https://fake.test/watch/slow-2");
    let h = Harness::new(&u, "Slow");
    let cancel = h.cancel.clone();
    let sink = h.factory.for_item(h.id);
    let p = Arc::clone(&provider);
    let task = tokio::spawn(async move { p.download(h.download_ctx(), sink).await });

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let err = task.await.expect("no panic").expect_err("cancel must win");
    assert!(matches!(err, ProviderError::Canceled));
}

#[tokio::test(start_paused = true)]
async fn the_fallback_timeline_serves_every_other_url() {
    let provider = fixture();
    let u = url("https://fake.test/watch/anything-else");
    let mut h = Harness::new(&u, "Anything");
    let outcome = provider
        .download(h.download_ctx(), h.factory.for_item(h.id))
        .await
        .expect("must succeed");
    assert_eq!(
        outcome
            .filename
            .as_ref()
            .map(aulos_core::paths::RelPath::as_str),
        Some("plain.mp4")
    );
    assert_eq!(stages(&h.drain()), [Stage::Downloading]);
}

#[tokio::test(start_paused = true)]
async fn the_built_in_script_needs_no_configuration_at_all() {
    let provider = FakeProvider::new();
    let u = url("https://anything.test/watch/1");
    let mut h = Harness::new(&u, "Hello: World?");
    let p = paths(h.dir.path());

    // Resolve yields exactly one synthetic video entry named after the URL.
    let entries = provider
        .resolve(&u, h.resolve_ctx(&p))
        .await
        .expect("resolve");
    assert_eq!(entries.len(), 1);
    assert_eq!(&*entries[0].title, "1");
    assert_eq!(&*entries[0].media_id, "fake:1");
    assert!(!entries[0].is_playlist());

    let outcome = provider
        .download(h.download_ctx(), h.factory.for_item(h.id))
        .await
        .expect("download");
    // The name is sanitised: `:` and `?` are not legal on every filesystem.
    let name = outcome
        .filename
        .as_ref()
        .map(aulos_core::paths::RelPath::as_str)
        .expect("a filename");
    assert!(name.ends_with(".mp4"), "{name}");
    assert!(!name.contains(':') && !name.contains('?'), "{name}");
    assert!(h.out_dir().join(name).exists());
    assert_eq!(
        stages(&h.drain()),
        [Stage::Preparing, Stage::Downloading],
        "the built-in script reports both running stages"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rust_built_timeline_is_the_terse_path_for_a_unit_test() {
    let provider = FakeProvider::new().with_timeline(Timeline {
        download: vec![
            Step::Stage(Stage::Downloading),
            Step::Wait(Duration::from_secs(60)),
            Step::Progress {
                percent: 25.0,
                speed: Some(1000.0),
                eta: Some(3),
            },
            Step::File {
                slot: FileSlot::Chapter,
                name: "c.mp4".to_owned(),
                size: 8,
            },
            Step::Finish {
                filename: "out.mkv".to_owned(),
                size: 2048,
            },
            Step::Fail(ErrorCode::Internal),
        ],
        ..Timeline::new()
    });
    let u = url("https://any.test/x");
    let mut h = Harness::new(&u, "x");
    let outcome = provider
        .download(h.download_ctx(), h.factory.for_item(h.id))
        .await
        .expect("finish is terminal, so the trailing fail never runs");
    assert_eq!(outcome.size, Some(2048));
    assert_eq!(outcome.chapter_files.len(), 1);
    let msgs = h.drain();
    assert_eq!(percents(&msgs), [25.0]);
    assert_eq!(
        msgs.iter()
            .filter(|m| matches!(m, ProgressMsg::Progress { .. }))
            .count(),
        1
    );
    match &msgs[1] {
        ProgressMsg::Progress { raw, .. } => {
            assert_eq!(
                *raw,
                RawProgress {
                    downloaded_bytes: Some(512.0),
                    total_bytes: Some(2048.0),
                    speed: Some(1000.0),
                    eta: Some(3),
                    ..RawProgress::default()
                }
            );
        }
        other => panic!("expected progress, got {other:?}"),
    }
}

#[tokio::test]
async fn matching_hosts_slots_and_health_come_from_the_document() {
    let provider = FakeProvider::from_toml(
        r#"
id        = "command:fake"
score     = 250
own_slots = 3
health    = "degraded"
health_reason = "pretending to be broken"
hosts     = ["Example.COM", "second.test"]
"#,
    )
    .expect("must parse");
    assert_eq!(
        provider.id(),
        ProviderId::parse("command:fake").expect("id")
    );
    assert_eq!(provider.catalog().provider, "command:fake");
    assert_eq!(provider.own_slots(), Some(3));
    assert_eq!(
        provider.matches(&url("https://www.example.com/x")),
        Match::Strong(250),
        "host matching is case-insensitive and by substring"
    );
    assert_eq!(
        provider.matches(&url("https://second.test/x")),
        Match::Strong(250)
    );
    assert_eq!(provider.matches(&url("https://other.test/x")), Match::No);
    match provider.probe().await {
        ProviderHealth::Degraded(r) => assert_eq!(&*r, "pretending to be broken"),
        other => panic!("expected Degraded, got {other:?}"),
    }

    // With no `hosts`, everything matches — and `strong = false` makes it a fallback.
    let fallback = FakeProvider::from_toml("score = 1\nstrong = false\n").expect("must parse");
    assert_eq!(fallback.matches(&url("https://any.test/x")), Match::Weak(1));
    assert_eq!(fallback.own_slots(), None);
    assert!(fallback.probe().await.is_ok());
}

#[test]
fn a_broken_document_is_a_typed_error_not_a_panic() {
    let err = FakeProvider::from_toml("this is not toml").expect_err("must fail");
    assert!(matches!(err, FakeError::Toml(_)), "{err}");

    let err = FakeProvider::from_toml("[[timeline]]\nurl_regex = \"([\"\n").expect_err("must fail");
    assert!(matches!(err, FakeError::Regex { .. }), "{err}");

    let err = FakeProvider::from_toml("health = \"sideways\"\n").expect_err("must fail");
    assert!(matches!(err, FakeError::Invalid(_)), "{err}");

    let err = FakeProvider::from_toml("id = \"not a valid id\"\n").expect_err("must fail");
    assert!(matches!(err, FakeError::Invalid(_)), "{err}");

    let err = FakeProvider::from_toml("nonsense_key = 1\n").expect_err("unknown keys are rejected");
    assert!(matches!(err, FakeError::Toml(_)), "{err}");

    let err = FakeProvider::from_toml_file(Path::new("/nonexistent/timeline.toml"))
        .expect_err("must fail");
    assert!(matches!(err, FakeError::Io { .. }), "{err}");
}

#[test]
fn the_checked_in_fixture_loads_and_covers_every_scenario_wave_two_needs() {
    let provider = fixture();
    assert_eq!(provider.id(), ProviderId::parse("fake").expect("id"));
    assert_eq!(
        provider.matches(&url("https://fake.test/x")),
        Match::Strong(200)
    );
    assert_eq!(provider.matches(&url("https://youtube.com/x")), Match::No);
    let catalog = provider.catalog();
    assert_eq!(catalog.download_types.len(), 1);
    assert_eq!(&*catalog.download_types[0].id, "video");
}

#[tokio::test(start_paused = true)]
async fn a_degraded_provider_fails_a_job_with_provider_degraded_and_never_falls_through() {
    use std::sync::Arc as StdArc;

    use aulos_provider::registry::Registry;
    use aulos_provider::{DegradedProvider, Match as M};

    // The exact §6.4 wiring: a provider that failed to construct is registered degraded, keeps
    // matching its own URLs, and the *fallback* is registered after it.
    let broken = StdArc::new(DegradedProvider::new(
        ProviderId::parse("streamingcommunity").expect("id"),
        "AULOS_SC_HTTP=impersonate but the feature is off",
        aulos_provider::fake::fake_catalog(),
        Box::new(|u: &Url| {
            if u.host_str().is_some_and(|h| h.contains("sc.test")) {
                M::Strong(200)
            } else {
                M::No
            }
        }),
    ));
    let fallback = StdArc::new(
        FakeProvider::new()
            .with_id("ytdlp")
            .expect("id")
            .with_match(M::Weak(1)),
    );

    let mut registry = Registry::new();
    registry.register_degraded(broken, "AULOS_SC_HTTP=impersonate but the feature is off");
    registry.register(fallback);

    let u = url("https://sc.test/watch/1");
    let selected = registry.pick(&u, None).expect("something must match");
    assert_eq!(selected.id, "streamingcommunity");
    assert!(
        !selected.state.is_ready(),
        "the engine must see the degraded state and stop here"
    );
    assert_eq!(
        selected.runner_up.as_ref().map(|(id, _)| id.as_str()),
        Some("ytdlp"),
        "the runner-up is reported, but a Degraded winner must not fall through to it"
    );

    // And the provider itself refuses every operation with the right code and reason.
    let provider = registry.by_id(&selected.id).expect("registered");
    let mut h = Harness::new(&u, "Blocked");
    let p = paths(h.dir.path());
    let err = provider
        .resolve(&u, h.resolve_ctx(&p))
        .await
        .expect_err("a degraded provider must not resolve");
    assert_eq!(err.code(), ErrorCode::ProviderDegraded);
    assert_eq!(
        err.message(),
        "AULOS_SC_HTTP=impersonate but the feature is off"
    );
    let err = provider
        .download(h.download_ctx(), h.factory.for_item(h.id))
        .await
        .expect_err("a degraded provider must not download");
    assert_eq!(err.code(), ErrorCode::ProviderDegraded);
    assert!(h.drain().is_empty(), "and it must not emit progress");
}
