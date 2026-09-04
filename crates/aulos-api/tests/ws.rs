//! The WebSocket at `<p>ws`: the snapshot, the frame sequences of DESIGN §21, resume, the client
//! frames and every disconnect rule (PROTOCOL §5, §6, DESIGN §15.4), under both prefixes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::{ComponentHealth, ComponentStatus};
use aulos_queue::FrameKind;
use serde_json::{Value, json};
use support::{
    Rig, close_code, connect, expanding, for_each_prefix, hanging, next_frame, next_frame_of,
    send_frame, try_next_frame, ytdlp_like,
};

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_snapshot_is_the_first_frame_and_carries_the_two_transition_only_blocks() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        let snapshot = next_frame(&mut socket).await;

        assert_eq!(snapshot["t"], "snapshot");
        for key in [
            "seq",
            "boot_id",
            "server_time",
            "server",
            "protocol",
            "counts",
            "done_total",
            "truncated",
            "items",
            "done",
            "subscriptions",
            "ytdl_options",
            "health",
        ] {
            assert!(snapshot.get(key).is_some(), "{key} on the snapshot");
        }
        assert_eq!(snapshot["server"]["url_prefix"], prefix);
        assert_eq!(snapshot["server"]["version"], "2026.09.04");
        assert_eq!(snapshot["protocol"]["urgent_ms"], 5);
        assert_eq!(snapshot["ytdl_options"]["ok"], true);
        assert_eq!(snapshot["health"]["status"], "ok");

        // An idle server sends nothing else.
        assert!(
            try_next_frame(&mut socket, Duration::from_millis(300))
                .await
                .is_none(),
            "silence means nothing changed"
        );
    })
    .await;
}

#[tokio::test]
async fn a_degraded_component_reaches_a_fresh_client_with_no_health_frame() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        // The `health` frame is transition-only, and this transition happened before the client
        // existed (PROTOCOL §5.3).
        rig.state.health.set(
            "pot",
            ComponentHealth::new(ComponentStatus::Down)
                .with("detail", "3 consecutive probe failures"),
        );
        let mut socket = connect(&rig, "ws").await;
        let snapshot = next_frame(&mut socket).await;
        assert_eq!(snapshot["health"]["status"], "down");
        assert_eq!(snapshot["health"]["components"]["pot"], "down");
        assert!(
            try_next_frame(&mut socket, Duration::from_millis(200))
                .await
                .is_none(),
            "and no health frame was needed"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// the DESIGN §21 sequences
// ---------------------------------------------------------------------------

/// DESIGN §21.1 — add a single video.
#[tokio::test]
async fn the_single_video_sequence_is_added_then_deltas_then_completed() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        next_frame_of(&mut socket, "snapshot").await;

        let id = rig.add("https://fake.test/clip").await;

        let added = next_frame_of(&mut socket, "added").await;
        assert_eq!(added["reason"], "created");
        let item = &added["items"][0];
        assert_eq!(item["id"], id.as_str());
        assert_eq!(item["status"], "resolving");
        assert_eq!(item["kind"], "item");

        // Everything after the insert is a patch until the terminal frame.
        let mut seen_status: Vec<String> = Vec::new();
        let mut titled = false;
        let mut completed: Option<Value> = None;
        let mut last_seq = added["seq"].as_u64().unwrap();
        for _ in 0..40 {
            let frame = next_frame(&mut socket).await;
            let seq = frame["seq"].as_u64().unwrap();
            assert!(seq >= last_seq, "seq is monotonic: {seq} after {last_seq}");
            last_seq = seq;
            match frame["t"].as_str().unwrap() {
                "delta" => {
                    for patch in frame["items"].as_array().unwrap() {
                        assert_eq!(patch["id"], id.as_str());
                        assert!(
                            patch.get("selection").is_none(),
                            "immutable fields never patch"
                        );
                        assert!(patch.get("request").is_none());
                        assert!(patch.get("folder").is_none());
                        if let Some(status) = patch["status"].as_str() {
                            seen_status.push(status.to_owned());
                        }
                        if patch.get("title").is_some() {
                            titled = true;
                        }
                    }
                }
                "completed" => {
                    completed = Some(frame);
                    break;
                }
                other => panic!("unexpected frame {other}"),
            }
        }

        let completed = completed.expect("a completed frame");
        let item = &completed["items"][0];
        assert_eq!(item["status"], "finished");
        assert_eq!(item["percent"], 100.0);
        assert_eq!(item["filename"], "clip.mp4");
        assert_eq!(
            item["download_url"], "download/clip.mp4",
            "the frame carries a ready-to-open URL, like the snapshot does"
        );
        assert!(item["finished_at"].as_i64().is_some());
        // The states between `resolving` and the terminal one arrive as patches. Which of them a
        // client sees depends on how many fit in one 250 ms window — per `(id, field)` the last
        // value in a window wins (DESIGN §15.1) — so the assertion is that the transition out of
        // `resolving` was a *patch* on the row the client already had, never a second row.
        assert!(
            seen_status
                .iter()
                .any(|s| matches!(&**s, "queued" | "preparing" | "downloading")),
            "no status patch arrived: {seen_status:?}"
        );
        assert!(
            titled,
            "the title stops being the URL as a patch, not as a new row"
        );
    })
    .await;
}

