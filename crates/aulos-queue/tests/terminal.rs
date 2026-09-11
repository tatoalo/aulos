//! What a row is left carrying after the terminal write (DESIGN §8.7, §8.10, §13; PROTOCOL §2.3,
//! §2.4, §3.1).
//!
//! `msg` is the **live** status line — the stage label a provider last wrote, or the pre-terminal
//! hook's label — and production shipped it straight into the terminal row: yt-dlp's last
//! postprocessor frame (`"MoveFiles…"`, DESIGN §9.5) was still there on every finished download,
//! so a completed item rendered as a job stuck in its final postprocessor. These are the
//! regressions for that, for the other terminal statuses that had the same hole, for the group
//! roll-up that writes a terminal status without going through `Engine::terminate`, and for the
//! late frames that could put the line back.
//!
//! **No test here waits on wall-clock time.** The provider parks at a [`Gate`] and the test opens
//! it only once it has *observed* the state it wanted to act in, so the ordering every assertion
//! depends on is a barrier rather than a sleep long enough to probably win.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aulos_core::{
    DownloadRequest, ErrorCode, GroupId, Item, ItemId, Kind, SourceKind, SourceRef, Status,
};
use aulos_provider::fake::FakeProvider;
use aulos_provider::{
    DownloadCtx, Match, MediaEntry, Outcome, ProgressSink, Provider, ProviderError, ProviderHealth,
    ProviderId, ResolveCtx, Stage,
};
use aulos_queue::Action;
use support::{AlwaysPreTerminal, Harness, selection};
use tokio_util::sync::CancellationToken;

/// The postprocessor line yt-dlp's shim writes for its last `pp` frame (DESIGN §9.5).
const MOVE_FILES: &str = "MoveFiles…";

// ---------------------------------------------------------------------------
// the gated provider
// ---------------------------------------------------------------------------

/// A one-way gate a test opens once it has seen what it was waiting for.
type Gate = CancellationToken;

/// How a gated download ends once its gate opens.
#[derive(Clone, Copy)]
enum Ending {
    /// One small file.
    Finish,
    /// A non-retryable failure (DESIGN §8.8), so the item terminates rather than backing off.
    Fail(ErrorCode),
}

/// A provider that walks the **real** frame path — `ProgressSink` → aggregator → engine, the one
/// `EngineCmd::Finished` races (DESIGN §15.1) — and then parks at a gate.
///
/// It exists because injecting a frame through the engine handle proves nothing about that race:
/// a handle call is ordered against the outcome by the command channel, a provider frame is not.
/// Parking at the gate is what makes the ordering exact without a sleep: the frames are on the
/// wire, the outcome is not, and the test decides when that changes.
struct Gated {
    /// Delegate for everything that is not `download`.
    inner: FakeProvider,
    /// The `Postprocessing` line to emit before parking, if any.
    line: Option<&'static str>,
    /// A `Downloading` frame emitted **after** the postprocessing one, if any: the late frame a
    /// provider produces when a postprocessor of its own ran before the first byte, or when its
    /// last progress line overtakes its own postprocessor line.
    late: Option<&'static str>,
    gate: Gate,
    ending: Ending,
    /// Calls with an index below this run straight through, so a *restarted* run can be the one
    /// that parks.
    gate_from: usize,
    calls: AtomicUsize,
}

impl Gated {
    fn new(ending: Ending) -> Self {
        Self {
            inner: FakeProvider::from_toml("id = \"fake\"\nscore = 200\nhosts = [\"fake.test\"]\n")
                .unwrap(),
            line: None,
            late: None,
            gate: Gate::new(),
            ending,
            gate_from: 0,
            calls: AtomicUsize::new(0),
        }
    }

    /// Emits `line` as a `postprocessing` stage frame before parking.
    fn with_line(mut self, line: &'static str) -> Self {
        self.line = Some(line);
        self
    }

    /// Emits `line` as a `downloading` stage frame *after* the postprocessing one.
    fn with_late_downloading(mut self, line: &'static str) -> Self {
        self.late = Some(line);
        self
    }

    /// Lets the first `n` downloads run to their ending without parking.
    fn gate_from(mut self, n: usize) -> Self {
        self.gate_from = n;
        self
    }

    fn gate(&self) -> Gate {
        self.gate.clone()
    }
}

#[async_trait::async_trait]
impl Provider for Gated {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn matches(&self, url: &url::Url) -> Match {
        self.inner.matches(url)
    }

    fn catalog(&self) -> Arc<aulos_core::FormatCatalog> {
        self.inner.catalog()
    }

    async fn resolve(
        &self,
        url: &url::Url,
        ctx: ResolveCtx<'_>,
    ) -> Result<Vec<MediaEntry>, ProviderError> {
        self.inner.resolve(url, ctx).await
    }

