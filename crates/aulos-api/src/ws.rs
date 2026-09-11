//! The WebSocket at `<p>ws` (PROTOCOL §5, §6, DESIGN §15.4).
//!
//! The session is five rules and one loop:
//!
//! 1. **Subscribe before snapshotting.** The broadcast receiver is taken first, the published
//!    generation second. A frame produced in between is still in the receiver's buffer and is
//!    forwarded after the snapshot, filtered by `seq > snapshot.seq` — which is what makes "no
//!    lost updates on connect" true rather than likely.
//! 2. **The snapshot carries `ytdl_options` and `health`.** Both frames are transition-only, so a
//!    client connecting into a degraded server would otherwise learn nothing (PROTOCOL §5.3).
//! 3. **`?since=` is answered by the hub**, which either folds the window into one `resume` plus
//!    at most one frame of each kind, or says "take a snapshot" (PROTOCOL §6.2). A `since` whose
//!    `boot` is missing, empty or not a ULID never reaches the hub: it is a snapshot, because the
//!    server cannot confirm which boot the cursor came from.
//! 4. **A slow reader is disconnected, never buffered.** `Lagged` costs one resync; more than
//!    eight lags in a minute, or a send that blocks for `AULOS_WS_SEND_TIMEOUT_MS`, closes the
//!    socket with `1013`.
//! 5. **Mutations never arrive here.** The six client frames are all advisory; a read-only client
//!    is fully functional.
//!
//! # Deviations, and the BRIEF trims
//!
//! - **One task, not two.** DESIGN §15.4 step 3 asks for a reader task and a writer task sharing a
//!   `CancellationToken`. Splitting an `axum::extract::ws::WebSocket` needs `futures-util`, which
//!   is not in `aulos-api`'s DESIGN §3 dependency row, so the session is one `tokio::select!` over
//!   the socket, the bus and the keepalive tick. The property the two tasks existed for — a wedged
//!   writer cannot hold memory or block progress — is the *send timeout*, which this shape
//!   enforces directly.
//! - `hello` **topic narrowing**, `ack`, `watch` and `unwatch` are CUT (BRIEF). All four frames are
//!   still accepted and produce no error: a client written from PROTOCOL §5.11 must never be
//!   disconnected for sending one. `watch` needs nothing because the snapshot already carries
//!   every non-terminal child, and `ack` was advisory even in the design.
//! - There is no `ConnClosed` engine command and no `Drop` guard that sends one, because there is
//!   no watch registry to release (BRIEF). The `Drop` guard that remains is the client counter, so
//!   `healthz.ws.clients` cannot leak on any close path.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use aulos_core::{BootId, Seq};
use aulos_queue::{FrameKind, Resume, WireFrame};
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::v2::Q;
use crate::{ApiState, view};

/// The largest client frame the socket accepts; anything bigger closes with `1009`
/// (PROTOCOL §5.1, DESIGN §15.4 step 8).
pub const MAX_CLIENT_FRAME: usize = 1024 * 1024;

/// How often the server sends a WebSocket `Ping` (PROTOCOL §5.1).
pub const PING_INTERVAL: Duration = Duration::from_secs(20);

/// How long the server tolerates silence — no `Pong`, no data — before closing.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How many `Lagged` resyncs a socket may take inside [`LAG_WINDOW`] before it is closed `1013`
/// (DESIGN §15.4 step 5).
pub const LAG_BUDGET: u32 = 8;

/// The window the lag budget is counted over.
pub const LAG_WINDOW: Duration = Duration::from_secs(60);

/// `1013 Try Again Later` — too slow, or too many clients.
pub const CLOSE_TRY_LATER: u16 = 1013;

/// `1009 Message Too Big` — a client frame over [`MAX_CLIENT_FRAME`].
pub const CLOSE_TOO_BIG: u16 = 1009;

/// `1001 Going Away` — the server is shutting down.
pub const CLOSE_GOING_AWAY: u16 = 1001;

/// The `<p>ws` route.
pub fn router(state: ApiState) -> axum::Router {
    let p = state.cfg.url_prefix.clone();
    axum::Router::new()
        .route(&p.route("ws"), get(upgrade))
        .with_state(state)
}