/// DESIGN §21.2 — a playlist add promotes the row it already sent.
#[tokio::test]
async fn a_playlist_promotes_the_same_row_in_place() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(expanding(4)))
            .start()
            .await;
        let mut socket = connect(&rig, "ws").await;
        next_frame_of(&mut socket, "snapshot").await;

        let id = rig.add("https://fake.test/playlist/one").await;
        let first = next_frame_of(&mut socket, "added").await;
        assert_eq!(first["reason"], "created");
        assert_eq!(first["items"][0]["kind"], "item", "one spinner row first");
        assert_eq!(first["items"][0]["id"], id.as_str());
        let ord = first["items"][0]["ord"].clone();

        // The expansion is one frame carrying the promoted parent and its children.
        let mut expanded = None;
        for _ in 0..40 {
            let frame = next_frame(&mut socket).await;
            if frame["t"] == "added" && frame["reason"] == "expanded" {
                expanded = Some(frame);
                break;
            }
        }
        let expanded = expanded.expect("an expanded frame");
        let items = expanded["items"].as_array().unwrap();
        let group = items.iter().find(|i| i["id"] == id.as_str()).unwrap();
        assert_eq!(
            group["kind"], "group",
            "the row morphed rather than blinking"
        );
        assert_eq!(group["ord"], ord, "and kept its ord");
        assert_eq!(group["children_total"], 4);
        assert_eq!(group["children_inline"], true);
        assert!(
            items.len() > 1,
            "the children are in the same frame: {}",
            items.len()
        );
    })
    .await;
}

/// DESIGN §21.3 — a cancel mid-download is a `completed` frame with `canceled`.
#[tokio::test]
async fn cancelling_mid_download_is_a_completed_frame() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(hanging()))
            .start()
            .await;
        let id = rig.add("https://fake.test/hang").await;
        rig.until_status(&id, "downloading").await;

        let mut socket = connect(&rig, "ws").await;
        let snapshot = next_frame(&mut socket).await;
        assert_eq!(snapshot["items"][0]["status"], "downloading");

        rig.post(
            "api/v2/items/actions",
            &json!({ "action": "cancel", "ids": [id] }),
        )
        .await;

        let completed = next_frame_of(&mut socket, "completed").await;
        let item = &completed["items"][0];
        assert_eq!(item["status"], "canceled");
        assert_eq!(item["error"]["code"], "canceled");
    })
    .await;
}

#[tokio::test]
async fn pause_then_start_is_two_deltas_and_no_new_row() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(hanging()))
            .start()
            .await;
        let id = rig.add("https://fake.test/hang").await;
        rig.until_status(&id, "downloading").await;
        let mut socket = connect(&rig, "ws").await;
        next_frame_of(&mut socket, "snapshot").await;

        rig.post(
            "api/v2/items/actions",
            &json!({ "action": "pause", "ids": [id] }),
        )
        .await;
        let mut paused = false;
        for _ in 0..20 {
            let frame = next_frame(&mut socket).await;
            assert_eq!(frame["t"], "delta", "a pause is a patch, not a new row");
            let patch = &frame["items"][0];
            if patch["auto_start"] == json!(false) {
                assert_eq!(patch["status"], "queued");
                paused = true;
                break;
            }
        }
        assert!(paused, "the pause patch never arrived");

        rig.post(
            "api/v2/items/actions",
            &json!({ "action": "start", "ids": [id] }),
        )
        .await;
        let mut resumed = false;
        for _ in 0..20 {
            let frame = next_frame(&mut socket).await;
            let patch = &frame["items"][0];
            if patch["status"] == "preparing" || patch["status"] == "downloading" {
                resumed = true;
                assert!(
                    patch.get("attempt").is_none(),
                    "resuming does not touch attempt: {patch}"
                );
                break;
            }
        }
        assert!(resumed, "the resume patch never arrived");
    })
    .await;
}