    async fn download(
        &self,
        ctx: DownloadCtx<'_>,
        sink: ProgressSink,
    ) -> Result<Outcome, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        sink.stage(Stage::Preparing, None).await;
        sink.stage(Stage::Downloading, None).await;
        if call >= self.gate_from {
            if let Some(line) = self.line {
                // Exactly what the shim emits for
                // `{"t":"pp","postprocessor":"MoveFiles","status":"started"}` (DESIGN §9.5).
                sink.stage(Stage::Postprocessing, Some(line.into())).await;
            }
            if let Some(late) = self.late {
                sink.stage(Stage::Downloading, Some(late.into())).await;
            }
            tokio::select! {
                () = self.gate.cancelled() => {}
                () = ctx.cancel.cancelled() => return Err(ProviderError::Canceled),
            }
        }
        match self.ending {
            Ending::Fail(code) => Err(ProviderError::from_code(code, "scripted failure")),
            Ending::Finish => {
                let name = "A video.mp4";
                std::fs::write(ctx.out_dir.join(name), vec![0_u8; 1_024])
                    .map_err(|e| ProviderError::Disk(e.to_string()))?;
                let mut outcome = Outcome::file(aulos_core::RelPath::parse(name).unwrap(), 1_024);
                outcome.entry_final = Some(ctx.entry.state.clone());
                Ok(outcome)
            }
        }
    }

    async fn probe(&self) -> ProviderHealth {
        self.inner.probe().await
    }
}

/// A harness whose only provider parks at a gate carrying the postprocessor line, plus that gate.
async fn parked(ending: Ending) -> (Harness, Gate) {
    let provider = Arc::new(Gated::new(ending).with_line(MOVE_FILES));
    let gate = provider.gate();
    let h = Harness::builder().provider(provider).build().await;
    (h, gate)
}

/// Waits for the row to be carrying the live postprocessor line, and fails loudly if it never is —
/// the assertion that keeps every "and then it is gone" below from passing vacuously.
async fn until_live_line(h: &Harness, id: ItemId) -> Item {
    let running = h
        .until(id, "the live postprocessor line", |i| {
            i.msg.as_deref() == Some(MOVE_FILES)
        })
        .await;
    assert_eq!(
        running.status,
        Status::Postprocessing,
        "the line belongs to a real state while it is live"
    );
    running
}

// ---------------------------------------------------------------------------
// the terminal write
// ---------------------------------------------------------------------------

/// The bug, end to end: a postprocessor line is on the row while it runs, and gone the moment it
/// is `finished` — on the persisted row and on the view the `completed` frame is built from.
#[tokio::test]
async fn a_finished_item_carries_no_postprocessor_message() {
    let (h, gate) = parked(Ending::Finish).await;
    let id = h.add("https://fake.test/watch/pp").await;
    until_live_line(&h, id).await;
    gate.cancel();

    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(
        done.msg, None,
        "a finished row's status line is null, not the last postprocessor it ran"
    );
    assert_eq!(done.error, None);
    assert!(done.filename.is_some(), "and it really did produce a file");

    let completed = h.events.completed();
    assert_eq!(completed.len(), 1, "one terminal frame");
    assert_eq!(completed[0].id, id);
    assert_eq!(
        completed[0].msg, None,
        "the published view is what the wire's `completed` frame carries"
    );
}

/// The same defect on the other terminal status. `"MoveFiles…"` is no more a *reason* for a
/// failure than it is for a success, and a v2 client renders `msg` as the row's subtitle either
/// way — the only thing that hid this on v1 is the shim substituting the error text.
#[tokio::test]
async fn a_failed_item_carries_no_postprocessor_message_either() {
    let (h, gate) = parked(Ending::Fail(ErrorCode::Unavailable)).await;
    let id = h.add("https://fake.test/watch/pp").await;
    until_live_line(&h, id).await;
    gate.cancel();

    let failed = h.until_status(id, Status::Error).await;
    assert_eq!(
        failed.msg, None,
        "a failed row's status line is null too; the reason is in `error`"
    );
    let error = failed.error.expect("the failure is still reported");
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(!error.message.is_empty(), "with a message a human can read");
}

/// And on the third. A cancel lands on whatever the item was doing, so without the clear a
/// cancelled row reads `"MoveFiles…"` exactly like a finished one did.
#[tokio::test]
async fn a_canceled_item_carries_no_postprocessor_message_either() {
    let (h, _gate) = parked(Ending::Finish).await;
    let id = h.add("https://fake.test/watch/pp").await;
    until_live_line(&h, id).await;

    h.handle.actions(Action::Cancel, vec![id], None).await;
    let canceled = h.until_status(id, Status::Canceled).await;
    assert_eq!(
        canceled.msg, None,
        "the live line does not survive a cancel"
    );
    assert_eq!(
        canceled.error.expect("cancel records itself").code,
        ErrorCode::Canceled
    );
}