/// The query string of PROTOCOL §5.1.
#[derive(Debug, Default, Deserialize)]
pub struct WsQuery {
    /// Resume from this frame cursor instead of taking a snapshot.
    pub since: Option<u64>,
    /// The `boot_id` the cursor came from.
    pub boot: Option<String>,
    /// Include the completed window in the snapshot. Default `true`.
    pub done: Option<bool>,
    /// Pre-subscribe to these groups' children.
    ///
    /// v1.0: not implemented, see BRIEF — group collapsing and the `watch` frame are CUT, so the
    /// snapshot already carries every non-terminal child and this parameter is accepted and
    /// ignored rather than rejected.
    pub groups: Option<String>,
    /// The bearer token, when the proxy cannot forward cookies. Consumed by [`crate::auth`].
    pub token: Option<String>,
}

/// `GET <p>ws` — the upgrade.
pub async fn upgrade(
    State(state): State<ApiState>,
    Q(query): Q<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.protocols([crate::WS_SUBPROTOCOL])
        .max_message_size(MAX_CLIENT_FRAME)
        .max_frame_size(MAX_CLIENT_FRAME)
        .on_upgrade(move |socket| session(state, query, socket))
}

/// Decrements the live client count on **every** close path, including a panic.
struct ClientGuard(Arc<crate::Live>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The whole session.
async fn session(state: ApiState, query: WsQuery, mut socket: WebSocket) {
    // Step 7 of DESIGN §15.4: the client cap. The connection is accepted far enough to be *told*
    // why it is going away, because a bare TCP close looks like a network fault to a phone.
    let live = Arc::clone(&state.live);
    let before = live.clients.fetch_add(1, Ordering::Relaxed);
    let guard = ClientGuard(Arc::clone(&live));
    if before >= u64::from(state.cfg.ws_max_clients) {
        live.slow_disconnects.fetch_add(1, Ordering::Relaxed);
        let seq = state.hub.head().0;
        let _ = socket
            .send(text(&json!({
                "t": "error",
                "seq": seq,
                "code": "too_many_clients",
                "message": "too many WebSocket clients; try again later",
            })))
            .await;
        close(&mut socket, CLOSE_TRY_LATER, "too many clients").await;
        drop(guard);
        return;
    }

    // Step 2: subscribe **first**, then read the published generation. A frame published between
    // the two is buffered in `rx` and forwarded after the snapshot, filtered against the cursor
    // the snapshot established — which is what makes "no lost updates on connect" true.
    let mut rx = state.hub.subscribe();

    // DESIGN §15.4 step 1 asks for a 100 ms grace before the snapshot, so that a `hello`'s topic
    // list could narrow it. **Topic narrowing is CUT** (BRIEF), so there is nothing left for the
    // grace to apply and waiting 100 ms would be pure connect latency on the one path this whole
    // package exists to make fast. `hello` is therefore handled in the loop like every other
    // client frame, and the snapshot goes out immediately.
    let done = query.done.unwrap_or(true);
    let mut cursor: Option<Seq> = match query.since {
        None => match send_snapshot(&state, &mut socket, done).await {
            Ok(seq) => seq,
            Err(()) => return,
        },
        // A `since` whose `boot` is absent, empty or not a ULID is discarded rather than folded
        // (PROTOCOL §6.2); `resume_boot` is the one place that decides, shared with `GET
        // api/v2/state`.
        Some(since) => match crate::v2::query::resume_boot(query.boot.as_deref()) {
            Some(boot) => match resume(&state, &mut socket, Seq(since), boot, done).await {
                Ok(seq) => seq,
                Err(()) => return,
            },
            None => match send_snapshot(&state, &mut socket, done).await {
                Ok(seq) => seq,
                Err(()) => return,
            },
        },
    };

    let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();
    let mut lags: u32 = 0;
    let mut lag_window_started = Instant::now();

    loop {
        let message = {
            tokio::select! {
                incoming = socket.recv() => incoming,
                frame = rx.recv() => {
                    match frame {
                        Ok(frame) => {
                            if cursor.is_none_or(|seen| frame.seq > seen) {
                                cursor = Some(frame.seq);
                                if forward(&state, &mut socket, &frame).await.is_err() {
                                    return;
                                }
                            }
                            continue;
                        }
                        Err(RecvError::Closed) => {
                            close(&mut socket, CLOSE_GOING_AWAY, "server shutting down").await;
                            return;
                        }
                        Err(RecvError::Lagged(n)) => {
                            live.lagged.fetch_add(1, Ordering::Relaxed);
                            if lag_window_started.elapsed() > LAG_WINDOW {
                                lags = 0;
                                lag_window_started = Instant::now();
                            }
                            lags += 1;
                            tracing::warn!(skipped = n, lags, "a ws client fell behind");
                            if lags > LAG_BUDGET {
                                live.slow_disconnects.fetch_add(1, Ordering::Relaxed);
                                close(&mut socket, CLOSE_TRY_LATER, "too slow").await;
                                return;
                            }
                            match send_snapshot(&state, &mut socket, done).await {
                                Ok(seq) => cursor = seq,
                                Err(()) => return,
                            }
                            continue;
                        }
                    }
                }
                _ = ping.tick() => {
                    if last_seen.elapsed() > IDLE_TIMEOUT {
                        tracing::debug!("closing an idle ws client");
                        close(&mut socket, CLOSE_GOING_AWAY, "idle").await;
                        return;
                    }
                    if send(&state, &mut socket, Message::Ping(bytes::Bytes::new()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
            }
        };

        let Some(incoming) = message else {
            return; // the socket closed
        };
        let incoming = match incoming {
            Ok(message) => message,
            Err(e) => {
                // tungstenite reports an oversized frame as a capacity error and has already
                // queued its own `1009`; the explicit close makes the intent visible in a test.
                let text = e.to_string();
                if text.contains("too large") || text.contains("Space limit") {
                    close(&mut socket, CLOSE_TOO_BIG, "frame too large").await;
                } else {
                    tracing::debug!(error = %text, "ws read error");
                }
                return;
            }
        };
        last_seen = Instant::now();

        match parse_client_frame(&incoming) {
            Some(ClientFrame::Ping { c }) => {
                let body = json!({
                    "t": "pong",
                    "seq": state.hub.head().0,
                    "server_time": state.now_ms(),
                    "c": c,
                });
                if send(&state, &mut socket, text(&body)).await.is_err() {
                    return;
                }
            }
            Some(ClientFrame::Resume { since, boot }) => {
                let answered = match boot {
                    Some(boot) => resume(&state, &mut socket, Seq(since), boot, done).await,
                    None => send_snapshot(&state, &mut socket, done).await,
                };
                match answered {
                    Ok(seq) => cursor = seq,
                    Err(()) => return,
                }
            }
            // `hello` is recorded and otherwise ignored: its `topics` list is CUT (BRIEF), so
            // there is nothing to narrow, and the client name is worth a log line.
            Some(ClientFrame::Hello { client }) => {
                tracing::debug!(client = %client, "ws hello");
            }
            // Accepted in silence: `ack` (advisory even in the design) and `watch`/`unwatch` (the
            // snapshot is never truncated in this build), plus the transport frames.
            Some(ClientFrame::Ack | ClientFrame::Watch | ClientFrame::Ignored) => {}
            Some(ClientFrame::Close) => return,
            None => {
                let body = json!({
                    "t": "error",
                    "seq": state.hub.head().0,
                    "code": "bad_frame",
                    "message": bad_frame_message(&incoming),
                });
                if send(&state, &mut socket, text(&body)).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Builds and sends a `snapshot`, then replays whatever the generation has not absorbed yet,
/// returning the cursor the pair establishes.
///
/// The cursor is an `Option` because `seq` is allocated from zero: the boot-time empty generation
/// reports `seq = 0`, which is the same value the very first frame carries. `None` means "nothing
/// is reflected in this snapshot yet", so the first frame is forwarded rather than mistaken for
/// one the snapshot already contains.
async fn send_snapshot(
    state: &ApiState,
    socket: &mut WebSocket,
    done: bool,
) -> Result<Option<Seq>, ()> {
    let published = state.state.snapshot();
    let cursor = if published.seq == Seq(0) && published.is_empty() {
        None
    } else {
        Some(published.seq)
    };
    let mut extra = Map::new();
    extra.insert("t".to_owned(), json!(FrameKind::Snapshot));
    extra.insert(
        "server".to_owned(),
        json!({
            "version": state.info.version,
            "yt_dlp": state.info.yt_dlp,
            "url_prefix": state.cfg.url_prefix,
            "started_at": state.info.started_at,
        }),
    );
    let body = crate::v2::query::snapshot_of(state, &published, done, Some(extra)).await;
    send(state, socket, text(&body)).await?;
    catch_up(state, socket, cursor).await
}

/// Replays the frames that are newer than the snapshot but older than this subscription.
///
/// "Subscribe first, snapshot second" covers a frame published *between* the two — it is buffered
/// in `rx` and filtered against the snapshot's cursor. It does **not** cover a frame published
/// *before* `subscribe` that the published generation has not absorbed yet, and that window is
/// open on every flush: [`aulos_queue::Aggregator::flush`] publishes all of a tick's frames and
/// republishes the generation last, so `hub.head() > published.seq` for the length of the delta
/// diff. Such a frame is in neither place, and when it is an item's last frame — a `completed` —
/// nothing later repairs the row.
///
/// The replay ring already holds it, and `?since=` already knows how to fold a window out of it,
/// so this is the same machinery pointed at the snapshot's own cursor.
async fn catch_up(
    state: &ApiState,
    socket: &mut WebSocket,
    cursor: Option<Seq>,
) -> Result<Option<Seq>, ()> {
    let seen = cursor.unwrap_or(Seq(0));
    if state.hub.head() <= seen {
        return Ok(cursor);
    }
    match state.hub.resume(seen, Some(state.hub.boot_id())) {
        Resume::Merged { to, frames, .. } => {
            for frame in &frames {
                forward(state, socket, frame).await?;
            }
            Ok(Some(to))
        }
        // `UpToDate` is the race closing under us; `Snapshot` means the gap is older than the
        // ring floor, which can only happen if the generation is `AULOS_REPLAY_FRAMES` behind —
        // there is nothing left to replay, and re-snapshotting would only loop.
        Resume::UpToDate | Resume::Snapshot => Ok(cursor),
    }
}

/// Answers a `?since=` cursor: a `resume` envelope plus the folded frames, or a fresh snapshot.
async fn resume(
    state: &ApiState,
    socket: &mut WebSocket,
    since: Seq,
    boot: BootId,
    done: bool,
) -> Result<Option<Seq>, ()> {
    match state.hub.resume(since, Some(boot)) {
        Resume::Snapshot => send_snapshot(state, socket, done).await,
        Resume::UpToDate => {
            let body = json!({
                "t": "resume",
                "seq": since.0,
                "from": since.0,
                "to": since.0,
                "merged": { "added": 0, "completed": 0, "removed": 0, "delta_items": 0 },
            });
            send(state, socket, text(&body)).await?;
            Ok(Some(since))
        }
        Resume::Merged {
            from,
            to,
            merged,
            frames,
        } => {
            let body = json!({
                "t": "resume",
                "seq": to.0,
                "from": from.0,
                "to": to.0,
                "merged": {
                    "added": merged.added,
                    "completed": merged.completed,
                    "removed": merged.removed,
                    "delta_items": merged.delta_items,
                },
            });
            send(state, socket, text(&body)).await?;
            for frame in &frames {
                forward(state, socket, frame).await?;
            }
            Ok(Some(to))
        }
    }
}

/// Sends one broadcast frame, filling in `download_url` where the frame can carry a file name.
async fn forward(state: &ApiState, socket: &mut WebSocket, frame: &WireFrame) -> Result<(), ()> {
    let published = state.state.load();
    let payload = match view::patch_frame(&state.cfg, &published, frame.kind, frame.as_str()) {
        Some(patched) => Utf8Bytes::from(patched),
        None => Utf8Bytes::try_from(frame.text.clone()).unwrap_or_else(|_| Utf8Bytes::from("{}")),
    };
    drop(published);
    send(state, socket, Message::Text(payload)).await
}

/// Sends one message under the configured send timeout (DESIGN §15.4 step 6).
///
/// A socket whose buffer stays full for `AULOS_WS_SEND_TIMEOUT_MS` is closed `1013`: a stalled
/// client must never be able to hold memory in the hub.
async fn send(state: &ApiState, socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    let deadline = Duration::from_millis(state.cfg.ws_send_timeout_ms.max(1));
    match tokio::time::timeout(deadline, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "ws send failed");
            Err(())
        }
        Err(_) => {
            state.live.slow_disconnects.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                timeout_ms = state.cfg.ws_send_timeout_ms,
                "closing a ws client whose send buffer stayed full"
            );
            close(socket, CLOSE_TRY_LATER, "send timeout").await;
            Err(())
        }
    }
}

/// Sends a close frame, best effort.
async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: Utf8Bytes::from(reason.to_owned()),
        })))
        .await;
}