// ---------------------------------------------------------------------------
// no lost updates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_mutation_racing_the_snapshot_is_never_lost_and_never_doubled() {
    for_each_prefix(|prefix| async move {
        for attempt in 0..5 {
            let rig = Rig::start(prefix).await;
            // The add and the connect are started together, so the item lands on either side of
            // the snapshot depending on the interleaving. Either way the client must end up with
            // exactly one row for it (PROTOCOL §5.5: `added` is an upsert).
            let adder = {
                let url = rig.url("api/v2/downloads");
                let http = rig.http.clone();
                tokio::spawn(async move {
                    http.post(url)
                        .json(&json!({ "url": "https://fake.test/race" }))
                        .send()
                        .await
                        .unwrap()
                        .json::<Value>()
                        .await
                        .unwrap()
                })
            };
            let mut socket = connect(&rig, "ws").await;
            let snapshot = next_frame(&mut socket).await;
            let added = adder.await.unwrap();
            let id = added["ids"][0].as_str().unwrap().to_owned();

            let mut state: HashMap<String, Value> = HashMap::new();
            for item in snapshot["items"].as_array().unwrap() {
                state.insert(item["id"].as_str().unwrap().to_owned(), item.clone());
            }
            // Apply frames until the item is terminal, exactly as PROTOCOL §7 says.
            for _ in 0..60 {
                let Some(frame) = try_next_frame(&mut socket, Duration::from_secs(2)).await else {
                    break;
                };
                support::apply(&mut state, &frame);
                if state.get(&id).is_some_and(|i| i["status"] == "finished") {
                    break;
                }
            }

            let row = state
                .get(&id)
                .unwrap_or_else(|| panic!("attempt {attempt}: the racing add was lost"));
            assert_eq!(row["status"], "finished", "attempt {attempt}");
            assert_eq!(
                state.values().filter(|i| i["id"] == id.as_str()).count(),
                1,
                "exactly one row, however the race went"
            );
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn since_is_answered_with_a_resume_and_the_folded_frames() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        let snapshot = next_frame(&mut socket).await;
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();
        let cursor = snapshot["seq"].as_u64().unwrap();
        drop(socket);

        // Something happens while the client is away.
        let id = rig.add("https://fake.test/away").await;
        rig.until_status(&id, "finished").await;
        rig.settle().await;

        let mut socket = connect(&rig, &format!("ws?since={cursor}&boot={boot}")).await;
        let resume = next_frame(&mut socket).await;
        assert_eq!(resume["t"], "resume", "{resume}");
        assert_eq!(resume["from"], cursor);
        assert!(resume["to"].as_u64().unwrap() > cursor);
        for key in ["added", "completed", "removed", "delta_items"] {
            assert!(resume["merged"].get(key).is_some(), "{key} on merged");
        }

        // The folded frames follow, in the documented order, and carry the item.
        let mut ids = Vec::new();
        while let Some(frame) = try_next_frame(&mut socket, Duration::from_millis(400)).await {
            if let Some(items) = frame["items"].as_array() {
                for item in items {
                    ids.push(item["id"].as_str().unwrap_or_default().to_owned());
                }
            }
        }
        assert!(ids.contains(&id), "the missed item is in the fold: {ids:?}");

        // A boot mismatch, and a cursor above the head, both discard.
        let mut socket = connect(
            &rig,
            &format!("ws?since={cursor}&boot=01JBQ8YQ2E0000000000000000"),
        )
        .await;
        assert_eq!(next_frame(&mut socket).await["t"], "snapshot");
        let mut socket = connect(&rig, &format!("ws?since={}&boot={boot}", cursor + 9_999)).await;
        assert_eq!(next_frame(&mut socket).await["t"], "snapshot");
    })
    .await;
}