/// The pre-terminal phase writes its own label (DESIGN §13). It is a live line like any other, so
/// the terminal write drops it too.
#[tokio::test]
async fn a_pre_terminal_hook_label_does_not_survive_the_terminal_write() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/remux").await;
    h.until(id, "the pre-terminal phase", |i| {
        i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
    })
    .await;

    h.handle.hooks_finished(id).await;
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.msg, None, "the hook's label is not a terminal summary");
    assert_eq!(done.size, Some(1_024), "and the outcome still landed");
}

// ---------------------------------------------------------------------------
// the group roll-up
// ---------------------------------------------------------------------------

/// A group's terminal status is written by `Engine::sync_group_status`, **not** by
/// `Engine::terminate` (DESIGN §8.6), so the rule has to hold there independently.
///
/// The row is seeded carrying `"Paused"` rather than driven into that state through the API,
/// because today no live writer reaches a group id: promotion clears `msg`, no provider frame
/// carries a group, and a `pause` on a group finds it already rolled to `queued` by the time its
/// own turn comes. That is a fact about the current callers, not an invariant — `park_running`
/// will set `Paused` on whatever id it is handed and `expand_targets` does put a group id on its
/// own target list — and a restart reloads whatever the column holds. This pins the roll-up so the
/// next writer that does reach a group cannot reintroduce the reported symptom on it.
#[tokio::test]
async fn a_finished_group_carries_no_status_line() {
    let mut group = seed_row(1, Status::Downloading, true);
    group.kind = Kind::Group;
    group.children_total = Some(1);
    group.msg = Some("Paused".into());
    let gid: GroupId = group.id;

    let mut child = seed_row(2, Status::Queued, true);
    child.group_id = Some(gid);
    child.group_index = Some(1);

    let h = Harness::builder().seed(vec![group, child]).build().await;

    let rolled = h
        .until(gid, "the group's terminal roll-up", |i| {
            i.status.is_terminal()
        })
        .await;
    assert_eq!(rolled.status, Status::Finished, "its only child finished");
    assert_eq!(
        rolled.msg, None,
        "a group's roll-up clears the line the same way an item's terminal write does"
    );
}

/// A seed row in whatever state a test needs (the shape `tests/recovery.rs` uses).
fn seed_row(ord: i64, status: Status, auto_start: bool) -> Item {
    let url = url::Url::parse(&format!("https://fake.test/watch/{ord}")).unwrap();
    Item {
        id: ItemId::new(),
        kind: Kind::Item,
        group_id: None,
        group_index: None,
        ord,
        url: url.clone(),
        canonical_key: format!("fake\u{1f}seed-{ord}").into(),
        provider: Some(support::pid("fake")),
        media_id: Some(format!("seed-{ord}").into()),
        title: format!("Seed {ord}").into(),
        status,
        auto_start,
        msg: None,
        error: None,
        request: DownloadRequest::new(url, selection()),
        entry: None,
        filename: None,
        size: None,
        chapter_files: Vec::new(),
        subtitle_files: Vec::new(),
        created_at: 1_700_000_000_000 + ord,
        started_at: None,
        finished_at: None,
        attempt: 0,
        source: SourceRef::bare(SourceKind::ApiV2),
        children_total: None,
        clear_after: None,
    }
}

// ---------------------------------------------------------------------------
// late frames
// ---------------------------------------------------------------------------

/// A stage frame the provider emitted before it finished can be handled after the outcome was:
/// `ProgressMsg::Stage` goes provider → aggregator → engine, `EngineCmd::Finished` goes provider →
/// engine. On a terminal row the late frame is dropped, not written.
#[tokio::test]
async fn a_stage_frame_that_arrives_after_the_terminal_write_is_refused() {
    let h = Harness::new().await;
    let id = h.add("https://fake.test/watch/x").await;
    h.until_status(id, Status::Finished).await;

    h.sink
        .for_item(id)
        .stage(Stage::Postprocessing, Some(MOVE_FILES.into()))
        .await;
    h.settle().await;

    let done = h.item(id).await.unwrap();
    assert_eq!(done.status, Status::Finished, "no edge out of terminal");
    assert_eq!(done.msg, None, "and nothing put the stale line back");
}