/// A JSON value as a text frame.
fn text(body: &Value) -> Message {
    Message::Text(Utf8Bytes::from(
        serde_json::to_string(body).unwrap_or_else(|_| "{}".to_owned()),
    ))
}

/// The six client frames of PROTOCOL §5.11, plus the transport ones.
#[derive(Debug)]
enum ClientFrame {
    /// `{"t":"hello","client":…}`.
    Hello {
        /// What the client calls itself.
        client: String,
    },
    /// `{"t":"ping","c":…}`.
    Ping {
        /// Echoed back verbatim so the client can measure RTT.
        c: Value,
    },
    /// `{"t":"resume","since":…,"boot":…}`.
    Resume {
        /// The cursor.
        since: u64,
        /// The boot it came from, or `None` when it was absent or unusable — which is a
        /// snapshot, not a fold (PROTOCOL §6.2).
        boot: Option<BootId>,
    },
    /// `{"t":"ack","seq":…}` — advisory, and CUT.
    Ack,
    /// `{"t":"watch"|"unwatch",…}` — CUT.
    Watch,
    /// A close frame.
    Close,
    /// Pong, ping or binary: nothing to do.
    Ignored,
}

/// Parses one client message. `None` means "not a frame this protocol defines".
fn parse_client_frame(message: &Message) -> Option<ClientFrame> {
    let raw = match message {
        Message::Text(text) => text.as_str(),
        Message::Close(_) => return Some(ClientFrame::Close),
        Message::Ping(_) | Message::Pong(_) => return Some(ClientFrame::Ignored),
        Message::Binary(_) => return None,
    };
    let value: Value = serde_json::from_str(raw).ok()?;
    match value.get("t").and_then(Value::as_str)? {
        "hello" => Some(ClientFrame::Hello {
            client: value
                .get("client")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
        }),
        "ping" => Some(ClientFrame::Ping {
            c: value.get("c").cloned().unwrap_or(Value::Null),
        }),
        "resume" => Some(ClientFrame::Resume {
            since: value.get("since").and_then(Value::as_u64)?,
            boot: crate::v2::query::resume_boot(value.get("boot").and_then(Value::as_str)),
        }),
        "ack" => Some(ClientFrame::Ack),
        "watch" | "unwatch" => Some(ClientFrame::Watch),
        _ => None,
    }
}