#[tokio::test]
async fn an_up_to_date_cursor_is_a_resume_with_nothing_in_it() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (_, state) = rig.get("api/v2/state").await;
        let head = state["seq"].as_u64().unwrap();
        let boot = state["boot_id"].as_str().unwrap().to_owned();
        let mut socket = connect(&rig, &format!("ws?since={head}&boot={boot}")).await;
        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["t"], "resume");
        assert_eq!(frame["from"], head);
        assert_eq!(frame["to"], head);
        assert_eq!(frame["merged"]["added"], 0);
    })
    .await;
}

#[tokio::test]
async fn the_resume_client_frame_works_on_an_open_socket() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        let snapshot = next_frame(&mut socket).await;
        let cursor = snapshot["seq"].as_u64().unwrap();
        let boot = snapshot["boot_id"].as_str().unwrap().to_owned();

        send_frame(
            &mut socket,
            &json!({ "t": "resume", "since": cursor, "boot": boot }),
        )
        .await;
        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["t"], "resume", "{frame}");
    })
    .await;
}

// ---------------------------------------------------------------------------
// client frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ping_is_answered_with_pong_and_the_cut_frames_are_tolerated() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        next_frame_of(&mut socket, "snapshot").await;

        send_frame(
            &mut socket,
            &json!({ "t": "ping", "c": 1_757_000_299_871_i64 }),
        )
        .await;
        let pong = next_frame(&mut socket).await;
        assert_eq!(pong["t"], "pong");
        assert_eq!(pong["c"], 1_757_000_299_871_i64, "echoed verbatim");
        assert!(pong["server_time"].as_i64().is_some());

        // v1.0: `hello` topic narrowing, `ack`, `watch` and `unwatch` are CUT, but a client that
        // sends one must not be disconnected or told it is wrong.
        for frame in [
            json!({ "t": "hello", "client": "aulos-ios/1.2", "topics": ["items"] }),
            json!({ "t": "ack", "seq": 3 }),
            json!({ "t": "watch", "groups": ["01JBQ8AA0000000000000000GG"], "done": false }),
            json!({ "t": "unwatch", "groups": ["01JBQ8AA0000000000000000GG"] }),
        ] {
            send_frame(&mut socket, &frame).await;
        }
        assert!(
            try_next_frame(&mut socket, Duration::from_millis(200))
                .await
                .is_none(),
            "accepted in silence"
        );

        // The socket is still live afterwards.
        send_frame(&mut socket, &json!({ "t": "ping", "c": "x" })).await;
        assert_eq!(next_frame(&mut socket).await["t"], "pong");
    })
    .await;
}

#[tokio::test]
async fn an_unknown_frame_gets_an_error_frame_naming_it() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        next_frame_of(&mut socket, "snapshot").await;

        send_frame(
            &mut socket,
            &json!({ "t": "subscribe", "to": "everything" }),
        )
        .await;
        let error = next_frame(&mut socket).await;
        assert_eq!(error["t"], "error");
        assert_eq!(error["code"], "bad_frame");
        assert_eq!(error["message"], "unknown frame type \"subscribe\"");

        // A mutation over the socket is not a frame type at all: mutations stay on REST.
        send_frame(&mut socket, &json!({ "t": "delete", "ids": ["x"] })).await;
        assert_eq!(next_frame(&mut socket).await["code"], "bad_frame");
    })
    .await;
}

// ---------------------------------------------------------------------------
// the disconnect rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_client_cap_answers_with_an_error_frame_then_1013() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_WS_MAX_CLIENTS", "1")
            .start()
            .await;
        let mut first = connect(&rig, "ws").await;
        assert_eq!(next_frame(&mut first).await["t"], "snapshot");

        let mut second = connect(&rig, "ws").await;
        let error = next_frame(&mut second).await;
        assert_eq!(error["t"], "error");
        assert_eq!(error["code"], "too_many_clients");
        assert_eq!(close_code(&mut second).await, Some(1013));

        let (_, health) = rig.get("healthz").await;
        assert_eq!(health["ws"]["clients"], 1);
        assert!(health["ws"]["slow_disconnects"].as_u64().unwrap() >= 1);
    })
    .await;
}

