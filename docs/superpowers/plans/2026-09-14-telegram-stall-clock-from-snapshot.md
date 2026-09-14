# Telegram stall clock fed from the progress snapshot — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Telegram bot's `⚠️ Download seems stalled for {secs}s` warning fires only when a running download has reported no new progress for `TELEGRAM_STALL_TIMEOUT_SECONDS`, not merely because the row has sat in `downloading` for that long.

**Architecture:** `WatchRegistry` (crates/aulos-telegram/src/watch.rs) keeps `last_progress_at` per watched job, but the bot only ever calls `touch` from `DomainEvent::StatusChanged` (bot.rs:699). Progress never arrives as an event (DESIGN §15.1); the bot already pulls it from `ProgressReader` once per tick, but only in board mode and only for the board's rows (`refresh_progress`). The fix: on every tick, in both modes, before `due_warnings`, read each running watched job's snapshot view and `touch` it when its progress fingerprint changed since the last tick. A fingerprint that does not change (a genuinely stalled download) still trips the warning.

**Tech Stack:** Rust 2024, tokio (paused-time tests), workspace crates `aulos-core` / `aulos-telegram`.

**Spec:** `docs/DESIGN.md` §12.5 (the two warnings), §15.1 (progress is pulled from the snapshot, never an event). Root cause evidence: prod job `01M2FHBEVV97EGYP9259BPW3PA` (2026-09-14) entered `downloading` at ~08:43:26Z, yt-dlp reported fragments continuously until 08:48:27Z, and the warning landed at 08:46Z — exactly 180 s after the status change.

## Global Constraints

- Commit messages: lower-case `component: what changed` subject, the why in the body. **No AI attribution trailers** (no `Co-Authored-By`, no "Generated with", no session links) — repo rule.
- New code carries **no doc-comment prose**. At most one short `//` line where intent is non-obvious. Put the why in the commit body.
- Gates before every commit: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p aulos-telegram`.
- No `unwrap()` outside tests. Tests use `expect`/`unwrap` freely (`#[allow(clippy::expect_used)]` already on the module).
- Do not touch `crates/aulos-queue/src/watchdog.rs` — the engine's 900 s watchdog is a separate safety net and is correct.
- Work on branch `fix/telegram-stall-progress` off `main` (created in Task 1).

---

### Task 1: `WatchRegistry` learns progress from a snapshot view

**Files:**
- Modify: `crates/aulos-telegram/src/watch.rs` — `Watched` struct (lines ~49-73), `WatchRegistry::watch` (line ~161), new methods after `park` (line ~200), unit tests at the bottom of `mod tests`.

**Interfaces:**
- Consumes: `aulos_core::item::ItemView` fields `status`, `percent`, `downloaded_bytes`, `fragment_index`, `phase`, `phase_percent`, `msg`; existing `WatchRegistry::touch(id, now)`.
- Produces:
  - `pub struct ProgressMark { status: Status, percent: f64, downloaded_bytes: Option<u64>, fragment_index: Option<u32>, phase: Option<PhaseTag>, phase_percent: Option<f64>, msg: Option<Arc<str>> }` with `impl From<&ItemView> for ProgressMark`, `#[derive(Clone, PartialEq, Debug)]`.
  - `Watched.last_seen: Option<ProgressMark>` (pub, like the other fields).
  - `pub fn running_ids(&self) -> Vec<ItemId>` — ids of watched jobs whose `running_since.is_some()`.
  - `pub fn observe_progress(&mut self, view: &ItemView, now: Instant)` — stores `ProgressMark::from(view)` as `last_seen`; if the job is running and the mark differs from the previous `last_seen`, sets `last_progress_at = now` (not `touch`, which would also start the hard-timeout clock on a parked job). The baseline mark is recorded by `watch` from the `Added` view, so the first tick counts as progress only if the numbers moved since the add.

- [ ] **Step 1: Create the branch**

```bash
cd /Users/apogliaghi/Documents/GitHub/aulos && git checkout -b fix/telegram-stall-progress main
```

- [ ] **Step 2: Write the failing unit tests**

