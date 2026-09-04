//! The actor, end to end against a mocked transport: commands, the `cfg:` grammar, the message →
//! jobs path, the board under the limiter, and the five discrete notifications.
//!
//! No token, no network, no `teloxide::Bot`.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use aulos_core::ItemId;
use aulos_core::config::TelegramBoard;
use aulos_core::event::{DomainEvent, RemoveReason};
use aulos_core::source::{SourceKind, SourceRef};
use aulos_core::status::Status;
use aulos_telegram::{Call, Command, Incoming, MessageId, TelegramConfig, TgInitError};
use support::{CHAT, Harness, OTHER_CHAT, added, changed, completed, tg_view, view};

fn command(chat: i64, command: Command) -> Incoming {
    Incoming::Command { chat, command }
}

fn text(chat: i64, text: &str) -> Incoming {
    Incoming::Text {
        chat,
        text: text.to_owned(),
    }
}

fn callback(chat: i64, data: &str) -> Incoming {
    Incoming::Callback {
        chat,
        message: MessageId(500),
        query_id: "q1".to_owned(),
        data: data.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_replies_with_the_byte_identical_greeting() {
    let mut h = Harness::new().await;
    h.handle(command(CHAT, Command::Start)).await;

    assert_eq!(h.transport.count(), 1);
    assert_eq!(
        h.transport.texts()[0],
        "Hi! Send one or more links and I will queue them for download.\n\
         Use /config to set default format/quality for this chat."
    );
    assert_eq!(h.transport.calls()[0].chat(), Some(CHAT));
    assert!(
        h.transport.calls()[0].keyboard().is_none(),
        "/start has no keyboard"
    );
}

#[tokio::test]
async fn config_replies_with_the_body_and_the_main_keyboard() {
    let mut h = Harness::new().await;
    h.handle(command(CHAT, Command::Config)).await;

    assert_eq!(
        h.transport.texts()[0],
        "Current download config:\n\
         - Format: mp4\n\
         - Quality: best\n\
         - Split by chapters: off\n\
         - Playlist item limit: 0"
    );
    let keyboard = h.transport.calls()[0]
        .keyboard()
        .expect("the main keyboard")
        .clone();
    assert_eq!(
        keyboard.labels(),
        vec![
            "Format: mp4",
            "Quality: best",
            "Split Chapters: off",
            "Playlist Limit: 0"
        ]
    );
}

/// Legacy `_get_chat_config` created **and persisted** the defaults on first access.
#[tokio::test]
async fn a_chats_defaults_are_persisted_on_first_access() {
    let mut h = Harness::new().await;
    assert!(h.chat_configs().await.is_empty());
    h.handle(command(CHAT, Command::Config)).await;

    let stored = h.chat_configs().await;
    let cfg = stored.get(&CHAT).expect("the chat was persisted");
    assert_eq!(&*cfg.format, "mp4");
    assert_eq!(&*cfg.quality, "best");
    assert_eq!(stored.len(), 1);
}

/// DESIGN §12.2: an unauthorised chat is silently ignored plus one WARN log line.
#[tokio::test]
async fn an_unauthorised_chat_is_silently_ignored() {
    let mut h = Harness::new().await;
    for update in [
        command(999, Command::Start),
        command(999, Command::Config),
        text(999, "https://a.test/1"),
        callback(999, "cfg:menu:main"),
    ] {
        h.handle(update).await;
    }
    assert_eq!(h.transport.count(), 0, "not one API call");
    assert!(h.items().await.is_empty(), "and nothing queued");
    assert!(h.chat_configs().await.is_empty(), "and nothing persisted");
}

// ---------------------------------------------------------------------------
// the cfg: grammar
// ---------------------------------------------------------------------------

/// Every callback answers the query first, then edits the message it came from.
#[tokio::test]
async fn a_callback_answers_the_query_then_edits_in_place() {
    let mut h = Harness::new().await;
    h.handle(callback(CHAT, "cfg:menu:format")).await;

    let calls = h.transport.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        matches!(&calls[0], Call::Answer { query_id } if query_id == "q1"),
        "the query is answered first: {calls:?}"
    );
    match &calls[1] {
        Call::Edit {
            chat,
            message,
            text,
            keyboard,
        } => {
            assert_eq!(*chat, CHAT);
            assert_eq!(*message, MessageId(500), "the keyboard's own message");
            assert_eq!(text, "Select format");
            let k = keyboard.as_ref().expect("the format keyboard");
            assert_eq!(
                k.labels(),
                vec![
                    "any",
                    "mp4",
                    "ios",
                    "m4a",
                    "mp3",
                    "opus",
                    "wav",
                    "flac",
                    "thumbnail",
                    "Back",
                ]
            );
        }
        other => panic!("expected an edit, got {other:?}"),
    }
}

/// The four screens, each with its byte-identical prompt.
#[tokio::test]
async fn the_four_screens_have_their_legacy_prompts() {
    let mut h = Harness::new().await;
    for (data, want) in [
        ("cfg:menu:format", "Select format"),
        ("cfg:menu:quality", "Select quality"),
        ("cfg:menu:limit", "Select playlist limit"),
    ] {
        h.transport.clear();
        h.handle(callback(CHAT, data)).await;
        assert_eq!(h.transport.texts(), vec![want.to_owned()], "{data}");
    }
    h.transport.clear();
    h.handle(callback(CHAT, "cfg:menu:main")).await;
    assert!(h.transport.texts()[0].starts_with("Current download config:"));
}

/// DESIGN §12.2: changing the format resets a quality the new format does not offer, and the
/// change is persisted.
#[tokio::test]
async fn changing_the_format_resets_the_quality_and_persists() {
    let mut h = Harness::new().await;
    h.handle(callback(CHAT, "cfg:set:quality:1080")).await;
    h.handle(callback(CHAT, "cfg:set:format:mp3")).await;

    let stored = h.chat_configs().await;
    let cfg = stored.get(&CHAT).expect("persisted");
    assert_eq!(&*cfg.format, "mp3");
    assert_eq!(&*cfg.quality, "best", "1080 is not an mp3 quality");

    // And the rendered screen shows it.
    assert!(
        h.transport
            .texts()
            .last()
            .expect("a render")
            .contains("- Format: mp3")
    );
}

/// An out-of-list quality is ignored, exactly as legacy's `if value in available`.
#[tokio::test]
async fn an_out_of_list_quality_is_ignored() {
    let mut h = Harness::new().await;
    h.handle(callback(CHAT, "cfg:set:quality:9999")).await;
    let cfg = h.chat_configs().await;
    assert_eq!(
        cfg.get(&CHAT).map(|c| c.quality.to_string()),
        Some("best".to_owned())
    );
}

/// `cfg:set:quality:audio` on `any` stores the legacy pair and normalises to `(audio, m4a, best)`.
#[tokio::test]
async fn the_audio_pseudo_quality_round_trips_through_the_store() {
    let mut h = Harness::new().await;
    h.handle(callback(CHAT, "cfg:set:format:any")).await;
    h.handle(callback(CHAT, "cfg:set:quality:audio")).await;

    let stored = h.chat_configs().await;
    let cfg = stored.get(&CHAT).expect("persisted");
    assert_eq!((&*cfg.format, &*cfg.quality), ("any", "audio"));

    let selection = cfg.selection();
    assert_eq!(
        selection.download_type,
        aulos_core::selection::DownloadType::Audio
    );
    assert_eq!(selection.format.as_str(), "m4a");
    assert_eq!(selection.quality.as_str(), "best");

    // And a link now queues as audio.
    h.handle(text(CHAT, "https://a.test/1")).await;
    let rows = h.wait_for_items(1).await;
    assert_eq!(
        rows[0].request.selection.download_type,
        aulos_core::selection::DownloadType::Audio
    );
    assert_eq!(rows[0].request.selection.format.as_str(), "m4a");
}

#[tokio::test]
async fn the_split_toggle_and_the_limit_persist() {
    let mut h = Harness::new().await;
    h.handle(callback(CHAT, "cfg:toggle:split")).await;
    h.handle(callback(CHAT, "cfg:set:limit:20")).await;

    let stored = h.chat_configs().await;
    let cfg = stored.get(&CHAT).expect("persisted");
    assert!(cfg.split_by_chapters);
    assert_eq!(cfg.playlist_item_limit, 20);
    let last = h.transport.texts().last().cloned().expect("a render");
    assert!(last.contains("- Split by chapters: on"));
    assert!(last.contains("- Playlist item limit: 20"));
}

/// An unrecognised payload answers the query and renders nothing, as legacy did.
#[tokio::test]
async fn an_unknown_callback_answers_but_renders_nothing() {
    let mut h = Harness::new().await;
    h.handle(callback(CHAT, "cfg:menu:download_type")).await;
    let calls = h.transport.calls();
    assert_eq!(calls.len(), 1);
    assert!(matches!(calls[0], Call::Answer { .. }));
}

// ---------------------------------------------------------------------------
// message → jobs
// ---------------------------------------------------------------------------

/// DESIGN §12.3 step 5: **one** `Add` with three requests, all attributed to the chat.
#[tokio::test]
async fn one_message_with_three_urls_produces_one_add_of_three_telegram_jobs() {
    let mut h = Harness::new().await;
    h.handle(text(
        CHAT,
        "grab these https://a.test/1 https://a.test/2 https://a.test/3 thanks",
    ))
    .await;

    let rows = h.wait_for_items(3).await;
    assert_eq!(rows.len(), 3);
    // One `Add` means one contiguous `ord` run and one shared attribution.
    for row in &rows {
        assert_eq!(row.source.kind, SourceKind::Telegram);
        assert_eq!(row.source.reference.as_deref(), Some("4242"));
    }
    let mut ords: Vec<i64> = rows.iter().map(|r| r.ord).collect();
    ords.sort_unstable();
    assert!(
        ords.windows(2).all(|w| w[1] == w[0] + 1),
        "one batch, so the ords are contiguous: {ords:?}"
    );

    assert_eq!(
        h.transport.texts(),
        vec!["Queued 3 link(s) with current chat config.".to_owned()]
    );
}

#[tokio::test]
async fn a_message_with_no_links_says_nothing_at_all() {
    let mut h = Harness::new().await;
    h.handle(text(CHAT, "how far along is it?")).await;
    assert_eq!(h.transport.count(), 0);
    assert!(h.items().await.is_empty());
}

/// DESIGN §12.3 step 2: the exact message, then the truncated list is queued.
#[tokio::test]
async fn over_the_max_urls_limit_the_message_is_exact_and_the_rest_are_dropped() {
    let mut h = Harness::new().await;
    let body = (1..=14)
        .map(|i| format!("https://a.test/{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    h.handle(text(CHAT, &body)).await;

    let texts = h.transport.texts();
    assert_eq!(
        texts[0],
        "Too many links in one message (14). Maximum allowed: 10."
    );
    assert_eq!(texts[1], "Queued 10 link(s) with current chat config.");
    let rows = h.wait_for_items(10).await;
    assert_eq!(rows.len(), 10);
}

/// DESIGN §12.3 step 3: the guard's rejections are reported with their reasons and nothing else is
/// lost.
#[tokio::test]
async fn rejected_urls_are_reported_and_the_good_ones_still_queue() {
    let mut h = Harness::new().await;
    h.handle(text(
        CHAT,
        "https://a.test/ok http://127.0.0.1/x http://nas.local/y ftp://a.test/z",
    ))
    .await;

    let texts = h.transport.texts();
    assert_eq!(
        texts[0],
        "Ignored invalid links:\n\
         - http://127.0.0.1/x (private/local IP targets are not allowed)\n\
         - http://nas.local/y (local network hosts are not allowed)"
    );
    assert_eq!(texts[1], "Queued 1 link(s) with current chat config.");
    let rows = h.wait_for_items(1).await;
    assert_eq!(rows[0].url.as_str(), "https://a.test/ok");
}

#[tokio::test]
async fn a_message_of_only_bad_links_queues_nothing_and_says_why() {
    let mut h = Harness::new().await;
    h.handle(text(CHAT, "http://localhost:8081/x")).await;
    assert_eq!(
        h.transport.texts(),
        vec![
            "Ignored invalid links:\n- http://localhost:8081/x (local network hosts are not allowed)"
                .to_owned()
        ]
    );
    assert!(h.items().await.is_empty());
}

// ---------------------------------------------------------------------------
// the board
// ---------------------------------------------------------------------------

/// The first tick draws the board; a later tick with newer data edits it in place.
#[tokio::test]
async fn the_board_is_created_once_and_edited_afterwards() {
    let mut h = Harness::new().await;
    let id = ItemId::new();
    let mut v = tg_view(id, "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;

    let calls = h.transport.calls();
    assert_eq!(calls.len(), 1);
    assert!(matches!(calls[0], Call::Send { .. }), "created: {calls:?}");
    assert!(calls[0].text().starts_with("⬇️ Aulos — 1 active, 0 done"));
    assert!(calls[0].text().contains("A clip"));

    // Newer data, a full interval later.
    v.percent = 55.0;
    h.observe(&changed(&v, Status::Downloading)).await;
    h.advance(Duration::from_millis(3_000)).await;
    let edits = h.transport.edits();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].text().contains("55%"));
}

/// DESIGN §12.4: an unchanged render issues **no** API call — Telegram answers an unmodified edit
/// with a 400, and that 400 counts against the chat's rate budget.
#[tokio::test]
async fn an_unchanged_render_issues_no_api_call() {
    let mut h = Harness::new().await;
    let v = tg_view(ItemId::new(), "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;
    assert_eq!(h.transport.count(), 1);

    // Re-observe the identical view, wait out the interval, tick again: nothing to say.
    for _ in 0..4 {
        h.observe(&changed(&v, Status::Downloading)).await;
        h.advance(Duration::from_millis(4_000)).await;
    }
    assert_eq!(h.transport.count(), 1, "still just the first send");
}

/// DESIGN §12.4: at most one edit per `AULOS_TELEGRAM_EDIT_INTERVAL_MS` per chat.
#[tokio::test]
async fn edits_are_at_most_one_per_interval_per_chat() {
    let mut h = Harness::new().await;
    let id = ItemId::new();
    let mut v = tg_view(id, "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;
    h.transport.clear();

    // Twelve seconds of one-second ticks, each with genuinely new data: four intervals fit.
    for i in 1..=12 {
        v.percent = f64::from(i) * 5.0;
        h.observe(&changed(&v, Status::Downloading)).await;
        h.advance(Duration::from_secs(1)).await;
    }
    let edits = h.transport.edits().len();
    assert!(
        (3..=4).contains(&edits),
        "expected 3-4 edits in 12 s at a 3 s interval, got {edits}"
    );
    assert!(
        h.actor.health().edits_throttled_total >= 8,
        "the deferred ticks are counted: {:?}",
        h.actor.health()
    );
}

/// DESIGN §12.4: `RetryAfter(7)` sleeps and doubles the chat's interval; three successes halve it.
#[tokio::test(start_paused = true)]
async fn a_retry_after_doubles_the_interval_and_successes_halve_it_back() {
    let mut h = Harness::new().await;
    let id = ItemId::new();
    let mut v = tg_view(id, "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;

    // The very first draw is refused with a 429.
    h.transport
        .fail_next(vec![aulos_telegram::TgError::RetryAfter(
            Duration::from_secs(7),
        )]);
    h.tick().await;
    assert_eq!(h.transport.count(), 0, "the send was refused");

    // A 3 s interval has become 6 s, so a 3 s wait is no longer enough.
    v.percent = 10.0;
    h.observe(&changed(&v, Status::Downloading)).await;
    h.advance(Duration::from_millis(3_000)).await;
    assert_eq!(
        h.transport.count(),
        0,
        "the 7.25 s back-off has not elapsed"
    );
    h.advance(Duration::from_millis(4_500)).await;
    assert_eq!(h.transport.count(), 1, "…now it has");

    // Three successes in a row halve it back to the base.
    for i in 1..=3_u32 {
        v.percent = 20.0 + f64::from(i);
        h.observe(&changed(&v, Status::Downloading)).await;
        h.advance(Duration::from_millis(6_500)).await;
    }
    v.percent = 90.0;
    h.observe(&changed(&v, Status::Downloading)).await;
    let before = h.transport.count();
    h.advance(Duration::from_millis(3_100)).await;
    assert_eq!(
        h.transport.count(),
        before + 1,
        "the interval is back to 3 s"
    );
}

/// A group row carries `[done/total]`; a plain row carries a byte rate.
#[tokio::test]
async fn a_group_row_shows_its_aggregate() {
    let mut h = Harness::new().await;
    let mut v = tg_view(ItemId::new(), "Lo-fi beats", Status::Downloading, CHAT);
    v.kind = aulos_core::item::Kind::Group;
    v.children_total = Some(500);
    v.children_done = Some(12);
    v.percent = 21.0;
    v.speed = Some(1_468_006.0);
    h.observe(&added(&v)).await;
    h.tick().await;

    let text = h.transport.calls()[0].text().to_owned();
    assert!(text.contains("Lo-fi beats [12/500]"), "{text}");
    assert!(!text.contains("MB/s"), "a group has no single rate: {text}");
}

/// A mixed board, snapshot-tested through the actor: active, queued, group, terminal and overflow.
#[tokio::test]
async fn the_board_snapshot_covers_a_mixed_burst_and_the_overflow() {
    let mut h = Harness::new().await;

    let mut active = tg_view(
        ItemId::new(),
        "Rick Astley - Never Gonna Give You Up",
        Status::Downloading,
        CHAT,
    );
    active.percent = 68.0;
    active.speed = Some(3_250_586.0);
    active.eta = Some(41);

    let mut group = tg_view(ItemId::new(), "Lo-fi beats", Status::Downloading, CHAT);
    group.kind = aulos_core::item::Kind::Group;
    group.children_total = Some(500);
    group.children_done = Some(12);
    group.percent = 21.0;

    let queued = tg_view(ItemId::new(), "Big Buck Bunny", Status::Queued, CHAT);
    let mut done = tg_view(
        ItemId::new(),
        "Veritasium - The Big Misconception",
        Status::Finished,
        CHAT,
    );
    done.percent = 100.0;

    for v in [&active, &group, &queued, &done] {
        h.observe(&added(v)).await;
    }
    h.tick().await;
    assert_eq!(
        h.transport.calls()[0].text(),
        "⬇️ Aulos — 2 active, 1 done\n\
         \n\
         ▓▓▓▓▓▓▓░░░   68%  Rick Astley - Never Gonna Give You…\n\
         \u{20}             3.1 MB/s · ETA 0:41\n\
         ▓▓░░░░░░░░   21%  Lo-fi beats [12/500]\n\
         ⏳  0%  Big Buck Bunny  (queued)\n\
         ✅  Veritasium - The Big Misconception\n\
         \n\
         updated 00:00:00"
    );

    // Sixteen more jobs push it past the twelve-line cap.
    for i in 0..16 {
        let v = tg_view(ItemId::new(), &format!("Job {i}"), Status::Queued, CHAT);
        h.observe(&added(&v)).await;
    }
    h.advance(Duration::from_millis(3_100)).await;
    let text = h
        .transport
        .edits()
        .last()
        .expect("an edit")
        .text()
        .to_owned();
    assert!(text.contains("… +8 more"), "{text}");
    assert!(!text.contains("Job 8"), "the thirteenth line is collapsed");
}

/// `AULOS_TELEGRAM_BOARD=per_job` is the escape hatch: no board, only the discrete messages.
#[tokio::test]
async fn per_job_mode_draws_no_board() {
    let mut h = Harness::builder()
        .telegram(TelegramConfig {
            board: TelegramBoard::PerJob,
            ..TelegramConfig::for_test(vec![CHAT])
        })
        .build()
        .await;

    let mut v = tg_view(ItemId::new(), "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;
    assert_eq!(h.transport.count(), 0, "no board in per_job mode");

    v.status = Status::Finished;
    v.filename = Some("A clip.mp4".into());
    h.observe(&completed(&v)).await;
    assert_eq!(
        h.transport.texts(),
        vec!["✅ Download complete: A clip\nFile: A clip.mp4".to_owned()],
        "the discrete message still fires"
    );
}

/// A removed item leaves the board.
#[tokio::test]
async fn a_removed_item_leaves_the_board() {
    let mut h = Harness::new().await;
    let v = tg_view(ItemId::new(), "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;
    assert_eq!(h.actor.health().watched_jobs, 1);

    h.transport.clear();
    h.observe(&DomainEvent::Removed {
        ids: vec![v.id],
        reason: RemoveReason::Deleted,
    })
    .await;
    assert_eq!(h.actor.health().watched_jobs, 0, "the watch is dropped");

    // An empty board is not redrawn — there is nothing to draw — and it retires on its own.
    h.advance(Duration::from_millis(3_100)).await;
    assert_eq!(h.transport.count(), 0, "no edit for an empty board");
    h.advance(Duration::from_secs(61)).await;
    assert_eq!(h.actor.health().boards, 0, "the board is forgotten");
    assert!(
        h.transport.texts().iter().all(|t| !t.contains("A clip")),
        "and the removed row is never shown again: {:?}",
        h.transport.texts()
    );
}

/// Sixty seconds after the last job ends, the board becomes its summary.
#[tokio::test]
async fn the_board_retires_into_a_summary() {
    let mut h = Harness::new().await;
    let mut ok = tg_view(ItemId::new(), "Good", Status::Downloading, CHAT);
    let mut bad = tg_view(ItemId::new(), "Bad", Status::Downloading, CHAT);
    h.observe(&added(&ok)).await;
    h.observe(&added(&bad)).await;
    h.tick().await;

    ok.status = Status::Finished;
    ok.percent = 100.0;
    bad.status = Status::Error;
    h.observe(&completed(&ok)).await;
    h.observe(&completed(&bad)).await;
    h.transport.clear();

    h.advance(Duration::from_secs(61)).await;
    let texts = h.transport.texts();
    assert!(
        texts
            .iter()
            .any(|t| t == "✅ 1 download finished · ❌ 1 failed"),
        "expected the summary, got {texts:?}"
    );
    assert_eq!(h.actor.health().boards, 0, "the board is forgotten");
}

// ---------------------------------------------------------------------------
// notifications and the watchdogs
// ---------------------------------------------------------------------------

/// DESIGN §12.5: each discrete message fires **exactly once** per chat per job.
#[tokio::test]
async fn the_terminal_messages_fire_exactly_once_per_chat_per_job() {
    let mut h = Harness::builder()
        .telegram(TelegramConfig {
            board: TelegramBoard::PerJob,
            ..TelegramConfig::for_test(vec![CHAT])
        })
        .build()
        .await;

    let mut ok = tg_view(ItemId::new(), "Good", Status::Downloading, CHAT);
    let mut bad = tg_view(ItemId::new(), "Bad", Status::Downloading, CHAT);
    let mut gone = tg_view(ItemId::new(), "Dropped", Status::Downloading, CHAT);
    for v in [&ok, &bad, &gone] {
        h.observe(&added(v)).await;
    }

    ok.status = Status::Finished;
    ok.filename = Some("Good.mp4".into());
    bad.status = Status::Error;
    bad.msg = Some("HTTP 403".into());
    gone.status = Status::Canceled;

    // Deliver each `Completed` twice; the watch is dropped on the first, so the second is a no-op.
    for _ in 0..2 {
        h.observe(&completed(&ok)).await;
        h.observe(&completed(&bad)).await;
        h.observe(&completed(&gone)).await;
    }

    assert_eq!(
        h.transport.texts(),
        vec![
            "✅ Download complete: Good\nFile: Good.mp4".to_owned(),
            "❌ Download failed: Bad\nHTTP 403".to_owned(),
        ],
        "a cancellation is silent (parity), and nothing repeats"
    );
}

/// A failure with neither `msg` nor `error` falls back to the legacy text.
#[tokio::test]
async fn a_failure_with_no_message_uses_the_legacy_fallback() {
    let mut h = Harness::builder()
        .telegram(TelegramConfig {
            board: TelegramBoard::PerJob,
            ..TelegramConfig::for_test(vec![CHAT])
        })
        .build()
        .await;
    let mut v = tg_view(ItemId::new(), "Bad", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    v.status = Status::Error;
    h.observe(&completed(&v)).await;
    assert_eq!(
        h.transport.texts(),
        vec!["❌ Download failed: Bad\nDownload failed".to_owned()]
    );
}

/// DESIGN §12.5: the two warnings are separate messages, fire once per chat per job, and neither
/// cancels the download.
#[tokio::test]
async fn the_two_warnings_fire_once_and_mark_the_board_line() {
    let mut h = Harness::new().await;
    let v = tg_view(ItemId::new(), "Stuck", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;
    h.transport.clear();

    h.advance(Duration::from_secs(181)).await;
    let texts = h.transport.texts();
    assert!(
        texts
            .iter()
            .any(|t| t == "⚠️ Download seems stalled for 181s:\nhttps://a.test/watch/1"),
        "{texts:?}"
    );

    // Once only.
    h.transport.clear();
    h.advance(Duration::from_secs(60)).await;
    assert!(
        !h.transport.texts().iter().any(|t| t.contains("stalled")),
        "the stall warning must not repeat"
    );

    // The hard timeout is its own message, and the board line gains its marker.
    h.transport.clear();
    h.advance(Duration::from_secs(7_201)).await;
    let texts = h.transport.texts();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("⏱️ Download is taking longer than expected (")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("⏱️") && t.contains("Stuck")),
        "the board line carries the marker: {texts:?}"
    );
    assert_eq!(
        h.actor.health().watched_jobs,
        1,
        "neither warning cancels the download"
    );
}

/// Progress resets the stall clock, so a slow-but-moving download is never called stalled.
#[tokio::test]
async fn progress_keeps_the_stall_warning_away() {
    let mut h = Harness::new().await;
    let mut v = tg_view(ItemId::new(), "Slow", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;

    for i in 1..=10 {
        v.percent = f64::from(i);
        h.observe(&changed(&v, Status::Downloading)).await;
        h.advance(Duration::from_secs(100)).await;
    }
    assert!(
        !h.transport.texts().iter().any(|t| t.contains("stalled")),
        "1000 s of steady progress is not a stall"
    );
}

// ---------------------------------------------------------------------------
// AULOS_TELEGRAM_WATCH_ALL
// ---------------------------------------------------------------------------

/// BRIEF: `false` reproduces the legacy blind spot; `true` reports an API-sourced job.
#[tokio::test]
async fn watch_all_decides_whether_an_api_job_is_reported() {
    for (watch_all, expect) in [(false, 0_usize), (true, 1)] {
        let mut h = Harness::builder()
            .telegram(TelegramConfig {
                watch_all,
                ..TelegramConfig::for_test(vec![CHAT])
            })
            .build()
            .await;
        let v = view(
            ItemId::new(),
            "From the web",
            Status::Downloading,
            SourceRef::bare(SourceKind::ApiV2),
        );
        h.observe(&added(&v)).await;
        h.tick().await;
        assert_eq!(
            h.transport.count(),
            expect,
            "AULOS_TELEGRAM_WATCH_ALL={watch_all}"
        );
        assert_eq!(h.actor.health().watched_jobs, expect);
    }
}

/// With `watch_all` on, a non-Telegram job fans out to every allowed chat; a Telegram job does not.
#[tokio::test]
async fn watch_all_fans_out_while_a_telegram_job_stays_in_its_chat() {
    let mut h = Harness::new().await;

    let api = view(
        ItemId::new(),
        "From the web",
        Status::Downloading,
        SourceRef::bare(SourceKind::ApiV2),
    );
    h.observe(&added(&api)).await;
    h.tick().await;
    let chats: Vec<i64> = h.transport.calls().iter().filter_map(Call::chat).collect();
    assert_eq!(chats.len(), 2, "one board per allowed chat: {chats:?}");
    assert!(chats.contains(&CHAT) && chats.contains(&OTHER_CHAT));

    h.transport.clear();
    let tg = tg_view(ItemId::new(), "From the bot", Status::Downloading, CHAT);
    h.observe(&added(&tg)).await;
    h.advance(Duration::from_millis(3_100)).await;
    let touched: Vec<i64> = h.transport.calls().iter().filter_map(Call::chat).collect();
    assert!(
        touched.iter().all(|c| *c == CHAT) || touched.contains(&CHAT),
        "the bot's own job reaches the asking chat: {touched:?}"
    );
}

// ---------------------------------------------------------------------------
// the loop, and the startup gate
// ---------------------------------------------------------------------------

/// The real `select!` loop, driven through the channel the polling adapter would use.
#[tokio::test]
async fn the_spawned_loop_handles_an_update_from_the_channel() {
    let h = Harness::new().await;
    let transport = Arc::clone(&h.transport);
    let (tx, task) = h.spawn();

    tx.send(command(CHAT, Command::Start))
        .await
        .expect("the actor is listening");
    for _ in 0..200 {
        if transport.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(transport.count(), 1);
    assert!(transport.texts()[0].starts_with("Hi! Send one or more links"));
    task.abort();
}

/// `TelegramHealthHandle` keeps reporting after `spawn` has consumed the actor.
///
/// Without it `healthz.components.telegram` freezes at its boot values, because `health()` needs
/// `&self` and `spawn` takes the actor by value.
#[tokio::test]
async fn the_health_handle_still_reports_after_spawn_consumes_the_actor() {
    let mut h = Harness::new().await;
    // One watched job, so the numbers are non-zero and cannot be confused with the boot state.
    let id = ItemId::new();
    let v = tg_view(id, "A clip", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;

    let health = h.actor.health_handle();
    let before = h.actor.health();
    assert_eq!(before.watched_jobs, 1);
    assert_eq!(before.boards, 1);

    let (tx, task) = h.spawn();
    // The loop republishes on every pass, so one tick of its own 1 Hz timer is enough.
    for _ in 0..400 {
        if health.health().watched_jobs == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let live = health.health();
    assert!(live.enabled);
    assert_eq!(live.watched_jobs, 1, "read through the handle, after spawn");
    assert_eq!(live.boards, 1);
    drop(tx);
    task.abort();
}

/// `TelegramActor::new` is the only place that owns the token, so it is the only place that can
/// hand the long-polling loop a bot; `with_transport` has neither.
#[tokio::test]
async fn only_the_token_owning_constructor_exposes_a_bot() {
    let h = Harness::new().await;
    assert!(
        h.actor.bot().is_none(),
        "a mocked transport has no teloxide bot to poll"
    );

    let tg = TelegramConfig {
        token: "123456:not-a-real-token-and-never-used".into(),
        ..TelegramConfig::for_test(vec![CHAT])
    };
    let actor = aulos_telegram::TelegramActor::new(
        Arc::new(tg),
        h.store.clone(),
        h.engine.clone(),
        Arc::new(aulos_core::catalog::ytdlp_catalog()),
        Arc::clone(&h.clock) as Arc<dyn aulos_core::Clock>,
    )
    .expect("the three startup gates all pass");
    assert!(
        actor.bot().is_some(),
        "`poll_updates` needs this one, and nothing else can build it without the token"
    );
}

/// DESIGN §12.1: three startup gates, each a silent no-op with one log line.
#[tokio::test]
async fn the_startup_gates_refuse_to_build_an_actor() {
    use aulos_core::catalog::ytdlp_catalog;
    use aulos_core::{Clock, FakeClock};

    let h = Harness::new().await;
    let clock = Arc::new(FakeClock::default()) as Arc<dyn Clock>;
    let catalog = Arc::new(ytdlp_catalog());
    let transport = aulos_telegram::MockTransport::new() as Arc<dyn aulos_telegram::Transport>;

    let disabled = TelegramConfig {
        enabled: false,
        ..TelegramConfig::for_test(vec![CHAT])
    };
    assert_eq!(
        aulos_telegram::TelegramActor::with_transport(
            Arc::new(disabled),
            h.store.clone(),
            h.engine.clone(),
            Arc::clone(&catalog),
            Arc::clone(&clock),
            Arc::clone(&transport),
        )
        .err(),
        Some(TgInitError::Disabled)
    );

    let no_chats = TelegramConfig::for_test(Vec::new());
    assert_eq!(
        aulos_telegram::TelegramActor::with_transport(
            Arc::new(no_chats),
            h.store.clone(),
            h.engine.clone(),
            Arc::clone(&catalog),
            Arc::clone(&clock),
            Arc::clone(&transport),
        )
        .err(),
        Some(TgInitError::NoAllowedChats)
    );

    // The token gate lives on `new`, which is the only constructor that needs one.
    let no_token = TelegramConfig {
        token: "   ".to_owned(),
        ..TelegramConfig::for_test(vec![CHAT])
    };
    assert_eq!(
        aulos_telegram::TelegramActor::new(
            Arc::new(no_token),
            h.store.clone(),
            h.engine.clone(),
            catalog,
            clock,
        )
        .err(),
        Some(TgInitError::MissingToken)
    );
}