/// The `error` frame's message for a frame the server does not understand (PROTOCOL §5.10).
fn bad_frame_message(message: &Message) -> String {
    let Message::Text(text) = message else {
        return "expected a JSON text frame".to_owned();
    };
    match serde_json::from_str::<Value>(text.as_str()) {
        Ok(value) => match value.get("t").and_then(Value::as_str) {
            Some(kind) => format!("unknown frame type \"{kind}\""),
            None => "a client frame must carry a \"t\" field".to_owned(),
        },
        Err(_) => "a client frame must be one JSON object".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(raw: &str) -> Option<ClientFrame> {
        parse_client_frame(&Message::Text(Utf8Bytes::from(raw.to_owned())))
    }

    #[test]
    fn the_six_client_frames_all_parse() {
        assert!(matches!(
            frame(r#"{"t":"hello","client":"aulos-ios/1.2","topics":["items"]}"#),
            Some(ClientFrame::Hello { .. })
        ));
        assert!(matches!(
            frame(r#"{"t":"ping","c":17}"#),
            Some(ClientFrame::Ping { .. })
        ));
        assert!(matches!(
            frame(r#"{"t":"ack","seq":9}"#),
            Some(ClientFrame::Ack)
        ));
        assert!(matches!(
            frame(r#"{"t":"watch","groups":["x"],"done":true}"#),
            Some(ClientFrame::Watch)
        ));
        assert!(matches!(
            frame(r#"{"t":"unwatch","groups":["x"]}"#),
            Some(ClientFrame::Watch)
        ));
        let resumed = frame(r#"{"t":"resume","since":10240,"boot":"01JBQ8YQ2E0000000000000000"}"#);
        match resumed {
            Some(ClientFrame::Resume { since, boot }) => {
                assert_eq!(since, 10_240);
                assert!(boot.is_some());
            }
            other => panic!("expected a resume, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_frame_is_reported_by_name() {
        assert!(frame(r#"{"t":"subscribe"}"#).is_none());
        let message = Message::Text(Utf8Bytes::from(r#"{"t":"subscribe"}"#.to_owned()));
        assert_eq!(
            bad_frame_message(&message),
            "unknown frame type \"subscribe\""
        );
        let message = Message::Text(Utf8Bytes::from("not json".to_owned()));
        assert_eq!(
            bad_frame_message(&message),
            "a client frame must be one JSON object"
        );
    }

    #[test]
    fn a_close_or_a_pong_is_not_an_error() {
        assert!(matches!(
            parse_client_frame(&Message::Pong(bytes::Bytes::new())),
            Some(ClientFrame::Ignored)
        ));
        assert!(matches!(
            parse_client_frame(&Message::Close(None)),
            Some(ClientFrame::Close)
        ));
    }

    #[test]
    fn the_documented_limits_are_the_design_numbers() {
        assert_eq!(MAX_CLIENT_FRAME, 1_048_576);
        assert_eq!(PING_INTERVAL, Duration::from_secs(20));
        assert_eq!(IDLE_TIMEOUT, Duration::from_secs(60));
        assert_eq!(LAG_BUDGET, 8);
    }
}