Append inside `mod tests` in `crates/aulos-telegram/src/watch.rs`, after `neither_watchdog_fires_on_an_item_that_has_not_started`'s closing brace (before the `Mark::Stalled.glyph()` test is fine too — anywhere inside the module):

```rust
    #[tokio::test(start_paused = true)]
    async fn a_moving_snapshot_resets_the_stall_clock_without_any_event() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let mut v = item(SourceRef::bare(SourceKind::ApiV2));
        let start = Instant::now();
        r.watch(&v, start);
        assert_eq!(r.running_ids(), vec![v.id]);

        for i in 1..=10u64 {
            v.percent = i as f64 * 5.0;
            v.downloaded_bytes = Some(i * 1_000_000);
            r.observe_progress(&v, start + Duration::from_secs(i * 100));
            assert!(
                r.due_warnings(start + Duration::from_secs(i * 100)).is_empty(),
                "progress at +{}s is not a stall",
                i * 100
            );
        }
        assert_eq!(
            r.get(v.id).expect("watched").last_progress_at,
            start + Duration::from_secs(1_000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unchanged_snapshot_still_reads_as_a_stall() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let mut v = item(SourceRef::bare(SourceKind::ApiV2));
        let start = Instant::now();
        r.watch(&v, start);
        v.percent = 43.2;
        v.downloaded_bytes = Some(9_000);
        r.observe_progress(&v, start + Duration::from_secs(10));

        for s in [20u64, 60, 120, 180] {
            r.observe_progress(&v, start + Duration::from_secs(s));
            assert!(r.due_warnings(start + Duration::from_secs(s)).is_empty());
        }
        r.observe_progress(&v, start + Duration::from_secs(191));
        let stalls = r.due_warnings(start + Duration::from_secs(191));
        assert_eq!(stalls.len(), 1, "{stalls:?}");
        assert_eq!(stalls[0].kind, Mark::Stalled);
        assert_eq!(stalls[0].secs, 181, "measured from the last change at +10 s");
    }

    #[tokio::test(start_paused = true)]
    async fn speed_and_eta_alone_are_not_progress() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let mut v = item(SourceRef::bare(SourceKind::ApiV2));
        let start = Instant::now();
        r.watch(&v, start);
        r.observe_progress(&v, start);

        v.speed = Some(1.0);
        v.eta = Some(99);
        r.observe_progress(&v, start + Duration::from_secs(100));
        v.speed = Some(2.0);
        v.eta = Some(50);
        r.observe_progress(&v, start + Duration::from_secs(181));
        assert_eq!(r.due_warnings(start + Duration::from_secs(181)).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_postprocessing_phase_that_moves_is_progress() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let mut v = item_in(SourceRef::bare(SourceKind::ApiV2), Status::Postprocessing, true);
        let start = Instant::now();
        r.watch(&v, start);
        r.observe_progress(&v, start);

        v.phase_percent = Some(10.0);
        r.observe_progress(&v, start + Duration::from_secs(170));
        v.phase_percent = Some(20.0);
        r.observe_progress(&v, start + Duration::from_secs(340));
        assert!(r.due_warnings(start + Duration::from_secs(340)).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_parked_job_is_not_touched_by_its_snapshot_and_is_not_listed_as_running() {
        let mut r = WatchRegistry::new(180, 7_200, true, vec![7]);
        let mut v = item_in(SourceRef::bare(SourceKind::ApiV2), Status::Queued, true);
        let start = Instant::now();
        r.watch(&v, start);
        assert!(r.running_ids().is_empty());

        v.percent = 50.0;
        r.observe_progress(&v, start + Duration::from_secs(30));
        assert_eq!(r.get(v.id).expect("watched").running_since, None);
        assert!(r.due_warnings(start + Duration::from_secs(1_000)).is_empty());
    }
```

Note the first test compares `last_progress_at` and `running_since` — both are already `pub` fields on `Watched`.

- [ ] **Step 3: Run the tests to verify they fail to compile**

Run: `cargo test -p aulos-telegram --lib watch::tests 2>&1 | tail -20`
Expected: compile errors — `no method named running_ids`, `no method named observe_progress`.