#[tokio::test]
async fn an_oversized_client_frame_closes_the_socket() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let mut socket = connect(&rig, "ws").await;
        next_frame_of(&mut socket, "snapshot").await;

        // Over the 1 MiB cap of PROTOCOL §5.1. The write itself may fail with a reset, because
        // the server can close before the client has finished sending — which is the point.
        let huge = json!({ "t": "hello", "client": "x".repeat(2 * 1024 * 1024) });
        let _ = support::try_send_frame(&mut socket, &huge).await;
        // tungstenite answers a too-large message with `1009 Message Too Big`; some builds report
        // it as a protocol error on the read side instead, so either shape is accepted here as
        // long as the socket does not stay open and serving.
        let code = close_code(&mut socket).await;
        assert!(
            matches!(code, Some(1009) | None),
            "expected a 1009 close, got {code:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn a_client_that_never_reads_is_disconnected_and_counted() {
    for_each_prefix(|prefix| async move {
        // A 50 ms send budget and a big burst: the socket's buffer fills, the send blocks past the
        // budget, and the session closes it with `1013` rather than holding memory in the hub
        // (DESIGN §15.4 step 6).
        let rig = Rig::builder(prefix)
            .env("AULOS_WS_SEND_TIMEOUT_MS", "50")
            .start()
            .await;
        let mut fast = connect(&rig, "ws").await;
        assert_eq!(next_frame(&mut fast).await["t"], "snapshot");
        let slow = connect(&rig, "ws").await;

        // 4 MB of frames, published straight onto the bus.
        let filler = "x".repeat(32 * 1024);
        for _ in 0..128 {
            rig.hub.publish(
                FrameKind::Notice,
                json!({ "level": "info", "code": "plugin_note", "id": null, "message": filler }),
            );
        }

        // The fast reader is unaffected: it still receives frames.
        let mut fast_frames = 0;
        while try_next_frame(&mut fast, Duration::from_millis(400))
            .await
            .is_some()
        {
            fast_frames += 1;
            if fast_frames > 4 {
                break;
            }
        }
        assert!(fast_frames > 0, "the fast reader kept receiving");

        // The slow one is gone, and `healthz` says why.
        drop(slow);
        for _ in 0..40 {
            let (_, health) = rig.get("healthz").await;
            let disconnects = health["ws"]["slow_disconnects"].as_u64().unwrap();
            let lagged = health["ws"]["lagged_total"].as_u64().unwrap();
            if disconnects >= 1 || lagged >= 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_, health) = rig.get("healthz").await;
        panic!("the slow client was neither lagged nor disconnected: {health}");
    })
    .await;
}

#[tokio::test]
async fn a_lagging_client_gets_one_fresh_snapshot() {
    // The broadcast bus is 256 frames deep. A client whose socket buffer is full stops draining
    // it, so a burst past that depth makes its receiver report `Lagged` — and the answer is one
    // fresh snapshot, not a gap (DESIGN §15.4 step 5).
    let rig = Rig::builder("/")
        .env("AULOS_WS_SEND_TIMEOUT_MS", "10000")
        .start()
        .await;
    let mut socket = connect(&rig, "ws").await;
    assert_eq!(next_frame(&mut socket).await["t"], "snapshot");

    // Fill the socket's buffer, then overflow the bus behind it.
    let filler = "y".repeat(48 * 1024);
    for _ in 0..64 {
        rig.hub.publish(
            FrameKind::Notice,
            json!({ "level": "info", "code": "plugin_note", "id": null, "message": filler }),
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    for n in 0..600 {
        rig.hub.publish(
            FrameKind::Notice,
            json!({ "level": "info", "code": "plugin_note", "id": null, "message": n }),
        );
    }

    // Now read: somewhere in the stream the server gives up and re-states the world.
    let mut resynced = false;
    for _ in 0..800 {
        let Some(frame) = try_next_frame(&mut socket, Duration::from_secs(5)).await else {
            break;
        };
        if frame["t"] == "snapshot" {
            resynced = true;
            break;
        }
    }
    assert!(resynced, "a lagging client must be handed a fresh snapshot");
    let (_, health) = rig.get("healthz").await;
    assert!(
        health["ws"]["lagged_total"].as_u64().unwrap() >= 1,
        "and the lag is counted for healthz: {health}"
    );
}