/// The same race one step earlier: between `Finished` and `HooksFinished` the row is not terminal
/// yet, but the download is over and the pre-terminal phase owns its `msg` and `status`
/// (DESIGN §13). A late frame there must not overwrite the hook's label either.
#[tokio::test]
async fn a_stage_frame_that_arrives_during_the_finalizer_phase_is_refused() {
    let h = Harness::builder()
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;
    let id = h.add("https://fake.test/watch/remux").await;
    h.until(id, "the pre-terminal phase", |i| {
        i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
    })
    .await;

    h.sink
        .for_item(id)
        .stage(Stage::Downloading, Some(MOVE_FILES.into()))
        .await;
    h.settle().await;

    let parked = h.item(id).await.unwrap();
    assert_eq!(
        parked.status,
        Status::Postprocessing,
        "a late frame cannot drag a finalising row back to downloading"
    );
    assert_eq!(
        parked.msg.as_deref(),
        Some("Re-encoding audio"),
        "the hook owns the line until HooksFinished"
    );

    h.handle.hooks_finished(id).await;
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.msg, None);
}

/// The awaiting-hooks refusal arm is scoped to *this* run, not to the id.
///
/// `postprocessing` is a running status, so a pause during the pre-terminal window takes
/// `Engine::park_running`, which parks the row as `queued` — and used to leave the id in
/// `pending_hooks` (only `cancel_one` cleared it). The restarted run's frames were then all
/// refused by the arm this fix added, so the row sat at `preparing` with no `downloading`
/// transition until the hook deadline fired `finalise_success` against the *previous* run's
/// outcome, on a row with a live download process.
#[tokio::test]
async fn pausing_inside_the_pre_terminal_phase_frees_the_row_for_its_next_run() {
    // Call 0 finishes at once, so the row reaches the pre-terminal phase; call 1 — the restarted
    // run — parks at the gate where the test can look at it.
    let provider = Arc::new(Gated::new(Ending::Finish).gate_from(1));
    let gate = provider.gate();
    let h = Harness::builder()
        .provider(provider)
        .pre_terminal(Arc::new(AlwaysPreTerminal("Re-encoding audio")))
        .build()
        .await;

    let id = h.add("https://fake.test/watch/remux").await;
    h.until(id, "the pre-terminal phase", |i| {
        i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
    })
    .await;

    let paused = h.handle.actions(Action::Pause, vec![id], None).await;
    assert_eq!(paused.applied, vec![id], "postprocessing is pausable");
    let parked = h
        .until(id, "the parked row", |i| i.status == Status::Queued)
        .await;
    assert_eq!(parked.msg.as_deref(), Some("Paused"));
    assert!(!parked.auto_start, "parked means the user has to say go");

    h.handle.actions(Action::Start, vec![id], None).await;
    // The whole point: the restarted run's own frames are applied. Without the `pending_hooks`
    // removal this never leaves `preparing`.
    h.until(id, "the restarted run reporting progress", |i| {
        i.status == Status::Downloading
    })
    .await;

    gate.cancel();
    h.until(id, "the second pre-terminal phase", |i| {
        i.status == Status::Postprocessing && i.msg.as_deref() == Some("Re-encoding audio")
    })
    .await;
    h.handle.hooks_finished(id).await;

    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.msg, None);
    assert_eq!(done.size, Some(1_024), "the second run's outcome landed");
}

/// A provider's own line for a frame that arrives after it has already reported postprocessing.
const LATE_LINE: &str = "Still downloading…";

/// A stage frame that would move a row *backwards* along `preparing → downloading →
/// postprocessing` is a late one, and a late frame is not a reason to log at WARN on a download
/// that is going perfectly well. It carries its line onto the row it finds and leaves the status
/// where it is.
#[tokio::test]
async fn a_late_downloading_frame_keeps_its_line_without_dragging_the_row_back() {
    let provider = Arc::new(
        Gated::new(Ending::Finish)
            .with_line(MOVE_FILES)
            .with_late_downloading(LATE_LINE),
    );
    let gate = provider.gate();
    let h = Harness::builder().provider(provider).build().await;
    let id = h.add("https://fake.test/watch/late").await;

    let row = h
        .until(id, "the late frame's line", |i| {
            i.msg.as_deref() == Some(LATE_LINE)
        })
        .await;
    assert_eq!(
        row.status,
        Status::Postprocessing,
        "a late frame never moves a row back down the happy path"
    );
    // And no published view ever showed the row going backwards either.
    let statuses: Vec<Status> = h.events.changes(id).iter().map(|v| v.status).collect();
    let back = statuses
        .windows(2)
        .any(|w| w[0] == Status::Postprocessing && w[1] == Status::Downloading);
    assert!(!back, "got {statuses:?}");

    gate.cancel();
    let done = h.until_status(id, Status::Finished).await;
    assert_eq!(done.msg, None, "the terminal write still clears the line");
}