- [ ] **Step 4: Implement `ProgressMark`, `last_seen`, `running_ids`, `observe_progress`**

In `crates/aulos-telegram/src/watch.rs`:

Add to the imports:

```rust
use aulos_core::progress::PhaseTag;
use aulos_core::status::Status;
```

(`Status` is already imported inside `mod tests`; leave that import alone, or remove it if clippy flags it as unused/shadowing.)

Add above `pub struct Watched`:

```rust
#[derive(Clone, PartialEq, Debug)]
pub struct ProgressMark {
    pub status: Status,
    pub percent: f64,
    pub downloaded_bytes: Option<u64>,
    pub fragment_index: Option<u32>,
    pub phase: Option<PhaseTag>,
    pub phase_percent: Option<f64>,
    pub msg: Option<Arc<str>>,
}

impl From<&ItemView> for ProgressMark {
    fn from(v: &ItemView) -> Self {
        Self {
            status: v.status,
            percent: v.percent,
            downloaded_bytes: v.downloaded_bytes,
            fragment_index: v.fragment_index,
            phase: v.phase,
            phase_percent: v.phase_percent,
            msg: v.msg.clone(),
        }
    }
}
```

Add to `Watched` after `mark`:

```rust
    /// The snapshot as last observed on a tick.
    pub last_seen: Option<ProgressMark>,
```

In `WatchRegistry::watch`, in the `or_insert_with` initialiser, add:

```rust
            last_seen: Some(ProgressMark::from(item)),
```

Add after `park`:

```rust
    pub fn running_ids(&self) -> Vec<ItemId> {
        self.jobs
            .iter()
            .filter(|(_, w)| w.running_since.is_some())
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn observe_progress(&mut self, view: &ItemView, now: Instant) {
        let Some(w) = self.jobs.get_mut(&view.id) else {
            return;
        };
        let mark = ProgressMark::from(view);
        let moved = w.last_seen.as_ref() != Some(&mark);
        w.last_seen = Some(mark);
        if moved && w.running_since.is_some() {
            w.last_progress_at = now;
        }
    }
```

`observe_progress` writes `last_progress_at` directly rather than calling `touch`, so a snapshot can never *start* the hard-timeout clock on a parked job; only a `StatusChanged` to a running status does that (bot.rs:699).

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -p aulos-telegram --lib watch::tests 2>&1 | tail -20`
Expected: all `watch::tests` pass, including the five new ones.

- [ ] **Step 6: Gates**

```bash
cargo fmt --all && cargo clippy -p aulos-telegram --all-targets -- -D warnings
```
Expected: clean. If clippy complains about `PhaseTag` being `#[non_exhaustive]` in a derive, that is not an error — it only matters for matching. If it flags `cast_precision_loss` on `i as f64` in a test, use `f64::from(u32::try_from(i).expect("small"))` or write the loop over `1..=10u32` and compute bytes with `u64::from(i)`.

- [ ] **Step 7: Commit**

```bash
git add crates/aulos-telegram/src/watch.rs
git commit -m "telegram: a watch learns progress from the snapshot, not only from status changes

The stall clock was reset only by StatusChanged, and progress never arrives
as an event (DESIGN 15.1), so any download longer than
TELEGRAM_STALL_TIMEOUT_SECONDS was reported stalled while its fragments
were still landing. observe_progress compares a fingerprint of the
snapshot view (status, percent, bytes, fragment, phase, msg) against the
last tick and resets the clock only when it moved. Speed and eta are not
part of the fingerprint. A parked job is never touched by its snapshot so
the hard-timeout clock still starts only on a running status."
```

---

### Task 2: The bot feeds the registry from the snapshot on every tick, in both modes

**Files:**
- Modify: `crates/aulos-telegram/src/bot.rs` — `on_tick` (line ~774), new method next to `refresh_progress` (line ~854).
- Test: `crates/aulos-telegram/tests/actor.rs` — replace `progress_keeps_the_stall_warning_away` (line ~1175), add two tests after it.

**Interfaces:**
- Consumes: `WatchRegistry::running_ids()`, `WatchRegistry::observe_progress(&ItemView, Instant)` from Task 1; `self.progress: Option<Arc<dyn ProgressReader>>` (bot.rs:280); test harness `h.progress.publish(ItemView)`, `progressing(id, title, percent, speed, eta)`, `Harness::builder().telegram(TelegramConfig { board: TelegramBoard::PerJob, ..TelegramConfig::for_test(vec![CHAT]) })`.
- Produces: `fn refresh_liveness(&mut self, now: Instant)` on the actor (private).

- [ ] **Step 1: Rewrite the misleading integration test and add the two new ones**

In `crates/aulos-telegram/tests/actor.rs`, replace the whole `progress_keeps_the_stall_warning_away` test (its doc line included) with:

```rust
/// The bug this pins: progress never arrives as an event (DESIGN §15.1), so a stall clock reset
/// only by `StatusChanged` called every download longer than 180 s stalled while it was moving.
#[tokio::test]
async fn snapshot_progress_keeps_the_stall_warning_away() {
    let mut h = Harness::new().await;
    let id = ItemId::new();
    let v = tg_view(id, "Slow", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;

    // Only the snapshot moves — not one further event reaches the actor.
    for i in 1..=10u32 {
        h.progress.publish(progressing(
            id,
            "Slow",
            f64::from(i) * 5.0,
            Some(1_000.0),
            Some(100),
        ));
        h.advance(Duration::from_secs(100)).await;
    }
    assert!(
        !h.transport.texts().iter().any(|t| t.contains("stalled")),
        "1000 s of steady snapshot progress is not a stall: {:?}",
        h.transport.texts()
    );
}

#[tokio::test]
async fn a_snapshot_that_stops_moving_is_reported_stalled_from_its_last_change() {
    let mut h = Harness::new().await;
    let id = ItemId::new();
    let v = tg_view(id, "Stuck", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;

    h.progress
        .publish(progressing(id, "Stuck", 43.2, Some(1_000.0), Some(30)));
    h.advance(Duration::from_secs(100)).await;
    h.transport.clear();

    // The same numbers, tick after tick: 170 s later still nothing …
    h.advance(Duration::from_secs(170)).await;
    assert!(!h.transport.texts().iter().any(|t| t.contains("stalled")));

    // … and past 180 s since the last change, the warning, measured from that change.
    h.advance(Duration::from_secs(11)).await;
    let texts = h.transport.texts();
    assert!(
        texts
            .iter()
            .any(|t| t == "⚠️ Download seems stalled for 181s:\nhttps://a.test/watch/1"),
        "{texts:?}"
    );
}

#[tokio::test]
async fn per_job_mode_also_reads_the_snapshot_for_the_stall_clock() {
    let mut h = Harness::builder()
        .telegram(TelegramConfig {
            board: TelegramBoard::PerJob,
            ..TelegramConfig::for_test(vec![CHAT])
        })
        .build()
        .await;
    let id = ItemId::new();
    let v = tg_view(id, "Slow", Status::Downloading, CHAT);
    h.observe(&added(&v)).await;
    h.tick().await;

    for i in 1..=10u32 {
        h.progress.publish(progressing(
            id,
            "Slow",
            f64::from(i) * 5.0,
            Some(1_000.0),
            Some(100),
        ));
        h.advance(Duration::from_secs(100)).await;
    }
    assert!(
        !h.transport.texts().iter().any(|t| t.contains("stalled")),
        "{:?}",
        h.transport.texts()
    );
}
```

Check the `use` block at the top of `actor.rs` already imports `TelegramConfig` and `TelegramBoard` (the `per_job_mode_draws_no_board` test uses them, so it does) and `progressing` from `support` (the snapshot tests at ~line 1455 use it, so it does).

- [ ] **Step 2: Run the three tests to verify the first and third fail**

Run: `cargo test -p aulos-telegram --test actor stall 2>&1 | tail -30`
Expected: `snapshot_progress_keeps_the_stall_warning_away` FAILS (a "stalled" text appears), `per_job_mode_also_reads_the_snapshot_for_the_stall_clock` FAILS the same way, `a_snapshot_that_stops_moving_is_reported_stalled_from_its_last_change` may already pass or fail on the `181s` figure — either is fine at this step.

- [ ] **Step 3: Implement `refresh_liveness` and call it from `on_tick`**

In `crates/aulos-telegram/src/bot.rs`, change the start of `on_tick`:

```rust
    async fn on_tick(&mut self) {
        let now = self.clock.instant();
        self.refresh_liveness(now);

        // The two watchdogs. In board mode they are sent as separate messages — they are alerts,
        // not state — and the board line gains its marker (DESIGN §12.5).
        for warning in self.watches.due_warnings(now) {
```

Add the method directly above `fn refresh_progress`:

```rust
    // Both modes, before the watchdogs: progress is never an event (DESIGN §15.1).
    fn refresh_liveness(&mut self, now: Instant) {
        let Some(reader) = self.progress.as_deref() else {
            return;
        };
        for id in self.watches.running_ids() {
            if let Some(view) = reader.view(id) {
                self.watches.observe_progress(&view, now);
            }
        }
    }
```

`Instant` here is `tokio::time::Instant`; check the existing `use` lines in bot.rs (`self.clock.instant()` returns it, and `WatchRegistry` imports `tokio::time::Instant`). Add `use tokio::time::Instant;` if it is not already imported.

- [ ] **Step 4: Run the actor tests**

Run: `cargo test -p aulos-telegram --test actor 2>&1 | tail -30`
Expected: all pass, including the three from Step 1 and the untouched `the_two_warnings_fire_once_and_mark_the_board_line` (no snapshot is published there, so the row still stalls at 181 s).

- [ ] **Step 5: Gates**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p aulos-telegram
```
Expected: all clean and green.

- [ ] **Step 6: Commit**

```bash
git add crates/aulos-telegram/src/bot.rs crates/aulos-telegram/tests/actor.rs
git commit -m "telegram: the stall watchdog reads the progress snapshot on every tick

refresh_progress only ran in board mode and only fed the board rows; the
watch table never saw the numbers, so the stall warning fired 180 s into
every longer download whatever yt-dlp was reporting (prod, 2026-09-14,
MccJdr61xnc: 604 fragments still landing when the warning went out).
refresh_liveness runs first on the tick in both modes. The old test
simulated progress as repeated StatusChanged events the engine never
publishes; it now moves only the snapshot, and a snapshot that stops
moving is still reported, measured from its last change."
```

---

### Task 3: DESIGN §12.5 records where the stall clock's progress comes from

**Files:**
- Modify: `docs/DESIGN.md` — §12.5, the paragraph beginning "Both clocks time the **download**" (line ~3350).

- [ ] **Step 1: Add one paragraph after "…one bogus warning per queued item into every allowed chat."**

```markdown
The stall clock is reset by **progress**, and progress is never an event (§15.1): on every tick,
in both modes, the bot reads each running watched job's snapshot view and treats a change in
`status`, `percent`, `downloaded_bytes`, `fragment_index`, `phase`, `phase_percent` or `msg` as
progress. `speed` and `eta` are not progress. A snapshot whose numbers stop moving is reported
stalled, measured from the last change. The engine's own 900 s stall notice (§8.11) is the safety
net underneath and is unaffected.
```

- [ ] **Step 2: Commit**

```bash
git add docs/DESIGN.md
git commit -m "design: the telegram stall clock is fed from the progress snapshot"
```

---

### Task 4: Full workspace gates and a prod build check

**Files:** none modified.

- [ ] **Step 1: Run the full gates**

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked" | sort | uniq -c
```
Expected: every `test result: ok`, zero `FAILED`.

- [ ] **Step 2: Report**

`git log --oneline main..HEAD` should list exactly three commits. Deployment to the VPS (`ghcr.io/tatoalo/aulos:latest` via the docker workflow on push to main, or the manual `docker build --platform linux/amd64` + `docker save | ssh … docker load` recipe) is done by the session owner after the PR, not by this task.
