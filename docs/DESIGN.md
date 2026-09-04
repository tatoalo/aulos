# Aulos Server — Definitive Architecture (DESIGN.md)

Status: **binding**. This document supersedes everything in `docs/design-candidates/`.
Base skeleton: the `migration` candidate. Grafted: the bounded-snapshot / durable-`seq` /
in-place-expansion machinery from `realtime`, and the per-URL catalog / plugin-manifest /
error-taxonomy / `last_sent`-diff machinery from `extensibility`.

Where this document conflicts with `docs/BRIEF.md`, the BRIEF wins. Where it conflicts with a
candidate proposal, this document wins.

Companion documents:
- `docs/PROTOCOL.md` — the v2 wire protocol, written for a Swift client author.
- `docs/PLAN.md` — parallelisable implementation work packages.

Reading order for implementers: §2 (topology) → §3 (crates) → §4 (domain) → §6 (providers) →
§8 (queue) → §10 (protocol internals) → §14 (config) → Appendix A (legacy traceability).

---

## 1. Theses

| # | Thesis | Consequence |
|---|---|---|
| T1 | The VPS `docker-compose.yml` must not need editing to cut over. | Every legacy env var name/default/semantic is preserved (§14). New knobs are `AULOS_*` with safe defaults. `AULOS_DB_PATH` defaults inside `STATE_DIR`. |
| T2 | Rollback must be free. | The importer **reads** the legacy JSON and never mutates, renames, quarantines or deletes it. A downgrade to the Python image resumes from untouched JSON (§7.6). |
| T3 | Progress is the hot path and must never touch disk, locks or the scheduler. | Progress flows on a dedicated mpsc into one aggregator task, is coalesced at a fixed cadence, diffed against last-sent, serialised **once** into `Bytes`, and broadcast as `Arc<WireFrame>` (§9, §10). |
| T4 | One immutable ULID per record, everywhere. | `url` is data. The v1 shim is the only place that resolves URL→id, by lookup (§11.3). |
| T5 | One shape on the wire. | Groups are `items` rows with `kind:"group"`. One Swift struct, one array, one decoder. |
| T6 | Everything the client must decide is contractual, not inferred. | The snapshot advertises `protocol.delta_semantics`, `batch_ms`, `urgent_ms`, `boot_id`; every optional field is always serialised (`null`, never absent). |
| T7 | Connect cost is bounded, forever. | Done-window, group-children threshold, `truncated` block, paged history (§9.6). A year of use does not make foregrounding the app slower. |
| T8 | Operability is a feature. | `healthz` covers every component including the POT sidecar; the sidecar is supervised; the cutover has a rehearsal and a written runbook (§16, §19). |
| T9 | Nothing is changed silently. | Appendix A maps legacy spec §1–§12 to v2. Appendix B is the complete intentional-change list with a user-visible reason each. |
| T10 | A new site is one file, no recompile. | The `command` provider manifest (§6.5) and the `[[hook]]` manifest (§13.5) are the extension surface, both fully specified. |

---

## 2. Runtime topology

### 2.1 Process tree inside the container

```
PID 1  tini -g --
  └── /usr/local/bin/aulos-entrypoint          (sh: PUID/PGID/UMASK/CHOWN_DIRS, then exec)
        └── aulos-server                        (Rust, tokio multi-thread)
              ├── [supervised]  bgutil-pot server                     own pgid, §16.2
              ├── [job child]   python3 /app/python/ytdlp_runner.py    own pgid, one per yt-dlp job
              ├── [job child]   N_m3u8DL-RE | ffmpeg                   own pgid, one per SC job
              ├── [job child]   <command plugin argv>                  own pgid, one per plugin job
              └── [hook child]  ffmpeg | ffprobe | <hook command>      own pgid, short-lived
```

`tini -g` stays PID 1 so `docker stop` reaps grandchildren even if the server crashes. **Every**
child is spawned with `process_group(0)` so it is its own group leader and `killpg` can never
reach a sibling (§8.7, risk R9).

### 2.2 Task topology inside `aulos-server`

```
                    ┌───────────────────────────────────────────────────────┐
 HTTP / WS ────────►│ aulos-api (axum)                                      │
                    │  v2 REST · v2 WS · v1 shim · healthz · files · metrics│
                    └──┬──────────────────▲────────────────▲────────────────┘
                       │ EngineCmd        │ StateView      │ broadcast::Receiver
                       │ (mpsc 1024)      │ (ArcSwap read) │ <Arc<WireFrame>>
                    ┌──▼──────────────────┴────────────────┴────────────────┐
                    │ QueueEngine — ONE task, owns all queue state:         │
                    │ ready deques (per priority), slots, cancel registry,  │
                    │ group counters, dedupe index, ord allocator           │
                    └─┬────────┬──────────────┬──────────────┬─────────────┘
     StoreCmd (mpsc)  │        │ spawn        │ spawn        │ DomainEvent (mpsc 4096)
                      │        │ resolve      │ download     │
        ┌─────────────▼──┐ ┌───▼──────────┐ ┌─▼──────────┐ ┌─▼────────────────────────┐
        │ Store actor    │ │ ResolvePool  │ │ RunPool    │ │ EventRouter (§2.2.1)     │
        │ 1 writer thread│ │ Semaphore(4) │ │ global +   │ └─┬──────┬──────┬──────┬───┘
        │ + N readers    │ └──────┬───────┘ │ per-provider│   │      │      │      │
        └────────────────┘        │         └─┬──────────┘   │      │      │      │
                                  │ ProgressMsg (mpsc 8192)  │      │      │      │
                                  └───────────►┌─────────────▼──┐   │      │      │
                                               │ Aggregator     │   │      │      │
                                               │ cells, last_sent│  │      │      │
                                               │ 250 ms tick +   │  │      │      │
                                               │ 25 ms urgent    │  │      │      │
                                               │ ArcSwap publish │  │      │      │
                                               └───────┬─────────┘  │      │      │
                                                       │ WireFrame  │      │      │
                                               ┌───────▼─────────┐  │      │      │
                                               │ EventHub        │  │      │      │
                                               │ seq · ring ·    │  │      │      │
                                               │ broadcast(256)  │  │      │      │
                                               └─────────────────┘  │      │      │
                                                                    │      │      │
    HookDispatcher (§13) ◄──────────────────────────────────────────┘      │      │
    TelegramActor  (§12) ◄─────────────────────────────────────────────────┘      │
    (future) APNs Notifier (§12.6) ◄──────────────────────────────────────────────┘
    ClearScheduler · PotSupervisor · ConfigWatcher · PluginWatcher  (§16)

    Producers into the router's single inbox: QueueEngine, SubscriptionScheduler (§14),
    ConfigWatcher / PluginWatcher (§17.2), HealthRegistry and PotSupervisor (§16).
```

Design rules:

| Rule | Why |
|---|---|
| `QueueEngine` is a **single task with owned state**, no `Mutex`. Every mutation is a message. | Deterministic; unit-testable with a fake clock; cancel/start/complete races become message ordering, not lock ordering. |
| Progress **never** enters the `QueueEngine`. | 500 children × 20 msg/s cannot starve `POST /downloads`. |
| Stage transitions (`preparing`→`downloading`→`postprocessing`→terminal) **do** enter the engine (they are persisted). ≤ 6 messages per job. | Restart consistency. |
| `DomainEvent` is the single fan-out point for everything non-queue, and the **`EventRouter` is the only owner of the event receiver** (§2.2.1). | Adding an APNs `Notifier` later is one `subscribe()` call at wiring time, zero engine changes. An `mpsc::Receiver` has exactly one owner, so a design in which four consumers each take "the" receiver does not compile; the router is what turns one channel into N inboxes. |
| The store is an actor with **one** writer thread and a read pool. | SQLite WAL: one writer, many readers; no `Connection` across an `await`. |
| The Aggregator owns `last_sent` and derives deltas by **diffing**, never by a hand-maintained dirty-field mask. | A missed mask bit is a permanently stale field on the client with no error anywhere. A diff cannot have that bug. |

#### 2.2.1 `EventRouter` — the one-to-N fan-out

`DomainEvent` has many producers and many consumers. A `tokio::sync::mpsc::Receiver` has exactly
one owner, and a `broadcast` channel cannot express per-subscriber capacity or per-subscriber drop
policy (and silently drops the *oldest* message for a slow reader, which for a `Completed` event
means a hook never runs). So the fan-out is an explicit, tiny task that owns the single receiver
and pushes into one bounded inbox per subscriber.

It lives in **`aulos-core::event`** (it needs nothing but `tokio::sync`), so every subscriber crate
can name its types without a dependency inversion.

```rust
/// Cheap-to-clone producer handle. Held by the engine, the scheduler, the watchers, healthz.
#[derive(Clone)]
pub struct EventSender(mpsc::Sender<DomainEvent>);
impl EventSender {
    /// `send().await` — never dropped, never reordered. Bounded at 4096 (§2.3).
    pub async fn publish(&self, ev: DomainEvent);
    /// For sync contexts (Drop guards, signal handlers). Returns Err on a full or closed channel.
    pub fn try_publish(&self, ev: DomainEvent) -> Result<(), TryPublishError>;
}

/// One subscriber's private inbox. This is what `HookDispatcher::spawn`, `TelegramActor::spawn`
/// and `Aggregator::spawn` take — NOT a raw `mpsc::Receiver<DomainEvent>`.
pub struct EventInbox {
    pub name: &'static str,
    rx: mpsc::Receiver<Arc<DomainEvent>>,
}
impl EventInbox {
    pub async fn recv(&mut self) -> Option<Arc<DomainEvent>>;
    pub fn recv_many(&mut self, buf: &mut Vec<Arc<DomainEvent>>, max: usize) -> impl Future<Output = usize>;
    /// Events dropped for THIS subscriber since start (0 for a `Block` subscriber).
    pub fn dropped(&self) -> u64;
}

pub struct SubscriberSpec {
    pub name: &'static str,          // stable; used as the metric label and in logs
    pub capacity: usize,             // its own inbox depth
    pub policy: DropPolicy,
    pub filter: EventFilter,         // a bitset over DomainEvent discriminants
}
pub enum DropPolicy {
    /// The router awaits this subscriber. Applying backpressure to every other subscriber is
    /// acceptable because losing one of these events is a correctness bug.
    Block,
    /// `try_send`; on a full inbox the **newest** event is dropped, a WARN is logged at most once
    /// per 10 s per subscriber, and `aulos_event_dropped_total{subscriber}` is incremented.
    /// Newest — not oldest — because the oldest event may be the `Completed` that arms a hook.
    DropNewest,
}

pub struct EventRouter { /* … */ }
impl EventRouter {
    pub fn new(capacity: usize) -> (Self, EventSender);
    /// Registration happens once, at wiring time (§16.1 step 11–14), BEFORE `spawn`.
    /// Registering after `spawn` is a programmer error and panics in debug, returns Err in release.
    pub fn subscribe(&mut self, spec: SubscriberSpec) -> EventInbox;
    pub fn spawn(self) -> JoinHandle<()>;
}
```

**Guarantees.**

| Property | Statement |
|---|---|
| Ordering | Per subscriber, strict FIFO in publish order. All subscribers observe the *same relative order* of the events they receive. There is **no** cross-subscriber synchronisation: subscriber A may be 40 events ahead of subscriber B. |
| Cost | The router clones an `Arc<DomainEvent>` per subscriber, never the payload. A 500-child `Added` event is one allocation total. |
| Filtering | A subscriber receives only the discriminants in its `filter`; a filtered-out event is not counted as dropped. Filtering is what keeps the hook inbox from being flooded by progress-driven `StatusChanged`. |
| Shutdown | Dropping `EventSender`s closes the inbox chain; each subscriber drains what it already has and then sees `None`. |
| Liveness | `Block` subscribers must never `await` anything unbounded while between `recv()` calls. The Aggregator satisfies this (it only touches memory); the hook dispatcher and Telegram do not, which is exactly why they are `DropNewest`. |

**The registered subscribers, complete:**

| Subscriber | Capacity | Policy | Filter | Owner |
|---|---|---|---|---|
| `aggregator` | 1024 | `Block` | `Added \| StatusChanged \| Completed \| Removed \| SubscriptionChanged \| SubscriptionRemoved \| YtdlOptionsReloaded \| ProvidersReloaded \| HealthChanged \| Notice` (everything — it is the wire) | `aulos-queue::aggregator` (§15.1) |
| `hooks` | 256 | `DropNewest` | `Finishing \| Completed` | `aulos-hooks::dispatcher` (§13) |
| `telegram` | 512 | `DropNewest` | `Added \| StatusChanged \| Completed \| Removed \| Notice` | `aulos-telegram` (§12.1) |
| *(future)* `apns` | 256 | `DropNewest` | `Completed \| Notice` | a `Notifier` impl (§12.6) |

The `SubscriptionScheduler` is a **producer only** (`SubscriptionChanged`, `SubscriptionRemoved`);
it consumes no events, so it registers no inbox. The earlier topology sketches implied otherwise.

`DomainEvent::Finishing` (§8.1, §13) is the one event that is **not** on the wire: it is delivered to
`hooks` only, because it exists solely to give a pre-terminal hook its turn before the engine writes
the terminal status. The aggregator's filter therefore deliberately omits it — a `Finishing` event
would otherwise produce a frame for a state transition that has not happened yet.

`aulos_event_dropped_total{subscriber} > 0` is a WARN-level signal in `healthz` (§16.7): it means a
hook or a Telegram notification was silently skipped, which is the only user-visible consequence
the fan-out can have.

### 2.3 Channel and backpressure inventory

| Channel | Type | Cap | Full behaviour |
|---|---|---|---|
| `EngineCmd` | `mpsc` | 1024 | `send().await`; the API handler awaits (bounded by axum concurrency). Never dropped. |
| `ProgressMsg::Progress` | `mpsc` | 8192 | `try_send`, **drop on full**, `aulos_progress_dropped_total` += 1. Losing a percent tick is invisible; blocking a downloader is not. |
| `ProgressMsg::Stage` / `::File` | same channel | — | `send().await`. Never dropped. |
| `DomainEvent` (router inbox) | `mpsc<DomainEvent>` | 4096 | `send().await` from every producer. Owned **only** by the `EventRouter` (§2.2.1). Never dropped. |
| Per-subscriber event inbox | `mpsc<Arc<DomainEvent>>` | per §2.2.1 | `Block` (aggregator) or `DropNewest` + `aulos_event_dropped_total{subscriber}` (hooks, telegram, notifiers). |
| `StoreCmd` | `mpsc` | 1024 | `send().await`. A full store queue means disk trouble and callers *should* slow down. |
| WS bus | `broadcast<Arc<WireFrame>>` | 256 | Never blocks the sender; a lagging reader gets `Lagged(n)` → one fresh `snapshot` → continue (§10.5). |
| Telegram outbound | `mpsc` | 256 | Drop the oldest *progress edit*; terminal messages are never dropped. |
| Per-job stdout/fd3 | OS pipe | 64 KiB | Child blocks — correct backpressure on a chatty child. |
| Per-job stderr | pipe → `StderrRing` | 64 lines / 32 KiB | Oldest evicted. **Must be drained**: a full 64 KiB stderr pipe deadlocks the child forever. This is a failure mode legacy could not have (it had no pipe) and is a mandatory part of `proc::Child`. |

Rule applied throughout: **bounded everywhere; the only unbounded thing in the process is the
SQLite file.** Anything droppable without user-visible loss (progress, notices, hook retries) is
dropped with a counter; anything else applies backpressure to its producer.

---

## 3. Crates

Workspace `resolver = "3"`, edition 2024, `rust-version = "1.95"`.
`[workspace.lints.clippy]` sets `unwrap_used = "deny"`, `expect_used = "warn"`,
`disallowed_methods = "deny"`, plus a curated `pedantic` subset.

| Crate | Depends on (beyond the ubiquitous five) | Modules |
|---|---|---|
| `aulos-core` | ulid, url, time, arc-swap, regex, tokio(sync) | `id`, `status`, `item`, `request`, `selection`, `catalog`, `source`, `event` (incl. `EventRouter`, §2.2.1), `subscription` (`SubscriptionRecord`, `SubscriptionView`, **and the `SubscriptionsHandle` command handle**, §14.1), `telegram` (`ChatConfig`), `health` (`HealthRegistry`, `HealthView`, `ComponentStatus`), `reload` (`ReloadReport`), `ports` (`HookStore`), `config`, `ytdl_options`, `paths`, `error`, `clock`, `progress`, `prefix` |
| `aulos-store` | aulos-core, rusqlite(bundled), rusqlite_migration, ulid, base64, time, tokio | `lib` (`Store` handle), `actor`, `readers`, `schema`, `items`, `subscriptions`, `telegram`, `kv`, `alloc` (hi/lo allocators), `import`, `import::legacy_model` |
| `aulos-provider` | aulos-core, tokio, tokio-util, toml, regex, nix, url | `provider`, `entry`, `sink`, `registry`, `outcome`, `proc`, `manifest` (`plugin.toml` model), `command`, `hookspec`, `humansize`, `fake` (feature `fake`) |
| `aulos-provider-ytdlp` | aulos-core, aulos-provider, tokio, tokio-util, nix, command-fds, url | `lib`, `formats`, `opts`, `runner`, `progress`, `outtmpl`, `errmap`, `python/ytdlp_runner.py` (shipped asset) |
| `aulos-provider-sc` | aulos-core, aulos-provider, tokio, tokio-util, wreq + wreq-util \| reqwest, scraper, regex, strip-ansi-escapes, url, futures-util | `lib`, `http` (`ScHttp` trait), `inertia`, `watch`, `season`, `embed`, `jit`, `nm3u8dl`, `ffmpeg`, `mux`, `progress` |
| `aulos-queue` | aulos-core, aulos-store, aulos-provider, tokio, tokio-util, arc-swap, bytes, smallvec, indexmap, rand, url | `engine`, `cmd`, `slots`, `priority`, `resolve`, `run`, `cancel`, `recovery`, `groups`, `clear`, `aggregator`, `hub`, `publish`, `ring`, `hookstore` (`EngineHookStore`, §13.3) |
| `aulos-api` | aulos-core, aulos-store, aulos-queue, axum, axum-server, tower, tower-http, tokio, tokio-util, arc-swap, bytes, mime_guess, percent-encoding, sha2, url, rustls, rustls-pemfile, metrics, metrics-exporter-prometheus | `lib`, `v2/*`, `ws`, `v1`, `files`, `health`, `metrics`, `error`, `cors`, `trace`, `auth` |
| `aulos-telegram` | aulos-core, aulos-store, aulos-queue, teloxide, governor, indexmap, rand, tokio, tokio-util, url | `bot`, `commands`, `config_ui`, `urls`, `watch`, `render`, `limiter` |
| `aulos-subscriptions` | aulos-core, aulos-store, aulos-provider, aulos-queue, tokio, tokio-util, rand, url | `manager`, `scheduler`, `check`, `detect`, `model`, `public` |
| `aulos-hooks` | aulos-core, aulos-provider, reqwest, quick-xml, time, tokio, tokio-util, url | `dispatcher`, `jellyfin`, `nfo`, `audio_sync`, `ffprobe`, `manifest_hook` (community `[[hook]]`) — reaches item state **only** through `aulos_core::ports::HookStore` (§13), never through `aulos-store` or `aulos-queue` |
| `aulos-server` | everything | `main`, `wiring`, `pot`, `signals`, `bootstrap`, `cli` |
| `aulos-workspace-tests` | aulos-core (dev), toml (dev) | dev-only crate, `publish = false`, **no `src/`** — it exists to own `tests/arch.rs`, the §3 gate. It is the twelfth workspace member and the target of `cargo test -p aulos-workspace-tests arch` (§18.4). |

**The ubiquitous five.** `serde`, `serde_json`, `thiserror`, `tracing` and `async-trait` are
permitted in **every** crate, are omitted from the rows above, and are ignored by the gate. They
carry no architectural information: forbidding `serde_json` in a crate that has to name a
`serde_json::Value` in a public signature would only push the same type through a re-export.
`anyhow` is the inverse — it is allowed **only** in `aulos-server` (BRIEF §18) and the gate enforces
that as rule A5.

Everything else is enumerated, and the rows are the **whole** truth: a crate may use only what it
declares. A public signature counts — Rust has no implicit transitive `use`, so a crate that names
`aulos_core::Config` in a `pub fn` must declare `aulos-core`, which is why every provider crate
lists it. Five rules, all mechanically enforced by `tests/arch.rs` (owned by
`aulos-workspace-tests`), which parses every `Cargo.toml` and fails on a violation:

| # | Rule | Why |
|---|---|---|
| A1 | **No `aulos-provider*` crate may depend on `aulos-store` or `aulos-queue`.** | This is the property that keeps providers replaceable and separately testable. |
| A2 | **No crate other than `aulos-store` may depend on `rusqlite`.** This is what "`aulos-api` never sees SQL" means concretely: `aulos-api` uses only `Store`'s typed reads and cannot name `Store::read`'s `&Connection` parameter without adding the dependency the test forbids. | One storage engine, one place to change it. |
| A3 | **No crate other than `aulos-api` and `aulos-server` may depend on `axum`.** | Keeps the engine, the store and the providers usable from a test harness and from the CLI. |
| A4 | **No crate may depend on `aulos-api`, except `aulos-server`.** | The API is a leaf. |
| A5 | **No crate other than `aulos-server` may depend on `anyhow`**, and every other crate's error types are `thiserror` enums with `code()`/`retryable()`. | BRIEF §18. A library that erases its error type cannot drive the §8.8 retry policy. |

Four consequences worth stating, because each is easy to get wrong and each was wrong in an
earlier draft of this document:

- **Every type that appears in a `DomainEvent` payload lives in `aulos-core`.** `DomainEvent` is
  declared in `aulos-core::event`, so `SubscriptionView`, `HealthView` and `ReloadReport` cannot
  live in `aulos-subscriptions`, `aulos-server` and `aulos-provider` respectively — that would be
  three dependency cycles. They are `aulos-core` types (`subscription`, `health`, `reload`);
  the crates that *produce* them are downstream of `aulos-core` and construct them freely.
  `SubscriptionRecord` and `ChatConfig` are in `aulos-core` for the same reason: `aulos-store`
  persists them (§7.1) and `aulos-store` depends only on `aulos-core`.
- **`HealthRegistry` is an `aulos-core` type.** It is written by `aulos-server` (§16) and read by
  `aulos-api` (`ApiState.health`), and `aulos-server` depends on `aulos-api`. Putting the registry
  in `aulos-server` would be a cycle. It needs nothing but `arc-swap` and `serde`.
- **`aulos-hooks` uses a port, not the store.** A hook must read `entry_json` (§13.2) and update
  `size` (§13.3). Rather than give the hook crate the whole store, `aulos-core::ports` declares a
  three-method `HookStore` trait (§13). It is implemented by **`aulos-queue::EngineHookStore`**, not
  by `Store`: the read (`entry_blob`) is delegated to the store's read pool, but both writes
  (`set_size`, `drop_entry_blob`) become `EngineCmd`s, so the engine's item cache and the
  Aggregator's `last_sent` observe them and the change reaches clients as a `delta` (§13.3 — writing
  SQLite directly behind the engine's back would leave `size` permanently wrong on every connected
  client until a restart). The hook tests run against a `HashMap`-backed fake with no SQLite and no
  engine at all.
- **`aulos-api` does not depend on `aulos-subscriptions`.** `SubscriptionsHandle` is a handle over
  an mpsc of `SubCmd`, so it lives in `aulos-core::subscription` next to `SubscriptionView`, and
  `aulos-subscriptions::Manager` owns the receiving half. That keeps the API a leaf over three
  crates instead of four and removes the one wave-1 → wave-2 edge that would otherwise force
  WP-16 to land before WP-14 (PLAN, dependency graph).

### 3.1 Binary CLI surface

| Command | Purpose |
|---|---|
| `aulos-server serve` | Normal run. Also the **default** when no subcommand is given (`Cli { cmd: Option<Cmd> }`, `None ⇒ Serve`), and the image's `CMD` passes it explicitly as well. |
| `aulos-server check-config` | Parse env + `YTDL_OPTIONS*`, print the effective config table (secrets `«redacted»`), exit 0/1. No port bound. |
| `aulos-server import --state-dir DIR --db PATH [--dry-run] [--force]` | Run the legacy importer standalone. `--dry-run` runs the whole thing against `:memory:` and prints the report, writing nothing. |
| `aulos-server doctor` | Probe `ffmpeg`, `ffprobe`, `N_m3u8DL-RE`, `deno`, `python3`, `yt-dlp`, `bgutil-pot`; print versions; exit non-zero if a **required** tool is missing. |
| `aulos-server print-schema` | Dump the SQLite DDL and the JSON Schema of every v2 wire type. Used to generate the iOS models and to test them in CI. |
| `aulos-server repair-ids [--db PATH] [--dry-run]` | The documented recovery for the boot consistency check of §4.1 (risk R8). Opens the DB read-write, sets `meta.ord_hwm = MAX(items.ord) + 1`, advances `meta.seq_hwm` by 1 000 000 so no client cursor can collide with a re-issued frame sequence, prints both counters before and after, exits 0. `--dry-run` prints and changes nothing. |
| `aulos-server healthcheck` | Loads the config the same way `serve` does (so `URL_PREFIX` normalisation applies), issues `GET http://127.0.0.1:$PORT<prefix>healthz` with a 5 s timeout, exits 0 when the body's `status` is `ok` or `degraded`, 1 otherwise. This is the container `HEALTHCHECK` (§18.1) — a shell interpolating a raw `${URL_PREFIX}` would bypass the normalisation of §17.1 and produce `…:8081metubehealthz`. |

---

## 4. Domain model (`aulos-core`)

### 4.1 Identity and ordering

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(Ulid);              // Display/FromStr = 26-char Crockford base32
pub type GroupId = ItemId;            // a group IS an items row with kind = Group

/// Subscription id. A validated string newtype, NOT a Ulid: imported legacy ids are UUIDv4
/// strings and must stay stable so any script or bookmark that stored one keeps working.
/// New ids are minted as ULIDs, so the *shape* is uniform going forward without a second
/// representation or a `legacy_id` column.  Pattern: ^[A-Za-z0-9_-]{1,64}$
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubId(Box<str>);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);              // frame sequence — the protocol cursor
pub type Ord0 = i64;                  // item creation order — the client sort key

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BootId(Ulid);              // minted once per process start
```

**Two counters, one allocator design.** `ord` and `seq` mean different things and are deliberately
separate (overloading one counter with both meanings is what makes "sort key" and "protocol
cursor" fight each other), but both come from the same durable, **reserve-before-use** hi/lo
allocator:

```rust
pub trait HiLoAllocator: Send + Sync {
    /// Hands out the next value. Blocks (~60 µs) once per `block` values to reserve the next
    /// block in `meta` BEFORE any value from it is handed out.
    fn next(&self) -> i64;
    fn current(&self) -> i64;
}
```

- Reservation is `BEGIN IMMEDIATE; UPDATE meta SET value = value + :block WHERE key = :k; SELECT value; COMMIT;`
  and the in-memory cursor is set **after** the commit returns. A crash therefore skips up to
  `block-1` values and can never re-issue one. This is the fix for both "an unclean shutdown
  re-issues used `ord` values and every subsequent insert dies on the UNIQUE index" and "a client
  resuming with `since=10251` after a restart presents a cursor above the new head".
- Block sizes: `ord` 256, `seq` 1024.
- **Seeding at open, per counter — they are seeded differently because only one of them has a
  column to compare against:**

  | Counter | Seeded with | Boot consistency check |
  |---|---|---|
  | `ord` | `max(meta.ord_hwm, SELECT COALESCE(MAX(ord), -1) + 1 FROM items)` | If `meta.ord_hwm < COALESCE(MAX(items.ord), -1) + 1`, the `meta` row is *behind* the table: a reserved block was consumed and lost, so a subsequent insert would re-issue an `ord` and die on the UNIQUE index. The process **refuses to start**. |
  | `seq` | `meta.seq_hwm` (there is no `seq` column: a frame sequence orders *frames*, and frames are not persisted, §23 decision 24) | If `meta.seq_hwm` is absent, unparseable, or lower than the value the previous process recorded in `meta.seq_hwm_witness` (written on every graceful shutdown), the process **refuses to start**. The client-side guard against a *restored older DB* is `boot_id`, not this check (§15.3) — a new `boot_id` forces a full snapshot, so a cursor above the head can never be answered with a delta. |

  Both refusals print the same instruction: **`aulos-server repair-ids`** (§3.1), which is the
  documented recovery and exists as a real subcommand. This is risk R8.
- `boot_id` is in every `snapshot` and in `healthz`. A client whose `since` came from a different
  `boot_id` is handed a full snapshot, never a delta.

### 4.2 Status (closed, per BRIEF §6)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status { Queued, Resolving, Preparing, Downloading, Postprocessing, Finished, Error, Canceled }

impl Status {
    pub const fn is_terminal(self) -> bool;  // Finished | Error | Canceled
    pub const fn is_active(self) -> bool;    // Resolving | Preparing | Downloading | Postprocessing
    pub const fn is_running(self) -> bool;   // Preparing | Downloading | Postprocessing (holds a slot)
    pub const fn v1(self) -> &'static str;   // §11.5
}
```

Legal transitions; anything else is a bug and trips a `debug_assert!`:

```
Resolving ──► Queued                               (resolve produced one entry, or children)
Queued ──► Resolving                               (a retry of an unresolved item)
Queued ──► Preparing ──► Downloading ──► Postprocessing ──► Finished
                     └───────────────────────────────────► Error
Queued(auto_start=true) ──► Queued(auto_start=false)          (pause: un-schedule)
Preparing | Downloading | Postprocessing ──► Queued(auto_start=false)
                                                   (pause: kill the job, keep the partial file)
Queued(auto_start=false) ──► Queued(auto_start=true)          (start)
any non-terminal ──► Canceled
Error | Canceled ──► Queued                        (explicit retry or auto-retry, attempt += 1)
Finished | Error | Canceled ──► (removed)          (delete · clear · CLEAR_COMPLETED_AFTER)
```

`Queued` carries `auto_start: bool`. `auto_start = false` is the legacy `pending` bucket: one
status, one flag. The client renders "Queued" vs "Paused" from the flag; the v1 shim projects the
two onto the legacy `queue`/`pending` arrays.

No edge is needed for the pre-terminal hook phase of §13: the engine writes
`Downloading → Postprocessing` (already legal), runs those hooks, and only then writes the terminal
status. There is deliberately no `Finished → Postprocessing` edge, and a hook still cannot change a
status — it can only delay the terminal write.

A `Queued` item may also carry a non-null `error`: that is the **pre-download problem** case
(an upcoming livestream, or an entry-level `msg`), which legacy expressed as `status="pending"`
with a populated `error` string. See §8.4 and §11.4 — it is *not* a terminal state, the item is
simply not scheduled until the user presses start or its subscription re-queues it.

### 4.3 Request and selection

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadType { Video, Audio, Captions, Thumbnail }

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec { Auto, H264, H265, Av1, Vp9 }

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleMode { AutoOnly, ManualOnly, PreferManual, PreferAuto }

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub download_type: DownloadType,
    pub codec: Codec,
    pub format: FormatId,      // newtype over Box<str>, validated against the provider catalog
    pub quality: QualityId,    // newtype over Box<str>, likewise
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: Url,
    pub selection: Selection,
    pub folder: Option<RelDir>,                  // validated + containment-checked at add time
    pub custom_name_prefix: Box<str>,            // no "..", no leading / or \
    pub playlist_item_limit: u32,                // 0 = unlimited
    pub auto_start: bool,
    pub split_by_chapters: bool,
    pub chapter_template: Box<str>,
    pub subtitle_language: SubtitleLang,         // ^[A-Za-z0-9][A-Za-z0-9-]{0,34}$
    pub subtitle_mode: SubtitleMode,
    pub ytdl_options_presets: Vec<Box<str>>,
    pub ytdl_options_overrides: serde_json::Map<String, Value>,
    pub provider_hint: Option<ProviderId>,       // forces a provider; None = registry decides
}
```

### 4.4 Attribution — flat, two always-present fields

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub kind: SourceKind,          // "api_v2" | "api_v1" | "telegram" | "subscription" | "restart" | "retry"
    pub r#ref: Option<Box<str>>,   // chat id, subscription id, or request id — always the key, may be null
}
```

This replaces the internally-tagged enums both other candidates used. Two fields, both always
present, no per-variant payload, no hand-written Swift `Decodable`, no nested switch. The
subscription *name* and the Telegram *message id* are not on the wire; they are in the DB row and
in logs, which is where they are actually used.

### 4.5 The persisted item

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind { Item, Group }

pub struct Item {                       // the DB row, minus transient progress
    pub id: ItemId,
    pub kind: Kind,
    pub group_id: Option<GroupId>,
    pub group_index: Option<u32>,
    pub ord: Ord0,
    pub url: Url,
    pub canonical_key: Box<str>,        // §8.5 dedupe key
    pub provider: Option<ProviderId>,
    pub media_id: Option<Box<str>>,     // the provider's own id (the legacy `id`)
    pub title: Box<str>,
    pub status: Status,
    pub auto_start: bool,
    pub msg: Option<Box<str>>,          // human stage text / last provider message
    pub error: Option<WireError>,        // terminal error, code + already-cleaned message
    pub request: DownloadRequest,
    pub entry: Option<EntryBlob>,        // compacted provider entry (§7.5)
    pub filename: Option<RelPath>,       // relative to the item's download root
    pub size: Option<u64>,
    pub chapter_files: Vec<FileRef>,
    pub subtitle_files: Vec<FileRef>,
    pub created_at: UnixMs,
    pub started_at: Option<UnixMs>,
    pub finished_at: Option<UnixMs>,
    pub attempt: u16,
    pub source: SourceRef,
    pub children_total: Option<u32>,     // groups only
    pub clear_after: Option<UnixMs>,
}
```

### 4.6 `ItemView` — the one wire shape

`ItemView` is what REST returns, what the WS `snapshot` contains, and what `added`/`completed`
carry. Groups are `ItemView`s with `kind:"group"`. There is no second record type, no
`[key, info]` pair, no `AnyCodable`.

```rust
#[derive(Clone, PartialEq, Serialize)]
pub struct ItemView {
    pub id: ItemId,
    pub kind: Kind,                       // "item" | "group"
    pub ord: i64,                         // THE sort key. ORDER BY ord ASC, id ASC.
    pub group_id: Option<ItemId>,
    pub group_index: Option<u32>,
    pub url: Arc<str>,
    pub title: Arc<str>,
    pub status: Status,                   // the closed 8-value enum, groups included
    pub auto_start: bool,
    pub provider: Option<Arc<str>>,

    // progress — always numbers, never strings
    pub percent: f64,                     // 0.0..=100.0, NOT optional, 0.0 before any progress
    pub speed: Option<f64>,               // bytes/s
    pub eta: Option<i64>,                 // whole seconds
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>,
    pub fragment_count: Option<u32>,
    pub phase: Option<Arc<str>>,          // "video" | "audio" | "fragment" | "remux" | "audio_sync" | …
    pub phase_percent: Option<f64>,       // postprocessor progress, independent of `percent`

    // text
    pub msg: Option<Arc<str>>,
    pub error: Option<WireError>,         // { code, message, provider, provider_code }

    // outputs
    pub filename: Option<Arc<str>>,
    pub size: Option<u64>,
    pub download_url: Option<Arc<str>>,   // PUBLIC_HOST_URL|_AUDIO_URL + percent-encoded filename.
                                          // Relative to `<p>` with the stock defaults; ABSOLUTE
                                          // when the operator points those at a CDN origin. §4.6.2
    pub chapter_files: Arc<[FileRef]>,
    pub subtitle_files: Arc<[FileRef]>,

    // request echo — immutable for the record's life, therefore NEVER present in a `delta`
    pub selection: SelectionView,         // { download_type, codec, format, quality }
    pub folder: Option<Arc<str>>,
    pub request: RequestView,             // the remaining eight request fields, §4.6.1

    // times, unix millis
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub attempt: u16,
    pub source: SourceRef,

    // groups only — null on items
    pub children_total: Option<u32>,
    pub children_done: Option<u32>,
    pub children_error: Option<u32>,
    pub children_active: Option<u32>,
    pub children_inline: Option<bool>,    // false ⇒ children were not sent; fetch or `watch`
}

#[derive(Clone, PartialEq, Serialize)]
pub struct FileRef { pub filename: Arc<str>, pub size: Option<u64>,
                     pub download_url: Option<Arc<str>>, pub lang: Option<Arc<str>> }

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct WireError { pub code: ErrorCode, pub message: Arc<str>,
                       pub field: Option<Arc<str>>,        // null on item errors; set on 400s
                       pub provider: Option<Arc<str>>, pub provider_code: Option<Arc<str>> }
```

`WireError` carries `field` so that **one** type serves both surfaces: the HTTP error envelope is
`WireError` plus `request_id` (§5), and `Item.error` is `WireError` verbatim. A client
hand-writing the struct from PROTOCOL.md §1.5 decodes both. On an item error `field` is `null`;
`provider`/`provider_code` are the inverse, usually null on an HTTP 400.

#### 4.6.1 `RequestView` — the rest of the request echo

v1 projected every request field back to the client (legacy spec §4), and the row carries them all.
Echoing only `selection` would make v2 a strictly *narrower* payload than v1 and would make
"download this again with the same options" and a request inspector impossible in a v2-only client.
So the remaining eight fields ship as one nested object:

```rust
#[derive(Clone, PartialEq, Serialize)]
pub struct RequestView {
    pub custom_name_prefix: Arc<str>,
    pub playlist_item_limit: u32,
    pub auto_start: bool,                 // the requested value; `ItemView.auto_start` is current
    pub split_by_chapters: bool,
    pub chapter_template: Arc<str>,
    pub subtitle_language: Arc<str>,
    pub subtitle_mode: SubtitleMode,
    pub ytdl_options_presets: Arc<[Arc<str>]>,
    pub ytdl_options_overrides: Arc<serde_json::Map<String, Value>>,
}
```

- `ytdl_options_overrides` values whose **key** matches the secret pattern of §16.5
  (`(?i)(cookie|password|passwd|token|key|secret|proxy)`) are serialised as the string
  `"«redacted»"`. The key set is preserved so a client can still say *"3 overrides"* and show
  which ones. `GET api/v2/debug/options` is the authenticated way to see the real merged dict.
- `selection`, `folder` and `request` are set once at insert and never change, so the diff
  (§15.1) can never emit them in a `delta`. A `debug_assert` in the diff macro enforces it, and
  PROTOCOL.md §5.4 states it as a client-facing guarantee.
- `attempt` is *not* in here: it changes, so it stays a top-level mutable field.

#### 4.6.2 `download_url` — relative or absolute, and why

`download_url = <PUBLIC_HOST_URL or PUBLIC_HOST_AUDIO_URL> + percent_encode(filename)`.

Those two variables are free-form legacy strings (spec §1.1) whose defaults are `download/` and
`audio_download/` — relative — but which an operator is explicitly allowed to point at a CDN or a
separate file host, e.g. `PUBLIC_HOST_URL=https://cdn.example/`. So the field is **sometimes
absolute**, and a client that unconditionally resolves it against `base + <p>` produces a broken
URL for that deployment.

The contractual rule, stated in PROTOCOL.md §2.3 and §12: *if the value parses as an absolute URL
(it has a scheme), open it as-is; otherwise resolve it against your base URL plus `<p>`.* That is
one line in a client and it is correct for both configurations. The same rule applies to
`FileRef.download_url`. The server never rewrites an operator's absolute prefix into a relative
one, because the whole point of the setting is to move file traffic off this process.

**Serialisation rules, enforced by one test per rule:**

| Rule | Enforcement |
|---|---|
| Every field is **always** serialised. `Option::None` becomes JSON `null`; a key is never absent from a full `ItemView`. | No `skip_serializing_if` anywhere in `ItemView`. A `print-schema` test asserts the key set is exactly the struct's field set. Kills iOS pain point #19 (the lazily-created `filename`). |
| `percent` is a JSON number in `[0,100]`, never `null`, never a string. | `f64`, not `Option<f64>`. `Finished` ⇒ `100.0`; `Error`/`Canceled` keep the last value. |
| `eta` is integer seconds or `null`; `speed` is bytes/s `f64` or `null`; every byte count is an integer or `null`. | Newtypes at the provider boundary. `serde_json` with `arbitrary_precision` off. Deletes `decodeFlexibleDoubleIfPresent` and the 3-branch `eta` decoder. |
| `status` is exactly the 8-value closed set, **including on groups**. | One `Status` enum, `#[serde(rename_all="lowercase")]`, no fallthrough. A group's status is a documented roll-up (§8.6), not a separate vocabulary. |
| Sort key is `ord`: monotonic, server-assigned, stable across restarts, documented on every list endpoint. | DB column; `ORDER BY ord ASC, id ASC` everywhere. Kills iOS pain points #6/#10/#11. |
| An item's `id` never changes, from `POST` acknowledgement to deletion — including when it becomes a group. | ULID minted in the API handler before validation completes; playlist promotion **reuses** the id and the `ord` (§8.6). |
| `selection`, `folder` and `request` never appear in a `delta`. | They are written once at insert. The diff macro `debug_assert!`s that these fields are equal between `last_sent` and `new`; a change would be a bug, not a wire event. |
| `error` may be non-null on a **`queued`** item (the pre-download-problem case, §8.4), not only on `error`/`canceled`. | One `WireError` field, no second "warning" field. PROTOCOL.md §2.3 documents the nullability accordingly. |

### 4.7 Progress cell (memory only, never persisted)

```rust
#[derive(Clone, Copy, Default)]
pub struct ProgressCell {
    pub percent: f64, pub speed: Option<f64>, pub eta: Option<i64>,
    pub downloaded_bytes: Option<u64>, pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>, pub fragment_count: Option<u32>,
    pub phase: Option<PhaseTag>, pub phase_percent: Option<f64>,
    pub source_tag: u64,             // hash of stream/filename/tmpfilename; a change resets the clamp
    pub last_frame_at: Instant,      // set on EVERY frame received, even dropped ones
    pub last_applied_at: Instant,    // set only on applied frames
}
```

`last_frame_at` is bumped by the channel receiver **before** the drop decision, so a sustained
drop storm and a genuinely stalled download are distinguishable (this is the two-line fix for the
stall-watchdog hole both other candidates left).

`percent` is computed by `aulos_core::progress::Normalizer`, a line-by-line port of
`_calculate_progress_percent`:

```rust
pub struct Normalizer { prev: Option<f64>, source_tag: u64 }
impl Normalizer {
    pub fn apply(&mut self, m: &RawProgress, status: Status) -> f64;
}
```

Rules, one named unit test each, plus golden vectors ported 1:1 from the Python test suite and a
`proptest` monotonicity invariant:

| Case | Behaviour |
|---|---|
| `status == finished` | `100.0` |
| `total_bytes` exact and > 0 | `downloaded / total * 100` |
| fragments known | `floor = idx/count*100`, `ceil = min((idx+1)/count*100, 99.9)`, result = `estimate.clamp(floor, ceil)`; with no estimate, `floor` |
| no fragments and `total_bytes_estimate <= downloaded_bytes` | estimate ignored (the bogus 1 KiB/1 KiB HLS frame) |
| nothing usable | keep previous |
| `source_tag` changed | reset the monotonic floor (video→audio leg of a merge) |
| always | clamp to `[0.0, 99.9]` while active, never decrease below `prev` |

The `[0,99.9]` clamp is **kept** (Appendix B, K1): existing behaviour, and with a real
`postprocessing` status the client no longer needs 100.0 to mean "the bar is done".

---

## 5. Errors

One shared wire taxonomy in `aulos_core::error::ErrorCode` — a `#[non_exhaustive]` enum with
`snake_case` serde. Every HTTP error and every terminal item error carries a code from this list,
so a client branches on `bot_check` instead of regex-matching prose. This is what deletes the iOS
`AddResultClassifier.cleanMessage` and its `"ERROR: "` prefix stripping (iOS ask #12) — messages
are cleaned server-side, once.

| Code | HTTP | Item-terminal? | Meaning |
|---|---|---|---|
| `bad_request` | 400 | — | malformed body, unparseable field |
| `validation_failed` | 400 | — | field-level validation; `field` set |
| `unsupported_url` | 400 | ✓ | no provider matched and the scheme is unusable |
| `overrides_disabled` | 400 | — | `ALLOW_YTDL_OPTIONS_OVERRIDES=false` |
| `unknown_preset` | 400 | — | preset name not in the catalogue |
| `folder_invalid` | 400 | — | containment violation, missing dir, or `CUSTOM_DIRS=false` |
| `unauthorized` | 401 | — | auth failure — **never a redirect** |
| `not_found` | 404 | — | unknown item / group / subscription id |
| `conflict` | 409 | — | duplicate subscription URL; `AULOS_DEDUPE_MODE=strict` duplicate |
| `payload_too_large` | 413 | — | cookie upload > **1 000 000 bytes** (the legacy decimal cap, §16.6); batch add > `AULOS_MAX_BATCH_URLS` |
| `auth_required` | — | ✓ | provider needs credentials/cookies (login, members-only, private) |
| `bot_check` | — | ✓ | YouTube bot check — the POT-sidecar signal |
| `geo_restricted` | — | ✓ | |
| `unavailable` | — | ✓ | removed, terminated, deleted |
| `not_yet_live` | — | ✓ | `is_upcoming` |
| `no_format` | — | ✓ | requested format not available |
| `network` | — | ✓ | transport, 5xx, timeout — **retryable** |
| `throttled` | — | ✓ | HTTP 429 — retryable after 60 s |
| `postprocessing_failed` | — | ✓ | |
| `disk_full` | — | ✓ | `OSError` errno 28 / `ENOSPC` |
| `tool_missing` | — | ✓ | ffmpeg / N_m3u8DL-RE / python3 absent |
| `provider_degraded` | — | ✓ | the selected provider is in `Degraded` state (§6.4) |
| `timeout` | — | ✓ | resolve/job/stall deadline |
| `canceled` | — | ✓ | user cancel |
| `contract` | — | ✓ | shim/plugin protocol violation |
| `socketio_removed` | 501 | — | `GET <p>socket.io/*`. The only 501 in the taxonomy; it exists so a stale Socket.IO client fails loudly instead of hanging on a handshake (§11.1). |
| `state_unavailable` | 503 | — | SQLite busy/locked; `Retry-After: 1` |
| `internal` | 500 | ✓ | bug; the message is a request id, details only in logs |

Wire envelope, identical for HTTP errors and item errors:

```json
{ "error": { "code": "bot_check",
             "message": "Sign in to confirm you're not a bot",
             "field": null,
             "provider": "ytdlp",
             "provider_code": "ExtractorError",
             "request_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA" } }
```

The envelope is exactly `WireError` (§4.6) plus `request_id`. `Item.error` is exactly `WireError`.
There is **one** Rust struct and **one** Swift struct for both, which is why `field` is a member of
`WireError` and not of the HTTP envelope alone.

`thiserror` per crate; `anyhow` only in `aulos-server`. Every library error type has
`fn code(&self) -> ErrorCode` and `fn retryable(&self) -> bool`; the retry policy (§8.8) reads
exactly those two methods and nothing else.

---

## 6. Provider system (`aulos-provider`)

### 6.1 The trait

```rust
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProviderId(Arc<str>);              // "ytdlp" | "streamingcommunity" | "command:<name>" | "fake"

/// Match score 0..=255. Highest wins; ties break by registration order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Match { No, Weak(u8), Strong(u8), Forced }

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaEntry {
    pub media_id: Box<str>,
    pub title: Box<str>,
    pub url: Url,                       // canonical page url ("webpage_url or url")
    pub kind: EntryKind,                // Video | Playlist { title, entries } | Redirect { url }
    pub pre_error: Option<WireError>,   // e.g. "Live stream is scheduled to start at …"
    pub live: LiveStatus,               // NotLive | IsUpcoming { at } | IsLive | WasLive
    pub state: serde_json::Value,       // provider-native blob; compacted by §7.5 before storage
    pub hints: EntryHints,              // { playlist_index, playlist_count, channel_index, ext, duration, … }
}

pub struct ResolveCtx<'a> {
    pub item_id: ItemId,
    pub request: &'a DownloadRequest,
    pub ytdl_options: Arc<YtdlOptions>,   // already layered: env → file → presets → overrides
    pub paths: &'a Paths,
    pub flat: bool,                       // subscription scan mode
    pub playlist_end: Option<u32>,
    pub cancel: CancellationToken,
    pub deadline: Instant,
}

pub struct DownloadCtx<'a> {
    pub item_id: ItemId,
    pub entry: &'a MediaEntry,
    pub request: &'a DownloadRequest,
    pub ytdl_options: Arc<YtdlOptions>,
    pub out_dir: PathBuf,                 // resolved and containment-checked
    pub tmp_dir: PathBuf,
    pub outtmpl: OutTmpl,                 // default + chapter, playlist/channel fields pre-resolved
    pub cancel: CancellationToken,
}

pub struct Outcome {
    pub filename: Option<RelPath>,
    pub size: Option<u64>,
    pub chapter_files: Vec<FileRef>,
    pub subtitle_files: Vec<FileRef>,
    pub entry_final: Option<serde_json::Value>,   // for the NFO hook
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn matches(&self, url: &Url) -> Match;
    /// The client-facing format/quality catalogue for this provider (§6.6).
    fn catalog(&self) -> Arc<FormatCatalog>;
    /// Metadata only. Must respect `ctx.cancel` and `ctx.deadline`.
    async fn resolve(&self, url: &Url, ctx: ResolveCtx<'_>) -> Result<Vec<MediaEntry>, ProviderError>;
    /// Performs the download. Must emit progress through `sink` and honour `ctx.cancel`.
    async fn download(&self, ctx: DownloadCtx<'_>, sink: ProgressSink) -> Result<Outcome, ProviderError>;
    /// Optional provider-specific concurrency cap, acquired INSTEAD of the global slot.
    fn own_slots(&self) -> Option<usize> { None }
    /// Readiness for healthz and for Degraded gating.
    async fn probe(&self) -> ProviderHealth { ProviderHealth::Ok }
}
```

`ProviderError` is a `thiserror` enum whose variants map 1:1 onto the §5 item-terminal codes:
`Unsupported`, `AuthRequired`, `BotCheck`, `GeoRestricted`, `Unavailable`, `NotYetLive`,
`NoFormat`, `Network`, `Throttled`, `Postprocessing`, `Disk`, `ToolMissing(&'static str)`,
`Timeout`, `Canceled`, `Contract(String)`, `Degraded(String)`, `Other(String)`. It exposes
`code()` and `retryable()`; nothing else in the system inspects the variant.

### 6.2 `ProgressSink`

```rust
pub struct ProgressSink { tx: mpsc::Sender<ProgressMsg>, id: ItemId }

impl ProgressSink {
    /// LOSSY, non-blocking, latest-wins. Safe to call 1000×/s.
    pub fn progress(&self, p: RawProgress);
    /// LOSSLESS, awaited. Never dropped.
    pub async fn stage(&self, s: Stage, msg: Option<Box<str>>);
    pub async fn file(&self, slot: FileSlot, f: FileRef);
    /// Structured passthrough to `tracing`, span-attached to the item. Never reaches clients.
    pub fn log(&self, level: Level, msg: &str);
}
pub enum Stage { Preparing, Downloading, Postprocessing }
pub enum FileSlot { Chapter, Subtitle }
```

### 6.3 Registry and selection

```rust
pub struct Registry { providers: Vec<Arc<dyn Provider>>, by_id: HashMap<ProviderId, usize>,
                      state: HashMap<ProviderId, ProviderState> }

pub enum ProviderState { Ready, Degraded { reason: Box<str>, since: UnixMs, failures: u32 } }

impl Registry {
    pub fn pick(&self, url: &Url, hint: Option<&ProviderId>) -> Selected;
    pub fn by_id(&self, id: &ProviderId) -> Option<&Arc<dyn Provider>>;
    pub fn catalog_for(&self, url: &Url) -> (ProviderId, Arc<FormatCatalog>, MatchReason);
    pub fn merged_catalog(&self) -> Arc<MergedCatalog>;
    pub fn reload_commands(&mut self, dir: &Path) -> ReloadReport;
}
pub struct Selected { pub id: ProviderId, pub score: u8, pub reason: MatchReason,
                      pub runner_up: Option<(ProviderId, u8)>, pub state: ProviderState }
```

Registration order and scores:

| Provider | Score | Notes |
|---|---|---|
| `command:<name>` | `Strong(priority)`, default `100`; `150` on `host_regex`; `250` when `path_regex` also matches | a plugin can deliberately outrank SC with `priority = 201` |
| `streamingcommunity` | `Strong(200)` on a dispatchable path, `Match::No` otherwise | hostname lower-cased **contains** `streamingcommunity` (or an `AULOS_SC_EXTRA_HOSTS` entry) **and** the path contains `/watch/`, `/titles/` or `/season-`. Legacy detected by host but *dispatched* by path (`app/extractors/streamingcommunity.py:387-397`), returning `None` for anything else so the add fell through to yt-dlp; a host-only match would terminate those URLs with `unsupported_url` instead. §10.2, Appendix B C46 |
| `ytdlp` | `Weak(1)` | registered **last**; the catch-all fallback |

`provider_hint` in the request produces `Match::Forced`, which beats every score.

### 6.4 `Degraded` providers — visible, not absent

A provider that fails to construct (a malformed `plugin.toml`, a missing binary, an `ScHttp`
feature compiled out and explicitly requested) is registered as
`ProviderState::Degraded { reason }` rather than dropped. Consequences:

- It appears in `GET api/v2/providers` and in `healthz` with the reason string.
- It **still matches** its URLs, and an item routed to it fails immediately with
  `provider_degraded` and the reason. It does **not** silently fall through to `ytdlp`, which
  would download a login page and call it a success.
- Repeated runtime failures arm a circuit breaker: 5 failures in 10 minutes ⇒ `Degraded` for a
  10-minute cool-down, then one probationary attempt.

**The one documented fall-through, and why it is not the same thing.** A `Degraded` provider never
falls through, per the paragraph above. But a *healthy* provider that resolves a URL it turns out
not to understand is a different case, and legacy handled it by retrying through yt-dlp
(`__extract_info`: `if entry: return entry`, otherwise fall through — legacy spec §9.1). So:

> When `provider.resolve()` returns **`ProviderError::Unsupported`** and `Selected.runner_up` is
> `Some((id, _))` and that provider's state is `Ready`, the engine retries resolution **once** with
> the runner-up (§8.4). Exactly one retry, only for `Unsupported`, only from a `Ready` provider to a
> `Ready` provider, and it is recorded on the item as
> `msg = "Retrying with <runner-up>"`, counted in `aulos_resolve_fallthrough_total{from,to}` and
> logged at INFO.

Every other `ProviderError` — `Degraded`, `AuthRequired`, `BotCheck`, `Network`, `GeoRestricted`,
`Unavailable`, `NoFormat`, … — terminates the item, because those all mean "the right provider tried
and failed", and downloading a login page through the fallback and calling it a success is the
failure mode §6.4 exists to prevent. `AULOS_RESOLVE_FALLTHROUGH=false` disables the retry
entirely.

### 6.5 `command` plugins — the community provider format

Discovery: at boot, on `SIGHUP`, on `POST api/v2/plugins/reload`, and on a debounced (1 s)
`notify` event over `AULOS_PLUGINS_DIR` (default `${PLUGINS_DIR:-/config/plugins}`).
Layout:

```
$AULOS_PLUGINS_DIR/
  bandcamp/
    plugin.toml        required
    resolve.py         any executable or interpreted file
    download.sh
    README.md          optional
```

Each directory containing a `plugin.toml` becomes provider id `command:<dirname>`; the dirname
must match `^[a-z0-9][a-z0-9_-]{0,31}$`. A manifest failure yields `Degraded(reason)`, never a
startup failure and never silence. `ReloadReport { added, updated, removed, failed }` is published
as a `providers` WS frame so a client's catalogue updates live. Hot reload never touches a running
job: a job holds `Arc<CommandProvider>` for its lifetime and the registry swaps the `Arc` for new
jobs. `AULOS_PLUGINS_ENABLED=false` disables discovery entirely.

#### 6.5.1 Complete `plugin.toml` schema

A manifest may declare a provider (`[match]` + `[download]`), one or more community hooks
(`[[hook]]`, §13.5), or both. A hook-only manifest is valid and omits `[match]`/`[download]`.

| Key | Type | Req | Default | Meaning |
|---|---|---|---|---|
| `manifest_version` | int | ✓ | — | must be `1`; otherwise `Degraded("unsupported manifest_version")` |
| `name` | string | ✓ | — | display name |
| `version` | string | ✓ | — | shown to clients |
| `description` / `homepage` | string | | `""` | shown in the catalogue |
| `authors` | [string] | | `[]` | |
| **`[match]`** | | provider | | at least one of `hosts` / `host_regex` |
| `match.hosts` | [string] | | `[]` | host suffix match, case-insensitive; score 100 |
| `match.host_regex` | string | | — | anchored regex over the normalised host; score 150 |
| `match.path_regex` | string | | — | if set **and** it matches, the score is promoted to 250 |
| `match.exclude_path_regex` | string | | — | veto ⇒ `Match::No` |
| `match.schemes` | [string] | | `["http","https"]` | |
| `match.priority` | int 0..=255 | | derived | overrides the derived score |
| **`[capabilities]`** | | | | |
| `capabilities.resolve` | bool | | `false` | `false` ⇒ a synthetic single `Video` entry from the URL |
| `capabilities.playlists` | bool | | `false` | |
| `capabilities.streaming_resolve` | bool | | `true` when `resolve` | resolve stdout is read line-by-line and children stream out |
| `capabilities.subtitles` / `chapters` / `thumbnails` / `nfo_capable` | bool | | `false` | catalogue hints |
| `capabilities.cancel` | enum | | `"process_group"` | `process_group` \| `cooperative` \| `none` |
| **`[limits]`** | | | | |
| `limits.max_concurrent` | int 1..=32 | | `1` | per-provider semaphore |
| `limits.uses_global_slot` | bool | | `true` | `false` ⇒ `own_slots()` only, like SC |
| `limits.max_concurrent_resolves` | int | | `1` | |
| `limits.min_request_interval_ms` | int | | `0` | politeness gate between spawns |
| `limits.resolve_timeout_secs` | int | | `AULOS_PLUGIN_TIMEOUT_RESOLVE` (60) | |
| `limits.download_stall_secs` | int | | `600` | no progress ⇒ kill |
| `limits.download_hard_timeout_secs` | int | | `0` (off) | |
| `limits.max_output_bytes` | int | | `67108864` | cumulative stdout+stderr budget; over ⇒ kill + `contract` |
| `limits.memory_bytes` | int | | `0` (off) | `RLIMIT_AS` |
| `limits.cpu_secs` | int | | `0` (off) | `RLIMIT_CPU` |
| `limits.nofile` | int | | `1024` | `RLIMIT_NOFILE` |
| **`[resolve]`** | | iff `capabilities.resolve` | | |
| `resolve.command` | [string] | ✓ | — | argv; element 0 resolved against the plugin dir, then `PATH` |
| `resolve.format` | enum | | `"json_lines"` | `json_lines` \| `json` |
| `resolve.stdin` | enum | | `"none"` | `none` \| `json` (the whole request as one JSON line) |
| `resolve.cwd` | string | | plugin dir | |
| **`[download]`** | | provider | | |
| `download.command` | [string] | ✓ | — | argv template |
| `download.cwd` | string | | plugin dir | |
| `download.stdin` | enum | | `"none"` | `none` \| `json` |
| `download.expect_output` | enum | | `"path_template"` | `path_template` \| `result_frame` \| `newest_in_dir` |
| `download.output_ext` | string | | `"mp4"` | used by `path_template` and `{out_path}` |
| `download.overwrite` | bool | | `true` | |
| **`[progress]`** | | | `{kind="none"}` | |
| `progress.kind` | enum | ✓ | — | `json_lines` \| `regex` \| `none` |
| `progress.source` | enum | | `"stdout"` | `stdout` \| `stderr` \| `both` |
| `progress.strip_ansi` | bool | | `true` | |
| `progress.cr_as_newline` | bool | | `true` | Spectre.Console / ffmpeg repaints |
| `progress.last_match_wins` | bool | | `true` | per read chunk |
| `progress.min_interval_ms` | int | | `250` | source-side rate cap |
| `progress.patterns` | [string] | ✓ if `regex` | — | named groups from `{percent, downloaded, total, speed, eta, status, fragment_index, fragment_count, msg}` |
| `progress.units` | table | | `{}` | e.g. `speed = "auto"`, `eta = "hms"`; `auto` parses `KB/KiB/MB/MiB/GB/GiB`, **1024-based** (matches N_m3u8DL-RE) |
| `progress.status_map` | table | | `{}` | captured `status` text → Aulos `Stage`/terminal |
| **`[env]`** | | | | |
| `env.pass` | [string] | | `[]` | env var **names** inherited from the server process |
| `env.set` | table | | `{}` | literal env; values accept the same templates as argv and `${VAR}` |
| **`[headers]`** | | | `{}` | name → template; exposed as `AULOS_HEADER_<NAME>` and via `{headers_curl}` / `{headers_crlf}` |
| **`[catalog]`** | | | derived | §6.6; absent ⇒ a single `video`/`mp4`/`best` type with an honest notice |
| **`[[hook]]`** | array of tables | | `[]` | §13.5 |

**Templates** available in `resolve.command`, `download.command`, `env.set`, `headers`:

| Token | Value |
|---|---|
| `{url}`, `{url_host}`, `{url_path}`, `{url_query}` | the item URL and its parsed parts |
| `{media_id}`, `{title}` | from the resolved entry |
| `{out_dir}`, `{tmp_dir}` | absolute, already created |
| `{out_name}` | sanitised basename without extension (prefix + template-resolved) |
| `{out_path}` | `{out_dir}/{out_name}.{output_ext}` |
| `{output_ext}` | from the manifest |
| `{download_type}`, `{format}`, `{quality}`, `{codec}` | the selection |
| `{subtitle_language}`, `{subtitle_mode}` | the selection |
| `{state}` | the entry's provider state as compact JSON |
| `{state.<key>}` | one field of it, JSON-scalar-stringified (only when `capabilities.resolve`) |
| `{playlist_index}`, `{playlist_count}`, `{playlist_title}` | when the item is in a group; empty otherwise |
| `{cookies_file}` | `STATE_DIR/cookies.txt` if present, else `""` |
| `{headers_curl}` / `{headers_crlf}` | `-H "K: V"` argv pairs / a `K: V\r\n` blob (ffmpeg style) |
| `{plugin_dir}` | the plugin's own directory |

Substitution is **argv-level, never shell-level**: each argv element is templated and passed
verbatim to `execvp`. There is no implicit `sh -c`; a plugin that wants a shell writes
`command = ["/bin/sh","-c","…"]` and owns the consequences. An **unknown token is a load-time
validation error**, not a silent empty string — the single most common plugin-author complaint,
and free to fix.

#### 6.5.2 Load-time validation

| Check | Failure |
|---|---|
| `manifest_version == 1`; `name`/`version` non-empty; dirname pattern | `Degraded` |
| at least one of `match.hosts` / `match.host_regex` (when a provider is declared) | `Degraded` |
| all regexes compile; `host_regex` auto-anchored with a WARN if it is not | `Degraded` |
| `download.command` non-empty and argv[0] resolves to an existing executable | `Degraded` |
| every template token known; `{state.*}` only when `capabilities.resolve` | `Degraded` |
| `progress.patterns` compile, use only known group names, and have ≥ 1 group | `Degraded` |
| `catalog` ids match `^[a-z0-9_]+$` and are unique | `Degraded` |
| plugin dir not world-writable; no file setuid/setgid | `Degraded` — refuse to execute |
| `[[hook]]` `on` values in the closed set; `http.url` parses; `debounce_ms ≤ 3600000` | `Degraded` |
| `limits.*` outside hard caps (`max_concurrent ≤ 32`, timeouts ≤ 24 h) | clamp + WARN |

#### 6.5.3 Execution and isolation

Spawn policy for both `resolve` and `download`: cleared environment plus `env.pass`/`env.set`, cwd
from the manifest, no inherited fds beyond stdio, own process group (`process_group(0)`),
`nice(5)`, `RLIMIT_AS`/`RLIMIT_FSIZE`/`RLIMIT_CPU`/`RLIMIT_NOFILE` from `[limits]`, and `out_dir`
/ `tmp_dir` as the only paths handed over.

**Plugins are not a security boundary.** A plugin runs as the server user and can do anything that
user can. This is stated in bold in the plugin guide, and `GET api/v2/providers` exposes every
plugin's full argv so an operator can audit what is installed. The plugin dir is
operator-controlled by definition.

`resolve` stdout line shapes (`json_lines`):

```json
{"t":"group","media_id":"bc:album:914","title":"Album — Deluxe","kind":"playlist","expected":8}
{"t":"entry","media_id":"bc:track:914-1","url":"https://bandcamp.com/track/1","title":"Track 1",
 "duration":183.0,"state":{"stream_id":"a91f"}}
{"t":"note","message":"2 tracks are region-locked and were skipped"}
{"t":"error","code":"unavailable","message":"album 914 not found","retryable":false}
```

A bare object without `t` is accepted as `t:"entry"` (author ergonomics). `media_id` is optional
and defaults to `sha256(url)[..16]`. Unknown fields are ignored; an unknown `t` produces one WARN
and is skipped, so the format can grow.

Download success criteria:

| `expect_output` | Success |
|---|---|
| `path_template` | exit 0 **and** `{out_path}` exists and is non-empty |
| `result_frame` | exit 0 **and** a `{"t":"result","path":"…","size":…,"artifacts":[…]}` line was printed |
| `newest_in_dir` | exit 0 **and** ≥ 1 file in `{out_dir}` newer than job start; newest wins |

A non-zero exit becomes `ProviderError::Other` carrying the last 2 KiB of stderr, ANSI-stripped,
surfaced to the user in `error.message`. Plugin authors need that.

#### 6.5.4 Example: a complete provider plugin

`/config/plugins/bandcamp/plugin.toml`:

```toml
manifest_version = 1
name        = "Bandcamp"
version     = "0.3.1"
description = "Bandcamp albums and tracks"
authors     = ["someone <someone@example.org>"]

[match]
hosts      = ["bandcamp.com"]
host_regex = '^([a-z0-9-]+\.)?bandcamp\.com$'
path_regex = '^/(album|track)/'
schemes    = ["https"]

[capabilities]
resolve           = true
playlists         = true
streaming_resolve = true
cancel            = "process_group"

[limits]
max_concurrent          = 2
uses_global_slot        = true
min_request_interval_ms = 400
resolve_timeout_secs    = 45
download_stall_secs     = 300
max_output_bytes        = 33554432
memory_bytes            = 1073741824

[resolve]
command = ["python3", "resolve.py", "{url}"]
format  = "json_lines"

[download]
command = ["python3", "download.py",
           "--url", "{url}", "--state", "{state}", "--quality", "{quality}",
           "--out", "{out_path}", "--tmp", "{tmp_dir}"]
expect_output = "result_frame"
output_ext    = "flac"

[progress]
kind            = "regex"
source          = "both"
strip_ansi      = true
cr_as_newline   = true
last_match_wins = true
min_interval_ms = 250
patterns = [
  '(?P<percent>[\d.]+)%\s+(?P<downloaded>[\d.]+\s*[KMG]i?B)\s*/\s*(?P<total>[\d.]+\s*[KMG]i?B)',
  '(?P<speed>[\d.]+\s*[KMG]i?B)/s',
  'ETA\s+(?P<eta>\d{1,2}:\d{2}(:\d{2})?)',
  'stage=(?P<status>fetch|mux|done)',
]
units      = { downloaded = "auto", total = "auto", speed = "auto", eta = "hms" }
status_map = { fetch = "downloading", mux = "postprocessing", done = "finished" }

[env]
pass = ["HTTPS_PROXY", "NO_PROXY"]
set  = { PYTHONUNBUFFERED = "1", BC_TOKEN = "${BC_TOKEN}" }

[headers]
Referer = "https://bandcamp.com/"

[[catalog.download_types]]
id    = "audio"
label = "Audio"
  [[catalog.download_types.formats]]
  id             = "flac"
  label          = "FLAC (source)"
  default_quality = "best"
  qualities      = [{ id = "best", label = "Source" }]
  [[catalog.download_types.formats]]
  id             = "mp3"
  label          = "MP3 (transcode)"
  notice         = "Transcoded with ffmpeg; adds ~5 s per track."
  default_quality = "320"
  qualities      = [{ id = "320", label = "320 kbps" }, { id = "192", label = "192 kbps" }]
```

`plugins/examples/bandcamp/` in the repo ships exactly this manifest plus a 40-line `resolve.py`
and `download.py`, and `tests/plugin_example.rs` runs it end-to-end against `wiremock`. That test
doubles as the plugin author's template.

### 6.6 The format / quality catalog

The legacy `get_available_formats()` list was hard-coded in `main.py` and — contrary to what the
iOS client assumes — was **never emitted to clients**; it was only passed to the Telegram bot.
(Verified: `app/main.py` emits `added/updated/completed/canceled/cleared/all/configuration/
custom_dirs/ytdl_options_changed/subscriptions_all/subscription_*` and nothing else. The iOS
`formats` handler has always been dead code.) So there is no compatibility constraint here, only
an opportunity.

```rust
pub struct FormatCatalog {
    pub provider: ProviderId,
    pub version: u32,                     // bumped on any change; part of the ETag
    pub naming: NamingPolicy,             // Template | Provider  (SC ignores OUTPUT_TEMPLATE)
    pub download_types: Vec<DownloadTypeSpec>,
}
pub struct DownloadTypeSpec { pub id: Box<str>, pub label: Box<str>,
                              pub formats: Vec<FormatSpec>, pub default_format: Box<str>,
                              pub options: Vec<OptionSpec> }
pub struct FormatSpec { pub id: Box<str>, pub label: Box<str>,
                        pub qualities: Vec<QualitySpec>, pub default_quality: Box<str>,
                        pub codecs: Vec<CodecSpec>,       // empty ⇒ codec not applicable
                        pub notice: Option<Box<str>>, pub flags: FormatFlags }
pub struct FormatFlags { pub advisory: bool, pub requires_ffmpeg: bool,
                         pub lossy_remux: bool, pub slow: bool }
pub struct QualitySpec { pub id: Box<str>, pub label: Box<str>, pub notice: Option<Box<str>> }
pub struct OptionSpec { pub id: Box<str>, pub label: Box<str>, pub kind: OptionKind,
                        pub default: Value, pub choices: Vec<Choice>, pub help: Option<Box<str>> }
pub enum OptionKind { Bool, Int { min: i64, max: i64 }, Enum, Text { pattern: Option<Box<str>> }, Path }
pub struct Choice { pub id: Box<str>, pub label: Box<str> }
pub enum NamingPolicy { Template, Provider }   // serialises as "template" | "provider"

impl FormatCatalog {
    /// The legacy Telegram keyboard list (spec §8, `main.py:390 get_available_formats()`),
    /// derived from this catalog so there is exactly one catalogue in the codebase. §12.2.
    pub fn bot_formats(&self) -> Vec<BotFormat>;
}
pub struct BotFormat { pub id: Box<str>, pub qualities: Vec<Box<str>> }
```

Every one of these is a **wire type**: PROTOCOL.md §4.6 gives the JSON for `OptionSpec`,
`OptionKind`, `Choice`, `FormatFlags`, `QualitySpec` and `naming`, and `print-schema` emits their
JSON Schema. `options` is the mechanism that satisfies iOS ask 16 (richer `/add` options advertised
server-side); shipping the struct without a documented wire shape would leave that ask
unimplementable, so the two documents must be read together.

The `ytdlp` catalog is **exactly** the legacy matrix (Appendix A §6), now with labels, flags and
honest notices:

| download_type | format | qualities |
|---|---|---|
| `video` | `any` | best, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| `video` | `mp4` | best, **best_remux**, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| `video` | `ios` | best, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| `audio` | `m4a` | best, 192, 128 |
| `audio` | `mp3` | best, 320, 192, 128 |
| `audio` | `opus` / `wav` / `flac` | best |
| `captions` | `srt`, `txt`, `vtt`, `ttml`, `sbv`, `scc`, `dfxp` | best |
| `thumbnail` | `jpg` | best |

`best_remux` carries `flags.slow = true` and
`notice: "Re-encodes audio after download (slower; fixes SponsorBlock drift)"`.
`worst` carries the honest `notice: "This selector currently resolves to the best available
stream"` — the legacy quirk is **kept** (Appendix B, K1) but no longer lies to the user.

**`video`/`ios` carries the full height list, not just `best`.** Verified against
`app/main.py:610-620`: for `download_type=video` the allowed quality set is
`{best, worst, 2160, 1440, 1080, 720, 480, 360, 240}` for **every** video format including `ios`,
and `dl_formats.get_format` composes `vres = [height<={quality}]` into the ios selector chain
(spec §6.1) — so `{video, ios, 1080}` is legal in legacy *and* produces a real height-limited
selector. Giving `ios` a single `best` quality would reject a request the legacy server accepted,
and worse, §11.2 step 3 validates the v1 shim against the legacy matrix **first**: the request
would pass legacy validation and then fail catalog validation, producing an error legacy never
produced. `ios` therefore gets the same nine qualities as `any`, with the same `worst` notice.

**The Telegram keyboard's format list is a documented projection of this catalog**, not the catalog
itself — see §12.2 and `FormatCatalog::bot_formats()`.

**Validation is catalog-driven.** `DownloadRequest` validation is a lookup in the selected
provider's catalog, not a hard-coded match arm. Adding a format to `ytdlp` is one catalog edit; a
plugin gets validation, a picker and a Telegram keyboard for free. The v1 shim additionally keeps
the legacy hard-coded per-type lists as a pre-check so a legacy client's 400 strings stay
byte-identical.

Two endpoints serve it (§10.2): `GET api/v2/capabilities` returns the flat, legacy-shaped
`formats` array the shipped iOS `ServerFormat`/`ServerQuality` models already decode, **and**
`GET api/v2/catalog?url=<encoded>` returns the catalog of the provider that `Registry::pick` would
actually choose, with `{provider, match:{score, reason}, runner_up}`. Paste a StreamingCommunity
link and the quality picker honestly collapses to "Source" with an explanation; paste a YouTube
link and the full matrix appears — with no client release. Both carry an `ETag`.

---

## 7. Storage (`aulos-store`)

### 7.1 Handle and actor

```rust
pub struct Store { w: mpsc::Sender<WriteJob>, r: Arc<ReadPool>,
                   ord: Arc<dyn HiLoAllocator>, seq: Arc<dyn HiLoAllocator>,
                   metrics: Arc<StoreMetrics> }

/// Three-state patch for a nullable column. There is exactly one convention in the whole
/// `WriteOp` enum and this is it: `Keep` leaves the column alone, `Clear` writes SQL NULL,
/// `Set` writes the value. `Option<T>` is never used to mean "unchanged" on a nullable column,
/// because that overload is how a retry ends up leaving a stale `error` on a `queued` row.
pub enum FieldUpdate<T> { Keep, Clear, Set(T) }

pub enum WriteOp {
    InsertItems { items: Vec<Item> },                 // one txn for the whole batch
    SetStatus { id: ItemId, status: Status,
                msg: FieldUpdate<Box<str>>,
                error: FieldUpdate<WireError>,
                auto_start: Option<bool>,             // None = leave the column unchanged
                at: UnixMs },                         // timestamp rules below
    /// Pause / Start. The **only** thing that changes is `auto_start` (§8.7): the status stays
    /// `queued`, so this is not a `SetStatus`.
    SetAutoStart { id: ItemId, auto_start: bool, at: UnixMs },
    /// Re-attribution: boot recovery writes `kind:"restart"` (§8.9), a retry writes
    /// `kind:"retry"` (§4.4). `source` is on the wire, so it needs a write path of its own.
    SetSource { id: ItemId, source: SourceRef },
    SetResolved { id: ItemId, provider: ProviderId, media_id: Option<Box<str>>,
                  title: Box<str>, entry: Option<EntryBlob>, canonical_key: Box<str> },
    PromoteToGroup { id: ItemId, children_total: u32, title: Box<str> },
    SetOutput { id: ItemId, filename: Option<RelPath>, size: Option<u64> },
    /// Size only, leaving `filename` alone — what a hook that rewrites the produced file needs
    /// (§13.3). Reached only through `EngineHookStore`, never from `aulos-hooks` directly.
    SetSize { id: ItemId, size: u64 },
    PushFile { id: ItemId, slot: FileSlot, file: FileRef },
    DropEntryBlob { id: ItemId },
    BumpAttempt { id: ItemId },
    SetClearAfter { id: ItemId, at: Option<UnixMs> },
    DeleteItems(Vec<ItemId>),
    UpsertSubscription(Box<SubscriptionRecord>),
    MarkSeen { sub: SubId, ids: Vec<Box<str>>, at: UnixMs },
    PruneSeen { sub: SubId, keep: u32 },
    DeleteSubscriptions(Vec<SubId>),
    UpsertTelegramChat { chat_id: i64, config: ChatConfig },
    SetKv { key: Box<str>, value: Option<Value> },
}

pub enum Durability { Batched, Sync }

impl Store {
    pub async fn write(&self, ops: Vec<WriteOp>, d: Durability) -> Result<(), StoreError>;
    pub async fn read<T: Send + 'static>(
        &self, f: impl FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static
    ) -> Result<T, StoreError>;

    // typed reads
    pub async fn items(&self, f: ItemFilter) -> Result<Page<Item>, StoreError>;
    pub async fn item(&self, id: ItemId) -> Result<Option<Item>, StoreError>;
    pub async fn boot_state(&self) -> Result<BootState, StoreError>;   // §8.9
    pub async fn resolve_v1_token(&self, tok: &str) -> Result<Vec<ItemId>, StoreError>;  // §11.3
    pub async fn subscriptions(&self) -> Result<Vec<SubscriptionRecord>, StoreError>;
    pub async fn seen(&self, sub: &SubId) -> Result<HashSet<Box<str>>, StoreError>;
    pub async fn telegram_chats(&self) -> Result<HashMap<i64, ChatConfig>, StoreError>;
    pub async fn due_clears(&self, now: UnixMs) -> Result<Vec<ItemId>, StoreError>;

    /// The v1 shim's `done[]` source (§11.4). `ORDER BY ord ASC, id ASC`, terminal statuses only,
    /// hard-capped by `AULOS_V1_HISTORY_MAX` (0 = unlimited). Served from the read pool.
    pub async fn v1_done(&self, limit: Option<u32>) -> Result<Vec<Item>, StoreError>;

    pub fn next_ord(&self) -> Ord0;
    pub fn next_seq(&self) -> Seq;

    /// The durable allocators themselves, so `EventHub::new` (§15.3) can be handed the `seq`
    /// counter as a trait object. `ord`/`seq` stay private fields; these are the only accessors.
    pub fn ord_allocator(&self) -> Arc<dyn HiLoAllocator>;
    pub fn seq_allocator(&self) -> Arc<dyn HiLoAllocator>;
}

/// `aulos-hooks` reaches item state through this port and nothing else (§3 rule A1 rationale, §13).
/// Declared in `aulos_core::ports`. **Implemented by `aulos-queue::EngineHookStore`, not by
/// `Store`**: `entry_blob` is delegated to this crate's read pool, but the two writes become
/// `EngineCmd::HookWrite`s so the engine's item cache and the Aggregator's `last_sent` see them and
/// the change reaches clients as a `delta` (§13.3). A hook writing SQLite directly would leave
/// `size` wrong on every connected client until a restart, and `stress_consistency` (§20) could not
/// catch it because it compares the frame stream against the same stale in-memory snapshot.
#[async_trait]
pub trait HookStore: Send + Sync {
    async fn entry_blob(&self, id: ItemId) -> Result<Option<EntryBlob>, PortError>;
    async fn drop_entry_blob(&self, id: ItemId) -> Result<(), PortError>;
    async fn set_size(&self, id: ItemId, size: u64) -> Result<(), PortError>;
}
```

**`SetStatus` timestamp rules.** The single `at` field is written to different columns depending on
the status, and the table is normative because two writers guessing differently is how `started_at`
ends up null on a finished item:

| Condition | Columns written |
|---|---|
| always | `updated_at = at` |
| `status == Preparing` **and** `started_at IS NULL` | `started_at = at` |
| `status.is_terminal()` | `finished_at = at` |
| a terminal → non-terminal transition (retry, pause of a terminal — impossible — boot recovery) | `finished_at = NULL`; `started_at` is **kept**, so "when did this first start" survives a retry |
| any other status | neither `started_at` nor `finished_at` is touched |

Consequently `Retry` (§8.8) is exactly
`SetStatus { status: Queued, msg: Clear, error: Clear, auto_start: Some(true), at: now }` +
`BumpAttempt` + `SetSource { kind: "retry" }`, and `Pause` is a single `SetAutoStart` when the item
was `queued`, or `SetStatus { status: Queued, auto_start: Some(false), … }` when a running job had
to be killed first (§8.7).

- Writer: **one** dedicated OS thread (`std::thread::spawn`, not a tokio worker) holding a
  `rusqlite::Connection`. It drains up to 256 jobs per loop iteration into **one** transaction,
  extending the batch until `AULOS_DB_FLUSH_MS` (200) expires or 256 is reached. A `Sync` job
  short-circuits the extension and commits immediately. This is what turns "500 playlist inserts"
  into ~5 transactions instead of legacy's 500 whole-file JSON rewrites with 1000 `fsync`s.
- Readers: `ReadPool` = `Semaphore(AULOS_DB_READERS, default 4)` + that many threads, each with a
  `SQLITE_OPEN_READ_ONLY` connection. Reads never block writes (WAL).
- Pragmas: `journal_mode=WAL`, `synchronous=NORMAL` (`AULOS_DB_SYNCHRONOUS=FULL` available),
  `busy_timeout=5000`, `wal_autocheckpoint=512`, `journal_size_limit=67108864`,
  `foreign_keys=ON`, `temp_store=MEMORY`, `mmap_size=64MiB`, `cache_size=-16000`.
- `PRAGMA quick_check` at boot; `PRAGMA wal_checkpoint(TRUNCATE)` on graceful shutdown and every
  6 h. `healthz` reports `wal_bytes` and flips the store component to `down` above 256 MB.
- **The DB is derivable.** If it is corrupt, deleting it and restarting (re-import from the
  untouched legacy JSON, or an empty state) is a documented, valid recovery. That property is why
  T2 matters.
- `StoreError::Busy | ::Locked` map to HTTP `503 state_unavailable` with `Retry-After: 1`.

### 7.2 DDL (migration `0001_init.sql`)

```sql
PRAGMA journal_mode = WAL;

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
-- keys: schema_version, instance_id, created_at, ord_hwm, seq_hwm, seq_hwm_witness,
--       imported_from, imported_at, import_report
--   `seq_hwm_witness` is written on every graceful shutdown and is what the §4.1 boot
--   consistency check for `seq` compares against; it is load-bearing, not optional.

CREATE TABLE items (
  id                  TEXT    PRIMARY KEY,          -- ULID
  kind                TEXT    NOT NULL CHECK (kind IN ('item','group')),
  group_id            TEXT    REFERENCES items(id) ON DELETE CASCADE,
  group_index         INTEGER,
  ord                 INTEGER NOT NULL UNIQUE,      -- client sort key, monotonic, restart-stable
  url                 TEXT    NOT NULL,
  canonical_key       TEXT    NOT NULL,             -- dedupe key (§8.5) — indexed, NOT unique
  provider            TEXT,
  media_id            TEXT,
  title               TEXT    NOT NULL,
  status              TEXT    NOT NULL CHECK (status IN
                        ('queued','resolving','preparing','downloading',
                         'postprocessing','finished','error','canceled')),
  auto_start          INTEGER NOT NULL DEFAULT 1,
  msg                 TEXT,
  error_json          TEXT,                         -- serde(WireError)
  request_json        TEXT    NOT NULL,             -- serde(DownloadRequest)
  entry_json          TEXT,                         -- compacted provider entry (§7.5)
  filename            TEXT,
  size                INTEGER,
  chapter_files_json  TEXT    NOT NULL DEFAULT '[]',
  subtitle_files_json TEXT    NOT NULL DEFAULT '[]',
  source_json         TEXT    NOT NULL,             -- serde(SourceRef)
  attempt             INTEGER NOT NULL DEFAULT 0,
  children_total      INTEGER,                      -- groups only
  created_at          INTEGER NOT NULL,             -- unix ms
  started_at          INTEGER,
  finished_at         INTEGER,
  updated_at          INTEGER NOT NULL,
  clear_after         INTEGER
) STRICT;

CREATE INDEX items_status_ord    ON items(status, ord);
CREATE INDEX items_ord           ON items(ord);
CREATE INDEX items_group         ON items(group_id, group_index);
CREATE INDEX items_url           ON items(url);
CREATE INDEX items_media_id      ON items(media_id) WHERE media_id IS NOT NULL;
CREATE INDEX items_canonical     ON items(canonical_key);
CREATE INDEX items_clear_after   ON items(clear_after) WHERE clear_after IS NOT NULL;
CREATE INDEX items_finished_at   ON items(finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE subscriptions (
  id                     TEXT PRIMARY KEY,          -- ULID (new) or the imported legacy UUID
  name                   TEXT NOT NULL,
  url                    TEXT NOT NULL UNIQUE,
  enabled                INTEGER NOT NULL DEFAULT 1,
  check_interval_minutes INTEGER NOT NULL DEFAULT 60,
  request_json           TEXT NOT NULL,             -- the DownloadRequest template (sans url)
  last_checked           INTEGER,                   -- unix ms
  last_success           INTEGER,
  next_due               INTEGER,                   -- unix ms; the scheduler's persisted authority
  consecutive_failures   INTEGER NOT NULL DEFAULT 0,
  error                  TEXT,
  created_at             INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL
) STRICT;
CREATE INDEX subscriptions_due ON subscriptions(enabled, next_due);

CREATE TABLE subscription_seen (
  subscription_id TEXT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
  media_id        TEXT NOT NULL,
  seen_at         INTEGER NOT NULL,
  PRIMARY KEY (subscription_id, media_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX subscription_seen_age ON subscription_seen(subscription_id, seen_at DESC);

CREATE TABLE telegram_chats (
  chat_id     INTEGER PRIMARY KEY,
  config_json TEXT NOT NULL,
  updated_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE kv (                    -- runtime overrides (cookiefile), hook bookkeeping
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;
```

| Choice | Rationale |
|---|---|
| Groups live in `items` with `kind='group'` and `ON DELETE CASCADE` on children. | Deleting a playlist deletes its children in one statement; a group sorts and queries through the same code path as an item; the client decodes one type. |
| `ord` is `NOT NULL UNIQUE`, fed by the reserve-first hi/lo allocator and seeded at open from `max(meta.ord_hwm, MAX(ord)+1)`. | A stable client sort key that survives restart and an unclean shutdown, without a `MAX(ord)+1` query per insert. |
| `canonical_key` is indexed but **not** unique, and there is no partial unique index. | A playlist or channel page can legitimately contain the same video twice, and "give me the mp3 of the video I'm currently pulling as mp4" is a legitimate second item. Dedupe is an engine policy over `(canonical_key, selection)` for **user adds only** (§8.5), not a database constraint that would abort a 500-child transaction. |
| `subscription_seen` is a table, not a JSON list. | Legacy rewrote a 50 000-element JSON array on every check. Here a check writes only the new ids; `PruneSeen` is one `DELETE … WHERE seen_at < (SELECT seen_at … LIMIT 1 OFFSET :keep)`. |
| **No progress columns at all.** | Preserves the one genuinely good legacy property: progress never touches disk. There is no `event_log` of frames either. |
| `STRICT` tables. | Type drift fails at write time; the importer's tests depend on it. |
| `next_due` and `consecutive_failures` persisted. | A restart does not reset the subscription schedule, and backoff survives. |

### 7.3 Migrations

`rusqlite_migration` with an embedded `Vec<M>`; `meta.schema_version` is also written for human
inspection. Forward-only, additive; one up-only SQL file per migration under
`crates/aulos-store/migrations/`. `aulos-server print-schema` renders the current DDL, and an
`insta` snapshot test asserts that applying every migration from scratch equals the checked-in
`schema.sql`.

### 7.4 Wire-type schema generation

`aulos-server print-schema --json` emits a JSON Schema for `ItemView`, `WireError`,
`SubscriptionView`, `Capabilities`, `MergedCatalog` and every WS frame. Two consumers:
1. CI asserts the schema is unchanged unless the change is intentional (a snapshot test).
2. The iOS repo generates its Codable models from it, and a CI job validates recorded `/history`,
   `/version` and `api/v2/*` bodies against it. This is the mechanism that makes "hand-write Swift
   Codable types from PROTOCOL.md" safe.

### 7.5 Entry compaction

The provider entry blob is the one unbounded field. Rules ported from
`_compact_persisted_entry`, then tightened:

| Provider / case | Kept |
|---|---|
| `ytdlp`, non-playlist child | nothing (`entry_json = NULL`) |
| `ytdlp`, playlist/channel child | keys matching `^(playlist\|channel)`, plus `n_entries`, `__last_playlist_index` — needed to re-resolve `outtmpl` after a restart |
| `streamingcommunity` | the **whole** entry, in the `state` shape of §10.3 — needed for the JIT m3u8 (`base_url`, `title_id`, `episode_id`) and for NFO metadata. Rows imported from legacy JSON are translated into that shape at import time (§7.6.3a); nothing at runtime reads the legacy `_sc_*` key names. |
| `command` plugin | the entry's `state` object plus `media_id`/`title`/`url` |
| any | hard cap `AULOS_ENTRY_MAX_BYTES` (262144); over cap ⇒ store `{"__truncated":true}` and log a WARN |

Terminal `finished` items drop `entry_json` on the transition (matching legacy's
`_should_persist_entry() == identifier != "completed"`), **except** SC entries, which are kept
until the NFO hook has run and are then dropped.

### 7.6 Legacy JSON importer

Runs from `aulos-server bootstrap` **only when the DB file does not exist**, or on demand via
`aulos-server import`. It is the single most cutover-critical component.

#### 7.6.1 Inputs

| File in `STATE_DIR` | Legacy `kind` | Target |
|---|---|---|
| `queue.json` | `persistent_queue:queue` | `items`, status per §7.6.3, `auto_start = true` |
| `pending.json` | `persistent_queue:pending` | `items`, `status='queued'`, `auto_start = false` |
| `completed.json` | `persistent_queue:completed` | `items` with terminal status |
| `subscriptions.json` | `subscriptions` | `subscriptions` + `subscription_seen` |
| `telegram_bot_config.json` | *(bare object, no envelope)* | `telegram_chats` |
| `cookies.txt` | — | recorded as `kv['cookiefile']` runtime override |
| extensionless `queue`/`pending`/`completed`/`subscriptions` (`shelve`/pickle) | pickle | **not supported** (BRIEF out of scope). Detected and reported: *"legacy shelf found at …; start the Python image once so it migrates to JSON, then re-run the import."* |

The importer **never** renames, quarantines or deletes a legacy file — T2, and a unit test asserts
the inputs are byte-identical afterwards.

**Failure classes, and exactly what each one does.** This is the one place where an ambiguity
turns a cutover into a restart loop, so the taxonomy is explicit and there are only three classes:

| Class | Example | Report | Effect |
|---|---|---|---|
| **Record error** | one array element in a valid envelope is malformed, has a non-string `url`, or fails the v1→v2 migration | `warnings[]`, code `record_skipped`, with the file name and index | that record is skipped, the rest of the file imports, the import **commits**, boot succeeds |
| **File error** | malformed JSON, `schema_version ∉ {1,2}`, `kind` ≠ the expected string, an unreadable file | `errors[]`, code `file_invalid` | governed by the policy below |
| **Fatal** | the DB cannot be written, a legacy `shelve` is present (BRIEF out of scope), `STATE_DIR` unreadable | `errors[]` | always rollback + exit non-zero, no policy escape |

A **file error** is resolved by `AULOS_IMPORT_ON_ERROR ∈ fail | skip` (default **`fail`**), also
settable as `aulos-server import --skip-corrupt`:

- `fail` (default): rollback, delete the DB file, exit non-zero with the report printed. Nothing is
  half-imported and nothing is silently lost. This is the right default for a cutover, because the
  operator is watching and the legacy files are untouched, so the answer is "read the report, fix
  the fixture, retry".
- `skip`: the offending file contributes **no** records, its `errors[]` entry is downgraded to a
  `warnings[]` entry with code `file_skipped`, the rest of the import **commits**, and
  `healthz.components.importer.status` is `degraded` with the file name in `detail` (and it stays
  degraded for the life of the process, so it cannot be missed).

`skip` is the deliberate equivalent of legacy's behaviour: `AtomicJsonStore.load()` quarantined a
malformed or kind-mismatched file to `<path>.invalid.<ts>`, returned `None`, and the server
**started normally with that collection empty** (spec §5.2). Without a `skip` policy, a single
corrupt legacy JSON file would make the new container restart-loop forever where the old one kept
serving — a strictly worse operational property, and the reason this knob exists rather than being
"just use `--force`" (which only overrides the already-imported marker, §7.6.6). The one difference
from legacy is that we never move the operator's file: the quarantine is a log line and a report
entry, not a rename (T2).

`schema_version: 1` records go through `LegacyV1 → LegacyV2`, re-implementing
`DownloadInfo.__setstate__`:

| Legacy field state | Migration |
|---|---|
| `format ∈ {m4a,mp3,opus,wav,flac}` | `download_type=audio`, `codec=auto`, `format` unchanged |
| `format == "thumbnail"` | `download_type=thumbnail`, `format=jpg`, `quality=best` |
| `format == "captions"` | `download_type=captions`, `format = subtitle_format or "srt"`, `quality=best` |
| `quality == "best_ios"` | `download_type=video`, `format=ios`, `quality=best` |
| `quality == "audio"` | `download_type=audio`, `format=m4a`, `quality=best` |
| `ytdl_options_preset: str` | → `ytdl_options_presets: [str]` |
| missing `status` | `pending` |
| any missing post-v1 field | its documented default |

`{"__metube_bytes__":"<b64>"}` and `{"__metube_datetime__":"<iso>"}` wrappers inside `entry` are
decoded to bytes / RFC3339 strings.

#### 7.6.2 Identity assignment

1. Records are ordered by legacy `timestamp` (nanoseconds; missing ⇒ file order), **globally**
   across `completed` → `pending` → `queue`, so `ord` reflects real chronology.
2. `ord` is assigned `0,1,2,…`; `meta.ord_hwm` is set past the end.
3. `id = Ulid::from_datetime(timestamp)` so ULID lexicographic order matches `ord` (a debugging
   convenience; nothing relies on it).
4. `media_id` ← the legacy `id` field (the yt-dlp video id, possibly `"<prefix>.<id>"`);
   `url` ← the legacy `url`. **Both** are indexed, because the v1 shim accepts either as a key.
5. `canonical_key` is computed with the same function used at runtime (§8.5).
6. A duplicate legacy `url` across files (possible — legacy deduped only within `queue`) ⇒ keep
   the most advanced record (terminal > active > pending), log a `duplicate_url` warning, record
   the discarded one in the report.

#### 7.6.3 Status mapping

| Legacy `status` | Source file | Imported status | Notes |
|---|---|---|---|
| `pending` | `pending.json` | `queued`, `auto_start=false` | the user must press start |
| `pending` | `queue.json` | `queued`, `auto_start=true` | admitted by the scheduler, **not** all at once |
| `preparing`, `downloading` | `queue.json` | `queued`, `auto_start=true`, `attempt+=1`, `msg="Restarted after upgrade"` | the Python process is gone; the `.part` file is left for yt-dlp's own resume |
| `finished` | `completed.json` | `finished` | `filename`, `size`, `chapter_files` preserved |
| `error` | `completed.json` | `error` | `error`/`msg` preserved; `error.code = internal` unless the text matches the §6 taxonomy |
| anything else / absent | any | `queued` if in queue/pending, else `error` with `msg="Imported with unknown legacy status: <x>"` | never guesses `finished` |

`CLEAR_COMPLETED_AFTER` is applied at import using `finished_at = timestamp`. Already-past values
are cleared by the first `ClearScheduler` tick — the same net effect as legacy, which lost the
timer on restart entirely.

#### 7.6.3a StreamingCommunity entry translation

Legacy persisted the **whole** entry for any record whose `str(entry['extractor']).lower()`
contained `streamingcommunity` (spec §5.2), and the download-time gate read
`entry['_sc_needs_m3u8_extraction']` and `entry['_sc_base_url']` (spec §9.4). The v2 `state` object
(§10.3) uses different key names and splits out `title_id`/`episode_id`, which legacy never stored
separately — they were embedded in `id = sc_<title_id>[_<episode_id>]`. So an SC row imported
verbatim would carry an entry blob the v2 JIT extractor cannot read, and the NFO hook (§13.2,
which reads `entry_json`, not the on-disk `.info.json`) would see legacy keys. The importer
therefore translates, and this is a mandatory, tested step — not a passthrough:

```
if lower(entry.extractor or "").contains("streamingcommunity"):
    provider = "streamingcommunity"
    state.base_url               ← entry._sc_base_url
                                   fallback: "{scheme}://{host}" of the record's url
    state.needs_m3u8_extraction  ← entry._sc_needs_m3u8_extraction, default true
    (state.title_id, state.episode_id)
                                 ← parse the legacy media id  ^sc_(\d+)(?:_(\d+))?$
                                   fallback: parse the watch url  /watch/(\d+)(?:\?e=(\d+))?
                                   if both fail: title_id = null, needs_m3u8_extraction stays true
                                   and a `sc_ids_unresolved` warning is recorded — the JIT
                                   extractor re-derives them from the watch url at download time,
                                   so the item still works; only NFO `uniqueid` is affected
    state.season_number, state.episode_number, state.episode, state.series, state.ext,
    state.extractor, state.extractor_key
                                 ← copied verbatim
    state.legacy                 ← every remaining legacy entry key, verbatim
```

`state.legacy` exists so that NFO metadata legacy happened to carry (`plot`, `upload_date`,
`uploader`, `channel`, `tags`, `duration`, `original_url`, …) survives the translation and the NFO
hook keeps producing the same XML for a pre-cutover row. The `_sc_*` keys are **not** duplicated
into `state.legacy`; they are consumed.

The WP-05 fixture corpus carries an `sc-entry` directory (a real-shaped `queue.json` with one
queued SC episode and one queued SC movie) and asserts: the translated `state`, that the JIT
extractor accepts the imported blob, and that the NFO hook renders the same XML from the imported
blob as from a freshly resolved one.

#### 7.6.4 Subscriptions

```
subscriptions.json.items[*] →
  id                     ← record.id verbatim (a UUIDv4 string; SubId is a validated string)
  name, url, enabled     ← as-is (url .trim()'d; duplicate url ⇒ keep the first, warn)
  check_interval_minutes ← max(1, value)
  request_json           ← DownloadRequest from the flat legacy fields;
                           folder "" → None; chapter_template "" → the config default;
                           ytdl_options_preset → ytdl_options_presets
  last_checked           ← round(value * 1000)  (epoch s → ms)  or NULL
  next_due               ← last_checked + interval, or now + jitter(0..30 s) when NULL
  consecutive_failures   ← 0        (fresh start; the error text is preserved for display)
  error                  ← as-is
  seen_ids[*]            → subscription_seen(media_id, seen_at = last_checked or now),
                           newest-first, capped at SUBSCRIPTION_MAX_SEEN_IDS
```

#### 7.6.5 Telegram chat config

`telegram_bot_config.json` is a bare `{"<chat_id>": { …13 keys… }}` object. Each value is parsed
into `ChatConfig`, with the legacy `format`/`quality` pair normalised through the same
`normalize_download_selection` port the bot uses (§12.3), so a stored `{"format":"m4a"}` becomes
`download_type=audio, format=m4a`. Unknown keys are dropped with a DEBUG log. A chat id that is
not currently in `TELEGRAM_ALLOWED_CHAT_IDS` is still imported — the allow-list is checked at
message time, matching legacy.

#### 7.6.6 Transaction, idempotence, report

- The entire import is **one SQLite transaction**. A **fatal** error, or a **file error** under
  `AULOS_IMPORT_ON_ERROR=fail` (the default), ⇒ rollback, delete the DB file, exit non-zero with
  the report printed. There is never a half-imported DB. **Record errors never fail the import**,
  and neither do file errors under `AULOS_IMPORT_ON_ERROR=skip` — see the failure-class table in
  §7.6.1, which is the normative definition. A non-empty `warnings[]` never fails the boot; a
  non-empty `errors[]` fails it exactly when the policy above says so.
- On success, in the same transaction: `meta.imported_from`, `meta.imported_at`,
  `meta.import_report`, plus a marker file `<STATE_DIR>/.aulos-imported` containing the report, so
  a second start with a deleted DB does not silently re-import stale JSON. `--force` overrides.
- The report is logged at INFO as a table **and** served at `GET <p>api/v2/import-report`:

```json
{ "imported_at": 1757000000000,
  "state_dir": "/downloads/.metube",
  "files": [
    {"file":"queue.json","schema_version":2,"records":3,"imported":3,"skipped":0},
    {"file":"pending.json","schema_version":2,"records":0,"imported":0,"skipped":0},
    {"file":"completed.json","schema_version":1,"records":412,"imported":411,"skipped":1},
    {"file":"subscriptions.json","schema_version":2,"records":7,"imported":7,"skipped":0},
    {"file":"telegram_bot_config.json","schema_version":null,"records":2,"imported":2,"skipped":0}
  ],
  "warnings": [
    {"code":"duplicate_url","detail":"https://youtu.be/x in queue.json and completed.json; kept finished"},
    {"code":"unknown_status","detail":"completed.json[87] status=\"cancelled\" → error"}
  ],
  "errors": [],
  "seen_ids_imported": 3121,
  "items": {"queued": 3, "finished": 401, "error": 10, "canceled": 0} }
```

- `--dry-run` runs the whole importer against `:memory:` and prints the same report without
  touching disk. This is the runbook's mandatory rehearsal step (§19.2).

---

## 8. Queue engine (`aulos-queue`)

### 8.1 Commands and events

```rust
pub enum EngineCmd {
    Add { requests: Vec<DownloadRequest>, source: SourceRef,
          ack: oneshot::Sender<Result<AddOutcome, AddError>> },
    /// The v1 shim's bounded synchronous pre-resolve (§11.2). Completes when every listed id has
    /// left `resolving`; ids already out of `resolving` are reported immediately. The **caller**
    /// owns the deadline (`tokio::time::timeout`), so a slow resolve cannot pin engine state.
    WaitResolved { ids: Vec<ItemId>, ack: oneshot::Sender<Vec<ResolveReport>> },
    Start  { ids: Vec<ItemId>, ack: AckActions },     // queued(!auto_start) → queued(auto_start)
    Pause  { ids: Vec<ItemId>, ack: AckActions },     // §8.7: un-schedule, or park a running job
    Cancel { ids: Vec<ItemId>, ack: AckActions },
    Retry  { ids: Vec<ItemId>, ack: AckActions },     // error|canceled → queued, attempt += 1
    Delete { ids: Vec<ItemId>, delete_file: Option<bool>, ack: AckActions },
    CancelResolve { scope: CancelScope, ack: AckActions },   // v1 `cancel-add`, v2 `cancel-resolve`
    Watch   { conn: ConnId, groups: Vec<GroupId>, done: bool,
              ack: oneshot::Sender<Vec<Arc<ItemView>>> },
    Unwatch { conn: ConnId, groups: Vec<GroupId>, ack: oneshot::Sender<()> },
    /// Socket closed: drop **every** group this connection watched. The WS task must send this on
    /// every close path (§15.4) — normal close, 1001, 1009, 1013, reader error, writer error.
    ConnClosed { conn: ConnId },
    /// A hook's engine-mediated writeback (§13.3). This is the only way `aulos-hooks` can touch a
    /// row, and it is why `set_size` is not a direct SQLite write. The engine persists it, updates
    /// its item cache, and publishes `StatusChanged { from == to }` — the generic "this persisted
    /// row changed, re-diff it" signal — so the Aggregator emits a `delta` carrying exactly the
    /// changed field. On a pre-terminal write the following `Completed` frame carries it anyway.
    HookWrite { id: ItemId, write: HookWrite, ack: oneshot::Sender<Result<(), PortError>> },
    /// Pre-terminal hooks for this item are done; finalise it (§13, phase `PreTerminal`).
    HooksFinished { id: ItemId, outcome: Box<Outcome> },
    // internal
    Resolved { id: ItemId, result: Result<Vec<MediaEntry>, ProviderError> },
    Stage    { id: ItemId, stage: Stage, msg: Option<Box<str>> },
    Finished { id: ItemId, outcome: Box<Outcome> },
    Failed   { id: ItemId, err: ProviderError },
    SlotFreed,
    Tick,                                            // 1 Hz: clear_after, watchdogs
}

/// The ack type shared by `Start`/`Pause`/`Cancel`/`Retry`/`Delete`/`CancelResolve`/`Unwatch`.
/// It is a type alias, not a struct: there is exactly one actions result shape.
pub type AckActions = oneshot::Sender<ActionsResult>;

pub enum HookWrite { Size(u64), DropEntryBlob }

pub struct AddOutcome { pub ids: Vec<ItemId>, pub duplicates: Vec<Duplicate>, pub generation: u64 }
/// One entry per id handed to `WaitResolved`.
pub struct ResolveReport { pub id: ItemId, pub kind: Kind,
                           pub outcome: Result<(), WireError> }
pub struct Duplicate { pub url: Arc<str>, pub existing_id: ItemId }
pub struct ActionsResult { pub applied: Vec<ItemId>, pub skipped: Vec<Skipped> }
pub struct Skipped { pub id: ItemId, pub reason: SkipReason }
#[derive(Serialize)] #[serde(rename_all = "snake_case")]
pub enum SkipReason { NotFound, AlreadyTerminal, NotCancelable, NotStartable, NotRetryable, NotPausable }

/// `Watch`'s connection identity. A `u64` newtype minted by an atomic counter in `aulos-api`
/// (`aulos_core::id::ConnId`). The engine owns the registry:
/// `watchers: HashMap<ConnId, HashSet<GroupId>>` plus a `HashMap<GroupId, u32>` refcount, both
/// mutated only by `Watch` / `Unwatch` / `ConnClosed`. Nothing else in the process holds a
/// connection→groups map.
pub struct ConnId(pub u64);

/// What a `cancel-add` cancels. Legacy's `cancel_add()` took no body and simply bumped a
/// process-global `_add_generation`, so a legacy client never had a generation to send and the
/// v1 route has none to pass (§11.1). `All` reproduces that exactly.
pub enum CancelScope {
    /// Bump `add_generation`, abort **every** in-flight resolve task, and mark the not-yet-created
    /// children of every in-flight expansion cancelled. What v1 `POST <p>cancel-add` sends.
    All,
    /// Only resolutions and expansions belonging to this `AddOutcome.generation`. Reachable from
    /// `POST api/v2/downloads/cancel-resolve` with a `generation` body field, which is why the
    /// `202` body of `POST api/v2/downloads` carries `generation` (PROTOCOL §4.1, §4.7) — without
    /// that field on the wire this variant would be unconstructible by any client.
    Generation(u64),
}

pub enum DomainEvent {
    Added(Vec<Arc<ItemView>>, AddReason),   // AddReason ∈ Created | Expanded | Retried
    /// A persisted row changed. `from == to` is legal and is the engine's generic
    /// "re-diff this row" signal — used by `HookWrite` (§13.3) and by any write that changes a
    /// mutable field without changing the status. The Aggregator diffs against `last_sent`, so
    /// nothing needs to say *which* field moved.
    StatusChanged { id: ItemId, from: Status, to: Status, view: Arc<ItemView> },
    Completed(Arc<ItemView>),               // terminal: finished | error | canceled
    /// One event per (reason, batch). A producer that removes ids for two different reasons
    /// publishes two events; the Aggregator groups by reason and the wire carries one `removed`
    /// frame per reason (§15.1, PROTOCOL §5.7).
    Removed { ids: Vec<ItemId>, reason: RemoveReason },
    /// Terminal work is done but the status has **not** been written yet: pre-terminal hooks get
    /// their turn here (§13). Delivered to the `hooks` subscriber only — never to the aggregator,
    /// so it produces no frame. The engine finalises on `HooksFinished`.
    Finishing(Arc<ItemView>),
    SubscriptionChanged(Arc<SubscriptionView>),      // SubscriptionView: aulos-core (§3)
    SubscriptionRemoved(SubId),
    YtdlOptionsReloaded { ok: bool, msg: Box<str>, update_time: Option<f64> },
    ProvidersReloaded(Arc<ReloadReport>),            // ReloadReport: aulos-core (§3)
    HealthChanged(Arc<HealthView>),                  // HealthView: aulos-core (§3)
    Notice { level: Level, code: &'static str, id: Option<ItemId>, message: Box<str> },
}
```

`DomainEvent` is declared in `aulos-core::event`, so **every type in a payload is an `aulos-core`
type** — that is the whole reason `SubscriptionView`, `ReloadReport` and `HealthView` live there and
not in the crates that produce them (§3). It is delivered to subscribers as `Arc<DomainEvent>`
through the `EventRouter` (§2.2.1); no consumer ever owns an `mpsc::Receiver<DomainEvent>`.

### 8.2 Engine state

```rust
struct Engine {
    store: Store,
    registry: Arc<RwLock<Registry>>,          // write-locked only on plugin reload
    events: mpsc::Sender<DomainEvent>,
    progress_tx: mpsc::Sender<ProgressMsg>,
    cfg: Arc<Config>,
    ytdl: Arc<ArcSwap<YtdlOptions>>,

    ready: [VecDeque<ItemId>; 4],             // one deque per Priority, each FIFO by ord
    resolving: HashMap<ItemId, JoinHandle<()>>,
    running: HashMap<ItemId, RunSlot>,        // { handle, cancel, pgid, provider, started }
    cancels: HashMap<ItemId, CancellationToken>,
    groups: HashMap<GroupId, GroupAcc>,
    global: Arc<Semaphore>,                   // MAX_CONCURRENT_DOWNLOADS
    provider_slots: HashMap<ProviderId, Arc<Semaphore>>,
    resolve_slots: Arc<Semaphore>,            // AULOS_RESOLVE_CONCURRENCY
    dedupe: HashMap<DedupeKey, ItemId>,       // non-terminal items only
    add_generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority { Retry = 0, Interactive = 1, Subscription = 2, Bulk = 3 }
```

`Priority` is derived, never requested:

| Source of the item | Priority |
|---|---|
| a retry (manual or automatic) | `Retry` |
| a direct add from API v2, API v1 or Telegram, not part of a group | `Interactive` |
| a subscription check | `Subscription` |
| a playlist/channel child (any group member) | `Bulk` |

This is why a link you just pasted starts next instead of queueing behind 486 playlist children.
Within a class the order is `ord` ascending, i.e. FIFO, i.e. legacy behaviour.

### 8.3 Add path (async, BRIEF §5)

`Add` does, **synchronously before acking**, per request:

1. Validate against the selected provider's catalog (§6.6) plus the legacy matrix.
2. Resolve and containment-check `folder`; `create_dir_all` when `CREATE_CUSTOM_DIRS`.
3. Check preset existence and the overrides gate.
4. Compute `canonical_key`, check `dedupe`.
5. Mint the ULID, allocate `ord`.

then one batched `InsertItems` store write with `Durability::Sync` and **`status = resolving`,
always** — including when `auto_start = false` — then `ack` with the ids, then publish
`DomainEvent::Added(_, Created)`. Everything after that is background. Target: p99 under 5 ms for
a single add, under 20 ms for a batch of 50.

`auto_start = false` items **still resolve immediately** (so the user sees a real title before
pressing start), and it is the *end* of resolution that lands them in `queued` with
`auto_start = false`, unscheduled. There is no "insert directly as `queued`" path: inserting a
`queued` row while its resolve task is running would contradict PROTOCOL.md §3.1 (`resolving` =
"metadata extraction is running") and the §4.2 transition graph. Legacy also resolved before
enqueueing, so this is behaviour-compatible and strictly faster to acknowledge.

Batch adds are capped at `AULOS_MAX_BATCH_URLS` (500); above that, `413 payload_too_large`.

`AddError` → HTTP: `Validation{field}` → 400 `validation_failed`; `PresetUnknown` → 400
`unknown_preset`; `OverridesDisabled` → 400 `overrides_disabled`; `Folder*` → 400
`folder_invalid`; `Duplicate{existing_id}` → **202 with the existing id** in `duplicates`
(`AULOS_DEDUPE_MODE=strict` makes it `409 conflict` instead).

### 8.4 Resolution and playlist expansion

- Each `resolving` item spawns a task that acquires a `resolve_slots` permit, calls
  `provider.resolve(...)` with a deadline of `AULOS_RESOLVE_TIMEOUT_SECS` (120), and sends
  `EngineCmd::Resolved`.
- `Resolved(Ok(entries))` with **one `Video` entry** ⇒ **the same item id**: `SetResolved`
  (provider, media_id, title, entry, canonical_key), status → `queued`; if `auto_start`, push to
  the priority deque and `schedule()`. On the wire this is one ~110-byte `delta`. No membership
  change, no `added`, no `removed`. This is the common case.
- `Resolved(Ok(entries))` with a **`Playlist`** ⇒ **in-place promotion**:
  `PromoteToGroup { id, children_total, title }` — the record the client already holds keeps its
  `id` **and** its `ord`, and its `kind` flips from `"item"` to `"group"`. Children are inserted
  in batches of 100 (one transaction each), each with `group_id = parent`, `group_index` (1-based),
  its own ULID and `ord`. The first flush emits **one** `added` frame carrying the updated group
  view plus the first `AULOS_SNAPSHOT_GROUP_INLINE` children, with `reason: "expanded"`; the
  remaining children follow in further `added` frames. Because `added` is an **upsert by id** and
  the group keeps its id and sort key, the row morphs in place: it does not blink and it does not
  move. This is the single best anti-flicker property in the protocol and it directly kills iOS
  pain points #11 and #18.
- `Resolved(Ok(entries))` with a **`Redirect`** ⇒ re-enter resolution, depth-capped at
  `AULOS_RESOLVE_MAX_DEPTH` (3); the legacy `already`-URL recursion guard is kept as well.
- **A child carrying `pre_error` is inserted as `queued` with `auto_start = false` and a non-null
  `error`** — *not* as `status = error`. Legacy built a `DownloadInfo` with `status = "pending"` and
  a populated `error` string for an entry with `live_status == "is_upcoming"` (with the release
  timestamp text) or an entry-level `msg` (spec §5.4); it sat in `queue.json`/`pending.json` and
  appeared in `/history`'s `queue`/`pending`, never in `done`. Inserting it as `error` would put an
  upcoming livestream in the shipped client's **Failed** section (§11.4 projects terminal statuses
  into `done[]`) and it would never start when the stream went live, because the subscription
  `is_live` re-queue (§14.3 step 5) matches on the *seen* set, not on failed items. As
  `queued(auto_start=false)` the row is one tap from starting, its subscription re-queues it when
  the stream begins, and the v1 projection puts it in `pending[]` with a populated `error` string —
  byte-compatible with legacy. The error text is preserved verbatim, including
  `Live stream is scheduled to start at {ts:%Y-%m-%d %H:%M:%S %z}`, with
  `error.code = not_yet_live` for an upcoming stream and `error.code = unsupported_url` (or the
  §9.6 mapping of the message) for an entry-level `msg`.
- `Resolved(Ok(entries))` with **zero entries**, or a `Video` entry with no usable id/url ⇒ status
  `error` with `error.code = unsupported_url` and the **verbatim legacy message**
  `Invalid/empty data was given.` (§11.7). Legacy produced exactly that string from `__add_entry`,
  and the v1 shim's pre-resolve (§11.2) reports it as `{"status":"error","msg":…}`.
- A root `_type` outside `{video, playlist, channel, url*}` ⇒ status `error` with
  `error.code = unsupported_url` and the verbatim legacy message
  `Unsupported resource "<etype>"`. The `ytdlp` shim emits it as an `error` frame (§9.6) so the
  string is produced in exactly one place.
- `Resolved(Err(ProviderError::Unsupported))` **and** a `Ready` runner-up exists **and**
  `AULOS_RESOLVE_FALLTHROUGH` (default `true`) ⇒ **one** retry through the runner-up (§6.4): the
  item stays `resolving`, `msg = "Retrying with <runner-up>"`, `aulos_resolve_fallthrough_total`
  increments, and the runner-up's result is final. This is the legacy `__extract_info` behaviour
  where a StreamingCommunity URL the extractor could not dispatch fell through to yt-dlp; the
  path-aware `matches()` of §10.2 covers the common case, and this covers a scrape that returns
  nothing.
- `Resolved(Err(e))` for every other `e` ⇒ status `error`, `error = WireError::from(e)`
  (code + cleaned message). No fall-through (§6.4).
- `playlist_item_limit > 0` ⇒ children beyond the limit are not created **and** `playlistend` is
  set in each child's yt-dlp options. Both legacy applications are preserved.

### 8.5 Dedupe

```rust
pub fn canonical_key(provider: &ProviderId, url: &Url, media_id: Option<&str>) -> Box<str>;
pub struct DedupeKey { pub canonical: Box<str>, pub selection: Selection }
```

`canonical` = `provider_id` + `\u{1f}` + the normalised target, where the normalised target is:

- the provider's own canonical id when known (`ytdlp`: `media_id` once resolved, so `youtu.be/x`,
  `youtube.com/watch?v=x` and `…&t=30` collapse; `sc`: `sc_<title>[_<ep>]`); otherwise
- the URL with lower-cased scheme+host, the default port stripped, the trailing `/` stripped, and
  a per-host tracking-param denylist applied (`si, feature, pp, utm_*` for YouTube hosts — the
  same list the iOS share extension applies, which lets the client stop doing it).

Policy, `AULOS_DEDUPE_MODE ∈ off | active | strict` (default `active`):

- The key is `(canonical, selection)`, so re-adding the same video as `mp3` after pulling it as
  `mp4` is a legitimate new item.
- Only **non-terminal** items participate. Re-adding a finished URL creates a new item (legacy
  allowed this, and it is how you re-download something you deleted).
- **Playlist children bypass dedupe entirely.** They are created by expansion, not by a user add,
  and a channel page listing the same video twice must not fail.
- `active` returns the existing id in `duplicates` (v1 shim: a plain `{"status":"ok"}`, matching
  legacy's silent skip). `strict` returns `409`.

This closes the legacy bug where only `queue` was checked, so re-adding a *pending* URL created a
second entry that silently replaced the first.

### 8.6 Groups

A group row never downloads. Its state is derived from its children, maintained **incrementally**
in `GroupAcc`, never recomputed per tick:

```rust
pub struct GroupAcc {
    pub total: u32, pub resolved: u32,
    pub counts: [u32; 8],           // one per Status
    pub downloaded: u64,            // Σ active child downloaded_bytes
    pub finished_bytes: u64,        // Σ size of finished children — completed work is never lost
    pub total_est: u64,             // Σ best-effort child total (bytes or estimate)
    pub n_with_total: u32,
    pub speed: f64,                 // Σ running child speed
}
```

`status` roll-up (a value from the same closed 8-value enum — a group is never `"active"` or any
other out-of-vocabulary string):

```
downloading    if any child is_running()
else queued    if any child is Queued or Resolving
else error     if any child is Error
else canceled  if all terminal and >= 1 Canceled
else finished
```

`percent` roll-up — **byte-weighted when the totals are known**, with a documented count-weighted
fallback (a 500-item playlist of 49 short clips plus one 4 GB file must not read 98 % while half
the bytes are outstanding):

```
if n_with_total == resolved && total_est > 0:
    percent = 100 * (finished_bytes + downloaded) / total_est
else:
    percent = 100 * (counts[Finished] + Σ_active(child.percent / 100)) / max(total, 1)
```

`speed` is the sum over running children; `eta` is `bytes_remaining / speed` when both are known,
else `null`. Aggregates are recomputed from scratch every 5 minutes and compared
(`debug_assert_eq!` in debug builds, a silent correction plus `aulos_group_drift_total` in
release) so incremental drift can never accumulate unnoticed.

`children_inline` is `false` on any group larger than `AULOS_SNAPSHOT_GROUP_INLINE` (50); those
children are omitted from the snapshot and the group id is listed in `truncated.groups`. A client
gets them either by `GET api/v2/items?group_id=<id>` (paged) or by sending a `watch` frame. Both
are optional: a client that ignores the feature simply shows the group row with its counters,
which is all a collapsed playlist needs. A 500-item playlist therefore adds **~400 bytes** to the
connect snapshot instead of ~125 KB.

Delta economics: children in `queued` are static and never appear in a delta. Per tick a
downloading 500-item playlist contributes one group delta plus one delta per running child.

**`children_inline` bounds the *snapshot*, not the delta stream.** Frames are serialised once and
shared by every client (§15.3), so there is no per-connection filtering: a running child of a
collapsed group produces a `delta` that every client receives, including clients that never got an
`added` for it. That is why PROTOCOL §5.4's guarantee is stated with one exception — a `delta` may
reference a child of a group whose `children_inline` is `false` — and why the client algorithm's
apply step already skips patches for unknown ids instead of creating a record. `watch` therefore
changes exactly one thing: the `added` pages a connection is sent. It does not change what is in
any `delta`, and unwatching does not suppress anything.

### 8.7 Scheduling, slots and cancellation

```
schedule():
  for prio in [Retry, Interactive, Subscription, Bulk]:
     scanned = 0
     while let Some(id) = ready[prio].front_from_cursor():
        if scanned == AULOS_SCHED_LOOKAHEAD { break }
        item = self.cached_item(id)                      // engine keeps an item cache; no DB hit
        prov = registry.by_id(item.provider)
        permit = match prov.own_slots() {
            Some(_) => provider_slots[prov].try_acquire(),   // NOT the global slot
            None    => global.try_acquire(),
        };
        match permit { Some(p) => { remove(id); spawn run_job(id, p, token) }
                       None    => { scanned += 1; advance_cursor() } }
```

- `own_slots()` providers bypass the global semaphore exactly like legacy's `sc_semaphore`, so a
  queued SC download never holds a global slot.
- The lookahead scan (default 32) is what stops a saturated SC pool from starving yt-dlp items
  and vice versa. It is bounded so scheduling stays O(1)-ish; combined with `Priority` it makes
  head-of-line blocking impossible in the cases users notice.
- **The engine keeps an in-memory item cache** for every non-terminal item plus the most recent
  `AULOS_MEM_DONE_ITEMS` terminal ones. `schedule()` and the published snapshot never go through
  the store actor.

Cancel:

| Item state | Action |
|---|---|
| `resolving` | cancel the token; the resolve task aborts; status → `canceled` |
| `queued` | remove from its ready deque; status → `canceled` |
| `preparing`/`downloading`/`postprocessing` | cancel the token → the provider `killpg(-pgid, SIGTERM)`, waits `AULOS_KILL_GRACE_MS` (5000), then `SIGKILL` → the run task returns `Canceled` → status → `canceled`, partials removed (`*.part`, `*.ytdl`, the SC segment dir, the partial mp4) |
| terminal | no-op, ack ok (idempotent) |
| group | recursively cancels every non-terminal child in one message and one transaction; the group becomes `canceled` |

Pause (new; the second half of iOS ask 11, whose first half — retry — is C35):

| Item state | Action |
|---|---|
| `queued`, `auto_start = true` | remove from its ready deque and write one `SetAutoStart { auto_start: false }` (§7.1). The status stays `queued`. Nothing was running, so nothing is lost. |
| `preparing`/`downloading`/`postprocessing` | cancel the token exactly as `Cancel` does (`killpg` SIGTERM → grace → SIGKILL), but **keep the partial file** (`*.part`/`*.ytdl` are not removed) and write `SetStatus { status: Queued, auto_start: Some(false), msg: Set("Paused"), error: Keep, at: now }` — **no** `BumpAttempt`, so `attempt` is unchanged. `Start` then re-runs the job and yt-dlp resumes from the `.part`. An SC job's partials *are* removed, because its m3u8 token is dead anyway (same rule as §8.9). |
| `queued`, `auto_start = false` | no-op, ack ok (idempotent) |
| `resolving` | `NotPausable` — resolution is seconds long and cancelling it would lose the record's identity for the client. Use `Cancel`. |
| terminal | `NotPausable` |
| group | pauses every pausable child in one message and one transaction; the group's roll-up becomes `queued` |

Pause is deliberately expressed with the **existing** state (`queued` + `auto_start = false`) and
not as a ninth status: BRIEF §6 closes the status vocabulary at eight, and "paused" and "never
started" are the same thing to every consumer — the scheduler skips both, the v1 shim projects both
into `pending[]`, and the client already renders the flag as "Paused" (§4.2). The action set is
therefore `start | pause | cancel | retry | delete`.

Cancel is **immediate at the API layer**: the status write and the terminal frame are emitted as
soon as the token is cancelled and the status is persisted. The HTTP response does not wait for
`SIGKILL`. The client sees `canceled` within one urgent flush (≤ 25 ms).

### 8.8 Retry policy

`Retry` is explicit (a user action, or v1 `POST /start` on a failed item). Automatic retry is
narrow on purpose: only errors whose `retryable()` is true (`network`, `throttled`, `timeout`) are
auto-retried, at most `AULOS_AUTO_RETRY_MAX` (2) times, with delay `30 s × 2^attempt` ± 20 %
jitter, at `Priority::Retry`. `auth_required`, `bot_check`, `geo_restricted`, `unavailable`,
`no_format`, `disk_full`, `tool_missing`, `contract`, `canceled` are **never** auto-retried.
`AULOS_AUTO_RETRY_MAX=0` disables the feature entirely. This is new behaviour (legacy had none)
and is why `attempt` is on the item and on the wire.

`not_yet_live` is listed as "retry likely to help, later" in PROTOCOL.md §1.6 but is **never**
auto-retried here, and that is not an inconsistency: an upcoming livestream is not a failed item at
all in this design — it is `queued(auto_start=false)` with a `not_yet_live` error (§8.4), so there
is nothing to retry. It starts when the user presses start, or when its subscription re-queues it
on `live_status == "is_live"` (§14.3 step 5). PROTOCOL.md §1.6's advice is for the case where the
code *does* appear on a terminal item (a stream that a provider reported as upcoming only after the
download started).

### 8.9 Boot recovery

Runs after migrations and the importer, **before** the HTTP listener binds, so the first client
sees a consistent snapshot.

| Found status | Action | Reason |
|---|---|---|
| `resolving` | → `queued`, `msg="Re-queued after restart"` | the resolve task is gone |
| `preparing`/`downloading`/`postprocessing` | → `queued` (`SetStatus`), `BumpAttempt`, and `SetSource { kind: "restart" }` (§7.1) | legacy restarted these all at once; the scheduler now admits them `MAX_CONCURRENT_DOWNLOADS` at a time |
| `queued`, `auto_start=true` | left as-is, pushed to its priority deque ordered by `ord` | |
| `queued`, `auto_start=false` | left as-is, not scheduled | the legacy `pending` bucket |
| terminal | untouched; `clear_after` re-armed; the most recent `AULOS_MEM_DONE_ITEMS` loaded into the cache | |
| groups | counters recomputed from children in one `GROUP BY` query | |

`AULOS_RESTART_POLICY=pause` instead parks in-flight items as `queued, auto_start=false`, for an
operator who wants to inspect before resuming.

Stale temp files: on boot, `*.part`/`*.ytdl` files in `TEMP_DIR` whose owning item no longer
exists are **logged, not deleted**, unless `AULOS_CLEAN_ORPHAN_TEMP=true`. Deleting user data on
boot by default is not acceptable. yt-dlp resumes HTTP downloads from `.part`, so leaving them is
also the faster choice. SC partials are deleted on cancel/restart because the m3u8 token is dead
anyway.

### 8.10 Clear, delete, `CLEAR_COMPLETED_AFTER`

- On a terminal `finished`/`error`, when `CLEAR_COMPLETED_AFTER > 0`, `clear_after = now + N` is
  **persisted**. `ClearScheduler` runs on the 1 Hz `Tick` plus a `sleep_until(min(clear_after))`
  fast path, and queries SQLite — so auto-clear also applies to items that have aged out of the
  in-memory done window. Legacy lost the timer on restart; ours survives.
- `Delete { delete_file }`: `None` means "use `DELETE_FILE_ON_TRASHCAN`". The field is named
  `delete_file`, singular, everywhere — the `EngineCmd` variant, `EngineHandle::actions`, the
  `POST api/v2/items/actions` body key and the `DELETE api/v2/items/{id}?delete_file=` query
  parameter — so the generated schema and the handler cannot disagree. When deleting files we
  remove `filename`, **every** `chapter_files`/`subtitle_files` entry, the SC `.info.json` and
  `.nfo` siblings. Legacy orphaned all of those. Each unlink is best-effort with a WARN; the DB
  row is deleted regardless.
- Deleting a group cancels any active child first, then relies on `ON DELETE CASCADE`.

### 8.11 Watchdogs

One watchdog per running job, driven off the job's `ProgressCell` with `sleep_until` recomputed
when `last_frame_at` advances — no polling, no lock, and it covers web and subscription downloads,
not only Telegram-originated ones (legacy polled every 15 s from inside the bot).

| Timer | Default | Action |
|---|---|---|
| stall | `AULOS_JOB_STALL_SECS` (900) | `Notice{code:"stalled"}` → WS `notice` + Telegram, **once**. Never cancels. |
| hard timeout | `AULOS_JOB_TIMEOUT_SECS` (0 = off) | `Notice{code:"job_timeout"}`, then cancel |
| Telegram stall warning | `TELEGRAM_STALL_TIMEOUT_SECONDS` (180) | the legacy message, once per chat per job |
| Telegram hard warning | `TELEGRAM_HARD_TIMEOUT_SECONDS` (7200) | the legacy message, once per chat per job |

The Telegram thresholds stay **separate** from the global job watchdog. Coupling a global
watchdog's default to a Telegram-specific variable (as one candidate did) is wrong: the bot's
warnings are notifications, the job watchdog is a safety net.

---

## 9. `aulos-provider-ytdlp` and the Python shim

Rust owns: option construction, format selection, process lifecycle, pgid kill, timeouts, progress
normalisation, `outtmpl` pre-resolution, error mapping. Python owns: calling `yt_dlp`. Nothing else.

Why a shim and not a CLI or PyO3: `YTDL_OPTIONS`, `YTDL_OPTIONS_FILE`, presets and per-request
overrides are **Python API option dicts**, not CLI flags. A dict cannot be faithfully rendered as
argv (`postprocessors`, `progress_hooks`, `extractor_args`, `null`-clears-a-key). PyO3 would bind
the binary to one interpreter ABI and destroy the "bump the yt-dlp pin without rebuilding Rust"
property.

### 9.1 Transport

- **stdin**: exactly one JSON object terminated by `\n`, then EOF. The shim never reads again.
- **fd 3**: the protocol channel — newline-delimited JSON, UTF-8, one object per line, flushed per
  line. Rust passes fd 3 as a pipe (via `command-fds`).
- **stdout**: redirected by the shim to a devnull-backed fd for the whole run.
- **stderr**: raw text, drained into `tracing` at DEBUG (`WARN` for lines matching
  `^(ERROR|WARNING)`) with `target = "ytdlp.child"`; the last 8 KiB retained as `tail` for error
  reporting. **Draining is mandatory** — a full pipe deadlocks the child.
- **line cap**: 8 MiB (`info` frames for large playlists are big). Over cap ⇒ kill and report
  `contract`.
- **exit codes**: `0` clean, `2` malformed job, `3` internal shim error, `64` protocol mismatch,
  `130` cancelled via SIGTERM/SIGINT.

**fd 3, not stdout, is the load-bearing decision here — and it is a deliberate override of
BRIEF §9, recorded in §23.1.** The BgUtils POT plugin, `yt-dlp-ejs` and
its `deno` grandchildren are known to print to stdout. Installing a yt-dlp `logger` object stops
*yt-dlp* from writing there but does nothing about a plugin or a grandchild. Putting the protocol
on stdout therefore risks a silent, intermittent, very expensive stream corruption. Legacy could
not have this bug because it used a pickled queue; we must not introduce it. BRIEF §9 says the
frames arrive "on stdout" and the BRIEF wins wherever the two conflict, so this override is
recorded in §23.1 rather than left as an undeclared contradiction between the two documents.

### 9.2 Rust → shim: the job

```json
{
  "v": 1,
  "protocol": 1,
  "job_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
  "mode": "download",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "options": {
    "quiet": true, "verbose": false, "no_color": true,
    "paths": { "home": "/downloads", "temp": "/downloads" },
    "outtmpl": { "default": "%(title)s.%(ext)s",
                 "chapter": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s" },
    "format": "bestvideo[height<=1080][ext=mp4]+bestaudio[ext=m4a]/best[height<=1080][ext=mp4]",
    "socket_timeout": 30,
    "ignore_no_formats_error": true,
    "merge_output_format": "mp4",
    "postprocessors": [{ "key": "FFmpegVideoConvertor", "preferedformat": "mp4" }],
    "impersonate": "chrome"
  },
  "coerce": { "impersonate": "ImpersonateTarget" },
  "policy": {
    "download_type": "video",
    "download_dir": "/downloads",
    "temp_dir": "/downloads",
    "caption_exts": [".vtt", ".srt", ".sbv", ".scc", ".ttml", ".dfxp"],
    "convert_srt_to_txt": false,
    "thumbnail_ext_rewrite": false,
    "emit_progress_every_ms": 100,
    "debug": false
  }
}
```

`mode ∈ "extract" | "download" | "outtmpl" | "selftest"`.

An `extract` job adds:

```json
{ "mode": "extract",
  "extract": { "flat": true, "noplaylist": true, "playlist_end": 50,
               "strict_retry": true, "stream_entries": true, "max_entries": 5000 } }
```

An `outtmpl` job carries `{"mode":"outtmpl","templates":["…"],"info":{…},"prefixes":["playlist"]}`
and returns the evaluated strings — this is how the playlist/channel `outtmpl` pre-resolution
keeps using **yt-dlp's own** `evaluate_outtmpl`, so the full template syntax (defaults
`%(x|Unknown)s`, math `%(playlist_index+100)d`, conditionals `%(playlist_index&{} - |)s`) works
exactly as before.

`policy` carries the small amount of decision-making the shim must do locally, because it needs
the postprocessor `info_dict`, which never crosses the boundary:
- caption filenames not ending in an allowed extension are not reported as artifacts;
- when the caption `format == "txt"`, the shim converts the `.srt` in place (strip cue numbers,
  timestamps, tags), deletes the `.srt`, and reports the `.txt`;
- for `download_type == "thumbnail"`, a `.webm` primary path is rewritten to `.jpg`.

`coerce` names option keys whose string values the shim converts to yt-dlp Python objects. Today
exactly one: `impersonate → ImpersonateTarget.from_str`. Adding another is a shim-only change. An
unknown coercion name is a `contract` error naming the key — never a silent behaviour change.

### 9.3 shim → Rust: frames

Every frame is `{"v":1,"t":<type>,"n":<u64>,"ts":<epoch float>, …}`. `n` starts at 1 and
increments; a gap means a lost line ⇒ `contract`, kill, item errors.

| `t` | Mode | Fields | Rust action |
|---|---|---|---|
| `hello` | both | `protocol`, `yt_dlp`, `python`, `pid`, `plugins:[str]`, `pot:{available,url}` | verify `protocol == 1` else kill + `contract`; record the yt-dlp version for `/version` and `healthz`; `pid` is the pgid leader |
| `resolved` | extract | `root:{type,id,title,webpage_url,extractor,playlist_count,uploader,uploader_id}` | emit a `Playlist` entry shell when `type ∈ {playlist,channel}` |
| `entry` | extract | `index`, `entry:{…flat info subset…}`, `note` | map to `MediaEntry`, stream it out |
| `progress` | download | §9.4 | `sink.progress(...)`; `sink.stage(Downloading)` on the first |
| `pp` | download | §9.5 | artifacts + `sink.stage(Postprocessing)` |
| `artifact` | download | `role`, `path`, `size`, `language`, `label` | `sink.file(...)` |
| `phase` | download | `msg` | sets `ItemView.msg` |
| `info` | extract | `entry` (the full `sanitize_info`'d dict) | stored per §7.5 |
| `log` | both | `level`, `message`, `extractor` | `sink.log(...)`; a `warning` matching a curated allow-list also becomes a `Notice` |
| `result` | both | download: `ok`, `retcode`, `filename`, `size`, `artifacts[]`; extract: `ok`, `count`, `truncated` | build `Outcome` / the entry list |
| `error` | both | `code`, `message`, `retryable`, `extractor`, `fatal`, `traceback` | map to `ProviderError` via §9.6 |
| `bye` | both | `elapsed_ms`, `frames`, `peak_rss_kb` | metrics; expect EOF next |

Ordering guarantees: `hello` first; exactly one of `result` or `error`; `bye` last. Any violation
is a `contract` error.

Example download transcript (abridged):

```jsonl
{"v":1,"t":"hello","n":1,"ts":1772668800.11,"protocol":1,"yt_dlp":"2026.8.30.232658.dev0","python":"3.13.2","pid":4821,"plugins":["bgutil_ytdlp_pot_provider"],"pot":{"available":true,"url":"http://127.0.0.1:4416"}}
{"v":1,"t":"progress","n":3,"ts":1772668801.90,"status":"downloading","filename":null,"tmpfilename":"/downloads/Rick.f616.mp4.part","downloaded_bytes":262144,"total_bytes":null,"total_bytes_estimate":58720256,"fragment_index":null,"fragment_count":null,"speed":1310720.0,"eta":44,"elapsed":0.3,"stream":"video"}
{"v":1,"t":"progress","n":5,"ts":1772668807.02,"status":"finished","filename":"/downloads/Rick.f616.mp4","downloaded_bytes":58720256,"total_bytes":58720256,"speed":null,"eta":null,"elapsed":5.4,"stream":"video"}
{"v":1,"t":"progress","n":6,"ts":1772668807.10,"status":"downloading","tmpfilename":"/downloads/Rick.f140.m4a.part","downloaded_bytes":65536,"total_bytes_estimate":3670016,"speed":655360.0,"eta":5,"elapsed":0.08,"stream":"audio"}
{"v":1,"t":"pp","n":8,"ts":1772668809.50,"postprocessor":"Merger","status":"started","filepath":"/downloads/Rick.mp4"}
{"v":1,"t":"pp","n":10,"ts":1772668812.75,"postprocessor":"MoveFiles","status":"finished","filepath":"/downloads/Rick.mp4","finaldir":null,"subtitles":[],"chapters":[]}
{"v":1,"t":"result","n":12,"ts":1772668812.80,"ok":true,"retcode":0,"filename":"/downloads/Rick.mp4","size":62390272,"artifacts":[{"role":"media","path":"/downloads/Rick.mp4","size":62390272}]}
{"v":1,"t":"bye","n":13,"ts":1772668812.81,"elapsed_ms":12700,"frames":13,"peak_rss_kb":91240}
```

Example extract transcript for a 500-item playlist (streamed, so the client sees the group and the
first children within a few hundred milliseconds):

```jsonl
{"v":1,"t":"hello","n":1,…}
{"v":1,"t":"resolved","n":2,"root":{"type":"playlist","id":"PL9tY0BWXOZFv","title":"Mix - lofi","webpage_url":"https://www.youtube.com/playlist?list=PL9tY0BWXOZFv","extractor":"youtube:tab","playlist_count":500,"uploader":"Chillhop","uploader_id":"@chillhop"}}
{"v":1,"t":"entry","n":3,"index":1,"entry":{"id":"aXbZ1","title":"Track 1","url":"https://www.youtube.com/watch?v=aXbZ1","webpage_url":"https://www.youtube.com/watch?v=aXbZ1","duration":183.0,"live_status":"not_live","uploader":"Chillhop"},"note":null}
… 499 more entry frames, emitted as yt-dlp's lazy playlist yields them …
{"v":1,"t":"result","n":503,"ok":true,"count":500,"truncated":false}
{"v":1,"t":"bye","n":504,"elapsed_ms":4180,"frames":504}
```

### 9.4 `progress` frame ↔ yt-dlp `progress_hooks`

The shim forwards **only** the legacy key allow-list plus two additions, so `YTDL_OPTIONS` cannot
make frames unboundedly large:

| Field | From | Notes |
|---|---|---|
| `status` | `d["status"]` | `downloading` \| `finished` \| `error` |
| `filename`, `tmpfilename` | same | **not sticky**: Rust overwrites its stored `tmpfilename` only when the key is present. Legacy overwrote it with `None` on every frame lacking it, which is why partial-file cleanup usually found nothing to delete. |
| `downloaded_bytes`, `total_bytes`, `total_bytes_estimate` | same | |
| `fragment_index`, `fragment_count` | same | bound the bogus early-HLS estimate |
| `speed` | same | bytes/s float |
| `eta` | same | integer seconds |
| `msg` | same | rare; yt-dlp puts text here on some errors |
| `elapsed` | same | **new**: lets Rust detect stalls without consulting a wall clock |
| `stream` | derived | **new**: `"video"`/`"audio"`/`"fragment"`/`"unknown"` from `info_dict["vcodec"/"acodec"]`, hashed into `ProgressCell.source_tag`. Legacy hashed the filename; being explicit is strictly better and makes the monotonic-clamp reset per merge leg deterministic. |

Shim-side rate limit: at most one `progress` frame per `policy.emit_progress_every_ms` (100)
**per `stream`**, plus every `status != "downloading"` frame unconditionally. That caps the worst
case (thousands of tiny HLS fragments) at ~10 frames/s/job *at the source*, before any Rust
coalescing.

Rust mapping:

```
first `progress` for a job   -> sink.stage(Downloading)
status == "downloading"      -> sink.progress(...)                        (lossy)
status == "finished"         -> sink.progress(percent_hint = 100 for THIS stream); no status change
status == "error"            -> a Notice; only the `error` frame terminates the item
```

`progress.status == "finished"` is **per stream**, not per item. Legacy conflated the two and then
had to correct itself with later frames; here only the `result` frame finishes an item.

### 9.5 `pp` frame ↔ yt-dlp `postprocessor_hooks`

```json
{"v":1,"t":"pp","n":9,"ts":1772668812.7,"postprocessor":"MoveFiles","status":"finished",
 "filepath":"/downloads/S01E02.mkv","finaldir":"/downloads/Show/Season 01",
 "subtitles":[{"path":"/downloads/Show/S01E02.en.srt","language":"en"}],
 "chapters":[{"path":"/downloads/Show/S01E02 - 01 - Intro.mkv","label":"Intro"}]}
```

| `postprocessor` | `status` | Behaviour |
|---|---|---|
| any | `started` | `sink.stage(Postprocessing)` + `msg = "<Postprocessor>…"` — the state legacy could not express |
| any | `processing` | throttled to 1 s; stays `Postprocessing` |
| `MoveFiles` | `finished` | `filepath = join(info_dict["__finaldir"], basename(filepath))` when `__finaldir` is present, else `filepath`; for `download_type == "captions"` also enumerate `info_dict["requested_subtitles"][*]["filepath"]` into `subtitles[]` |
| `SplitChapters` | `finished` | one `chapters[]` entry per `info_dict["chapters"][*]["filepath"]`, de-duplicated by path |
| `FFmpegExtractAudio`/`FFmpegVideoConvertor`/`Merger` | `finished` | primary artifact candidate (last one wins) |
| `EmbedThumbnail`, `FFmpegMetadata` | `finished` | `msg` update only |
| `Exec` | `finished`/`error` | on error, the exec return code goes into `message` and the item's postprocessing fails |

### 9.6 Error taxonomy

The shim classifies by exception class first, then by a small **ordered** regex table over the
message. The table lives in the shim so it versions with the yt-dlp pin; Rust maps codes to
`ProviderError` mechanically and never regex-matches prose itself.

| Shim `code` | Detected from | `ProviderError` / `ErrorCode` |
|---|---|---|
| `canceled` | `KeyboardInterrupt` after SIGTERM | `Canceled` |
| `unsupported_url` | `UnsupportedError` | `Unsupported` |
| `auth_required` | `ExtractorError` + `/sign in\|log in\|members-only\|private video/i` | `AuthRequired` |
| `geo_restricted` | `GeoRestrictedError` | `GeoRestricted` |
| `unavailable` | `/video unavailable\|removed by the uploader\|account.*terminated/i` | `Unavailable` |
| `not_yet_live` | `live_status == "is_upcoming"` or `/premieres in/i` | `NotYetLive` |
| `no_format` | `/requested format is not available/i` | `NoFormat` |
| `bot_check` | `/confirm you'?re not a bot\|failed to extract any player response/i` | `BotCheck` |
| `network` | `DownloadError` wrapping `URLError`/`TimeoutError`, `/HTTP Error 5\d\d/` | `Network` (retryable) |
| `throttled` | `/HTTP Error 429\|too many requests/i` | `Throttled` (retryable after 60 s) |
| `postprocessing_failed` | `PostProcessingError` | `Postprocessing` |
| `disk_full` | `OSError` errno 28 | `Disk` |
| `timeout` | the shim's own watchdog | `Timeout` |
| `bad_job` | request validation | `Contract` |
| `internal` | anything else | `Other` |

Messages are cleaned once, here: a leading `"ERROR: "` is stripped, `\r`/ANSI removed, and the
result is trimmed to 512 characters.

### 9.7 Rust side

```rust
pub struct RunnerHandle { child: Child, pgid: Pid, proto: FramedRead<Fd3, JsonLines>,
                          stderr: StderrRing }
pub async fn run_job(job: Job, sink: &ProgressSink, cancel: &CancellationToken)
    -> Result<RunnerOutcome, ProviderError>;
```

- Spawn: `Command::new("python3").arg(RUNNER_PATH)`, `.process_group(0)`, `stdin(piped)`,
  `stdout(null)`, `stderr(piped)`, fd 3 = pipe, env inherited plus `PYTHONUNBUFFERED=1`,
  `PYTHONDONTWRITEBYTECODE=1`.
- `select!` over: fd-3 lines, stderr lines, `child.wait()`, `cancel.cancelled()`, the stall timer
  and the hard timer.
- Kill: `killpg(-pgid, SIGTERM)`; still alive after `AULOS_KILL_GRACE_MS` ⇒ `killpg(-pgid,
  SIGKILL)`. A `Drop` guard does the same, so a panic cannot leak a child. This is the fix for
  legacy's `proc.kill()` orphaning ffmpeg grandchildren.
- `--replay <transcript.jsonl>` mode: the Rust reader can be driven from a recorded transcript with
  no Python and no network, which is how every frame type, every ordering violation and every error
  class is unit-tested. This is the cheapest possible insurance against nightly yt-dlp churn.

### 9.8 Format and option construction

`formats.rs` and `opts.rs` are a **literal port** of `dl_formats.py`, table-driven, with the
Appendix A §6 tables as `const` arrays. A property test asserts that every
(`download_type`, `codec`, `format`, `quality`) tuple in the legacy allow-list produces the exact
legacy selector string, compared against
`crates/aulos-provider-ytdlp/tests/golden/formats.json` — generated once from the legacy Python
and checked in. A diff fails CI.

Preserved quirks (Appendix B, K1): `format.startswith("custom:")` checked **first**; the `ios`
selector chain verbatim; `best_remux → "bestvideo+bestaudio/best"` with `opts.pop("format")`,
`merge_output_format="mp4"` and `FFmpegVideoConvertor`; `quality == "worst"` producing no `worst*`
selector; the audio postprocessor chain with the `writethumbnail` guard and the **string**
`preferredquality`; the per-mode caption `subtitleslangs` ordering; `null` in a preset clearing a
key; presets applied in request order; MeTube's own extraction keys applied **after** user options
so a preset cannot break `extract_flat`/`noplaylist`.

The one deliberate change: the late `Exec` postprocessor
(`python3 /app/app/audio_sync_fix.py %(filepath)q`) is replaced by the in-process `audio_sync`
hook (§13.3).

---

## 10. `aulos-provider-sc` (StreamingCommunity)

### 10.1 HTTP client decision and fallback

| Option | Verdict |
|---|---|
| **`wreq`** (the maintained rename of `rquest`; BoringSSL, Chrome JA3/JA4 + HTTP/2 fingerprint) | **Primary**, behind cargo feature `sc-impersonate` (default on for `x86_64-unknown-linux-gnu`). Legacy relies on `curl_cffi impersonate="chrome"`; the vixcloud/Cloudflare fronting is fingerprint-sensitive and losing it silently breaks every SC download. |
| **`wreq-util`** (the profile tables) | **Required with `wreq`.** `wreq` ships only `Emulation`/`EmulationBuilder`; the named browser profiles — the actual cipher/curve/sigalg lists, GREASE, extension permutation and Chrome's HTTP/2 SETTINGS and pseudo-header *order* — live in this separate crate. A hand-built `TlsOptions` gets a Chrome-*shaped* ClientHello, not a byte-exact JA3/JA4. `http::impersonate` uses `Profile::Chrome131`, which must stay in step with the `USER_AGENT` and `sec-ch-ua` this crate sends. |
| plain **`reqwest`** (rustls) with hand-set Chrome headers | **Compiled-in fallback**, always present. Selected by `AULOS_SC_HTTP=plain`, and the only client used in the test matrix (no BoringSSL in CI). |
| the pinned versions | `wreq = 6.0.0-rc.31` and `wreq-util = 3.0.0-rc.14`. Both 6.x/3.x lines are still release candidates, and Cargo will not match a prerelease from a bare `"6"`/`"3"` requirement. Verified to compile and link on the pinned 1.95 toolchain (WP-08), so the BRIEF's plain-`reqwest` escape hatch is **not** taken and `sc-impersonate` stays default-on. The builder stage and the `clippy`/`test`/`release` CI jobs must keep their `cmake clang libclang-dev` installs for the vendored BoringSSL and bindgen. |
| shipping `curl-impersonate` in the image | **Rejected.** A third code path for the least likely case. If Cloudflare escalates to a JS challenge, TLS impersonation is not enough anyway and the answer is a `command` plugin, which is exactly what the plugin system is for. |

Both sit behind one trait, so the scraping logic is client-agnostic and unit-testable against
`wiremock`:

```rust
#[async_trait]
pub trait ScHttp: Send + Sync {
    async fn get(&self, req: ScReq) -> Result<ScRes, ScError>;
    fn impersonating(&self) -> bool;
}
```

`AULOS_SC_HTTP ∈ auto | impersonate | plain` (default `auto` = impersonate when compiled in). If
the feature is off for the target, the provider logs exactly one WARN at boot naming the
degradation and registers as `Ready` with `impersonating: false` (visible in `healthz` and
`GET api/v2/providers`) — it does not fail to start. If `impersonate` is explicitly requested and
unavailable, the provider registers `Degraded("sc-impersonate not compiled in")`.

### 10.2 Scrape pipeline

| Step | Request | Extract | Cache |
|---|---|---|---|
| S1 site version | `GET {base}/it` | `div#app[data-page]` → JSON → `.version` (the Inertia asset version) | per `base`, TTL 30 min, single-flight |
| S2 Inertia page | `GET {base}{path}` with `x-inertia: true`, `x-inertia-version: <S1>`, `Accept: application/json` | JSON `props` | title/season pages 60 s |
| S3 embed page | `GET props.embedUrl` | the first `<iframe src=…>` (vixcloud) | never |
| S4 stream params | `GET <iframe src>` | scan `<script>` bodies containing `masterPlaylist`: `'token': '<t>'`, `'expires': '<digits>'`, `window.streams = [...]` (JSON — pick `active == true`, else the first, take `url`, unescape `\/`), fallback `url: '<u>'` inside `masterPlaylist`; `window.canPlayFHD = true` ⇒ append `h=1`; **preserve existing query params** (`ub`, `ab`, `b`); append `token` and `expires`; reassemble | never (tokens expire in minutes) |

On a `403`/`404`/`409` from S2 the cached version is invalidated and S1 re-run **once** — the
Inertia version rotates on every site deploy, and legacy simply failed. HTML parsing uses
`scraper` (html5ever) for the two `find` operations and `regex` for the script scraping, matching
BeautifulSoup's first-match-in-document-order semantics exactly.

`AULOS_SC_EXTRA_HOSTS` (comma list of host substrings) lets an operator add a mirror without a
release. `matches()` is **host-and-path**, reproducing legacy's split between detection and
dispatch:

```
host_ok = hostname.to_lowercase().contains("streamingcommunity")
          || AULOS_SC_EXTRA_HOSTS.iter().any(|h| hostname.to_lowercase().contains(h))
path_ok = path.contains("/watch/") || path.contains("/titles/") || path.contains("/season-")

match (host_ok, path_ok) {
    (true, true)  => Match::Strong(200),
    (true, false) => Match::No,     // legacy: extract() logged "Unsupported URL format" and
                                    // returned None, so __extract_info fell through to yt-dlp
    _             => Match::No,
}
```

`can_extract` (host-only) is still what legacy *detected* with, but `extract()` dispatched on the
path and returned `None` for anything else, at which point `__extract_info` retried the URL through
yt-dlp (legacy spec §9.1, `app/extractors/streamingcommunity.py:387-397`). A host-only
`Strong(200)` would make `https://streamingcommunity.example/search?q=x` — or a browse page, or a
mirror's homepage — terminate with `unsupported_url` where the old server would have handed it to
yt-dlp. Returning `Match::No` for a non-dispatchable path is what keeps that behaviour, and the
runner-up retry of §6.4 covers the remaining legacy case where the scrape itself yielded nothing.
Both are recorded in Appendix A.9 and Appendix B C46.

### 10.3 Entry shapes — semantically preserved, explicitly translated

```
media_id = "sc_<title_id>"                     (movie)
         | "sc_<title_id>_<episode_id>"        (episode)
title    = "<Name>"                            (movie)
         | "<Name> S01E02"                     (episode with no episode name)
         | "<Name> S01E02 - <ep name>"         (episode with a name)
url      = the /watch/<id>[?e=<ep>] page url
state    = { "base_url": "...", "title_id": 1234, "episode_id": 5678,
             "needs_m3u8_extraction": true, "season_number": 1, "episode_number": 2,
             "episode": "<ep name>", "series": "<title name>"|null, "ext": "mp4",
             "extractor": "streamingcommunity", "extractor_key": "StreamingCommunity",
             "legacy": { …any keys carried over from an imported legacy entry… } }
```

`media_id`, `title` and `url` are **byte-identical** to legacy (spec §9.3). The `state` object is
**not**: it renames legacy's `_sc_base_url`/`_sc_needs_m3u8_extraction` and splits out
`title_id`/`episode_id`, which legacy only ever had embedded in `id`. Two consequences, both
handled explicitly rather than assumed away:

1. **Imported rows are translated, not copied.** §7.6.3a is the normative migration rule
   (including deriving `title_id`/`episode_id` from the legacy `id` or the watch URL, and carrying
   everything else through as `state.legacy`). Nothing at runtime reads a `_sc_*` key.
2. **The on-disk `.info.json` keeps the legacy flat shape.** The DB blob and the sidecar file are
   two different surfaces with two different consumers. Users' own `Exec` postprocessors and the
   legacy `jellyfin_nfo_generator.py` CLI read the sidecar, and both expect the legacy keys, so
   §10.5 writes it in legacy shape — flat, with `_sc_base_url` and `_sc_needs_m3u8_extraction` —
   and an `insta` snapshot test pins it against a file captured from the Python server. Existing
   `.nfo` files and existing sidecars therefore keep working unchanged.

The m3u8 URL resolved during extraction is still **discarded** — only the watch URL is persisted,
and the m3u8 is re-extracted just in time at download time, because tokens expire fast.

### 10.4 Season resolution: 2 requests, not ~60

This is the one substantive improvement in the SC port. Verified against
`app/extractors/streamingcommunity.py:129-187`: `extract_episode` performs
watch → embed → iframe → `get_m3u8_from_embed` **per episode**, purely as a validity probe, and
then throws the m3u8 away, because only the watch URL is persisted. A 20-episode season is ~60
HTTP round trips for information that is already present in `props.loadedSeason.episodes`.

`season.rs` therefore performs **zero** embed/stream requests during resolution:
`GET /it/titles/{id}-{slug}` for the name, `GET /it/titles/{id}-{slug}/season-{n}` for the episode
list, then it synthesises one `MediaEntry` per episode straight from the JSON. Two requests. The
just-in-time re-extraction at download time is unchanged, so nothing about the produced file
changes. `extract_title` for a TV title resolves each season's JSON (one request each) and emits
one flattened playlist, exactly as legacy did.

`AULOS_SC_META_CONCURRENCY` (4) bounds the per-season fetches for a multi-season title.

### 10.5 Download engines

`download()` re-runs S1–S4 with a fresh cookie jar (`jit.rs`), then:

```
out_path = out_dir / f"{sanitize(title)}.mp4"     # [<>:"/\|?*] -> "_", trailing ". " trimmed
also write out_dir / f"{sanitize(title)}.info.json"   in the LEGACY FLAT SHAPE (§10.3 note 2):
     id, title, url, webpage_url, ext, _type, extractor, extractor_key,
     season_number, episode_number, episode, series,
     _sc_needs_m3u8_extraction, _sc_base_url, plus state.legacy spread at the top level
```

- The pure-debug `GET` of the m3u8 that legacy performed on every download is **removed**.
- `SC_USE_FFMPEG=true` ⇒ `msg = "Starting ffmpeg download..."` and the ffmpeg path.
  Otherwise `msg = "Starting N_m3u8DL-RE download..."` and:

```
N_m3u8DL-RE <m3u8> --save-dir <out_dir> --save-name <safe_title>
  --tmp-dir <tmp_dir or out_dir/.tmp> --thread-count <SC_THREAD_COUNT>
  --auto-select --del-after-done --no-log
  --mux-after-done format=mp4:muxer=ffmpeg --log-level INFO
  -H "User-Agent: …" -H "Referer: …" -H "Origin: …" [-H "Cookie: …"]
```

  On a non-zero exit: WARN, `msg = "N_m3u8DL-RE failed, retrying with ffmpeg..."`, partial
  cleanup, then the ffmpeg path (which does report errors). Argv is byte-identical to legacy.
- ffmpeg path: a CRLF-joined header blob, `ffprobe -show_entries format=duration` (30 s timeout,
  failure tolerated), then
  `ffmpeg -y -headers <hdrs> -i <m3u8> -c copy -bsf:a aac_adtstoasc -progress pipe:1 <out>`;
  progress from `out_time_ms`/`total_size`/`speed=<x>x`, emitted at most every 0.5 s.
- **Gapless fallback mux** (`mux.rs`), preserved exactly: if the expected mp4 is missing but a
  segment directory exists, collect every `.m4s/.ts/.mp4/.m4a/.aac` under it, sort by
  **natural (numeric-aware) filename order** — explicitly *not* mtime, because parallel downloads
  scramble mtimes — binary-concatenate in 1 MiB chunks into `_merged.ts`, then
  `ffmpeg -y -i _merged.ts -map 0 -c copy -bsf:a aac_adtstoasc -movflags +faststart <out>` with a
  600 s timeout. ffmpeg's `concat` demuxer must **never** be used: it pads each segment to its
  container duration, injecting a ~64 ms A/V gap and a dropped frame per join.
  The natural comparator is ~30 hand-written lines with a `proptest`, not the effectively
  unmaintained `natord` crate.
- Progress parsing (`progress.rs`), ported with its unit tests: strip ANSI/OSC, convert `\r` to
  `\n`, then take the **last** match of each pattern (Spectre.Console repaints several frames per
  read and the first is usually `0/100 0.00%`):

| Pattern | Fields |
|---|---|
| `(\d+)/(\d+)\s+([\d.]+)%` | `fragment_index` = segment index, `fragment_count` = segment count |
| `([\d.]+)\s*(KB\|MB\|GB)\s*/\s*([\d.]+)\s*(KB\|MB\|GB)` | real byte sizes, 1024-based |
| `([\d.]+)\s*(KB\|MB\|GB)ps` | `speed` in bytes/s |
| `(\d{2}):(\d{2}):(\d{2})(?=\s\|$)` | `eta` in seconds |

  Δ vs legacy: segment counts go to `fragment_index`/`fragment_count` instead of being *abused* as
  `downloaded_bytes`/`total_bytes`, and the byte fields stay `null` until real sizes are parsed.
  The byte counters stop lying and the percent stays monotonic.
- Partial cleanup removes the partial mp4 and rmtree's `<tmp>/<safe_title>`,
  `<tmp>/<safe_title>.tmp`, `<out_dir>/<safe_title>`.

`own_slots() = Some(SC_MAX_CONCURRENT_DOWNLOADS)`, acquired **instead of** the global slot —
exactly legacy's `sc_semaphore` positioning.

**Output naming is deliberately kept**: `<download_dir>/<sanitised title>.mp4` plus
`.info.json`, ignoring `OUTPUT_TEMPLATE*` entirely. Existing Jellyfin libraries depend on those
paths. `AULOS_SC_USE_OUTPUT_TEMPLATE=true` opts in to template-based naming for anyone who wants
`<series>/Season 01/…`; default `false`.

---

## 11. The v1 compatibility shim (`aulos-api::v1`)

Purpose: the **currently shipped** iOS build (`metube_ios` @ `8622a2f`), the README bookmarklet
and the iOS Shortcut must keep working unchanged through cutover. The shim is a pure translation
layer over the v2 core: it builds `DownloadRequest`s and `EngineCmd`s and projects `ItemView` down
to the legacy shape. It owns **no state**. Mounted iff `AULOS_V1_ENABLED=true` (default), so it
can be switched off after the v2 client ships, and its eventual removal is a one-line change.

### 11.1 Route parity

| Legacy route | Provided | Deviation |
|---|---|---|
| `POST <p>add` | yes | HTTP 200 kept; `Content-Type` is now `application/json` (was `text/plain`). **Resolution failures are still reported in the body** via the bounded synchronous pre-resolve of §11.2 (`AULOS_V1_ADD_RESOLVE_WAIT_MS`, default 10 000 ms) — without it, async add would silently turn every bad URL into `{"status":"ok"}` and the shipped share extension's only failure path would never fire (Appendix B C45, risk R24). Δ: on a resolution failure the item **remains** in the queue as `error` (legacy created no record at all), so it is visible to a v2 client and appears once in v1 `done[]` |
| `GET <p>presets` | yes | none |
| `POST <p>cancel-add` | yes | body ignored, exactly as legacy. The shim sends `CancelResolve { scope: CancelScope::All }` (§8.1) — legacy's `cancel_add()` also took no argument and simply bumped a process-global counter, so there is no generation for a v1 client to send and none for the shim to invent. Δ: it now actually aborts in-flight resolution and marks not-yet-created children cancelled, where legacy only checked between entries |
| `POST <p>subscribe` | yes | `{"status":"ok","subscription":{…13 keys…}}` unchanged |
| `GET <p>subscriptions` | yes | exactly the legacy 13-key projection |
| `POST <p>subscriptions/update` | yes | a bad `enabled` is now **400**, not a leaked 500 |
| `POST <p>subscriptions/delete` | yes | `[]` still 400 |
| `POST <p>subscriptions/check` | yes | returns `200 {"status":"ok","job_id":…}` **immediately** instead of blocking for minutes |
| `POST <p>delete` | yes | `ids` may be URLs, legacy media ids **or** ULIDs (§11.3) |
| `POST <p>start` | yes | `ids: null` is 400, not 500; terminal items are retried |
| `POST <p>upload-cookies` | yes | same messages, same cap — **1 000 000 bytes**, decimal, exactly as legacy (`Cookie file too large (max 1MB)`); §16.6 |
| `POST <p>delete-cookies`, `GET <p>cookie-status` | yes | same messages |
| `GET <p>history` | yes | all three keys always present (§11.4) |
| `GET <p>version` | yes | additive `url_prefix`, `protocol` |
| `GET <p>robots.txt` | yes | identical |
| `GET /` when `URL_PREFIX != "/"` | yes | `302` to `URL_PREFIX` |
| `GET <p>download/*`, `<p>audio_download/*` | yes | `Range` support added |
| `OPTIONS` on all of the above | yes | `{"status":"ok"}` + CORS; v2 routes also send `Access-Control-Allow-Methods` |
| `GET <p>` (Angular index + `metube_theme` cookie) | **no** | returns a tiny JSON identity document; no HTML, no cookie |
| `GET <p>socket.io/*` | **no** | `501` with `{"error":{"code":"socketio_removed","message":"Socket.IO is not supported; use <prefix>ws (protocol v2) or GET <prefix>api/v2/state"}}` — deliberate, so a stale client fails loudly instead of hanging on a handshake. `socketio_removed` is a member of the §5 `ErrorCode` enum and 501 is a documented status, so the code appears in the `print-schema` snapshot and in the `aulos_http_errors_total{code}` label set |
| `POST <p>cancel-add` (v2 equivalent) | yes | the same handler backs `POST api/v2/downloads/cancel-resolve`, which additionally accepts `{"generation": n}` so a v2-only client can abort one specific 500-item add (PROTOCOL §4.7) |

### 11.2 `POST <p>add` translation

The shim runs legacy migration and current-schema parsing in the legacy order:

1. Body must be a JSON object, else 400 with the legacy reason string as `error.message`
   (`Invalid JSON request body` / `JSON request body must be an object`).
2. When `download_type` is absent, apply `_migrate_legacy_request` — the exact table:

| legacy `format` | legacy `quality` | → `download_type` | `codec` | `format` | `quality` |
|---|---|---|---|---|---|
| `m4a\|mp3\|opus\|wav\|flac` | any | `audio` | `auto` | same | unchanged |
| `thumbnail` | any | `thumbnail` | `auto` | `jpg` | `best` |
| `captions` | any | `captions` | `auto` | `subtitle_format` or `srt` | `best` |
| other | `best_ios` | `video` | `video_codec` | `ios` | `best` |
| other | `audio` | `audio` | `auto` | `m4a` | `best` |
| other | else | `video` | `video_codec` | legacy `format` | legacy `quality` |

3. Validate with the legacy hard-coded matrix **first** (so 400 strings stay byte-identical), then
   with the catalog.
4. `auto_start`: legacy compared `is True`, so the JSON string `"true"` silently routed to
   *pending*. The shim accepts real booleans and the strings `true/false/1/0/on/off`
   (case-insensitive). The iOS app sends a real boolean, a Shortcut sends a string, and both
   meant "start it".
5. A duplicate URL yields `200 {"status":"ok"}` with no new item, matching legacy's silent skip.
6. **Bounded synchronous pre-resolve, then the response.** The shim submits `EngineCmd::Add`, then
   awaits `EngineCmd::WaitResolved { ids }` under a `AULOS_V1_ADD_RESOLVE_WAIT_MS` (default
   `10000`) timeout before answering:

   | Outcome within the window | Response |
   |---|---|
   | every id left `resolving` successfully | `200 {"status":"ok","ids":[…]}` |
   | one or more ids ended in `error` | `200` with `{"status":"error","msg":"<the failing items' messages, joined with a comma and a space>"}` — legacy's own joiner for a playlist's child errors (`', '.join`) |
   | the window expires first | `200 {"status":"ok","ids":[…]}`, one WARN, `aulos_v1_add_resolve_total{outcome="timeout"}` |
   | `AULOS_V1_ADD_RESOLVE_WAIT_MS=0` | `200 {"status":"ok","ids":[…]}` immediately — pure async, and error reporting for v1 is knowingly given up |

   This exists because **async add (BRIEF §5) otherwise removes the shipped client's only failure
   path.** `AddResultClassifier` (ios-client-reference §5 step 6) decides success purely by parsing
   this body, and the "Couldn't add to Aulos" notification is the only signal the share extension
   can produce. With a 200 emitted before extraction, an unsupported URL, an
   `Invalid/empty data was given.`, a geo-block or a `Unsupported resource "<etype>"` would all read
   as success and the item would fail silently in a queue the user may not open for hours. The wait
   is not a regression in latency either: legacy's `/add` blocked for the *whole* extraction with no
   ceiling, so this is strictly faster and now bounded. It applies to the **v1 route only** — `POST
   api/v2/downloads` is unconditionally async, because a v2 client watches the item's status.

   Validation failures (steps 1–5) still answer **before** any of this, as a `400` carrying the
   legacy reason string; only *resolution* failures use the table above.

7. The response is always `200` for the non-validation cases: success `{"status":"ok"}` (plus an
   additive `"ids":[…]`, which old clients ignore); business error
   `{"status":"error","msg":"<text>"}` — which is exactly the shape the iOS `AddResultClassifier`
   already handles.

#### 11.2.1 Request-parsing leniencies the shim must reproduce

`parse_download_options` was permissive in five ways beyond `auto_start`, and a Shortcut, the README
bookmarklet or a hand-rolled `curl` may depend on any of them. All five are **v1-shim only**; v2 is
strict and answers `400 validation_failed`.

| Legacy leniency | Source | Shim behaviour |
|---|---|---|
| the singular key `ytdl_options_preset` | legacy spec §2.2; `app/main.py:282-296` | accepted as an alias of `ytdl_options_presets` |
| `ytdl_options_presets` given as a bare **string** | same | wrapped into a one-element list |
| `ytdl_options_overrides` given as a **JSON string** rather than an object | same | parsed once; invalid ⇒ 400 `ytdl_options_overrides must be valid JSON`; parses to a non-object ⇒ 400 `ytdl_options_overrides must be a JSON object` |
| `playlist_item_limit` given as a numeric **string** (`"5"`, `" 5 "`) | same (Python `int()`) | parsed with `trim()` then `i64::from_str`; failure ⇒ 400 `playlist_item_limit must be an integer` |
| `check_interval_minutes` given as a numeric string (`/subscribe`) | same | same treatment; failure ⇒ 400 `check_interval_minutes must be an integer`, `< 1` ⇒ 400 `check_interval_minutes must be at least 1` |

Anything else of the wrong type is a 400 with the legacy string, as before.

### 11.3 Id resolution for `delete` / `start`

Legacy keyed everything by `url`; the shipped iOS app sends `item.url ?? item.id`, and
`clearCompleted` sends **only** urls. Each token in `ids` is therefore resolved through a ladder:

```
resolve(token) -> Vec<ItemId>
  1. token parses as a ULID and that item exists      -> [that id]
  2. exact match on items.url                          -> all matching ids
  3. exact match on items.media_id                     -> all matching ids
  4. otherwise empty  (recorded in `skipped`, logged at DEBUG)
```

Ties (the same URL added twice) resolve to **all** matches — what a legacy user expects from a
URL-keyed API. One query:
`SELECT id FROM items WHERE id IN (…) OR url IN (…) OR media_id IN (…)`.

`where` semantics:

| `where` | v2 action |
|---|---|
| `"queue"` | `Cancel` on non-terminal, then `Delete` — legacy dropped the row entirely, so the item disappears and the app's optimistic local removal stays correct |
| `"done"` | `Delete` with `delete_file = DELETE_FILE_ON_TRASHCAN` |
| other / missing | `400` (legacy sent a reasonless 400) |

`POST <p>start` maps `queued(auto_start=false)` → `Start`, and `error`/`canceled` → `Retry`.
Legacy only handled the pending case; retry-on-failed is additive, closes iOS pain point #24
without a client change, and is reachable today from a Shortcut.

### 11.4 `GET <p>history` projection

```json
{ "queue":   [ /* resolving | preparing | downloading | postprocessing | queued(auto_start=true) */ ],
  "pending": [ /* queued && !auto_start  — including pre-download-problem items, §8.4 */ ],
  "done":    [ /* finished | error */ ] }
```

**Where the three arrays come from, and how much they cost.** This matters because legacy's
`/history` returned the *entire* `completed` collection — every completed record ever, unless
cleared — and the shipped iOS build renders all of it in its "Completed" section. Silently serving
a 500-row window instead would drop 3 700 rows out of the client at cutover.

| Array | Source | Bound |
|---|---|---|
| `queue`, `pending` | `StateView::load()` — the published in-memory snapshot (§15.2), which holds **every** non-terminal record by construction | none needed |
| `done` | `Store::v1_done(limit)` (§7.1): `SELECT … WHERE status IN ('finished','error') ORDER BY ord ASC, id ASC`, on a read-pool connection, using the `(status, ord)` index | `AULOS_V1_HISTORY_MAX`, default **`0` = unlimited**, i.e. full legacy fidelity |

So v1 `done[]` is deliberately **not** the `AULOS_MEM_DONE_ITEMS` window that v2's `snapshot.done`
uses. v2 is honest about being windowed (`done_total`, `truncated.done`, paged `api/v2/items`);
v1 has no vocabulary for any of that — no paging, no `truncated`, and `HistoryResponse` declares
all three arrays non-optional — so the only faithful answer is the whole set. This is the one place
where v1 is *more* expensive than v2: a 4 000-row `/history` is roughly 3 MB of JSON and ~15 ms of
query on the read pool, on every connect and every pull-to-refresh of the shipped client. Three
mitigations, none of which change the payload: the query never touches the writer, the projection
omits `entry` (the single biggest contributor, C22), and an operator with a huge history can set
`AULOS_V1_HISTORY_MAX` to cap it — at which point the **oldest** rows are dropped, keeping the most
recent, and a WARN naming the cap is logged once per hour. Appendix B row C42 records the knob and
its default.

| Rule | Reason |
|---|---|
| Groups (`kind == "group"`) are **omitted**; only children appear. | Legacy had no group concept. A group row would render in the old client as an "In Progress" item that never progresses — worse than nothing. |
| An item with a pre-download problem (an upcoming livestream, or an entry-level `msg`) appears in `pending[]` with `status: "pending"` and a populated `error` string. | Byte-compatible with legacy, which produced exactly that (spec §5.4). §8.4 is why such an item is `queued(auto_start=false)` and not `error`: as `error` it would land in `done[]` and show up in the shipped client's **Failed** section. |
| `canceled` items are **omitted** from all three arrays. | The shipped `DownloadStatus` has no `canceled` case and maps unknown → `.pending`, so a cancelled row would be stuck in "In Progress" forever. Legacy made cancels vanish; this is faithful. v2 clients see them. |
| Order within each array is `ord` ascending. | Deterministic. The old client re-sorts by title anyway. |
| All three keys are **always** present, even when empty. | `HistoryResponse` declares all three non-optional; a missing key fails the whole decode. |

Per-item field projection:

| Legacy field | Value |
|---|---|
| `id` | `media_id` when present, else the ULID. Legacy `id` was the yt-dlp video id, optionally `"<prefix>.<id>"`; the prefixing is reproduced. |
| `title` | `title`, same prefixing |
| `url` | `url` |
| `status` | §11.5 |
| `percent` | `percent` (always a number; legacy was sometimes `null`, which the client already clamps) |
| `speed`, `eta` | f64/null, integer seconds/null |
| `downloaded_bytes`, `total_bytes`, `total_bytes_estimate`, `fragment_index`, `fragment_count` | as-is |
| `msg` | `msg` for active items, `error.message` for terminal errors (legacy overloaded `msg`) |
| `error` | `error.message` as a plain string (legacy was a string). Also populated for a `queued` pre-download-problem item, which is exactly what legacy did for an upcoming livestream (§8.4) |
| `filename`, `size` | as-is, `null` when unknown — the key is now always present |
| `quality`, `format`, `codec`, `download_type` | from `selection` |
| `folder`, `custom_name_prefix`, `playlist_item_limit`, `split_by_chapters`, `chapter_template`, `subtitle_language`, `subtitle_mode`, `ytdl_options_presets`, `ytdl_options_overrides` | from `request` |
| `timestamp` | `created_at * 1_000_000` (legacy used `time.time_ns()`) |
| `subtitle_files`, `chapter_files` | as-is |
| `entry` | **omitted.** It was the full yt-dlp info dict, the single biggest payload contributor; the iOS client never reads it and no known consumer breaks. |

`DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` are emitted as
**strings** by the v1 shim (legacy never int-coerced them) and as **numbers** by v2.

### 11.5 Status mapping v2 → v1

| v2 status | v1 `status` | Rationale |
|---|---|---|
| `queued` (`auto_start=false`) | `pending` | appears in `pending[]` |
| `queued` (`auto_start=true`) | `pending` | appears in `queue[]`, exactly like legacy's queued-but-not-started |
| `resolving` | `pending` | legacy had no such state; the item existed only after extraction |
| `preparing` | `preparing` | 1:1 |
| `downloading` | `downloading` | 1:1 |
| `postprocessing` | `downloading` | legacy showed a frozen `downloading` during ffmpeg; identical UX, and `msg` carries the phase |
| `finished` | `finished` | 1:1 |
| `error` | `error` | 1:1 |
| `canceled` | *(item omitted)* | §11.4 |

### 11.6 The one accepted regression, and CORS parity

The shim does not provide Socket.IO (`all`, `updated`, `added`, `completed`, `canceled`,
`cleared`, `formats`, `configuration`, `custom_dirs`, `ytdl_options_changed`, `subscriptions_all`,
`subscription_*`). The shipped iOS build degrades to: **no live updates, but `GET /history` on
connect and on pull-to-refresh still works** — which is what it already does, because it distrusts
the socket and fetches `/history` on every connect. This is the single user-visible regression
during the overlap window, and it is why the runbook ships the v2 iOS build **first**, in the same
session (§19.3). A `socketioxide` shim (~400 lines, one more dependency, reproducing the
double-encoded payloads) was considered and **rejected**: BRIEF §8 says Socket.IO is not provided,
and the mitigation is scheduling, not code.

CORS: for v1 routes, exactly legacy behaviour — if `Origin` is present and (`*` is in
`CORS_ALLOWED_ORIGINS` or the origin is listed), set `Access-Control-Allow-Origin: <Origin>` and
`Access-Control-Allow-Headers: Content-Type`; no methods header, no credentials. v2 routes
additionally send `Access-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS`,
`Vary: Origin` and `Access-Control-Max-Age: 600`. `Access-Control-Allow-Credentials` is never sent
(legacy did not, and the iOS client sets `Cookie` manually).

### 11.7 Legacy strings that must be byte-identical

Appendix A marks these **K**, and WP-15's golden replay compares against them, so they are written
down here rather than left implicit in the Python source. Every string below is produced in exactly
one place in the Rust tree, and a unit test asserts each one literally.

**Add / validation** (`app/main.py`, legacy spec §2.2 / §5.4):

| Condition | String |
|---|---|
| body is not JSON | `Invalid JSON request body` |
| body is not an object | `JSON request body must be an object` |
| missing required fields | `missing 'url', 'download_type', or 'quality'` |
| bad `download_type` | `download_type must be one of ['video', 'audio', 'captions', 'thumbnail']` (the Python list repr, reproduced verbatim) |
| bad `playlist_item_limit` | `playlist_item_limit must be an integer` |
| bad `ytdl_options_overrides` | `ytdl_options_overrides must be valid JSON` · `ytdl_options_overrides must be a JSON object` · `ytdl_options_overrides are disabled` |
| empty / unusable resolved entry | `Invalid/empty data was given.` (§8.4) |
| unmappable `_type` | `Unsupported resource "<etype>"` (§8.4, §9.6) |
| upcoming livestream | `Live stream is scheduled to start at {ts:%Y-%m-%d %H:%M:%S %z}` (§8.4) |
| folder rejected | `A folder for the download was specified but CUSTOM_DIRS is not true in the configuration.` · `Folder "X" must resolve inside the base download directory "Y"` · `Folder "X" for download does not exist inside base directory "Y", and CREATE_CUSTOM_DIRS is not true in the configuration.` |

**Subscriptions** (legacy spec §2.1 / §7.4):

| Condition | String |
|---|---|
| bad interval type | `check_interval_minutes must be an integer` |
| interval below 1 | `check_interval_minutes must be at least 1` |
| `subscriptions/update` without an id | `missing subscription id` |
| `subscriptions/update` with nothing updatable | `no valid fields to update` |
| unknown id | `Subscription not found` |
| `subscriptions/delete` with a missing or empty list | `missing ids list` |
| `subscriptions/check` with a non-list `ids` | `ids must be a list` |
| empty URL | `Missing URL` |
| duplicate URL | `This URL is already subscribed` |
| unresolvable URL | `Could not resolve URL` |
| single-video URL | `This URL points to a single video, not a channel or playlist. Use Download instead.` |
| bad `enabled` (400 here, a leaked 500 in legacy — Δ C25) | `enabled must be a boolean` |

**Cookies** (legacy spec §2.1):

| Condition | String |
|---|---|
| no file part | `No cookies file provided` |
| over the cap | `Cookie file too large (max 1MB)` — the cap is 1 000 000 bytes |
| success | `Cookies uploaded (N bytes)` |
| nothing to delete | `No uploaded cookies to delete` |

**`GET <p>robots.txt`** with `ROBOTS_TXT` unset serves exactly, as `text/plain`:

```
User-agent: *
Disallow: /download/
Disallow: /audio_download/
```

(three lines, each `\n`-terminated, no trailing blank line).

The Jellyfin four are in §13.1 and the yt-dlp option-loading four are in §17.2; both are quoted
there and are not repeated.

---

## 12. Telegram (`aulos-telegram`)

### 12.1 Actor shape

```rust
pub struct TelegramActor {
    bot: teloxide::Bot,
    cfg: Arc<TelegramConfig>,             // token, allowed_chat_ids, timeouts, max_urls
    store: Store,                         // per-chat config in `telegram_chats`
    engine: EngineHandle,
    events: EventInbox,                   // from the EventRouter (§2.2.1), not a raw Receiver
    chats: HashMap<i64, ChatBoard>,
    limiter: Limiter,                     // governor: one per chat + one global
}
struct ChatBoard {
    message_id: MessageId,
    jobs: IndexMap<ItemId, JobLine>,      // insertion-ordered, 12 visible + "+N more"
    last_edit: Instant, last_rendered: String, dirty: bool,
    finished_at: Option<Instant>,         // retired 60 s after the last job ends
}
```

`teloxide` runs its own long-polling dispatcher with `drop_pending_updates = true` (legacy
parity). Handlers send `EngineCmd`s and never block. The actor's `select!` loop handles dispatcher
messages, `DomainEvent`s, and a 1 Hz tick that drives edits and watchdogs (legacy polled at 15 s).

Startup gating, all silent no-ops with one log line each, exactly as legacy: `TELEGRAM_BOT_ENABLED`
false ⇒ "Telegram bot disabled"; empty `TELEGRAM_BOT_TOKEN` ⇒ error, no start; empty
`TELEGRAM_ALLOWED_CHAT_IDS` after parsing ⇒ error, no start.

### 12.2 Command and config parity (texts byte-identical)

| Trigger | Behaviour |
|---|---|
| `/start` | `Hi! Send one or more links and I will queue them for download.\nUse /config to set default format/quality for this chat.` |
| `/config` | the config text plus the main inline keyboard |
| config text | `Current download config:\n- Format: {format}\n- Quality: {quality}\n- Split by chapters: {on\|off}\n- Playlist item limit: {n}` |
| main keyboard | `Format: {f}` → `cfg:menu:format`; `Quality: {q}` → `cfg:menu:quality`; `Split Chapters: {on\|off}` → `cfg:toggle:split`; `Playlist Limit: {n}` → `cfg:menu:limit` |
| `cfg:menu:format` | text `Select format`, one button per `bot_formats()` id (the nine below) + `Back` → `cfg:menu:main` |
| `cfg:menu:quality` | text `Select quality`, one button per quality of the current format + `Back` |
| `cfg:menu:limit` | text `Select playlist limit`, buttons `0 1 5 10 20` + `Back` |
| `cfg:set:format:{f}` | set the format; if the current quality is not in the new format's list, reset to the first |
| `cfg:set:quality:{q}` | set only if present in the current format's list, else ignore |
| `cfg:set:limit:{n}` | parse int, ignore on failure |
| `cfg:toggle:split` | flip `split_by_chapters` |
| any callback | `answer_callback_query`, then `edit_message_text` |
| unauthorised chat | silently ignored plus one WARN log line including the chat id |

**The keyboard's format list is the legacy nine, byte-identical — derived from the one shared
catalog, not equal to it.** These are two different things and conflating them breaks the bot. The
`ytdlp` catalog (§6.6) is keyed by `download_type` and contains 16 format ids (`jpg`, not
`thumbnail`; seven caption formats) and no `audio` pseudo-quality; the legacy bot list
(`app/main.py:390 get_available_formats()`, verified) is a flat nine entries with two quirks the
`cfg:` grammar depends on. There is still exactly one catalogue in the codebase:
`FormatCatalog::bot_formats()` (§6.6) is the documented projection, and its output is asserted
byte-for-byte against the legacy list in WP-16.

| Button id | Qualities offered | Maps to `(download_type, format, quality)` |
|---|---|---|
| `any` | best, 2160, 1440, 1080, 720, 480, 360, 240, worst, **audio** | `(video, any, q)`; `audio` ⇒ `(audio, m4a, best)` |
| `mp4` | best, **best_remux**, 2160, 1440, 1080, 720, 480, 360, 240, worst | `(video, mp4, q)` |
| `ios` | best | `(video, ios, best)` |
| `m4a` | best, 192, 128 | `(audio, m4a, q)` |
| `mp3` | best, 320, 192, 128 | `(audio, mp3, q)` |
| `opus` / `wav` / `flac` | best | `(audio, <id>, best)` |
| `thumbnail` | best | `(thumbnail, jpg, best)` — the button keeps the legacy id, the catalog id is `jpg` |

The mapping column is `normalize_download_selection` (§12.3), the same port the importer uses for
`telegram_bot_config.json` (§7.6.5). Consequences, all deliberate:

- The `cfg:` grammar is unchanged: no `cfg:menu:download_type`, no `cfg:set:download_type`. There
  is nothing to add, because a flat format id *is* a `download_type` in this projection — picking
  `m4a` is how a user reaches audio, `thumbnail` is how they reach a thumbnail, and `any` + the
  `audio` pseudo-quality is the legacy shortcut. The chat config stores the legacy `format`/
  `quality` pair, normalised on read, exactly as legacy did.
- **Caption formats are not reachable from the keyboard**, because legacy's list had no caption
  entry either — a caption default was only ever settable by hand-editing
  `telegram_bot_config.json`. An imported chat whose stored config says `captions` keeps working
  (`normalize_download_selection` maps it to `captions/srt/best`); the keyboard simply will not
  show it as the current format's name. Adding a `download_type` menu is a future change, and it
  would need an Appendix B row and a new `cfg:` verb; it is not smuggled in here.
- `ios` shows only `best` in the bot even though the catalog now offers nine heights for it
  (§6.6), because the legacy bot list showed only `best`. The bot is the parity surface; the API is
  the honest one.

Per-chat config lives in `telegram_chats` (SQLite) instead of `telegram_bot_config.json`; the
legacy file is imported once (§7.6.5). Defaults on first access are the legacy 13 keys verbatim.

### 12.3 Message → jobs

1. Extract URLs with `https?://[^\s<>()\[\]{}"']+`, `rstrip` each of `.,;:!?)]}>'"`, dedupe
   preserving order.
2. If the count exceeds `TELEGRAM_MAX_URLS_PER_MESSAGE`, reply
   `Too many links in one message (N). Maximum allowed: M.` and truncate.
3. SSRF guard (`urls::validate`): scheme ∈ {http, https}; host required; reject `localhost` and
   `*.local`; if the host parses as an IP, reject loopback / private / link-local / multicast /
   reserved / unspecified / unique-local. **Added**: also reject `0.0.0.0/8`, IPv4-mapped IPv6
   (`::ffff:10.0.0.1`) and `[::1]`. Rejected URLs are reported as
   `Ignored invalid links:\n- <url> (<reason>)`.
4. `normalize_download_selection` port: audio formats ⇒ `audio`; `thumbnail` ⇒
   `thumbnail/jpg/best`; `captions` ⇒ `captions/srt/best`; `quality == "audio"` ⇒
   `audio/m4a/best`; `quality == "best_ios"` ⇒ `video/ios/best`; else pass through.
5. **One** `EngineCmd::Add` with all URLs and
   `source = { kind: "telegram", ref: "<chat_id>" }`. This replaces the `contextvars` hack and
   means every job the bot creates is attributable — including playlist children, which inherit
   the parent's `source`.
6. Reply `Queued N link(s) with current chat config.` and, if any failed,
   `Some links failed:\n- <url>: <msg>`.

### 12.4 Live progress board (new)

One board message per chat, created on the first job of a burst and edited in place. Plain text,
no Markdown, to avoid entity-escaping bugs:

```
⬇️ Aulos — 3 active, 1 done

▓▓▓▓▓▓▓░░░  68%  Rick Astley - Never Gonna Give You…
              3.1 MB/s · ETA 0:41
▓▓░░░░░░░░  21%  Lo-fi beats [12/500]
              1.4 MB/s · ETA 6:12
⏳  0%  Big Buck Bunny            (queued)
✅  Veritasium - The Big Misconception

updated 14:02:11
```

Bars are 10 blocks; group lines show `[done/total]` instead of a byte rate; at most 12 lines with
`… +N more`; terminal lines linger 60 s with ✅/❌/🚫 then drop. The board is edited to
`✅ 4 downloads finished · ❌ 1 failed` 60 s after the last job ends.
`AULOS_TELEGRAM_BOARD ∈ board | per_job` (default `board`) is the escape hatch.

Rate-limit design — this is the part that breaks naive implementations:

| Limit | Reality | Budget |
|---|---|---|
| per-chat message/edit rate | ~1 msg/s sustained per chat; `editMessageText` counts against it | one `governor` GCRA limiter per chat: 1 edit per `AULOS_TELEGRAM_EDIT_INTERVAL_MS` (3000), burst 1 |
| global bot rate | ~30 msg/s | one global limiter, 20/s, burst 5 |
| `message is not modified` (400) | Telegram rejects an unchanged edit | compare against `last_rendered` and skip the API call entirely |
| `429` with `retry_after` | must be respected or throttling escalates | on `RequestError::RetryAfter(d)`: sleep the chat limiter `d + 250 ms`, **double** that chat's effective interval up to 30 s, halve it back after 3 consecutive successes |
| network / 5xx | | 3 attempts with exponential backoff, then drop **this** edit — the next tick carries newer data; stale edits are never queued |

`teloxide::adaptors::Throttle` is deliberately **not** stacked on top: two independent limiters on
the least critical path in the system is redundant machinery, and the `governor` layer is the one
that knows about `last_rendered` and the per-chat interval. `healthz` exports
`edits_throttled_total` so over-budget behaviour is observable.

### 12.5 Discrete notifications (parity, kept)

| Event | Message |
|---|---|
| item finished | `✅ Download complete: {title}` + `\nFile: {filename}` when known |
| item error | `❌ Download failed: {title}\n{msg or error or "Download failed"}` |
| item canceled | silent (the watch is dropped) |
| stall | `⚠️ Download seems stalled for {secs}s:\n{url}`, once per chat per job |
| hard timeout | `⏱️ Download is taking longer than expected ({secs}s):\n{url}`, once per chat per job |

Neither timeout cancels the download (parity). In board mode the two warnings are sent as
**separate** messages — they are alerts, not state — and the board line gains a `⚠️`/`⏱️` marker.

### 12.6 The `Notifier` seam (for APNs later)

```rust
#[async_trait]
pub trait Notifier: Send + Sync {
    fn id(&self) -> &'static str;
    fn interested(&self, item: &ItemView) -> bool;
    async fn on_event(&self, ev: &DomainEvent);
}
```

The trait is declared in **`aulos-core::event`**, next to `DomainEvent` and the `EventRouter`, not
in `aulos-telegram` — so a future APNs notifier can live in its own crate, implement the trait, and
be registered as one more `EventRouter` subscriber (§2.2.1) with no change to any existing crate.
`TelegramNotifier` is the first implementation and lives in `aulos-telegram`.

`TelegramNotifier` is interested iff `item.source.kind == "telegram"` **or**
`AULOS_TELEGRAM_WATCH_ALL=true`. That knob defaults to **`true`**: on a single-user box, the
legacy blind spot where web and subscription downloads were invisible to the bot is a bug, not a
feature, and the board is rate-limited anyway. Set it to `false` for exact legacy behaviour.

An APNs notifier later implements the same trait with `interested = |_| true` plus a device-token
table, and changes nothing else. No device-token table, no APNs key and no separate webhook
notifier ship now: webhooks are already covered by the community `[[hook]]` manifest (§13.5), and
a push service is a separate deployment decision.

---

## 13. Post-completion hooks (`aulos-hooks`)

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> Arc<str>;
    fn ordering(&self) -> i16;                     // lower runs first
    /// When this hook runs relative to the terminal status write. §13 "Phases".
    fn phase(&self) -> HookPhase { HookPhase::PostTerminal }
    fn applies(&self, item: &Item) -> bool;
    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError>;
}

/// `PreTerminal` hooks run while the item is still `postprocessing`, **before** the engine writes
/// the terminal status; `PostTerminal` hooks run after it, which is what every notifier wants.
pub enum HookPhase { PreTerminal, PostTerminal }

pub struct HookDispatcher { hooks: Vec<Arc<dyn Hook>> }   // per-hook inbox, concurrency 2

/// Everything a hook is given. Borrowed for the duration of one `run`, so a hook holds no state.
pub struct HookCtx<'a> {
    /// The terminal item, exactly as persisted. `item.status` is one of the three terminal values.
    pub item: &'a Item,
    /// The item's provider entry blob, already loaded through the port (§7.1 `HookStore`) —
    /// `None` when the row has no blob (a plain yt-dlp child) or it was already dropped.
    /// This is what the NFO hook reads (§13.2); it never touches the on-disk `.info.json`.
    pub entry: Option<&'a EntryBlob>,
    /// Absolute directory the item's primary file lives in (download or audio root + `folder`).
    pub out_dir: &'a Path,
    /// Absolute path of the primary produced file, when there is one.
    pub file: Option<&'a Path>,
    /// A `ProgressSink` for THIS item, so `phase`/`phase_percent` from a long-running hook flow
    /// through the ordinary aggregator path (§13.3). There is no second progress mechanism.
    pub sink: &'a ProgressSink,
    /// Item-store port for the two writes a hook is allowed to make: `set_size` (§13.3) and
    /// `drop_entry_blob` (§13.2). Both are **engine-mediated**: the implementation is
    /// `aulos-queue::EngineHookStore`, which turns them into `EngineCmd::HookWrite`s so the
    /// engine's item cache, the Aggregator's `last_sent` and therefore every connected client see
    /// the new value (§7.1, §13.3). A hook can NEVER change an item's *status* — the phase
    /// mechanism below is how a hook influences *when* the status is written, not what it is.
    pub store: &'a dyn HookStore,
    /// Effective config, for `JELLYFIN_*`, `AULOS_NFO_*`, tool paths.
    pub cfg: &'a Config,
    /// The coalesced batch this invocation represents. `len == 1` for an undebounced hook;
    /// it is what `{count}`, `{titles_json}` and `{filenames_json}` render from (§13.4).
    pub batch: &'a [BatchEntry],
    /// Cancelled on shutdown. A hook must observe it or be killed at the grace deadline.
    pub cancel: &'a CancellationToken,
    pub clock: &'a dyn Clock,
}
pub struct BatchEntry { pub id: ItemId, pub title: Arc<str>, pub filename: Option<Arc<str>>,
                        pub status: TerminalStatus, pub error: Option<WireError> }

/// Builds a per-item `ProgressSink` from the one `ProgressMsg` channel. Declared in
/// `aulos_provider::sink`; the dispatcher is handed one at spawn.
#[derive(Clone)]
pub struct ProgressSinkFactory { /* mpsc::Sender<ProgressMsg> */ }
impl ProgressSinkFactory { pub fn for_item(&self, id: ItemId) -> ProgressSink; }
```

The dispatcher subscribes to `DomainEvent::Finishing | DomainEvent::Completed` (an `EventInbox`
from the `EventRouter`, capacity 256, `DropNewest` — §2.2.1; a dropped event means one skipped hook
run and is counted).

**Phases — how a hook runs before the item is terminal.** Legacy's `audio_sync_fix.py` was a late
yt-dlp `Exec` postprocessor, i.e. it ran *inside* the download, before the item was terminal
(legacy spec §5.9, §10). A port that only ever ran after the terminal write could not reproduce
that: it would have to move a `finished` item back to `postprocessing`, which §4.2 does not allow
and which §13's "a hook can never change an item's status" forbids. So the dispatcher has two
phases and the engine drives them:

```
run task  ──► EngineCmd::Finished { id, outcome }
engine       if any applicable hook has phase == PreTerminal:
engine            SetStatus{ status: Postprocessing, msg: Set(<first pre-terminal hook's label>) }
engine            publish DomainEvent::Finishing(view)          # hooks inbox only, no frame
engine       else:
engine            finalise now (SetOutput + SetStatus{terminal} + SetClearAfter)

hooks        run every PreTerminal hook in `ordering()` order, sequentially
hooks   ──►  EngineCmd::HooksFinished { id, outcome }            # always sent, even on failure
engine       finalise: SetOutput + SetStatus{terminal} + SetClearAfter
engine       publish DomainEvent::Completed(view)               # the wire's `completed` frame
hooks        run every PostTerminal hook (jellyfin, nfo, community hooks)
```

Three properties this buys, all of which the single-phase version broke:

- The `Downloading → Postprocessing → Finished` edges are the ones already in §4.2. **No new
  status, no new transition, no `Finished → Postprocessing` edge.**
- The client sees `postprocessing` with `msg = "Re-encoding audio"` and a moving `phase_percent`
  for the whole re-encode, then exactly one `completed` frame carrying the **final** `size` — so
  the `set_size` writeback lands before the terminal frame rather than after it, and no client is
  ever handed a `size` that a hook is about to change.
- The download slot is released when the provider finished, not when the hooks did (§13's original
  guarantee, unchanged) — a pre-terminal hook does not block the next download.

`HooksFinished` is sent even if a pre-terminal hook fails or panics, and the engine finalises with
the outcome it already had (§13.3 step 6). A pre-terminal hook that never answers is bounded by the
dispatcher's own per-hook timeout, after which the engine finalises anyway and logs at WARN. There
is exactly one pre-terminal hook today: `audio_sync`.
`TerminalStatus` is `aulos_core::status::TerminalStatus` — `{ Finished, Error, Canceled }`, with
`TryFrom<Status>` — and it is the same type a community manifest's `on = [...]` parses into (§13.4,
`HookSpec.on`). Hooks run **outside** the download slot —
legacy ran cleanup inside the semaphore, blocking the next download. A hook failure never changes
the item's status; it is logged, counted, and surfaced in `healthz`. `AULOS_HOOKS_ENABLED=false`
disables the lot.

`ordering()` makes the built-in sequence deterministic, which matters: audio-sync rewrites the
file, so the NFO and the Jellyfin scan must come after it.

| Hook | `phase` | `ordering` | `applies` |
|---|---|---|---|
| `audio_sync` | `PreTerminal` | 10 | `outcome is success && selection == (video, mp4, best_remux)` — evaluated on the *pending* terminal status, since the row is still `postprocessing` |
| `nfo` | `PostTerminal` | 20 | `finished && provider == "streamingcommunity" && AULOS_NFO_ENABLED` |
| community `[[hook]]` | `PostTerminal` | 50 (configurable) | its own `on` + `when` filters |
| `jellyfin` | `PostTerminal` | 90 | `finished && JELLYFIN_SYNC_ENABLED` |

`applies()` for a `PostTerminal` hook reads `item.status`, which is already terminal. For a
`PreTerminal` hook the row is still `postprocessing`, so it reads the *prospective* outcome carried
by `HookCtx.batch[0].status` instead — which is why `BatchEntry.status` is a `TerminalStatus` and
not an `Option`.

### 13.1 Jellyfin refresh, debounced and targeted

- Each completion arms/extends an `AULOS_JELLYFIN_DEBOUNCE_SECS` (30) trailing timer, but the fire
  time is **capped** at `first_at + AULOS_JELLYFIN_MAX_WAIT_SECS` (300), so a 500-item playlist
  still refreshes every 5 minutes instead of only at the very end. A plain trailing-edge debounce
  would make a long playlist invisible in Jellyfin for hours.

| Condition | Request |
|---|---|
| `JELLYFIN_LIBRARY_ID` empty | `POST {base}/Library/Refresh` — legacy behaviour, refreshes all libraries |
| `JELLYFIN_LIBRARY_ID` set | `POST {base}/Items/{id}/Refresh?metadataRefreshMode={JELLYFIN_METADATA_REFRESH_MODE}&imageRefreshMode={JELLYFIN_IMAGE_REFRESH_MODE}&replaceAllMetadata=false&replaceAllImages=false` — the targeted refresh BRIEF §13 asks for |

- Headers `Accept: application/json`, `Authorization: MediaBrowser Token="<key>"`, no body,
  timeout `JELLYFIN_SYNC_TIMEOUT_SECONDS`. A targeted refresh returning 400/404 falls back **once**
  to the global `/Library/Refresh` and logs a WARN naming the bad library id — a mistyped
  `JELLYFIN_LIBRARY_ID` must not silently disable sync.
- Legacy error message shapes are preserved verbatim, all four of them:

| Condition | Message (byte-identical to legacy) |
|---|---|
| `JELLYFIN_URL` blank | `JELLYFIN_URL is required` |
| `JELLYFIN_API_KEY` blank | `JELLYFIN_API_KEY is required` |
| non-2xx response | `Jellyfin refresh failed with HTTP {code}: {details}`, `details` preferring the JSON `message`/`Message` field |
| transport error | `Jellyfin refresh request failed: {err}` |

- **Precondition handling matches legacy's warn-and-return, upgraded to be visible.** Legacy's
  `refresh_jellyfin_library` *raised* on a blank URL or key, but `__sync_jellyfin_library` checked
  first and simply warned and returned per download (spec §5.8, §10), so a misconfigured deployment
  logged the same line forever and never refreshed. Here: with `JELLYFIN_SYNC_ENABLED=true` and a
  blank `JELLYFIN_URL` or `JELLYFIN_API_KEY`, the hook logs the corresponding message once **at
  boot** at WARN, sets `healthz.components.jellyfin` to `degraded` with that message as `detail`
  for the life of the process, `applies()` returns `false`, and every completion is a silent no-op.
  The message text is unchanged; only its delivery is fixed.
- Retries: 3 attempts, 2 s / 8 s backoff, then give up until the next completion.
- `healthz.components.jellyfin` reports `{status, last_success_at, last_error, pending}`.

### 13.2 NFO generation (now actually wired)

A port of `jellyfin_nfo_generator.py`, in-process with `quick-xml`, applied to SC items. The
legacy script existed but was **never referenced by any code path**.

- Input: the item's `entry_json` (SC entries are stored whole) — **not** the on-disk
  `.info.json`, so there is nothing to delete and no race with the user.
- Output: `<base>.nfo` next to the produced file. Root `episodedetails` when any of
  `series`/`season_number`/`episode_number` is set, else `movie`.
- Elements in legacy order: `title`, `originaltitle`, (`showtitle`, `season`, `episode`,
  `subtitle`), `plot`, `year`, `premiered` (from `upload_date` `YYYYMMDD`), `dateadded`
  (`%Y-%m-%d %H:%M:%S`, UTC now), `studio`, `director` (from `uploader` or `channel`),
  `uniqueid type="streamingcommunity"|"youtube"`, `website` (`original_url` or `webpage_url`), up
  to 20 `tag`, `runtime` in whole minutes. Pretty-printed, blank lines removed.
- The SC downloader still writes `<safe_title>.info.json` (parity — users' own `Exec`
  postprocessors may consume it). After a successful NFO write it is deleted **only when**
  `AULOS_NFO_DELETE_INFO_JSON=true`, default **`false`**. The legacy CLI always deleted it;
  making that opt-in avoids breaking anyone's pipeline on the wrong side of a cutover.
- Once the NFO exists, the item's `entry_json` is dropped from the DB via
  `ctx.store.drop_entry_blob(item.id)` (§7.1) — the port, not the store.

### 13.3 `best_remux` audio-sync fix, in-process

A port of `audio_sync_fix.py` with no `Exec` postprocessor and no hard-coded `/app/app/...` path.

1. Skip unless the produced file exists and ends in `.mp4`.
2. `ffprobe -v error -select_streams v -show_entries stream=codec_type -of json` — skip when there
   is no video stream.
3. Duration via `ffprobe -v error -show_entries format=duration -of json`, 30 s timeout;
   `timeout = max(600, ceil(duration / 2))` seconds, or `1800` when unknown.
4. `ffmpeg -y -loglevel warning -i <file> -map 0 -dn -ignore_unknown -c copy -c:a aac -b:a 256k
   -movflags +faststart <tmp>` into a sibling temp file, then `rename(tmp, file)`, then
   `ctx.store.set_size(item.id, new_size)` through the `HookStore` port (§7.1). That call is
   **engine-mediated** — `EngineHookStore` turns it into `EngineCmd::HookWrite { Size }`, the
   engine writes `WriteOp::SetSize`, updates its item cache and republishes, and the Aggregator
   diffs it against `last_sent` like any other change. It is the only item write this hook makes,
   and routing it through the engine rather than straight into SQLite is what keeps `size` correct
   on every connected client (a direct store write would leave the in-memory snapshot, the delta
   baseline and every client that already had the row permanently stale until a restart, and
   `stress_consistency` could not see it because it compares the frames against that same stale
   snapshot).
5. This hook is `HookPhase::PreTerminal` (§13), so while it runs the item is genuinely
   `postprocessing` — the engine wrote that status and published `Finishing` *before* dispatching,
   and it will not write `finished` until `HooksFinished` comes back. `msg = "Re-encoding audio"`,
   `phase = "audio_sync"`, and `phase_percent` is parsed from `-progress pipe:1` when the duration
   is known, so a 40-minute re-encode shows a moving secondary bar instead of a frozen UI. This is
   the state legacy expressed by running inside yt-dlp; the ordering also means the single
   `completed` frame carries the post-re-encode `size`.
6. On failure (non-zero exit, timeout, panic): WARN, temp file removed, `HooksFinished` sent with
   the unchanged outcome, and the item becomes `finished` with the **original** file and its
   original `size`. Legacy's `Exec` failure made yt-dlp report a postprocessor error, i.e. a
   perfectly good download looked broken — a strictly worse outcome.

Progress plumbing: the hook is handed a `ProgressSink` for the item (the dispatcher constructs one
from the same `ProgressMsg` channel), so `phase_percent` flows through the ordinary aggregator
path. There is no second progress mechanism.

### 13.4 Community hooks — the `[[hook]]` manifest (BRIEF §13)

Community hooks use the **same** `plugin.toml` format as providers. A manifest may contain only
`[[hook]]` tables (no `[match]`, no `[download]`), which is the normal case for a Plex/Emby/ntfy
integration.

| Key | Type | Req | Default | Meaning |
|---|---|---|---|---|
| `id` | string | ✓ | — | unique within the manifest; the hook is `hook:<dir>/<id>` |
| `on` | [string] | ✓ | — | subset of `["finished","error","canceled"]` |
| `ordering` | int | | `50` | lower runs first; built-ins are 10/20/90 |
| `debounce_ms` | int | | `0` | 0 = fire per event. > 0 coalesces events in a trailing window |
| `max_wait_ms` | int | | `10 × debounce_ms` | hard cap on the debounce window |
| `timeout_ms` | int | | `10000` | per attempt |
| `retries` | int | | `2` | attempts after the first, exponential 2 s/8 s |
| `when.provider` | [string] | | any | filter |
| `when.download_type` | [string] | | any | filter |
| `when.folder_prefix` | [string] | | any | filter on the item's `folder` |
| `http` | table | one of | — | `{ method, url, headers, body }`; `method` default `POST`, `body` default `""` |
| `command` | [string] | one of | — | argv template, spawned exactly like a provider command (§6.5.3) |

Placeholders in `url`, `headers`, `body` and `command`, plus `${ENV}` interpolation of the
server's own environment at load time:

| Token | Value |
|---|---|
| `{id}` | the item ULID |
| `{title}`, `{url}`, `{provider}`, `{status}` | as named |
| `{filename}` | path relative to the download root |
| `{folder}` | the item's `folder`, or `""` |
| `{download_url}` | the public URL, percent-encoded |
| `{size}`, `{download_type}`, `{format}`, `{quality}` | as named |
| `{error_code}`, `{error_message}` | empty on success |
| `{count}` | number of coalesced events in a debounced batch (1 when `debounce_ms = 0`) |
| `{titles_json}`, `{filenames_json}` | JSON arrays for a debounced batch |

For an `http` hook, a placeholder inside `url` is percent-encoded; inside `body` and `headers` it
is JSON-escaped when the body parses as JSON, otherwise inserted raw. A `command` hook substitutes
at argv level, never through a shell (identical rules and identical isolation to §6.5.3).
A non-2xx response or a non-zero exit is retried per `retries`, then logged and counted; the item
is never affected.

Examples that cover the BRIEF's named cases:

```toml
manifest_version = 1
name    = "Media server refresh"
version = "1.0.0"

# Plex: exactly the GET the BRIEF names
[[hook]]
id          = "plex"
on          = ["finished"]
debounce_ms = 30000
max_wait_ms = 300000
http = { method = "GET",
         url = "http://plex:32400/library/sections/3/refresh?X-Plex-Token=${PLEX_TOKEN}" }

# Emby
[[hook]]
id          = "emby"
on          = ["finished"]
debounce_ms = 30000
http = { method = "POST",
         url = "http://emby:8096/Library/Refresh",
         headers = { "X-Emby-Token" = "${EMBY_TOKEN}", Accept = "application/json" } }

# ntfy: one push per outcome, no debounce
[[hook]]
id = "ntfy"
on = ["finished", "error"]
http = { method = "POST",
         url = "https://ntfy.sh/my-aulos-topic",
         headers = { Title = "Aulos: {status}", Priority = "default" },
         body = "{title}\n{filename}{error_message}" }

# A local script, for anything the two shapes above cannot express
[[hook]]
id       = "post-process"
on       = ["finished"]
when     = { download_type = ["video"], folder_prefix = ["Series/"] }
command  = ["/config/scripts/organise.sh", "{filename}", "{folder}", "{title}"]
timeout_ms = 60000
```

The legacy `JELLYFIN_*` env vars keep working by **materialising the built-in jellyfin hook's
config** at boot (BRIEF §13) — they are not translated into a `[[hook]]` table, so there is
exactly one Jellyfin code path and the debounce/targeting logic of §13.1 applies.
`plugins/examples/media-server-hooks/plugin.toml` ships the file above.

---

## 14. Subscriptions (`aulos-subscriptions`)

### 14.1 Model and projection

`SubscriptionRecord` mirrors the legacy dataclass field for field. It, `SubscriptionView` **and
`SubscriptionsHandle`** are `aulos-core` types (§3): the store persists the record,
`DomainEvent::SubscriptionChanged` carries the view, and the handle is what `aulos-api`'s
`ApiState` holds. The handle is a cheap clone over an `mpsc::Sender<SubCmd>` — no logic, so
`aulos-core::subscription` declares `SubscriptionsHandle`, `SubCmd`, `SubChanges`, `CheckJob`,
`SubError` and `SubsHealth`, and `aulos-subscriptions::Manager` owns the receiver. That is what
keeps `aulos-api` from depending on this crate at all. The wire projection is the
legacy `to_public_dict()` **plus three additive keys** in v2; the v1 shim emits exactly the legacy
13 and nothing more.

```json
{ "id": "9c1f…", "name": "Veritasium", "url": "https://www.youtube.com/@veritasium",
  "enabled": true, "check_interval_minutes": 60,
  "download_type": "video", "codec": "auto", "format": "any", "quality": "best",
  "folder": "", "last_checked": 1757000100000, "seen_count": 314, "error": null,
  "next_due": 1757003700000, "consecutive_failures": 0, "checking": false }
```

`last_checked` and `next_due` are **milliseconds** in v2; the v1 shim divides `last_checked` by
1000 and emits a float, matching legacy's `time.time()`. Secrets and knobs
(`custom_name_prefix`, `ytdl_options_*`, the seen set) stay unexposed, as legacy. `timestamp` is
in-memory only and never persisted, as legacy.

### 14.2 Scheduler

One `tokio::task` per subscription, all in a `JoinSet` so shutdown is one `abort_all()`:

```
loop {
    sleep_until(next_due)                    // persisted, so a restart keeps the schedule
    if !enabled { park on Notify }
    permit = check_slots.acquire().await     // AULOS_SUB_CHECK_CONCURRENCY (2)
    result = timeout(AULOS_SUB_CHECK_TIMEOUT_SECS, check_one(sub)).await
    match result {
      Ok(_)  => { failures = 0; next_due = now + interval + jitter(±10%) }
      Err(e) => { failures += 1;
                  backoff = min(interval * 2^min(failures,8), AULOS_SUB_BACKOFF_MAX_SECS);
                  next_due = now + backoff + jitter(±10%);
                  error = e.to_string() }
    }
    persist(last_checked = now, next_due, failures, error)   // one row + only the new seen ids
    events.publish(SubscriptionChanged)                      // EventSender, §2.2.1 — the
                                                             // scheduler is a producer only
}
```

| Legacy problem | Fix |
|---|---|
| first check at boot **+60 s** | first check at `now + AULOS_SUB_FIRST_CHECK_DELAY_SECS` (10) `+ jitter(0..30 s)`, or at `next_due` if that is later. Jitter spreads 40 channels so they do not hit YouTube in the same second. |
| 60 s tick granularity | per-subscription timers: a 5-minute subscription fires at 5 minutes, not at the next 60 s multiple |
| sequential checks; one slow feed blocks all | `AULOS_SUB_CHECK_CONCURRENCY` permits plus a per-check timeout |
| a failure did not update `last_checked` ⇒ a permanently broken feed re-extracted every 60 s forever | `last_checked` is **always** updated; exponential backoff to `AULOS_SUB_BACKOFF_MAX_SECS` (6 h) |
| `POST /subscriptions/check` awaited every check | returns immediately with a `job_id`; progress is observable via `checking: true/false` on the `subscription` frame |
| a restart reset the schedule | `next_due` and `consecutive_failures` are persisted |

`enabled = false` parks the task; an update `Notify`s it. Adding or deleting a subscription
spawns or aborts its task.

### 14.3 Check algorithm (parity where it matters)

1. Flat extraction through the **provider registry** (`resolve` with `flat: true`,
   `playlist_end: SUBSCRIPTION_SCAN_PLAYLIST_END`), so a subscription works for any provider — a
   StreamingCommunity series or a plugin feed, not only yt-dlp sources. For `ytdlp` the option
   layering uses the **same order as everywhere else** (MeTube's keys applied *after* user
   options), fixing the legacy asymmetry where `YTDL_OPTIONS` could break subscription extraction
   while leaving normal adds intact.
2. `is_media_entry` port, verbatim: not `playlist|multi_video|channel`, no `entries`, has
   `webpage_url|url`, and — when `ie_key`/`extractor_key` contains `playlist|channel|tab` — at
   least one of `duration, timestamp, release_timestamp, upload_date, view_count, live_status,
   availability` is non-null.
3. Tab-page recursion, verbatim: if there are no media entries and depth < 1, try the first up to
   5 child URLs (this is what handles YouTube "channel of tabs" pages).
4. `_type == "video"` or zero entries ⇒
   `error = "This URL points to a single video, not a channel or playlist. Use Download instead."`
   and this **counts as a failure** for backoff purposes. Legacy hot-retried it forever.
5. New items = entries whose `media_id` is not in `subscription_seen`, **plus** already-seen
   entries whose `live_status == "is_live"` (parity: a live stream is re-queued when it starts).
6. Queue them as one `EngineCmd::Add` batch with
   `source = { kind: "subscription", ref: "<sub id>" }` at `Priority::Subscription`. Entries that
   fail validation are **not** marked seen (parity — they retry) and their messages are collected
   into `error` (first 3, `"; "`-joined).
7. `MarkSeen` writes only the new ids; `PruneSeen` trims to `SUBSCRIPTION_MAX_SEEN_IDS` by
   `seen_at DESC`.
8. Backfill suppression on **subscribe** is preserved exactly: every currently visible media id is
   marked seen without queueing, **except** entries with `live_status == "is_upcoming"`.
9. `add_subscription` keeps the legacy guards verbatim: URL `.trim()`, the unique-URL index plus an
   in-flight `pending_urls` set, and the exact messages `Missing URL`,
   `This URL is already subscribed`, `Could not resolve URL`, and the single-video message above.
10. `update_subscription` accepts only `enabled`, `check_interval_minutes` and `name`, exactly as
    legacy — but a bad `enabled` is a `400 validation_failed`, not a leaked 500.
11. `folder == ""` becomes `None`, so downloads land in the base dir (parity).

---

## 15. Realtime internals (`aulos-queue::aggregator`, `::hub`, `aulos-api::ws`)

`docs/PROTOCOL.md` is the normative wire contract. This section is the implementation.

### 15.1 Aggregator

```rust
struct Aggregator {
    cells: HashMap<ItemId, ProgressCell>,
    norm: HashMap<ItemId, Normalizer>,
    last_sent: HashMap<ItemId, Arc<ItemView>>,   // the delta baseline
    dirty: HashSet<ItemId>,
    pending_added: Vec<Arc<ItemView>>,
    pending_completed: Vec<Arc<ItemView>>,
    pending_removed: Vec<(ItemId, RemoveReason)>,
    published: Arc<ArcSwap<Published>>,
    hub: EventHub,
    tick: Interval,                              // AULOS_WS_BATCH_MS (250)
    urgent_at: Option<Instant>,                  // AULOS_WS_URGENT_MS (25)
}
```

The loop is `select!` over `rx.recv()`, `tick.tick()` and `sleep_until(urgent_at)`. It is
deliberately **not** `biased`, and `recv_many` is bounded to 256 per wake, so a hot inbox can
never starve the tick or the urgent deadline — which is precisely the 500-item-playlist path this
design exists to make fast.

- `Progress` ⇒ bump `last_frame_at`, apply through the `Normalizer`, update the cell, mark dirty.
- `Stage` / `File` ⇒ forward to the engine (persisted) **and** set `urgent_at = now + urgent_ms`.
- `Added` / `Completed` / `Removed` / `Notice` ⇒ queue them and set `urgent_at`.
- `Finishing` is **not** in this subscriber's filter (§2.2.1): it is a hooks-only event and must
  produce no frame, because the status it precedes has not been written yet.

**Urgency classification, complete — the rule is "text is urgent, numbers are batched".**

| Change | Class |
|---|---|
| any `Stage` transition, any `File` artifact | urgent (25 ms) |
| `Added`, `Completed`, `Removed`, `Notice`, `SubscriptionChanged`, `YtdlOptionsReloaded`, `ProvidersReloaded`, `HealthChanged` | urgent |
| a frame that changes any **text** field — `msg`, `title`, `phase` — whatever carried it | **urgent** |
| `percent`, `speed`, `eta`, `downloaded_bytes`, `total_bytes`, `total_bytes_estimate`, `fragment_index`, `fragment_count`, `phase_percent` | batched (250 ms) |

The text rule is what makes the shim's `phase` frame (§9.3 — it only sets `ItemView.msg`) and the
SC engine's `msg` transitions (§10.5 — `"Starting N_m3u8DL-RE download..."`,
`"N_m3u8DL-RE failed, retrying with ffmpeg..."`) prompt. Those are neither `Stage` nor `File`, and
without an explicit rule they would sit in the 250 ms batch; legacy broadcast every one of them
immediately (spec §3.1), and they are the only feedback a user gets during the multi-second gap
before an SC download produces its first byte. The check is mechanical, not per-call-site: the
flush classifier compares the three text fields against `last_sent` and, on any difference, pulls
the flush forward. So a provider cannot forget to mark a message urgent, and a numeric-only tick
never resets the batch cadence.

**Delta derivation is a diff, not a mask.** On flush, for each dirty id, the freshly rebuilt
`Arc<ItemView>` is compared field-by-field against `last_sent`, and only the differing fields are
written (plus `id`, always). A generated `fn diff(old: &ItemView, new: &ItemView, w: &mut Writer)`
— one line per wire field, produced by a small proc-macro-free `macro_rules!` over the field list —
guarantees that adding a field to `ItemView` cannot be forgotten: the macro enumerates the same
list the serializer does, and a compile-time exhaustiveness test asserts the two lists match.
Consequence: a field that changes but is not diffed is impossible, and a download stalled at
43.2 % produces **zero bytes** on the wire.

**Flush order is fixed: `added` → `completed` → `removed`\* → `delta` → `republish`**, identical to
the order PROTOCOL.md §4.3 and §6.3 mandate for a client (this document previously stated a
different order; PROTOCOL.md is normative and it is also the correct one). Two properties depend on
it, and both fail under any other order:

- **`added` first**, so a `delta` or a `completed` can never reference an id the client has not
  seen. This is the guarantee PROTOCOL.md §5.4 states as "a `delta` never contains an id you have
  not seen".
- **`removed` after `added`**, so an item created *and* deleted inside the same 250 ms window
  cannot leave a ghost row. Both are pending in the same flush; emitting `removed` first would have
  the client discard an id it does not yet have (documented as a no-op) and then *add* it. Emitting
  it last leaves the client with nothing, which is correct. The same reasoning makes the §15.3
  resume merge rule "an `added` that was later `removed` is dropped entirely" consistent with the
  live path rather than a special case.

`delta` sits after `removed` purely because a patch for a just-removed id is then a guaranteed
no-op (the client's apply step never creates a record), so the flush needs no cross-bucket
filtering. `republish` (the `ArcSwap::store` of §15.2) is last so a REST reader can never observe a
published snapshot that is newer than the frames on the socket.

\* **`removed` is grouped by reason, and that is the one place "at most one frame per kind" is
relaxed.** `pending_removed` is a `Vec<(ItemId, RemoveReason)>` — a reason **per id** — while the
wire frame carries one reason for the whole frame (PROTOCOL §5.7). A user delete (`deleted`)
landing in the same 250 ms window as a `CLEAR_COMPLETED_AFTER` expiry (`auto_cleared`) or a group
cascade (`group_cascade`) is therefore not representable in a single frame. The resolution, stated
identically here and in PROTOCOL §5.7/§6.3:

> On flush, `pending_removed` is grouped by `RemoveReason` and emitted as **one `removed` frame per
> distinct reason**, in the fixed reason order `deleted`, `cleared`, `auto_cleared`,
> `group_cascade`, all of them occupying the `removed` position in the flush order. A flush emits
> between zero and four `removed` frames. Dropping the reason, or picking one reason by precedence,
> were both rejected: `reason` is what lets a client distinguish "the user did this" from "the
> server tidied up", which is the difference between an undo affordance and a silent
> disappearance.

`GET api/v2/state?since=` expresses the same thing as an **array** of `{ids, reason}` groups rather
than a single object, so the REST delta and the socket carry the same information in the same
order (PROTOCOL §4.3).

- An empty batch emits **no frame at all**. An idle server is silent, which matters for the iOS
  radio and battery.
- More than `AULOS_WS_MAX_DELTAS_PER_FRAME` (200) dirty ids are split across consecutive frames
  with a persistent round-robin cursor, so no item can be starved. If splitting persists for more
  than 4 ticks, the effective tick backs off to `batch_ms × ceil(dirty / max)` capped at 2 s,
  logged once at WARN and exported as `aulos_ws_tick_backoff_ms`.

### 15.2 Published snapshot — lock-free reads

REST handlers and newly connecting WS sessions must read the whole state without touching the
engine or the store, otherwise a burst of app foregroundings queues behind progress work.

```rust
pub struct Published {
    pub seq: Seq, pub boot_id: BootId,
    pub items: Arc<[Arc<ItemView>]>,          // ord order
    pub done: Arc<[Arc<ItemView>]>,           // most recent AULOS_MEM_DONE_ITEMS, ord order
    pub by_id: Arc<HashMap<ItemId, u32>>,     // rebuilt only when membership changes
    pub counts: StatusCounts,
    pub done_total: u64,                      // from SQLite, refreshed on membership change
    pub truncated: Truncated,                 // { done: bool, groups: Vec<GroupId> }
}
pub struct StateView(Arc<ArcSwap<Published>>);
impl StateView { pub fn load(&self) -> Guard<Arc<Published>>; }   // ~2 ns, wait-free
```

Republish cost per tick with 500 items: one 4 KB memcpy of pointers, a pointer bump for `by_id`
when membership is unchanged, three allocations for the dirty views, and one `ArcSwap::store`.
Readers never block the writer and a reader holding a `Guard` across an `await` cannot stall the
aggregator.

Serving 500 items to a connecting client is therefore ~30 µs and **zero** database round trips.
This is the graft that keeps connect-to-list latency flat as history grows.

### 15.3 EventHub, `seq`, and the replay ring

```rust
pub struct WireFrame { pub seq: Seq, pub kind: FrameKind, pub text: Utf8Bytes }
pub struct RingEntry { pub seq: Seq, pub wire: Arc<WireFrame>, pub batch: Option<Arc<DeltaBatch>> }

pub struct EventHub {
    seq: Arc<dyn HiLoAllocator>,               // durable, reserve-first (§4.1)
    boot_id: BootId,
    tx: broadcast::Sender<Arc<WireFrame>>,     // capacity 256
    ring: Mutex<Ring>,                         // AULOS_WS_REPLAY_FRAMES / _REPLAY_BYTES
}
impl EventHub {
    pub fn publish(&self, kind: FrameKind, body: impl Serialize) -> Seq;   // serialises ONCE
    pub fn resume(&self, since: Seq, boot: Option<BootId>) -> Resume;
    pub fn head(&self) -> Seq;
}
pub enum Resume { Snapshot, UpToDate, Merged { from: Seq, to: Seq, frames: Vec<Arc<WireFrame>> } }
```

Serialising once into `Utf8Bytes` and broadcasting `Arc<WireFrame>` means N connected clients cost
N sends of a refcount, not N JSON encodings. That is the direct fix for the legacy "one full-object
broadcast per progress hook, double-encoded" problem.

The ring keeps two representations because they serve two jobs: `wire` is what live subscribers
get; `batch` is the structured `DeltaBatch` so a **resume can merge**. Replaying 180 raw delta
frames to a client that was away 45 s would send 180 frames of mostly-stale numbers; merging folds
them into one frame with the last value per `(id, field)`.

```rust
fn resume(&self, since: Seq, boot: Option<BootId>) -> Resume {
    if boot.is_some_and(|b| b != self.boot_id) { return Resume::Snapshot }   // different process
    if since >= self.head()                    { return Resume::UpToDate }
    if since <  self.ring.floor()              { return Resume::Snapshot }   // gap too old
    Resume::Merged { .. }                                                     // merge_after(since)
}
```

Merge rules, one property test each:

| Frame kind in the window | Rule |
|---|---|
| `delta` | per `(id, field)`, last value wins |
| `added` | accumulate; if the id is later `removed`, drop both |
| `completed` | accumulate as a full object; supersedes any earlier delta for that id |
| `removed` | accumulate **per reason**; drops any earlier `added`/`delta` for that id |
| `subscription`, `ytdl_options`, `notice`, `providers` | accumulate in order (rare, small) |

The merged result is emitted as one `resume` frame followed by at most one `added`, at most one
`completed`, at most one `removed` **per distinct reason** (§15.1, same fixed reason order) and at
most one `delta`, **in that order**. `since > head()` (possible after a DB restore) is `Snapshot`,
never an empty delta list — that case is the one where an empty answer would tell the client "you
are up to date" when it had in fact missed everything.

**The client `ack` frame is advisory only.** The ring is shared by every connected client, so
trimming it on one client's `ack` would silently break another client's `?since=`. `floor` is
therefore advanced **exclusively** by the frame and byte bounds below, and `ack` is used only to
compute that client's lag for `healthz`/metrics and to log a slow consumer. PROTOCOL §5.11 states
the same thing to client authors, so nobody builds a retention assumption on it.

Ring eviction advances `floor` and is bounded by frames **and** bytes
(`AULOS_WS_REPLAY_FRAMES` 512, `AULOS_WS_REPLAY_BYTES` 4 MiB), so a burst of large `added` frames
from a 500-item playlist cannot grow memory.

### 15.4 WS connection task

1. Accept the upgrade. Read an optional `hello` with a 100 ms grace.
2. **Subscribe to the broadcast first**, record `head()`, *then* build the snapshot from
   `StateView::load()`, send it, and forward buffered frames with `seq > snapshot.seq`. This
   ordering is what makes "no lost updates on connect" actually true. The snapshot builder also
   reads `HealthRegistry::snapshot()` and the `ArcSwap<YtdlOptions>` and includes both as the
   `health` and `ytdl_options` blocks of the frame (PROTOCOL §5.3): those two frames are
   transition-only, so a client connecting while the POT sidecar is down or the options file is
   broken would otherwise learn nothing over the socket and have to issue two extra REST calls.
   Legacy pushed `ytdl_options_changed` on connect for exactly this reason (legacy spec §3.1).
3. Two tasks per session sharing a `CancellationToken`: a reader (client frames, pong, close) and
   a writer (`broadcast::Receiver` → socket). Either ending kills both.
4. `select!` on broadcast recv, socket recv, and a 20 s WS-level `Ping`; close if no pong or data
   within 60 s (dead TCP with no FIN).
5. `RecvError::Lagged(n)` ⇒ count it, send `EngineCmd::Unwatch` for every group this connection
   was watching (the fresh snapshot re-applies the `truncated.groups` rule from scratch, so the
   old watch set is meaningless), send a fresh snapshot from the published view, continue. More
   than 8 lags in 60 s ⇒ close `1013`.
5a. **Every** close path — normal close, `1001`, `1009`, `1013`, a reader error, a writer error, a
   task panic — ends in `EngineCmd::ConnClosed { conn }`, sent from a `Drop` guard on the session
   so no path can forget it. That is the only thing that releases a connection's group watches
   (§8.1). `watch`/`unwatch` are otherwise idempotent and unknown group ids are ignored.
6. If the socket's send buffer is full for more than `AULOS_WS_SEND_TIMEOUT_MS` (5000), close
   `1013`. A stalled client can never hold memory in the hub.
7. `AULOS_WS_MAX_CLIENTS` (64); the next connection gets an `error` frame and close `1013`.
8. Client frames larger than 1 MiB ⇒ close `1009`.
9. `permessage-deflate` is **off**. Frames are 200–900 bytes; per-connection deflate dictionaries
   cost ~300 KB each and the field-diffing is a better compressor.

Mutations are **never** accepted over the socket. They stay on REST, which keeps auth,
idempotency and the error envelope in one place. The only client frames are `hello`, `ping`,
`resume`, `ack`, `watch`, `unwatch` — all optional; a read-only client is fully functional.

### 15.5 Memory bounds

| Structure | Bound | Steady state |
|---|---|---|
| engine item cache | non-terminal + `AULOS_MEM_DONE_ITEMS` (500) | ~0.8 KB/record; 1000 records ≈ 800 KB |
| `entry_json` | **not** held in memory; a DB column loaded on demand for the NFO hook | 0 |
| `Published` | 2 generations alive at once | 2 × 8 B × n plus shared views |
| replay ring | `min(512 frames, 4 MiB)` | ≤ 4 MiB |
| broadcast buffer | 256 `Arc`s, frames shared with the ring | ≤ 2 KB of pointers |
| per WS session | 1 receiver + a 64 KiB write buffer | ≤ 80 KB × ≤ 64 clients ⇒ ≤ 5 MB |
| per running job | `ProgressCell` + `StderrRing` (32 KiB) + line buffer (≤ 64 KiB) | ≤ 100 KB × 4 |
| SQLite | 16 MiB page cache + 64 MiB mmap (virtual) | ~20 MB RSS |

Target RSS for the stock configuration with 1000 queued items and 3 downloads: **under 90 MB**.
Legacy sits at 250–400 MB because it forks an interpreter per download.

---

## 16. The binary (`aulos-server`)

### 16.1 Startup order

```
1  parse env → Config (fatal on error, exit 2)                            §17
2  init tracing (LOGLEVEL, AULOS_LOG_FORMAT, third-party dampening)       §16.5
3  log the effective config table (secrets redacted)
4  mkdir -p DOWNLOAD_DIR, AUDIO_DOWNLOAD_DIR, TEMP_DIR, STATE_DIR, dirname(AULOS_DB_PATH)
5  open SQLite, PRAGMA quick_check, run migrations, seed the ord/seq allocators
6  if the DB was just created → run the legacy importer (one txn)          §7.6
7  load YTDL_OPTIONS + presets (fatal on error); adopt STATE_DIR/cookies.txt if present
8  discover command plugins and hook manifests; build the provider registry
9  doctor probes: python3 + yt-dlp are FATAL (the ytdlp provider is the fallback for everything);
   ffmpeg, ffprobe, N_m3u8DL-RE, deno are WARN and mark the component degraded
10 spawn the POT supervisor (if AULOS_POT_ENABLED)                         §16.2
11 build the EventRouter, `subscribe()` every consumer of §2.2.1's table, hand the EventSender to
   every producer; spawn Store actor, EventHub, Aggregator, QueueEngine (the router is spawned
   after the last `subscribe()`, i.e. after step 14)
12 boot recovery: re-queue in-flight items, recompute group counters       §8.9
13 spawn HookDispatcher, SubscriptionScheduler, ClearScheduler, ConfigWatcher, PluginWatcher
14 spawn the Telegram actor (if enabled and configured), then `EventRouter::spawn()` — no
   subscriber may be registered after this point
15 bind HOST:PORT (TLS when HTTPS=true), start axum with graceful shutdown
16 log "aulos-server <version> listening on <addr><prefix> (v1 shim: on|off)"
```

Steps 5–12 complete **before** the listener binds, so the first request already sees a consistent
snapshot. `SO_REUSEPORT` is set when the platform supports it (parity with legacy's
`supports_reuse_port()`), which also makes a blue/green port swap possible on the VPS. Every
spawned task goes into a `TaskTracker` so shutdown can await it.

### 16.2 POT sidecar supervisor

```rust
struct PotSupervisor { cmd: Vec<String>, url: Url, state: Arc<ArcSwap<PotState>> }
struct PotState { status: PotStatus, pid: Option<u32>, restarts: u32,
                  last_exit: Option<Box<str>>, last_probe: Option<ProbeResult>, since: UnixMs }
```

- Spawns `AULOS_POT_CMD` (default `bgutil-pot server`) with `process_group(0)`, stdout/stderr
  piped into `tracing` (`target = "bgutil_pot"`, INFO for stdout, WARN for stderr).
- On exit: log the code/signal, restart with backoff `1s, 2s, 4s, …, 60s` (±20 % jitter); the
  backoff resets after 60 s of healthy uptime. `restarts` is monotonic and exposed.
- Health probe every 15 s: `GET {AULOS_POT_URL}/ping`, falling back to a plain TCP connect on the
  host:port if that route 404s (the provider's route set changes across versions). **Three
  consecutive probe failures force a restart even when the process is still alive** — a wedged
  sidecar is worse than a dead one, because yt-dlp then fails bot checks silently, which is
  exactly the legacy failure nobody could see.
- After `AULOS_POT_MAX_RESTARTS` (10) in 10 minutes the supervisor enters `failed`, stops
  restarting, and logs an ERROR with remediation text. The server keeps serving; `bot_check`
  errors then carry a hint pointing at `healthz`.
- On shutdown: `SIGTERM` to the pgid, 5 s grace, `SIGKILL`.
- The entrypoint no longer starts it (legacy launched it as an unsupervised `&` child that the
  healthcheck could not see).

### 16.3 `healthz`, `livez`, `metrics`

`GET <p>healthz` → `200` when everything required works; `200` with `"status":"degraded"` when an
optional component is down; `503` **only** when the store is unusable or the WAL exceeds 256 MB —
the one condition that makes the service useless. The Docker `HEALTHCHECK` uses it (honouring
`URL_PREFIX`), so the container is restarted only for real failures.

```json
{ "status": "degraded",
  "version": "2026.09.04", "yt_dlp": "2026.8.30.232658.dev0",
  "boot_id": "01JBQ8YQ2E0000000000000000", "uptime_s": 43201,
  "url_prefix": "/", "v1_shim": true, "seq": 10293,
  "components": {
    "store":        { "status":"ok", "latency_ms":0.42, "wal_bytes":1048576, "db_bytes":41943040 },
    "queue":        { "status":"ok", "downloading":2, "postprocessing":0, "queued":5, "resolving":1,
                      "slots":{"global":{"total":3,"used":2},
                               "streamingcommunity":{"total":1,"used":0}},
                      "progress_dropped_total":0 },
    "pot":          { "status":"down", "pid":null, "restarts":3,
                      "last_exit":"exited with code 1", "endpoint":"http://127.0.0.1:4416",
                      "detail":"3 consecutive probe failures" },
    "ytdlp_runner": { "status":"ok", "python":"3.13.5", "yt_dlp":"2026.8.30.232658.dev0",
                      "plugins":["bgutil_ytdlp_pot_provider"] },
    "ffmpeg":       { "status":"ok", "version":"6.1.1" },
    "nm3u8dl":      { "status":"ok", "version":"v0.5.1-beta" },
    "deno":         { "status":"ok", "version":"2.x" },
    "ytdl_options": { "status":"ok", "update_time":1757000200.412, "presets":2 },
    "telegram":     { "status":"ok", "chats":2, "edits_throttled_total":11 },
    "jellyfin":     { "status":"ok", "last_success_at":1757000300000, "pending":false,
                      "runs_total":18, "failures_total":0 },
    "nfo":          { "status":"ok", "runs_total":7, "failures_total":0 },
    "audio_sync":   { "status":"ok", "runs_total":2, "failures_total":0, "phase":"pre_terminal" },
    "events":       { "status":"ok", "dropped":{"hooks":0,"telegram":0} },
    "subscriptions":{ "status":"ok", "total":7, "failing":1, "next_due_in_s":412 },
    "importer":     { "status":"ok", "imported_at":1757000000000, "warnings":2 }
  },
  "providers": [ { "id":"ytdlp", "state":"ready", "fallback":true },
                 { "id":"streamingcommunity", "state":"ready", "impersonating":true, "slots":1 },
                 { "id":"command:bandcamp", "state":"degraded",
                   "reason":"download.command[0] not executable" } ],
  "ws": { "clients":2, "frames_total":10293, "lagged_total":0, "slow_disconnects":0 } }
```

The `components` object above is **complete for the stock configuration** and is what WP-14
snapshot-tests: one entry per built-in hook (`jellyfin`, `nfo`, `audio_sync`) because §16.7 maps
`aulos_hook_runs_total{hook}` to `components.<hook>`, and an `events` entry because §16.7 maps
`aulos_event_dropped_total{subscriber}` to `components.events.dropped`. A community `[[hook]]` adds
one further entry keyed `hook:<dir>/<id>`, and a `command` plugin adds nothing here (plugins appear
under `providers`). §16.7 and this payload are asserted against each other in **both** directions,
so neither can grow a row the other lacks.

`GET <p>healthz?probe=deep` additionally re-runs the tool probes live (used by `doctor` and by the
runbook), rate-limited to one per 10 s. `GET <p>livez` returns `200 {"ok":true}` doing no work at
all, for load balancers. `GET <p>metrics` serves Prometheus text when
`AULOS_METRICS_ENABLED=true` (default `false`) — `metrics-exporter-prometheus` over the inventory
in **§16.7**, which is the single authoritative list of metric names, types and labels.

Note that `healthz` and `metrics` are two different surfaces with two different naming rules, on
purpose: the JSON above uses short, nested, human field names (`restarts`, `lagged_total`,
`edits_throttled_total`) because it is read by a person or by `jq`; `metrics` uses the flat
`aulos_*` names of §16.7 because it is read by Prometheus. Where both expose the same underlying
counter, §16.7 names the `healthz` path so the two can be reconciled.

### 16.4 Signals and graceful shutdown

| Signal | Behaviour |
|---|---|
| `SIGTERM` / `SIGINT` | 1. stop accepting HTTP (axum graceful); 2. close WS clients with `1001 "server shutting down"` so they reconnect rather than error; 3. stop the subscription scheduler and the Telegram poller; 4. **let in-flight downloads finish** for up to `AULOS_SHUTDOWN_GRACE_SECS` (20); 5. then `killpg SIGTERM` each job, 5 s, `SIGKILL`; 6. mark still-active items `queued` with `msg="Interrupted by shutdown"` so the next boot resumes them; 7. final aggregator flush; 8. drain the store actor, `wal_checkpoint(TRUNCATE)`, `PRAGMA optimize`, close; 9. `SIGTERM` the POT child; 10. `TaskTracker::wait()` with a 10 s ceiling, then exit 0. |
| `SIGHUP` | reload `YTDL_OPTIONS*` and re-scan `AULOS_PLUGINS_DIR` (`docker kill -s HUP` is a nice ops affordance). |
| `SIGQUIT` | log every task's state at ERROR and continue — a debug aid for a wedged container. |
| panic in a task | caught by the spawn wrapper, logged with the span; the owning item fails with `internal` and the request id. A panic in the **engine or store actor** is fatal by design: `abort()` after logging, because a corrupted queue is worse than a restart, and boot recovery is designed for exactly the hard-kill case. |

Hard kill (`SIGKILL`, OOM, host reboot): nothing is written at shutdown, so items remain
`downloading` in the DB and boot recovery (§8.9) converts exactly those to `queued`. WAL
guarantees no torn writes. The recovery unit tests seed every status directly.

### 16.5 Logging and tracing

- `tracing-subscriber` with `EnvFilter`. The base filter comes from `LOGLEVEL`; legacy's
  `dampenThirdPartyLoggers()` becomes default directives
  `hyper=warn,h2=warn,rustls=warn,reqwest=warn,teloxide=warn,notify=warn,html5ever=warn,tungstenite=warn`.
  `RUST_LOG` overrides everything (a documented escape hatch).
- `AULOS_LOG_FORMAT=json` emits one JSON object per line for log shippers; `text` is the compact
  human format, ANSI only when stderr is a TTY.
- Request ids: `tower-http::SetRequestIdLayer` accepting an inbound `X-Request-Id` (else a ULID)
  plus a `TraceLayer` span per request carrying `method, path, status, latency_ms, request_id`.
  `ENABLE_ACCESSLOG=false` puts that span at DEBUG, `true` at INFO — the same on/off knob as
  legacy.
- Every job gets a span `job{item_id, provider, url_host}`, and provider child stderr is logged
  inside it, so `grep item_id=01J…` yields the whole story of one download.
- **No eager formatting on hot paths.** Progress logging is `tracing::trace!` with structured
  fields, evaluated lazily. Legacy formatted an f-string per status message even with DEBUG off.
- Secrets — `TELEGRAM_BOT_TOKEN`, `JELLYFIN_API_KEY`, `AULOS_API_TOKEN`, and any `YTDL_OPTIONS`
  key matching `(?i)(cookie|password|passwd|token|key|secret|proxy)` — are replaced with
  `«redacted»` by a `Redact` newtype used in every `Debug`/`Display` impl and in `check-config`.

### 16.6 Auth and reverse-proxy posture

The VPS runs Authelia in front of the service and the server has no user model. That does not
change; what changes is that failures are honest.

| Concern | Design |
|---|---|
| Authentication | Delegated to the proxy. Cookies flow through untouched. When `AULOS_TRUSTED_PROXY_AUTH_HEADER` (e.g. `Remote-User`) is set, its absence on a v2 route yields `401 {"error":{"code":"unauthorized",…}}` — **never** a `303`. |
| Non-browser clients | `AULOS_API_TOKEN`, when set, accepts `Authorization: Bearer <token>` on any v1/v2 route as an alternative to proxy auth. Intended for the iOS Shortcut, the bookmarklet and `curl`. Compared in constant time. Default empty = disabled. |
| WebSocket auth | Cookies flow with the upgrade request; nothing extra is needed **provided the proxy forwards `Upgrade`/`Connection` for `<prefix>ws`** (a documented pre-flight check in the runbook). Where it cannot, `AULOS_API_TOKEN` is accepted as `Sec-WebSocket-Protocol: aulos.v2, bearer.<token>` or as `?token=<token>`. |
| CSRF | Every mutating v2 route requires `Content-Type: application/json`, so a cross-origin form POST cannot reach it, and CORS never grants credentials. |
| Path traversal | `folder`, `custom_name_prefix` and `chapter_template` reject `..` and leading separators; the resolved path is compared **component-wise** against the canonicalised base (`Path::components()` prefix match), which fixes the legacy `startswith` bug that let `/downloads-evil` pass as inside `/downloads`. Symlink escape is rejected. |
| SSRF | The Telegram guard (§12.3) is mandatory. The v1/v2 API adds run the same validator with `allow_private = AULOS_ALLOW_PRIVATE_TARGETS` (default `true` for the API, always `false` for Telegram), so a locked-down deployment can enable it everywhere. |
| Cookie upload | **1 000 000 bytes** — decimal, the legacy `limit 1_000_000` (legacy spec §2.1), *not* 1 MiB; a 1 020 000-byte file legacy rejected must still be rejected. Over the cap: v1 answers the byte-identical `Cookie file too large (max 1MB)`, v2 answers `413 payload_too_large` with the same message. Written atomically to `<STATE_DIR>/cookies.txt` mode `0600`, then registered as the `cookiefile` runtime override. |
| Container | Runs as `PUID:PGID` after the entrypoint drops privileges; no `CAP_*` required; a read-only root filesystem is supported (`/tmp` and the volumes are the only writable paths). |
| Secrets already leaked | `vps_setup.md` in the legacy tree contains a live Telegram token, a Jellyfin API key and a WireGuard private key. **Runbook step 1 is rotating all three.** `gitleaks` runs in CI and `.gitignore` covers the env file. |

### 16.7 Metric inventory

The complete set. Every name is `aulos_`-prefixed and follows Prometheus convention: `_total` for a
monotonic counter, `_seconds`/`_bytes` for units, no unit suffix on a gauge that names its own unit.
Individual counters named ad hoc elsewhere in this document resolve here. Adding a metric requires
adding a row.

| Metric | Type | Labels | `healthz` path | Meaning |
|---|---|---|---|---|
| `aulos_build_info` | gauge (always 1) | `version`, `yt_dlp`, `rustc` | `version`, `yt_dlp` | build identity |
| `aulos_uptime_seconds` | gauge | — | `uptime_s` | |
| `aulos_items` | gauge | `status` (the 8 values) | `components.queue.*` | current queue composition |
| `aulos_items_total` | counter | `status`, `source` | — | terminal transitions since boot |
| `aulos_slots_used` / `aulos_slots_total` | gauge | `pool` (`global` or a provider id) | `components.queue.slots` | §8.7 |
| `aulos_resolve_seconds` | histogram | `provider`, `outcome` | — | §8.4 |
| `aulos_download_seconds` | histogram | `provider`, `outcome` | — | |
| `aulos_downloaded_bytes_total` | counter | `provider` | — | |
| `aulos_progress_dropped_total` | counter | — | `components.queue.progress_dropped_total` | §2.3 progress drop policy |
| `aulos_event_dropped_total` | counter | `subscriber` | `components.events.dropped.<subscriber>` | §2.2.1 — a skipped hook or notification |
| `aulos_retries_total` | counter | `code`, `kind` (`auto`/`manual`) | — | §8.8 |
| `aulos_resolve_fallthrough_total` | counter | `from`, `to` | — | §6.4 / §8.4 — a resolve retried through the runner-up after `Unsupported` |
| `aulos_v1_add_resolve_total` | counter | `outcome` (`ok`/`error`/`timeout`/`disabled`) | — | §11.2 — the v1 add pre-resolve wait; a rising `timeout` share means `AULOS_V1_ADD_RESOLVE_WAIT_MS` is too low for this box |
| `aulos_group_drift_total` | counter | — | — | §8.6 accumulator correction |
| `aulos_store_write_seconds` | histogram | — | `components.store.latency_ms` | |
| `aulos_store_txn_total` | counter | `durability` | — | batching effectiveness |
| `aulos_store_ops_total` | counter | `op` (the `WriteOp` variant) | — | |
| `aulos_store_wal_bytes` | gauge | — | `components.store.wal_bytes` | 503 trigger above 256 MB |
| `aulos_store_busy_total` | counter | — | — | `state_unavailable` responses |
| `aulos_ws_clients` | gauge | — | `ws.clients` | |
| `aulos_ws_frames_total` | counter | `kind` (the `FrameKind`) | `ws.frames_total` | |
| `aulos_ws_frame_bytes` | histogram | `kind` | — | |
| `aulos_ws_lagged_total` | counter | — | `ws.lagged_total` | broadcast `Lagged` |
| `aulos_ws_slow_disconnects_total` | counter | — | `ws.slow_disconnects` | 1013 closes |
| `aulos_ws_tick_backoff_ms` | gauge | — | — | §15.1 frame-split backoff |
| `aulos_ws_resume_total` | counter | `outcome` (`merged`/`snapshot`/`up_to_date`) | — | §15.3 |
| `aulos_http_requests_total` | counter | `route`, `method`, `status` | — | |
| `aulos_http_request_seconds` | histogram | `route`, `method` | — | |
| `aulos_http_errors_total` | counter | `code` (the `ErrorCode`) | — | |
| `aulos_hook_runs_total` | counter | `hook`, `outcome` | `components.<hook>.runs_total` (one component per built-in hook: `jellyfin`, `nfo`, `audio_sync`; `hook:<dir>/<id>` for a community hook) | §13 |
| `aulos_hook_writebacks_total` | counter | `hook`, `kind` (`size`/`drop_entry_blob`) | — | §13.3 — engine-mediated hook writes |
| `aulos_hook_seconds` | histogram | `hook` | — | |
| `aulos_jellyfin_refreshes_total` | counter | `mode` (`global`/`targeted`), `outcome` | `components.jellyfin` | §13.1 |
| `aulos_subscription_checks_total` | counter | `outcome` | `components.subscriptions` | §14.2 |
| `aulos_subscription_items_queued_total` | counter | — | — | |
| `aulos_subscriptions_failing` | gauge | — | `components.subscriptions.failing` | |
| `aulos_telegram_messages_total` | counter | `kind` (`send`/`edit`), `outcome` | `components.telegram` | §12.4 |
| `aulos_telegram_edits_throttled_total` | counter | `reason` (`limiter`/`unchanged`/`retry_after`) | `components.telegram.edits_throttled_total` | |
| `aulos_pot_restarts_total` | counter | `reason` (`exit`/`probe`) | `components.pot.restarts` | §16.2 |
| `aulos_pot_up` | gauge (0/1) | — | `components.pot.status` | |
| `aulos_provider_state` | gauge | `provider`, `state` | `providers[]` | §6.4 |
| `aulos_child_processes` | gauge | `kind` (`ytdlp`/`sc`/`plugin`/`hook`) | — | leak detector |
| `aulos_config_reloads_total` | counter | `target`, `outcome` | `components.ytdl_options` | §17.2 |
| `aulos_import_warnings` / `aulos_import_errors` | gauge | — | `components.importer` | §7.6.6 |

---

## 17. Configuration (`aulos-core::config`)

### 17.1 Loading algorithm (semantics-compatible with legacy)

```rust
pub struct RawEnv(BTreeMap<String, String>);       // every key defaulted, all values strings
pub struct Config { /* typed fields */ }
pub fn load(env: &RawEnv) -> Result<Config, Vec<ConfigError>>;
```

1. Start from the `DEFAULTS` table (§17.3) and overlay `std::env::vars()`. **Everything is a
   string at this stage**, exactly like legacy.
2. `%%INDIRECTION`: a value starting with `%%` is replaced by the value of the named key
   (`AUDIO_DOWNLOAD_DIR=%%DOWNLOAD_DIR`, `TEMP_DIR=%%DOWNLOAD_DIR`). Resolution is iterative with
   a cycle check; a cycle or an unknown target is a fatal config error (legacy raised
   `AttributeError`; we report it).
3. Booleans accept **exactly** `true|false|True|False|on|off|1|0`; the truthy set is
   `{true, True, on, 1}`. Anything else is `INVALID_BOOLEAN`. The key list is legacy's `_BOOLEAN`
   plus the new `AULOS_*` booleans.
4. `URL_PREFIX`: append `/` if missing (so `""` → `"/"`) **and** prepend `/` if missing. Legacy did
   not prepend, so `URL_PREFIX=metube` produced routes like `metubeadd`; we normalise and log a
   WARN. One `Prefix` newtype then builds every path in the process — routes, the WS path,
   `download_url`, and the healthcheck — so prefix drift is structurally impossible.
5. `PUBLIC_HOST_URL`, `PUBLIC_HOST_AUDIO_URL`: append `/` only when non-empty.
6. `YTDL_OPTIONS_FILE`, `YTDL_OPTIONS_PRESETS_FILE`: values starting with `.` are canonicalised to
   absolute paths relative to the cwd, as legacy's `Path().resolve()` did.
7. Numbers are parsed with legacy's per-key leniency (documented in the table):
   `CLEAR_COMPLETED_AFTER` invalid ⇒ log + 0; `JELLYFIN_SYNC_TIMEOUT_SECONDS` invalid ⇒ warn + 20;
   `PORT` and `MAX_CONCURRENT_DOWNLOADS` invalid ⇒ **fatal** (legacy crashed on them anyway).
8. `YTDL_OPTIONS` + `YTDL_OPTIONS_FILE` and the presets pair are loaded (§17.2). A failure of
   either exits non-zero with the **exact legacy message strings**.
9. All errors are collected and printed as a table, then `exit(2)` — legacy exited on the first
   one. `aulos-server check-config` prints the effective config and exits 0/1 without binding.

Unknown `AULOS_*` variables are **rejected at boot** (typo protection: `AULOS_WS_BATCH_MS` vs
`AULOS_WS_BATCH_MSEC` is a silent no-op otherwise), with **one documented exception list**: names
marked *accepted and ignored* in §17.3 are recognised and skipped. That list exists because
`AULOS_*` is also the namespace the test harness and CI use — `AULOS_E2E` (BRIEF §17) is exported
into the container by `tests/e2e/run.sh`, and an `--env-file` or a CI job that passes its whole
environment would otherwise make the server exit 2. The reserved prefix `AULOS_E2E_*` is likewise
accepted and ignored in full. Unknown non-`AULOS_` variables are ignored — a container inherits a
lot.

### 17.2 `YTDL_OPTIONS` layering and hot reload

```rust
pub struct YtdlOptions {
    pub base: Map<String, Value>,          // env YTDL_OPTIONS, then FILE merged OVER it
    pub presets: BTreeMap<String, Map<String, Value>>,
    pub overrides: Map<String, Value>,     // runtime overrides (cookiefile)
    pub file_mtime: Option<f64>,
    pub loaded_at: Instant,
}
impl YtdlOptions {
    /// base → presets in request order → per-request overrides.
    /// `null` values are KEPT, so a preset can clear a global `download_archive`.
    pub fn layer(&self, presets: &[Box<str>], overrides: &Map<String, Value>) -> Map<String, Value>;
}
```

Held in `Arc<ArcSwap<YtdlOptions>>`. A job snapshots it at spawn, so a reload never mutates an
in-flight job's options. Error strings are preserved verbatim for the wire:
`Environment variable YTDL_OPTIONS is invalid`, `File "<path>" not found`,
`YTDL_OPTIONS_FILE contents is invalid`, and the presets analogues.

**Δ (better):** on a reload failure we keep the **last-good** `YtdlOptions` and report the error.
Legacy re-read `YTDL_OPTIONS` from env first and then failed, silently discarding the file's whole
contribution until the next successful reload — so a typo quietly changed download behaviour.
Ours changes nothing until the file parses.

`set_runtime_override("cookiefile", path)` / `remove_runtime_override` write into `kv` and are
re-applied after every reload, as legacy. On boot, if `<STATE_DIR>/cookies.txt` exists the override
is set (legacy did this only inside its `__main__` block).

**`ConfigWatcher`** — legacy used `watchfiles.awatch(<file>)` with a `samefile` filter, which has a
real failure mode: editors, `docker cp` and Ansible **replace** the file (`rename(tmp, target)`),
invalidating an inode-level watch. We watch the **parent directory**, non-recursively:

1. For each non-empty target, canonicalise it and `watch(parent, NonRecursive)`. Shared parents
   share one watch.
2. Accept an event iff `event.paths` contains a path whose **file name** equals the target's file
   name **and** the kind is `Create`, `Modify(Data|Any|Name)` or `Remove`. (Rename events are what
   an atomic replace produces; legacy's `{modified, added, deleted}` set maps to the same three.)
3. Coalesce with a `AULOS_CONFIG_DEBOUNCE_MS` (250) timer, so a `for` loop of `sed -i` edits
   triggers one reload.
4. Reload → on success `ArcSwap::store`, on failure keep the old one.
5. Publish `DomainEvent::YtdlOptionsReloaded { ok, msg, update_time }` where `update_time` is the
   mtime as fractional epoch seconds or `null` — the exact legacy payload — which becomes the WS
   `ytdl_options` frame.
6. A **deleted** file makes the reload fail with `File "<path>" not found`, keeps the last-good
   options, and marks `healthz.components.ytdl_options` degraded. Re-creating the file heals it,
   because the directory watch is still live. Legacy left you with env-only options and no way back
   short of a restart.
7. `POST <p>api/v2/ytdl-options/reload` forces the same path synchronously — the supported answer
   for a `/config` on NFS/SMB where inotify never fires.
8. An `AULOS_CONFIG_POLL_SECS` (30, 0 = off) fallback compares `(mtime, size)`; `notify`'s
   `PollWatcher` is used automatically when the inotify backend is unavailable (containers with
   exhausted `fs.inotify` limits).
9. **The presets file is watched too**, by the same machinery. Legacy did not, despite its README
   claiming otherwise.

### 17.3 Complete env var table

Legend: **L** = legacy name, meaning and default preserved · **L\*** = legacy name, behaviour note
in the Notes column · **N** = new (`AULOS_*`).

| Env var | Default | Type | Notes | |
|---|---|---|---|---|
| `DOWNLOAD_DIR` | `.` (image `/downloads`) | path | base dir for video/other; served at `<p>download/` | L |
| `AUDIO_DOWNLOAD_DIR` | `%%DOWNLOAD_DIR` | path | used when `download_type == audio`; served at `<p>audio_download/` | L |
| `TEMP_DIR` | `%%DOWNLOAD_DIR` | path | yt-dlp `paths.temp`; N_m3u8DL-RE `--tmp-dir` | L |
| `DOWNLOAD_DIRS_INDEXABLE` | `false` | bool | directory listing on the file routes; now a JSON listing, not HTML | L\* |
| `CUSTOM_DIRS` | `true` | bool | allows `folder`; gates `api/v2/custom-dirs` | L |
| `CREATE_CUSTOM_DIRS` | `true` | bool | `create_dir_all` a missing `folder` instead of erroring | L |
| `CUSTOM_DIRS_EXCLUDE_REGEX` | `(^\|/)[.@].*$` | regex, empty = none | invalid regex is now fatal at boot, not at first request | L\* |
| `DELETE_FILE_ON_TRASHCAN` | `false` | bool | also deletes chapter/subtitle/`.info.json`/`.nfo` siblings | L\* |
| `STATE_DIR` | `.` (image `/downloads/.metube`) | path | importer input, `cookies.txt`, default DB dir | L |
| `URL_PREFIX` | `''` → `/` | str | a missing leading `/` is now added, with a WARN | L\* |
| `PUBLIC_HOST_URL` | `download/` | str | prefix of `download_url` for non-audio items | L |
| `PUBLIC_HOST_AUDIO_URL` | `audio_download/` | str | prefix of `download_url` for audio items | L |
| `OUTPUT_TEMPLATE` | `%(title)s.%(ext)s` | str | yt-dlp `outtmpl.default` | L |
| `OUTPUT_TEMPLATE_CHAPTER` | `%(title)s - %(section_number)02d - %(section_title)s.%(ext)s` | str | `outtmpl.chapter`; the default request `chapter_template`; exposed in capabilities | L |
| `OUTPUT_TEMPLATE_PLAYLIST` | `%(playlist_title)s/%(title)s.%(ext)s` | str, empty = keep default | used when the entry has `playlist_index` | L |
| `OUTPUT_TEMPLATE_CHANNEL` | `%(channel)s/%(title)s.%(ext)s` | str, empty = keep | used when the entry has `channel_index` | L |
| `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` | `0` | int (0 = unlimited) | a **number** in v2, a **string** in the v1 shim (legacy never coerced it) | L\* |
| `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` | `60` | int minutes | same number/string split | L\* |
| `SUBSCRIPTION_SCAN_PLAYLIST_END` | `50` | int | `playlistend` for subscription scans (`max(…,1)` on add) | L |
| `SUBSCRIPTION_MAX_SEEN_IDS` | `50000` | int | `PruneSeen` cap | L |
| `CLEAR_COMPLETED_AFTER` | `0` | int s (invalid ⇒ 0 + error log) | now **survives restarts** and applies to items aged out of memory | L\* |
| `YTDL_OPTIONS` | `{}` | JSON object (else fatal) | | L |
| `YTDL_OPTIONS_FILE` | `''` | path | merged **over** `YTDL_OPTIONS`; hot-reloaded | L |
| `YTDL_OPTIONS_PRESETS` | `{}` | JSON object of objects | must be `dict[str, dict]` | L |
| `YTDL_OPTIONS_PRESETS_FILE` | `''` | path | **now watched** (the legacy README claimed it was) | L\* |
| `ALLOW_YTDL_OPTIONS_OVERRIDES` | `false` | bool | a non-empty `ytdl_options_overrides` is 400 when false | L |
| `CORS_ALLOWED_ORIGINS` | `''` | comma list, `*` = all | v2 also sends `Access-Control-Allow-Methods` | L\* |
| `ROBOTS_TXT` | `''` | path | resolved under `BASE_DIR` | L |
| `HOST` | `0.0.0.0` | str | bind address | L |
| `PORT` | `8081` | int (fatal) | bind port | L |
| `HTTPS` | `false` | bool | TLS via `CERTFILE`/`KEYFILE` | L |
| `CERTFILE` / `KEYFILE` | `''` | path | PEM only, loaded with `rustls-pemfile` | L |
| `BASE_DIR` | `''` | path | now used **only** to resolve `ROBOTS_TXT`; UI serving is out of scope | L\* |
| `DEFAULT_THEME` | `auto` | `light\|dark\|auto` | accepted and echoed in capabilities; **no cookie is set** (no web UI) | L\* |
| `MAX_CONCURRENT_DOWNLOADS` | `3` | int ≥ 1 (fatal) | global download slots | L |
| `LOGLEVEL` | `INFO` | str (unknown ⇒ INFO + warn) | tracing filter base | L |
| `ENABLE_ACCESSLOG` | `false` | bool | request span at INFO vs DEBUG | L |
| `SC_THREAD_COUNT` | `16` | int | `N_m3u8DL-RE --thread-count`; now read from `Config`, not re-read from env in a child | L\* |
| `SC_USE_FFMPEG` | `false` | bool | force ffmpeg for SC; single source of truth | L\* |
| `SC_MAX_CONCURRENT_DOWNLOADS` | `1` | int ≥ 1 | SC provider slots, acquired **instead of** a global slot | L |
| `JELLYFIN_SYNC_ENABLED` | `false` | bool | arms the jellyfin hook | L |
| `JELLYFIN_URL` | `''` | str (trailing `/` stripped) | | L |
| `JELLYFIN_API_KEY` | `''` | secret str | redacted everywhere | L |
| `JELLYFIN_SYNC_TIMEOUT_SECONDS` | `20` | float (invalid ⇒ warn + 20) | | L |
| `JELLYFIN_LIBRARY_ID` | `''` | str | **now implemented**: targeted `Items/{id}/Refresh` | L\* |
| `JELLYFIN_METADATA_REFRESH_MODE` | `Default` | `None\|ValidationOnly\|Default\|FullRefresh` | **now implemented** | L\* |
| `JELLYFIN_IMAGE_REFRESH_MODE` | `Default` | same set | **now implemented** | L\* |
| `TELEGRAM_BOT_ENABLED` | `false` | bool | | L |
| `TELEGRAM_BOT_TOKEN` | `''` | secret str | empty ⇒ the bot logs an error and does not start | L |
| `TELEGRAM_ALLOWED_CHAT_IDS` | `''` | comma list of i64 | empty ⇒ the bot refuses to start (kept) | L |
| `TELEGRAM_STALL_TIMEOUT_SECONDS` | `180` | int | bot stall warning | L |
| `TELEGRAM_HARD_TIMEOUT_SECONDS` | `7200` | int | bot "taking longer" warning | L |
| `TELEGRAM_MAX_URLS_PER_MESSAGE` | `10` | int | | L |
| `METUBE_VERSION` | `dev` | str | reported by `/version` and `healthz`; `AULOS_VERSION` is an accepted alias | L |
| `PLUGINS_DIR` | `/config/plugins` | path | the default for `AULOS_PLUGINS_DIR`. **Not a legacy variable** — it appears in neither `_DEFAULTS` nor anywhere else in the Python source; it is introduced by BRIEF §9 and is un-prefixed contrary to BRIEF §15, which §23.1 records as a deliberate deviation | N\* |
| `PUID` / `PGID` / `UID` / `GID` / `UMASK` / `CHOWN_DIRS` | `1000` / `1000` / — / — / `022` / `true` | entrypoint | `UID`/`GID` still win over `PUID`/`PGID`; `CHOWN_DIRS` gains a `recursive` value (§18.2) | L\* |
| `DOTNET_SYSTEM_GLOBALIZATION_INVARIANT` | `1` | image ENV | required by N_m3u8DL-RE (.NET) | L |
| `AULOS_DB_PATH` | `<STATE_DIR>/aulos.db` | path | kept inside `STATE_DIR` so cutover needs no compose change (T1) | N |
| `AULOS_DB_READERS` | `4` | int | read-pool size | N |
| `AULOS_DB_FLUSH_MS` | `200` | int | write-batching window | N |
| `AULOS_DB_SYNCHRONOUS` | `NORMAL` | `NORMAL\|FULL` | `FULL` for hosts with unreliable power | N |
| `AULOS_WS_BATCH_MS` | `250` | int 50..5000 | progress delta cadence (BRIEF §3) | N |
| `AULOS_WS_URGENT_MS` | `25` | int 0..1000 | promptness floor for status/added/completed/removed/notice | N |
| `AULOS_WS_MAX_DELTAS_PER_FRAME` | `200` | int | frame split threshold | N |
| `AULOS_WS_REPLAY_FRAMES` | `512` | int | replay-ring depth | N |
| `AULOS_WS_REPLAY_BYTES` | `4194304` | int | replay-ring byte cap | N |
| `AULOS_WS_MAX_CLIENTS` | `64` | int | hard cap on concurrent sockets | N |
| `AULOS_WS_SEND_TIMEOUT_MS` | `5000` | int | close a wedged socket with 1013 | N |
| `AULOS_MEM_DONE_ITEMS` | `500` | int | in-memory completed window; older history is paged from SQLite | N |
| `AULOS_SNAPSHOT_GROUP_INLINE` | `50` | int | inline a group's children up to this size | N |
| `AULOS_RESOLVE_CONCURRENCY` | `4` | int | resolution pool, separate from download slots | N |
| `AULOS_RESOLVE_TIMEOUT_SECS` | `120` | int | per-resolve deadline | N |
| `AULOS_RESOLVE_MAX_DEPTH` | `3` | int | redirect-entry recursion cap | N |
| `AULOS_RESOLVE_FALLTHROUGH` | `true` | bool | on `ProviderError::Unsupported`, retry the resolve **once** through a `Ready` runner-up provider (§6.4, §8.4). `false` = every provider's `Unsupported` is terminal | N |
| `AULOS_SCHED_LOOKAHEAD` | `32` | int | anti-head-of-line scan depth | N |
| `AULOS_MAX_BATCH_URLS` | `500` | int | batch-add cap; above ⇒ 413 | N |
| `AULOS_DEDUPE_MODE` | `active` | `off\|active\|strict` | §8.5 | N |
| `AULOS_JOB_STALL_SECS` | `900` | int (0 = off) | no-frame watchdog, warn only | N |
| `AULOS_JOB_TIMEOUT_SECS` | `0` | int (0 = off) | hard per-job wall clock, cancels | N |
| `AULOS_KILL_GRACE_MS` | `5000` | int | SIGTERM → SIGKILL grace for a process group | N |
| `AULOS_AUTO_RETRY_MAX` | `2` | int (0 = off) | automatic retry of retryable errors only | N |
| `AULOS_RESTART_POLICY` | `resume` | `resume\|pause` | what boot recovery does with in-flight items | N |
| `AULOS_CLEAN_ORPHAN_TEMP` | `false` | bool | delete orphan `*.part`/`*.ytdl` at boot | N |
| `AULOS_ENTRY_MAX_BYTES` | `262144` | int | entry-blob hard cap | N |
| `AULOS_CUSTOM_DIRS_MAX_DEPTH` | `8` | int | bounds the custom-dirs walk | N |
| `AULOS_PLUGINS_DIR` | `${PLUGINS_DIR:-/config/plugins}` | path | provider and hook manifests | N |
| `AULOS_PLUGINS_ENABLED` | `true` | bool | kill switch for all discovery | N |
| `AULOS_PLUGIN_TIMEOUT_RESOLVE` | `60` | int | default plugin resolve timeout | N |
| `AULOS_HOOKS_ENABLED` | `true` | bool | kill switch for the hook dispatcher | N |
| `AULOS_SC_HTTP` | `auto` | `auto\|impersonate\|plain` | SC HTTP client | N |
| `AULOS_SC_META_CONCURRENCY` | `4` | int | concurrent season fetches for a multi-season title | N |
| `AULOS_SC_EXTRA_HOSTS` | `''` | comma list | additional SC mirror host substrings | N |
| `AULOS_SC_USE_OUTPUT_TEMPLATE` | `false` | bool | opt in to template naming for SC (default keeps legacy naming) | N |
| `AULOS_POT_ENABLED` | `true` | bool | supervise `bgutil-pot` | N |
| `AULOS_POT_CMD` | `bgutil-pot server` | argv str | | N |
| `AULOS_POT_URL` | `http://127.0.0.1:4416` | str | health probe target | N |
| `AULOS_POT_MAX_RESTARTS` | `10` | int | per 10 min, then `failed` | N |
| `AULOS_JELLYFIN_DEBOUNCE_SECS` | `30` | int | trailing debounce | N |
| `AULOS_JELLYFIN_MAX_WAIT_SECS` | `300` | int | debounce cap, so a long playlist still refreshes | N |
| `AULOS_NFO_ENABLED` | `true` | bool | NFO hook for SC items | N |
| `AULOS_NFO_DELETE_INFO_JSON` | `false` | bool | legacy's CLI always deleted it; opt-in here | N |
| `AULOS_TELEGRAM_BOARD` | `board` | `board\|per_job` | live board vs one message per job | N |
| `AULOS_TELEGRAM_EDIT_INTERVAL_MS` | `3000` | int | per-chat edit budget | N |
| `AULOS_TELEGRAM_WATCH_ALL` | `true` | bool | report web/subscription jobs too; `false` = exact legacy blind spot | N |
| `AULOS_SUB_CHECK_CONCURRENCY` | `2` | int | concurrent subscription checks | N |
| `AULOS_SUB_CHECK_TIMEOUT_SECS` | `180` | int | per-check timeout | N |
| `AULOS_SUB_BACKOFF_MAX_SECS` | `21600` | int | backoff cap (6 h) | N |
| `AULOS_SUB_FIRST_CHECK_DELAY_SECS` | `10` | int | first check after boot, plus 0–30 s jitter | N |
| `AULOS_CONFIG_DEBOUNCE_MS` | `250` | int | option-file reload debounce | N |
| `AULOS_CONFIG_POLL_SECS` | `30` | int (0 = off) | mtime/size poll fallback for network mounts | N |
| `AULOS_LOG_FORMAT` | `text` | `text\|json` | | N |
| `AULOS_V1_ENABLED` | `true` | bool | mounts the v1 shim; set `false` after the v2 client ships | N |
| `AULOS_API_TOKEN` | `''` | secret str | optional bearer token for non-browser clients | N |
| `AULOS_TRUSTED_PROXY_AUTH_HEADER` | `''` | str | when set, its absence on a v2 route is a 401 | N |
| `AULOS_ALLOW_PRIVATE_TARGETS` | `true` | bool | SSRF guard for API adds (Telegram is always guarded) | N |
| `AULOS_METRICS_ENABLED` | `false` | bool | serve `<p>metrics` in Prometheus text format | N |
| `AULOS_SHUTDOWN_GRACE_SECS` | `20` | int | let in-flight downloads finish before killing them | N |
| `AULOS_VERSION` | `dev` | str | alias of `METUBE_VERSION`; the Dockerfile sets both | N |
| `AULOS_IMPORT_ON_ERROR` | `fail` | `fail\|skip` | what a legacy **file** error does: `fail` = rollback + exit non-zero (default); `skip` = omit that file, downgrade to a warning, commit, and mark the importer component degraded for the life of the process. `aulos-server import --skip-corrupt` sets `skip`. This is the documented escape from a restart loop caused by one corrupt legacy JSON file (§7.6.1) | N |
| `AULOS_V1_ADD_RESOLVE_WAIT_MS` | `10000` | int (0 = off) | how long v1 `POST <p>add` waits for resolution before answering, so a resolution failure is still reported as `{"status":"error","msg":…}` (§11.2). `0` makes the v1 route fully async and knowingly gives up body-level error reporting; the v2 add route is never affected | N |
| `AULOS_V1_HISTORY_MAX` | `0` | int (0 = unlimited) | cap on the v1 `GET <p>history` `done[]` array. `0` reproduces legacy exactly (the whole completed set). A non-zero value keeps the most recent N by `ord` and logs a WARN naming the cap once per hour (§11.4) | N |
| `AULOS_E2E`, `AULOS_E2E_*` | — | — | **accepted and ignored.** The `tests/e2e` harness marker of BRIEF §17; listed here so the unknown-`AULOS_*` check of §17.1 does not reject an environment that exports it | N |

---

## 18. Packaging and CI

### 18.1 Dockerfile

```dockerfile
# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.95
ARG DEBIAN=bookworm

# ---------- builder (cargo-chef for a cached dependency layer) ----------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-${DEBIAN} AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETPLATFORM
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    set -eux; case "$TARGETPLATFORM" in \
      linux/amd64) T=x86_64-unknown-linux-gnu;  P="" ;; \
      linux/arm64) T=aarch64-unknown-linux-gnu; P="gcc-aarch64-linux-gnu g++-aarch64-linux-gnu" ;; \
      *) echo "unsupported $TARGETPLATFORM" >&2; exit 1 ;; esac; \
    echo "$T" > /target.txt; rustup target add "$T"; \
    if [ -n "$P" ]; then apt-get update && apt-get install -y --no-install-recommends $P; fi
COPY --from=planner /src/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo chef cook --release --target "$(cat /target.txt)" --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked --target "$(cat /target.txt)" -p aulos-server && \
    cp "target/$(cat /target.txt)/release/aulos-server" /aulos-server

# ---------- runtime ----------
FROM debian:${DEBIAN}-slim
ARG TARGETARCH
ARG YTDLP_VERSION=2026.8.30.232658.dev0
ARG BGUTIL_TAG=v1.2.3
ARG NM3U8DL_VERSION=v0.5.1-beta
ARG NM3U8DL_BUILD=20251029
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl unzip file tini gosu \
      ffmpeg aria2 coreutils python3 python3-pip libssl3 libstdc++6 \
 && rm -rf /var/lib/apt/lists/* && mkdir -p /.cache && chmod 777 /.cache

# yt-dlp: the nightly pin, exactly as the legacy image did
RUN pip3 install --break-system-packages --no-cache-dir --no-deps "yt-dlp==${YTDLP_VERSION}"

# deno (yt-dlp[deno] / yt-dlp-ejs JS challenge solver)
RUN curl -fsSL https://deno.land/install.sh | DENO_INSTALL=/usr/local sh -s -- -y

# BgUtils POT: sidecar binary + yt-dlp plugin into site-packages
RUN set -eux; case "$TARGETARCH" in amd64) A=x86_64 ;; arm64) A=aarch64 ;; \
      *) echo "unsupported $TARGETARCH" >&2; exit 1 ;; esac; \
    B=https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/${BGUTIL_TAG}; \
    curl -fL -o /usr/local/bin/bgutil-pot "$B/bgutil-pot-linux-${A}"; \
    chmod +x /usr/local/bin/bgutil-pot; \
    PD="$(python3 -c 'import site; print(site.getsitepackages()[0])')"; \
    curl -fL -o /tmp/p.zip "$B/bgutil-ytdlp-pot-provider-rs.zip"; \
    unzip -oq /tmp/p.zip -d "$PD"; rm /tmp/p.zip

# N_m3u8DL-RE (StreamingCommunity HLS)
RUN set -eux; A=$([ "$TARGETARCH" = "arm64" ] && echo arm64 || echo x64); \
    curl -fL "https://github.com/nilaoda/N_m3u8DL-RE/releases/download/${NM3U8DL_VERSION}/N_m3u8DL-RE_${NM3U8DL_VERSION}_linux-${A}_${NM3U8DL_BUILD}.tar.gz" \
      -o /tmp/n.tgz && tar -xzf /tmp/n.tgz -C /usr/local/bin \
      && chmod +x /usr/local/bin/N_m3u8DL-RE && rm /tmp/n.tgz

COPY --from=builder /aulos-server /usr/local/bin/aulos-server
COPY crates/aulos-provider-ytdlp/python/ytdlp_runner.py /app/python/ytdlp_runner.py
COPY docker/entrypoint.sh /usr/local/bin/aulos-entrypoint
RUN sed -i 's/\r$//' /usr/local/bin/aulos-entrypoint && chmod +x /usr/local/bin/aulos-entrypoint
COPY plugins/examples /opt/aulos/plugins-examples

ENV PUID=1000 PGID=1000 UMASK=022 \
    DOWNLOAD_DIR=/downloads STATE_DIR=/downloads/.metube TEMP_DIR=/downloads \
    PORT=8081 SC_USE_FFMPEG=false \
    DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1 \
    AULOS_PLUGINS_DIR=/config/plugins \
    PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 RUST_BACKTRACE=1
VOLUME /downloads
EXPOSE 8081
HEALTHCHECK --interval=30s --timeout=5s --start-period=25s --retries=3 \
  CMD ["/usr/local/bin/aulos-server","healthcheck"]
ARG VERSION=dev
ENV METUBE_VERSION=$VERSION AULOS_VERSION=$VERSION
ENTRYPOINT ["/usr/bin/tini","-g","--","/usr/local/bin/aulos-entrypoint"]
CMD ["serve"]
```

Two details in that block are load-bearing:

- **The healthcheck is `aulos-server healthcheck` (§3.1), not a shell `curl`.** Interpolating
  `${URL_PREFIX:-/}` in the container shell bypasses the normalisation of §17.1: with
  `URL_PREFIX=metube` — the exact input C16 exists to fix — it yields
  `http://127.0.0.1:8081metubehealthz` and a permanently unhealthy container. The subcommand loads
  the same `Config` and builds the URL through the same `Prefix` newtype, so prefix drift is
  structurally impossible here too (risk R12), and the check no longer depends on `curl` being in
  the image.
- **`CMD ["serve"]`.** The entrypoint ends in `exec … aulos-server "$@"`, so with no `CMD` the
  binary would be invoked with no arguments. The CLI type is
  `Cli { #[command(subcommand)] cmd: Option<Cmd> }` with `None ⇒ Serve` (§3.1), so a bare
  invocation works, and `CMD ["serve"]` states it explicitly — belt and braces, and it also makes
  `docker run <img> doctor` do the obvious thing.

Notes: no Node stage (≈300 MB and a whole toolchain gone). Rust cross-compiles on the build host,
so the runtime stage's few `RUN`s are the only thing that would ever need emulation. `cargo-chef`
keeps dependency compilation cached, so a yt-dlp bump PR rebuilds in ~2 minutes because only the
runtime stage changes. `BGUTIL_TAG`, `NM3U8DL_VERSION` and `NM3U8DL_BUILD` are **pinned**:
resolving `latest` at build time (as legacy did for the POT provider) made the image
non-reproducible and could break a build with no repo change. The Dockerfile is kept fully
`TARGETARCH`/`TARGETPLATFORM`-parametrised, but **CI builds `linux/amd64` only** (BRIEF §16) — no
QEMU, no arm64 — so arm64 is a one-line workflow change later.

### 18.2 `docker/entrypoint.sh`

```sh
#!/bin/sh
set -eu
PUID="${UID:-$PUID}"          # legacy UID/GID still win, exactly as before
PGID="${GID:-$PGID}"
echo "Setting umask to ${UMASK}"
umask "${UMASK}"
echo "Creating download (${DOWNLOAD_DIR}), state (${STATE_DIR}), temp (${TEMP_DIR}) directories"
mkdir -p "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}" "${AUDIO_DOWNLOAD_DIR:-$DOWNLOAD_DIR}"

if [ "$(id -u)" -eq 0 ] && [ "$(id -g)" -eq 0 ]; then
  [ "${PUID}" -eq 0 ] && echo "Warning: running as root is not recommended; check PUID/PGID (or legacy UID/GID)"
  case "${CHOWN_DIRS:-true}" in
    false)     : ;;
    recursive) echo "Changing ownership recursively (legacy behaviour)"
               chown -R "${PUID}:${PGID}" "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}" ;;
    *)         echo "Changing ownership of the directories themselves and the state dir"
               chown    "${PUID}:${PGID}" "${DOWNLOAD_DIR}" "${TEMP_DIR}"
               chown -R "${PUID}:${PGID}" "${STATE_DIR}" ;;
  esac
  echo "Running aulos-server as ${PUID}:${PGID}"
  exec gosu "${PUID}:${PGID}" /usr/local/bin/aulos-server "$@"
else
  echo "User set by docker; running aulos-server as $(id -u):$(id -g)"
  exec /usr/local/bin/aulos-server "$@"
fi
```

`bgutil-pot` is **not** started here — the server supervises it, so it inherits the already-dropped
privileges and gets restarted when it dies.

`CHOWN_DIRS` gains a `recursive` value for exact legacy behaviour. `true` now means "the
directories themselves plus the state dir", which is the useful part and is O(1) instead of
O(library size): legacy `chown -R`'d the entire downloads volume on **every** container start,
which on a multi-TB library takes minutes. `/app` is no longer chowned at all (nothing there is
written at runtime). The mode used is logged in the first line of output, and it is listed as a
change in Appendix B.

### 18.3 Compose example

```yaml
services:
  aulos:
    image: ghcr.io/tatoalo/aulos-server:latest
    container_name: aulos
    restart: unless-stopped
    ports: ["8081:8081"]
    environment:
      PUID: "1000"
      PGID: "1000"
      UMASK: "022"
      CHOWN_DIRS: "false"
      DOWNLOAD_DIR: /downloads
      AUDIO_DOWNLOAD_DIR: /downloads/audio
      STATE_DIR: /downloads/.metube          # unchanged: the importer reads it
      TEMP_DIR: /downloads/.tmp
      MAX_CONCURRENT_DOWNLOADS: "3"
      SC_MAX_CONCURRENT_DOWNLOADS: "1"
      YTDL_OPTIONS_FILE: /config/ytdl-options.json
      JELLYFIN_SYNC_ENABLED: "true"
      JELLYFIN_URL: http://jellyfin:8096
      JELLYFIN_API_KEY: ${JELLYFIN_API_KEY}
      JELLYFIN_LIBRARY_ID: ${JELLYFIN_LIBRARY_ID}
      TELEGRAM_BOT_ENABLED: "true"
      TELEGRAM_BOT_TOKEN: ${TELEGRAM_BOT_TOKEN}
      TELEGRAM_ALLOWED_CHAT_IDS: ${TELEGRAM_ALLOWED_CHAT_IDS}
    volumes:
      - /srv/media:/downloads
      - /srv/aulos/config:/config
```

### 18.4 GitHub Actions

| Workflow | Trigger | Jobs |
|---|---|---|
| `ci.yml` | PR + push to `master` (`paths-ignore: **.md`) | `fmt` (`cargo fmt --check`) · `clippy` (`--all-targets --all-features -- -D warnings`) · `test` (`cargo test --workspace --locked`, `Swatinem/rust-cache`) · `arch` (`cargo test -p aulos-workspace-tests arch`) · `deny` (`cargo deny check advisories bans licenses sources`) · `python` (`ruff` + `py_compile` on `ytdlp_runner.py`, plus the shim contract test that pipes a canned job to it with a stubbed `yt_dlp`) · `coverage` (`cargo llvm-cov`; floors: workspace 70 %, `aulos-store::import` **90 %**, `aulos-api::v1` **90 %**) · `schema` (`print-schema` snapshot) · `gitleaks` |
| `docker.yml` | push to `master`, tags `v*`, manual dispatch | Buildx, **`linux/amd64` only**, GHA build cache, `VERSION=$(date +%Y.%m.%d)`, tags `ghcr.io/<repo>:latest`, `:<date>`, `:sha-<short>` (+ `:<tag>` on tags). Then `e2e` with `AULOS_E2E=1` on the built image, then `trivy image` (CRITICAL/HIGH, non-blocking) and a `syft` SBOM attached to the release. Push only on `master`/tags or explicit dispatch. |
| `dev-build.yml` | PR labelled/synchronised/closed | if the PR carries `dev`: amd64 build, `VERSION=dev-pr<N>`, push `ghcr.io/<repo>:dev`, comment on the PR. On close: delete the `dev` package version and comment. Ported unchanged. |
| `update-yt-dlp.yml` | cron `0 0 */3 * *` + manual | ported and hardened, §18.5 |
| `update-sidecars.yml` | cron `0 2 * * 1` + manual | the same pattern for the two newly-pinned versions, `BGUTIL_TAG` and `NM3U8DL_VERSION`/`_BUILD`, in **separate PRs** so a POT-provider regression is bisectable on its own |
| `upstream-sync-check.yml` / `upstream-sync-label.yml` | cron `0 3 * * 6` / issue closed | ported as-is; they track `alexta69/metube` releases and store the last-synced version in a `synced:<ver>` label |
| `release.yml` | tag `v*` | build the amd64 binary, attach it plus the SBOM and image digests, generate the body from `git log <prev tag>..HEAD` |

Every bump workflow uses `concurrency: { group: bump-<name>, cancel-in-progress: false }` so two
crons cannot race on the same branch.

### 18.5 The yt-dlp nightly bump automation

Roughly 75 % of legacy commits are auto-merged nightly yt-dlp bumps, so this is the
highest-leverage CI change available. Ported with the same shape — grep the pin,
`pip install --dry-run --pre`, `sed`, reusable branch `auto/update-yt-dlp-nightly-<ver>`, reuse an
open PR, the `automated` label if it exists, `gh pr merge --auto --squash`, a step summary — plus
four changes that matter:

| Change | Why |
|---|---|
| The pin lives in **one** place, `docker/Dockerfile`'s `ARG YTDLP_VERSION=`, and a second grep asserts the string appears exactly once, failing loudly if someone adds a duplicate. | Legacy grepped `yt-dlp==`; with a build arg the automation stays a one-line `sed` and the version is also settable via `docker build --build-arg`. |
| After the `sed`, the workflow **builds the amd64 image and runs a smoke job**: `aulos-server doctor`, plus a real `mode=extract` against a public CC-licensed video through the shim. Only then is the PR opened and auto-merge enabled. | This is the entire point of pinning a nightly: a bad nightly must fail in CI, not on the VPS. Legacy auto-merged on a version-string diff alone. |
| The commit message stays byte-identical: `upgrade yt-dlp nightly to <ver>`. | The existing release-notes tooling and muscle memory keep working. |
| The broad `TATOALO_REPO_PAT` is replaced by a fine-grained token with `contents:write` + `pull_requests:write` on this repo only; if absent, the PR is opened with `GITHUB_TOKEN` and auto-merge is skipped with a warning. | Least privilege. |

### 18.6 Dependencies

| Crate | Line | Why |
|---|---|---|
| `tokio` | 1 | mandated runtime; `full` in the binary, narrow features in libs |
| `tokio-util` | 0.7 | `CancellationToken` (BRIEF §10), `TaskTracker`, `codec` for the JSON-lines reader |
| `axum` | 0.8 | mandated; native `ws`, typed extractors |
| `tower` / `tower-http` | 0.5 / 0.6 | `TraceLayer`, `SetRequestIdLayer`, `CorsLayer`, body limits, `ServeFile` semantics |
| `hyper` / `hyper-util` | 1 / 0.1 | axum plumbing; the graceful-shutdown handle |
| `serde` / `serde_json` | 1 / 1 | the wire format and the yt-dlp option dicts (`arbitrary_precision` off so numbers stay plain) |
| `serde_with` | 3 | `DisplayFromStr` for ULIDs; the v1 shim's flexible numerics |
| `rusqlite` | 0.37 | mandated, `bundled` (no libsqlite3 in the image), `serde_json` + `functions` |
| `rusqlite_migration` | 2 | embedded forward-only migrations, no build-time codegen |
| `ulid` | 1 | mandated id scheme; `from_datetime` for the importer's ordering |
| `arc-swap` | 1 | the published snapshot, `YtdlOptions`, POT state |
| `bytes` | 1 | pre-serialised WS frames shared across clients without copies |
| `thiserror` / `anyhow` | 2 / 1 | mandated: libraries / binary |
| `tracing` / `tracing-subscriber` | 0.1 / 0.3 | mandated; `env-filter` + `json` + `fmt` |
| `async-trait` | 0.1 | object-safe async `Provider`/`Hook`/`Notifier` |
| `futures-util` | 0.3 | `buffer_unordered` for the SC season fetch and batch inserts |
| `url` | 2 | URL parsing/normalisation; the SSRF guard needs its IP classification |
| `regex` | 1 | plugin patterns, custom-dirs exclusion, Telegram URL extraction |
| `toml` | 0.9 | `plugin.toml`, with good error spans for authors |
| `notify` | 8 | mandated hot reload; has `PollWatcher` for NFS |
| `nix` | 0.30 | `killpg`, `setsid`, `setrlimit`, `waitpid` |
| `command-fds` | 0.3 | passing fd 3 to the shim without a hand-rolled `pre_exec` |
| `teloxide` | 0.17 | mandated bot framework |
| `governor` | 0.8 | per-chat + global GCRA token buckets, no background task |
| `reqwest` | 0.12 | Jellyfin, POT probe, hook HTTP, SC `plain` mode; `rustls-tls`, no OpenSSL |
| `wreq` | 6 | Chrome TLS/HTTP2 impersonation for SC (feature `sc-impersonate`) |
| `wreq-util` | 3 | the named Chrome fingerprint profiles `wreq` itself does not ship (feature `sc-impersonate`) |
| `scraper` | 0.23 | html5ever CSS selection for the SC pages |
| `quick-xml` | 0.38 | NFO writing with correct escaping |
| `strip-ansi-escapes` | 0.2 | N_m3u8DL-RE / Spectre.Console frame cleaning |
| `mime_guess` / `percent-encoding` | 2 / 2 | file-route `Content-Type`; `download_url` construction |
| `time` | 0.3 | timestamps, RFC3339, the NFO `dateadded` format (one date crate, not two) |
| `rand` | 0.9 | jitter for backoff and subscription spread |
| `indexmap` | 2 | insertion-ordered Telegram job lines; ordered option merging |
| `smallvec` | 1 | delta frames are mostly 1–3 items |
| `base64` | 0.22 | the importer's `__metube_bytes__` wrappers |
| `sha2` | 0.10 | catalog ETags, synthetic plugin entry keys |
| `rustls` / `rustls-pemfile` | 0.23 / 2 | `HTTPS=true` without OpenSSL |
| `axum-server` | 0.7 | the TLS acceptor with graceful shutdown |
| `clap` | 4 | the §3.1 subcommands; `derive` + `env` |
| `metrics` / `metrics-exporter-prometheus` | 0.24 / 0.16 | the optional `<p>metrics` endpoint |
| **dev** `insta` | 1 | snapshots of every JSON response and every WS frame |
| **dev** `wiremock` | 0.6 | Jellyfin, POT, SC, hook HTTP fixtures |
| **dev** `rstest` | 0.26 | table-driven config/validation matrices |
| **dev** `proptest` | 1 | percent monotonicity, natural sort, resume merge, serialisation invariants |
| **dev** `tempfile` | 3 | importer and file-route fixtures |
| **dev** `tokio-test` | 0.4 | paused-time scheduler and subscription tests |
| **dev** `tokio-tungstenite` | 0.27 | a real WS client in the integration tests |
| **dev** `assert_cmd` + `predicates` | 2 / 3 | the CLI subcommand tests |
| **dev** `criterion` | 0.7 | benchmarks (not CI gates, §20.3) |

Deliberately **not** used: `sqlx` (compile-time DB access needs a live DB in CI), `socketioxide`
(Socket.IO is out of scope, §11.6), `chrono` (one date crate is enough), `natord` (effectively
unmaintained since ~2015; a 30-line comparator with a `proptest` is safer), `figment`/`config`
(BRIEF §15 demands byte-exact legacy names and defaults, which is 250 lines of explicit code with
better errors), `dashmap` (the hot paths use owned state), `pyo3` (§9), `once_cell`/`lazy_static`
(`std::sync::LazyLock`), `curl-impersonate` (§10.1).

---

## 19. Cutover runbook

Assumptions: one VPS, docker-compose, service on port 8081 behind Authelia and a reverse proxy,
volume `/srv/media:/downloads`, state in `/srv/media/.metube`.

### 19.1 Pre-flight (T−7 days)

| # | Action |
|---|---|
| 1 | **Rotate the credentials leaked in `vps_setup.md`** — Telegram bot token, Jellyfin API key, WireGuard private key — into a `chmod 600`, git-ignored `.env`. |
| 2 | `docker compose exec metube ls -la /downloads/.metube` — confirm `queue.json`, `pending.json`, `completed.json`, `subscriptions.json`, `telegram_bot_config.json` exist and are `schema_version: 2`. If any extensionless legacy **shelf** is present, let the Python image run once so it migrates to JSON (pickle import is out of scope). |
| 3 | Back up state: `tar czf /root/metube-state-$(date +%F).tgz -C /srv/media .metube` (a few MB). |
| 4 | Record the current compose and image digest: `docker compose config > /root/compose.pre-aulos.yml; docker inspect --format '{{index .RepoDigests 0}}' metube-pot > /root/image.pre-aulos`. |
| 5 | Confirm the reverse proxy forwards `Upgrade`/`Connection` for `<prefix>ws` (§16.6). If it does not, plan on `AULOS_API_TOKEN`. |
| 6 | `docker pull ghcr.io/tatoalo/aulos-server:<tag>`. |

### 19.2 Rehearsal (T−1 day, zero downtime, read-only)

```bash
# 1. Dry-run the importer against the LIVE state dir. Reads only; writes nothing.
docker run --rm -v /srv/media:/downloads ghcr.io/tatoalo/aulos-server:<tag> \
  aulos-server import --state-dir /downloads/.metube --db /tmp/probe.db --dry-run
#    -> the import report. Expect 0 errors; investigate every warning.

# 2. Validate the config the compose file will actually produce.
docker run --rm --env-file /srv/aulos/.env -v /srv/media:/downloads \
  ghcr.io/tatoalo/aulos-server:<tag> aulos-server check-config

# 3. Probe the tools inside the image.
docker run --rm ghcr.io/tatoalo/aulos-server:<tag> aulos-server doctor

# 4. Shadow run on a spare port against a COPY of the state dir.
cp -a /srv/media/.metube /srv/media/.aulos-shadow
docker run -d --name aulos-shadow -p 8082:8081 --env-file /srv/aulos/.env \
  -e STATE_DIR=/downloads/.aulos-shadow -e AULOS_DB_PATH=/downloads/.aulos-shadow/aulos.db \
  -e TELEGRAM_BOT_ENABLED=false -e JELLYFIN_SYNC_ENABLED=false \
  -v /srv/media:/downloads ghcr.io/tatoalo/aulos-server:<tag>
curl -s localhost:8082/healthz | jq '.status, .components'
curl -s localhost:8082/history | jq 'keys'                # ["done","pending","queue"]
curl -s localhost:8082/api/v2/import-report | jq
curl -s localhost:8082/api/v2/capabilities | jq '.formats[0]'
websocat ws://localhost:8082/ws | head -5                 # add one small public video first
docker rm -f aulos-shadow && rm -rf /srv/media/.aulos-shadow
```

Telegram and Jellyfin are **disabled in the shadow** so it cannot double-post to the chat or
double-scan the library. That is the whole reason the shadow is safe to run against real state.

### 19.3 Cutover (T−0, ~2 minutes of downtime)

```bash
# 0. Announce in the Telegram chat. Ship the v2 iOS build to the device FIRST (TestFlight/Xcode),
#    because losing Socket.IO is a real regression for the currently installed build (§11.6).
# 1. Note in-flight work (it will be re-queued, not lost).
curl -s localhost:8081/history | jq '[.queue[]|select(.status=="downloading")]|length'
# 2. Stop the old service. The state files are left untouched.
docker compose stop metube
# 3. Edit compose: swap the image line only. Keep the service NAME and the volume mounts
#    identical so the proxy and Authelia need no change.
#      image: ghcr.io/tatoalo/aulos-server:<tag>
# 4. Start.
docker compose up -d metube
# 5. Verify, in this order:
curl -fsS localhost:8081/healthz | jq '.status, .components.pot.status, .components.importer'
curl -fsS localhost:8081/api/v2/import-report | jq '.errors, .warnings, .items'
curl -fsS localhost:8081/history | jq '{q:(.queue|length),p:(.pending|length),d:(.done|length)}'
#    ^ compare against the numbers recorded in 19.1 step 2
docker compose logs --since 2m metube | grep -iE 'error|warn' | head
# 6. Functional smoke, in this order (each exercises a different surface):
#    a) iOS app (v2 build): the snapshot arrives; a live download shows moving progress
#    b) iOS share sheet: add returns instantly, the item appears as `resolving`;
#       then share a deliberately bad URL (e.g. https://example.com/nope) and confirm the
#       "Couldn't add to Aulos" notification still fires — the R24 / §11.2 pre-resolve check
#    c) Telegram: send one link -> "Queued 1 link(s)", the board appears and updates
#    d) Subscriptions: GET /subscriptions | jq length  == the pre-cutover count,
#       then POST /subscriptions/check -> 200 immediately; watch one check finish in the logs
#    e) Jellyfin: after the smoke download finishes, confirm ONE scan (not N) in Jellyfin's log
#    f) A file URL: curl -sI "localhost:8081/download/<name>" -> 200 + Accept-Ranges: bytes
# 7. Watch for 30 minutes: healthz each minute, `docker stats` for RSS/CPU, one subscription tick.
```

### 19.4 Rollback (any time, under 2 minutes)

```bash
docker compose stop metube
cp /root/compose.pre-aulos.yml /srv/aulos/docker-compose.yml   # or edit the image line back
docker compose up -d metube
curl -fsS localhost:8081/history | jq 'keys'
```

| Question | Answer |
|---|---|
| Did Aulos modify the legacy JSON? | **No.** The importer opens it read-only; the only new files in `STATE_DIR` are `aulos.db*` and `.aulos-imported`. |
| What is lost by rolling back? | Everything after the cutover: items added, items completed, subscription `last_checked`/seen advances, Telegram chat-config changes. The Python server resumes from its own JSON, so a subscription may re-fetch something Aulos already downloaded (the file is on disk, so a `download_archive` or the dedupe check usually skips it). |
| Can I roll forward again later? | Yes, but the DB now exists so the importer will not re-run. Either `mv /downloads/.metube/aulos.db{,.bak}` and start, or `aulos-server import --force`. |
| What if the import report has errors? | With the default `AULOS_IMPORT_ON_ERROR=fail`: the DB is never created (atomic), the process exits non-zero, the container restart-loops, and `docker compose logs` shows the report. Two ways forward, both under a minute: roll back (below) and file the fixture; or, when the named file's contents are expendable, restart with `AULOS_IMPORT_ON_ERROR=skip` (equivalently `aulos-server import --skip-corrupt`), which imports every other file, records the skip as a warning, and leaves `healthz.components.importer` degraded so it cannot be forgotten. `skip` is the legacy behaviour — `AtomicJsonStore` quarantined a bad file and started with that collection empty — minus the rename (T2 keeps your file untouched). `--force` is *not* the escape hatch: it only overrides the already-imported marker. |
| A record inside a file is malformed? | Never fatal, under either policy: the record is skipped, a `record_skipped` warning is recorded, and the rest of the file imports (§7.6.1). |
| What if only one subsystem misbehaves? | Kill switches, no rebuild: `TELEGRAM_BOT_ENABLED=false`, `JELLYFIN_SYNC_ENABLED=false`, `AULOS_POT_ENABLED=false`, `AULOS_PLUGINS_ENABLED=false`, `AULOS_HOOKS_ENABLED=false`, `AULOS_SC_HTTP=plain`, `AULOS_AUTO_RETRY_MAX=0`, `AULOS_TELEGRAM_BOARD=per_job`, `AULOS_V1_ENABLED=false`, `AULOS_V1_ADD_RESOLVE_WAIT_MS=0` (if a slow resolver makes v1 adds feel sluggish — at the cost of the §11.2 body-level error reporting), `AULOS_RESOLVE_FALLTHROUGH=false`, `MAX_CONCURRENT_DOWNLOADS=1`. |

### 19.5 Post-cutover

| When | Action |
|---|---|
| +1 day | Confirm one full subscription cycle ran for every subscription (`healthz.components.subscriptions.failing == 0`). |
| +7 days | If the v2 iOS build is the only client, set `AULOS_V1_ENABLED=false` and re-smoke. Leave it `true` while the bookmarklet or the Shortcut are still in use. |
| +30 days | Archive the legacy JSON out of `STATE_DIR`. Only then is the rollback path gone; note it in the changelog. |

---

## 20. Testing strategy

| Layer | Tooling | What it proves |
|---|---|---|
| Unit — `aulos-core` | `rstest` | every row of the config table, `%%` indirection and cycles, every bad-boolean token, `URL_PREFIX` normalisation, path containment (including `/downloads-evil` and a symlink escape), the validation matrix, the legacy request-migration table, status transitions, `Normalizer` golden vectors ported from the Python tests |
| Unit — `aulos-provider-ytdlp` | golden JSON | `get_format`/`get_opts` for **every** legal tuple against `tests/golden/formats.json` generated from the legacy Python; option-layer precedence; `null`-clears-a-key; `best_remux` pops `format`; the caption mode/language ordering; the error-taxonomy regex table |
| Unit — the shim | `assert_cmd` | `mode=selftest`; a malformed job (exit 2); an unknown protocol (exit 64); a `file://` extract+download of a bundled 1 s clip; and a **`--replay <transcript.jsonl>`** suite that exercises every frame type, every ordering violation and every error class with no Python and no network |
| Unit — `aulos-provider` | `rstest` + `proptest` | match scoring and tie-breaks; registry select with hints and `Degraded`; the template renderer (all tokens, unknown-token rejection, injection safety); every manifest rejection reason; progress parsers against real captured N_m3u8DL-RE / ffmpeg output; `proc::Child` pgid kill (a `sh -c 'sleep 300 & wait'` grandchild must die); the stderr-drain deadlock case; the line cap |
| Unit — `aulos-provider-sc` | `wiremock` + captured HTML | Inertia version extraction and the 409-retry; watch/season/title parsing; `window.streams` + token/expires + `h=1`; query-param preservation; entry id/title shaping for movie/episode/season/title; the natural comparator (`proptest`); the gapless mux against synthetic TS segments (assert exact byte concatenation and exactly one ffmpeg invocation) |
| Unit — `aulos-store` | in-memory SQLite | migrations from scratch match the snapshot; every `WriteOp`; batch coalescing (assert the transaction count for 500 inserts); `PruneSeen` boundaries; **allocator crash safety** (reopen after a simulated crash mid-block and assert no value is re-issued) |
| Unit — **importer** | fixture corpus | `tests/fixtures/state/{v1,v2,mixed,corrupt,shelf-present}/` with real-shaped files. Asserts: the report, statuses, id/`ord` assignment, dedupe, seen-id import, atomic rollback on a deliberately corrupt fifth file, **and that every input file is byte-identical afterwards** (T2) |
| Unit — `aulos-telegram` | fake bot transport | the `cfg:` callback grammar; every message text byte-for-byte; the SSRF accept/reject table; limiter behaviour under a fake clock including a synthetic 429 |
| Integration — queue | `fake` provider + fake clock, no network | add/cancel/retry/delete; 500-item expansion; slot accounting (global vs `own_slots`); priority classes; lookahead anti-starvation; boot recovery from every seeded status; `clear_after`; group counter and byte-weighted percent correctness; dropped-progress accounting; the stall watchdog under a drop storm |
| Integration — API | in-process `axum::Router` + `reqwest` + `tokio-tungstenite` | every v2 endpoint request/response shape (`insta`); the full WS frame sequence for each §21 sequence; `?since=` delta vs snapshot vs `UpToDate`; a `since` above head after a simulated restart; ETag/304; the error envelope for every code. The whole suite runs **twice**, with `URL_PREFIX=/` and `URL_PREFIX=/metube/` |
| **Contract — v1 shim** | recorded legacy responses | The highest-value tests. `tests/v1_golden/` holds request/response pairs captured from the running Python server, replayed against the shim and compared field-by-field modulo an allow-list of documented deltas. Plus a JSON-Schema check generated by `aulos-server print-schema` run against `/history`, `/version` and `/add` — i.e. the shipped Swift models' expectations, mechanically. **The three checked-in corpora — `tests/v1_golden/`, `crates/aulos-provider-ytdlp/tests/golden/{formats,opts}.json`, and the `Normalizer` vectors — are produced by a named deliverable, PLAN WP-00**, with a `MANIFEST.json` recording the legacy commit (`fd35a66`), the image digest and the capture date. They are the sole mitigation for R1 and R21, so an unowned corpus is an unmitigated top risk |
| **Consistency** | `stress_consistency` | Reconstruct the client's state **purely from the frame stream** (`snapshot` + every subsequent frame, applying the documented delta semantics) and assert equality against the authoritative snapshot every 5 s under load. This is the only test that catches a desynced delta, and it is non-negotiable |
| Integration — hooks | `wiremock` + real ffmpeg on a 2 s generated clip | debounce coalescing and the `max_wait` cap; targeted vs global Jellyfin refresh; the 404 fallback; NFO XML snapshots for movie and episode; the audio-sync round trip and the "a hook failure never fails the item" invariant; `[[hook]]` http and command templates including `{count}` batching |
| Integration — subscriptions | fake provider + paused time | backfill suppression; `is_live` re-queue; the backoff curve and cap; the concurrency cap; `next_due` persistence across a simulated restart; the single-video rejection counting as a failure |
| Integration — plugins | the shipped example + `wiremock` | end-to-end resolve+download; every manifest rejection reason; a deliberately hostile plugin (infinite stdout, `sleep 1d`, a 1 GiB write) asserts each limit fires |
| Property | `proptest` | percent monotonicity; `ItemView` serialisation never omits a key; the resume merge is equivalent to sequential application; `resolve(token)` never returns an id twice |
| Load | `tests/load/` | 50 fake items updating every 50–200 ms with a 15 % failure rate (deliberately the same shape as the iOS `StressTestService`) with 5 WS clients. Asserts frames/s ≤ `1000/AULOS_WS_BATCH_MS` per client, p99 frame size, zero `Lagged`, and that a slow client cannot perturb a fast client's frame timing |
| e2e — docker | `tests/e2e/run.sh`, gated on `AULOS_E2E=1` | Builds the image, runs it (OrbStack locally, `docker` in CI), then: `healthz` green including POT; `POST api/v2/downloads` for a small public CC video; the WS sequence `added → delta → completed`; the file exists in the volume; `GET download/<name>` returns 200 and honours `Range`; `POST <p>add` (v1) works; a container restart mid-download resumes; `docker logs` contains no ERROR. A second profile seeds a legacy `STATE_DIR` and asserts the import report |

Coverage floors are enforced in CI only where a bug is silent and expensive: `aulos-store::import`
and `aulos-api::v1` at **90 %** line coverage, the workspace at 70 %.

Benchmarks (`criterion`) exist for `Normalizer`, delta diffing/serialisation, the store batch path,
and the 500-child insert, and their numbers are tracked in release notes — but they are **not** CI
pass/fail gates. Latency thresholds on shared GitHub runners flake, get muted, and then everyone
ignores the whole suite. The functional assertions that *are* gated are the ones that do not depend
on runner speed: frame **counts**, transaction **counts**, byte **sizes**, zero leaked processes,
zero `Lagged`, and no `.part` files left behind.

---

## 21. Sequences

### 21.1 Add a single video

```
iOS ──POST <p>api/v2/downloads {url, mp4/1080} ──────────────────────────► api
api    validate against the catalog, folder containment, presets, overrides   ~40 µs
api    mint ULID 01JBQ…AAA, allocate ord=981, compute canonical_key
api ──► EngineCmd::Add ─────────────────────────────────────────────────► engine
engine dedupe miss → store.write([InsertItems{status:resolving}], Sync)
engine publish Added([view], Created);  ack(ids=[01JBQ…AAA])
api ◄── 202 {"id":"01JBQ…AAA","ids":[…],"seq":10241}                       total 3–8 ms
hub    +25 ms  {"t":"added","seq":10241,"items":[{status:"resolving",percent:0}]}
engine spawn resolve (resolve_slots permit)
resolve ytdlp → python3 ytdlp_runner.py mode=extract → fd3: hello, info, result   0.6–4 s
engine  Resolved(Ok([one Video]))  → store.write([SetResolved, SetStatus{queued}])
engine  push ready[Interactive] → global.try_acquire OK → spawn run
run     provider.download() → runner mode=download, own pgid
run     sink.stage(Preparing) → aggregator → engine (persist) + urgent flush
hub     {"t":"delta","seq":10243,"items":[{"id":…,"status":"preparing","title":"Rick…"}]}
runner  progress frames ~10/s/stream → sink.progress (try_send) → cells (no locks)
agg     every 250 ms: {"t":"delta","items":[{id,percent,speed,eta,downloaded_bytes}]}  ~110 B
runner  pp Merger started → stage(Postprocessing), phase="remux"  → urgent delta
runner  pp MoveFiles finished{filepath} → sink.file / Outcome
run  ──► EngineCmd::Finished{outcome}
engine  store.write([SetOutput, SetStatus{finished}, SetClearAfter]); release the permit
hub     {"t":"completed","seq":10262,"items":[{…filename,size,download_url,percent:100}]}
hooks   audio_sync? no · nfo? no · jellyfin: arm the 30 s debounce
```

### 21.2 Add a 500-item playlist

| t | Actor | Action | Client sees |
|---|---|---|---|
| 0 | api | validate, mint `01JBQ…GG`, insert `{kind:item,status:resolving}` | `202 {"id":"01JBQ…GG"}` |
| +25 ms | hub | `added` (1 item, `resolving`) | one row, spinner |
| →6 s | resolve | `mode=extract`, `extract_flat=true`, `noplaylist=true`; entries stream from the shim | still one row |
| 6 s | engine | `PromoteToGroup{id:01JBQ…GG, children_total:500}` — **the id and `ord` the client already holds** | the row's `kind` flips to `group` **in place**: no blink, no move |
| 6.0 s | hub | one `added` frame, `reason:"expanded"`, carrying the group view + the first 50 children | a group row with counters + 50 child rows |
| 6.0–6.4 s | engine | remaining children inserted in 5 transactions of 100, with `group_index`, ULIDs, `ord`s, and the injected yt-dlp playlist fields (`playlist_index` zero-padded to the digit width, `playlist_count`, `playlist_autonumber`, `n_entries`, `__last_playlist_index`, the copied parent props) with `playlist*` outtmpl fields pre-resolved | further `added` frames, ≥ 25 ms apart, coalesced |
| 6.4 s | engine | schedule 3 children at `Priority::Bulk`; the rest stay `queued` | 3 rows progressing |
| every 250 ms | agg | one `delta` with ≤ 3 changed children plus the group's byte-weighted percent | one group bar + 3 rows |
| any time | api | a link pasted now enters at `Priority::Interactive` and starts **next**, not behind 497 children | |

Cost vs legacy: ~10 `added` frames instead of 500 unthrottled full-object broadcasts; ~6 SQLite
transactions instead of 500 whole-file JSON rewrites with 1000 `fsync`s; one child process for
resolution instead of 500 sequential executor extractions; ~400 bytes added to every subsequent
connect snapshot instead of ~125 KB.

### 21.3 Cancel mid-download

```
client ──POST <p>api/v2/items/actions {"action":"cancel","ids":["01JBQ…AAA"]} ──► api
api ──► EngineCmd::Cancel ──────────────────────────────────────────────────► engine
engine  running.get(id) → token.cancel()                                    t+0
engine  store.write([SetStatus{canceled, msg:"Canceled by user"}]); release the permit
engine  publish Completed(view{status:canceled})
api ◄── 200 {"applied":["01JBQ…AAA"],"skipped":[],"seq":…}                  t+1–6 ms
hub     +≤25 ms {"t":"completed","seq":…,"items":[{"status":"canceled",…}]}
run     provider select! sees cancel → killpg(-pgid, SIGTERM)               t+0.2 ms
run     wait up to AULOS_KILL_GRACE_MS, else killpg(-pgid, SIGKILL)
run     cleanup: *.part, *.ytdl, SC segment dir, partial mp4
engine  SlotFreed → schedule() admits the next item by priority
```

The HTTP response and the WS frame do **not** wait for `SIGKILL`. A v1 client calling
`POST <p>delete {ids:[url],where:"queue"}` gets the same path plus a `Delete`, so the item
disappears and the app's optimistic local removal stays correct.

### 21.4 Restart with in-flight downloads

```
docker stop → tini forwards SIGTERM → aulos-server
  t+0     stop accepting HTTP; WS clients get Close(1001)
  t+0     subscription scheduler + telegram poller stopped
  t+0..20 in-flight downloads keep running (AULOS_SHUTDOWN_GRACE_SECS)
  t+20    still running: killpg SIGTERM, 5 s, SIGKILL
  t+25    store.write([SetStatus{queued, msg:"Interrupted by shutdown", attempt+1} × n])
  t+25    final flush; wal_checkpoint(TRUNCATE); PRAGMA optimize; POT SIGTERM; exit 0
--- container replaced ---
  boot    config → tracing → open DB (exists ⇒ no import) → quick_check → migrations (no-op)
  boot    allocators seeded from max(meta.hwm, MAX(col)+1); NEW boot_id minted
  boot    recovery: resolving→queued; preparing|downloading|postprocessing→queued(attempt+1);
          group counters recomputed with one GROUP BY; clear_after re-armed
  boot    listener binds
  t+0     first WS client: {"t":"snapshot","boot_id":"<new>", …} with real titles, percent 0
  t+0     a client that reconnects with since=<old seq> and the OLD boot_id gets a full snapshot
  t+0     scheduler admits MAX_CONCURRENT_DOWNLOADS by priority; yt-dlp resumes from .part
```

Legacy re-`add`ed **every** queued item at once, each re-running metadata extraction on the shared
executor before the UI could connect. Here the DB already holds the resolved title and entry, so
recovery is a single `UPDATE` and the client sees real titles instantly.

### 21.5 Subscription tick

```
scheduler(sub 9c1f…)  sleep_until(next_due)
  wake → enabled → acquire a check permit (2 total)
  provider.resolve(url, flat, playlist_end=SUBSCRIPTION_SCAN_PLAYLIST_END=50)   ~1.5 s
  filter with is_media_entry → 50 entries
  store.seen(9c1f…) → HashSet of 314 ids (one indexed query, not a 50 000-element JSON parse)
  new = unseen (3) + already-seen with live_status=="is_live" (0)
  EngineCmd::Add(batch of 3, source={kind:"subscription",ref:"9c1f…"}, Priority::Subscription)
  store.write([MarkSeen{3}, PruneSeen{keep:50000},
               UpsertSubscription{last_checked, next_due=now+60min±10%, failures:0, error:null}])
  publish SubscriptionChanged → {"t":"subscription","seq":…,"subscription":{…,"seen_count":317}}
```

Failure branch (`Network("HTTP 403")`): `failures 0→1`, `next_due = now + min(60min × 2, 6h) ±10%`,
`error = "HTTP 403"`, `last_checked = now`. Three more failures ⇒ the next check is in 6 hours, not
in 60 seconds.

### 21.6 `YTDL_OPTIONS_FILE` edit

```
operator: vi /config/ytdl-options.json     (vim writes 4913, then renames tmp → target)
notify (watch on /config, non-recursive) delivers Create/Remove/Modify(Name) events
ConfigWatcher: keep only events whose FILE NAME == "ytdl-options.json"  → 1 accepted
ConfigWatcher: reset the 250 ms debounce; further events arrive → still ONE reload
t+250 ms  reload: YTDL_OPTIONS (env) → merge the file over it → re-apply runtime overrides
   success → ArcSwap::store(new); update_time = mtime
   failure → keep the previous YtdlOptions; msg = "YTDL_OPTIONS_FILE contents is invalid"
publish YtdlOptionsReloaded{ok, msg, update_time}
hub  {"t":"ytdl_options","ok":false,"msg":"YTDL_OPTIONS_FILE contents is invalid","update_time":…}
healthz.components.ytdl_options.status = "degraded" until a good reload
in-flight jobs: unaffected (each snapshotted its Arc<YtdlOptions> at spawn)
next job: picks up the new options
```

Deleting the file fails the reload with `File "<path>" not found`, keeps the last-good options, and
the **directory** watch survives, so re-creating the file heals it with no restart.

---

## 22. Risk register

Likelihood / Impact: L / M / H, ordered by product.

| # | Risk | L | I | Mitigation | Detection |
|---|---|---|---|---|---|
| R1 | **The v1 shim is subtly wrong and the shipped iOS build silently shows a broken queue during cutover.** | M | H | `tests/v1_golden/` replays captured legacy responses field-by-field; a JSON-Schema check generated from `print-schema` runs in CI; the shadow run exercises `/history` and `/add` against real state before any downtime. | `/history` counts compared against the pre-cutover numbers (19.3 step 5). |
| R2 | **Losing Socket.IO leaves the currently installed client without live updates.** | H | M | Accepted and scheduled: the v2 iOS build ships in the same session (19.3 step 0). The old client already fetches `/history` on connect and on pull-to-refresh, so it degrades to manual refresh, not a blank screen. `<p>socket.io` returns 501 with a pointer instead of hanging a handshake. | The 501 in access logs; the app's `.error` handler. |
| R3 | **yt-dlp option-dict drift** — a user's `YTDL_OPTIONS` holds something JSON cannot express. | L | H | Both `YTDL_OPTIONS` and `YTDL_OPTIONS_FILE` were already JSON in legacy, so anything a user has is expressible. `coerce` handles the one known object type; an unknown coercion is a loud `contract` error naming the key, never a silent behaviour change. | `healthz.components.ytdl_options`; the item's `error`. |
| R4 | **A nightly yt-dlp bump breaks extraction on the VPS.** | H | M | The bump PR builds the image and runs a real extract before auto-merging (§18.5). The pin is a build arg, so rolling back is `--build-arg YTDLP_VERSION=<old>` or the previous image tag, and because Python lives only in the runtime stage that image builds in ~2 min. | The bump PR's smoke job; `healthz.components.ytdlp_runner`; a mass of items failing with the same code. |
| R5 | **`wreq`/BoringSSL fails to build or churns.** | M | M | `ScHttp` trait with a compiled-in plain-`reqwest` implementation and the runtime switch `AULOS_SC_HTTP`; the feature is per-target; CI builds both feature combinations. Only StreamingCommunity degrades, and only if the site fingerprint-checks. | A CI build failure (blocking); at runtime a boot WARN naming the degradation, plus SC items failing with a 403. |
| R6 | **StreamingCommunity changes its page structure.** | H | M | All scraping is five small fixture-driven modules; each step failure has a distinct code so logs say *which* step broke; the version cache retries once on 403/404/409; `AULOS_SC_EXTRA_HOSTS` covers mirrors; failures are per-item, never process-wide. A user can bridge the gap with a `command` plugin without waiting for a release. | SC items failing with `Upstream("Could not get site version")`; the SC provider probe in `healthz`. |
| R7 | **An importer edge case loses history** (unknown status, duplicate URL, an SC entry blob, a 400 MB `completed.json`). | M | H | One atomic transaction, DB deleted on failure, legacy files never mutated (T2), a three-class failure taxonomy so it is never ambiguous whether the remaining files' data survives (§7.6.1), an `AULOS_IMPORT_ON_ERROR=skip` escape so one corrupt file cannot restart-loop the container the way it could in an earlier draft, an explicit SC entry translation with its own fixture (§7.6.3a), a fixture corpus including corrupt and mixed-version inputs, a mandatory `--dry-run` rehearsal, and the report served over HTTP and checked at cutover. | `api/v2/import-report`; the container restart loop with the report in the logs; `healthz.components.importer` degraded under `skip`. |
| R8 | **`ord`/`seq` allocator re-issues a value after an unclean shutdown**, breaking inserts or `?since=`. | L | H | Reserve-before-use hi/lo blocks; per-counter seeding at open (`ord` against `MAX(items.ord)+1`, `seq` against its own witness — they are different counters and comparing one to the other is meaningless, §4.1); a boot check that refuses to start on an inconsistency and prints `aulos-server repair-ids`, which is a real subcommand (§3.1) and not just a message; `boot_id` in every snapshot so a client can never apply frames across a generation. | The boot check; a `proptest` that simulates a crash mid-block. |
| R9 | **A process-group kill takes down something it should not.** | L | H | Every child is spawned with `process_group(0)`, so each is its own leader and the POT supervisor's child is a separate group again. An integration test asserts a cancel does not touch the POT pid and that `docker stop` reaps every grandchild. | `healthz.components.pot.restarts` jumping on a cancel would show it immediately. |
| R10 | **SQLite contention under a 500-item burst.** | L | H | WAL, one writer thread, batched transactions (≤ 256 ops), `busy_timeout=5000`, read-only reader connections, and `503 + Retry-After` rather than a 500 if it ever happens. The load test covers the burst. | `state_unavailable` responses; `healthz.components.store.latency_ms`. |
| R11 | **The progress drop policy hides a stuck download.** | L | M | Stage messages are never dropped; the stall watchdog reads `last_frame_at`, which is bumped **before** the drop decision, so a drop storm and a real stall are distinguishable; the drop counter is in `healthz`. | `progress_dropped_total > 0` is a WARN signal. |
| R12 | **`URL_PREFIX` handling diverges** somewhere (a route, the WS path, `download_url`, the healthcheck). | M | M | One `Prefix` newtype builds every path; the whole API integration suite runs twice, with `/` and `/metube/`; the healthcheck interpolates `${URL_PREFIX}`; `capabilities.url_prefix` lets the client verify. | The prefixed test run; a 404 on the healthcheck. |
| R13 | **Groups confuse a client** (a `kind:"group"` row rendered as a download). | L | M | The v1 shim omits groups entirely; v2 documents `kind`, the `children_*` counters and the closed status roll-up, with a pinned snapshot test. Children carry `group_id` so a client can collapse or ignore them freely. | Contract tests; the v2 smoke. |
| R14 | **Legacy `shelve` state still present on the VPS** (pickle import is out of scope). | L | H | Detected at import time with an actionable message; the runbook checks for it a week early. | The import report's `errors`. |
| R15 | **Debian bookworm ffmpeg is too old** for the SC or audio-sync paths. | L | M | `doctor` prints and asserts ffmpeg ≥ 6 at boot (WARN, not fatal); the e2e test does a real remux and a real audio-sync re-encode; `trixie` is a one-line base bump. | `healthz.components.ffmpeg.version`; the e2e remux test. |
| R16 | **`CHOWN_DIRS` semantics surprise an operator.** | M | L | Documented in the compose example and Appendix B; `CHOWN_DIRS=recursive` restores exact legacy behaviour; the entrypoint logs which mode it used. | Permission-denied errors in the first minute of logs. |
| R17 | **Telegram rate limits throttle the bot** because of the new live edits. | M | M | Per-chat GCRA at 1 edit/3 s plus a global 20/s cap; no-op edits suppressed by text comparison; `RetryAfter` doubles the chat's interval; `AULOS_TELEGRAM_BOARD=per_job` and a larger interval are escape hatches. | `healthz.components.telegram.edits_throttled_total`; 429s logged at WARN with the chat id. |
| R18 | **Scope**: eleven crates plus a dev-only test crate, a group model, plugins, hooks, a retry policy and a Telegram board are all new surface. | M | M | Every one has a kill switch and none is on the critical path of "add a URL, get a file". `PLAN.md` orders the waves so core + store + queue + api + ytdlp + the v1 shim land first, and the rest are independently disableable and testable. | Wave completion in `PLAN.md`. |
| R19 | **A community plugin or hook hangs, floods, or fills the disk.** | M | M | Per-plugin `max_concurrent`, resolve/stall/hard timeouts, `RLIMIT_AS\|FSIZE\|CPU\|NOFILE`, `max_output_bytes`, a 4 MiB line cap, own pgid, `nice(5)`, a 5-failures-in-10-minutes circuit breaker into `Degraded`, and `AULOS_PLUGINS_ENABLED=false`. | A slot stuck in `preparing`; the job stall watchdog; the `Degraded` state in `healthz`. |
| R20 | **A plugin is malicious** ("install this codec pack"). | L | H | No shell, fixed argv templates, sanitised placeholders, cleared env with an explicit `env.pass` allow-list, refusal to execute world-writable or setuid files, and `GET api/v2/providers` exposing every argv for audit. Documented in bold: a plugin runs with the server's privileges and is **not** sandboxed. | Operator audit; the argv in `healthz`/`providers`. |
| R21 | **The `Normalizer` port diverges**, making bars jump backwards. | M | L | Golden vectors ported 1:1 from the Python unit tests plus a `proptest` monotonicity invariant. | The golden test; visually obvious in the app. |
| R22 | **Secrets leak** (the legacy tree already contains live ones). | H | H | Runbook step 1 is rotation; a `chmod 600` `.env`; `gitleaks` in CI; the `Redact` newtype in every log path; `check-config` prints `«redacted»`. | `gitleaks`; a manual grep of the new repo before the first push. |
| R24 | **Async add silently swallows a bad URL for the shipped client during the overlap window.** `AddResultClassifier` decides success by parsing the `POST <p>add` body, so a 200 emitted before extraction turns "unsupported URL" into "queued" and the "Couldn't add to Aulos" notification never fires. | M | H | The bounded pre-resolve of §11.2 (`AULOS_V1_ADD_RESOLVE_WAIT_MS`, default 10 s) keeps the legacy body contract on the v1 route only, with the legacy `", "` joiner for multi-URL adds; `aulos_v1_add_resolve_total{outcome}` shows how often the window expires; the v2 route stays async, so the property disappears with the v1 shim. Scheduled like R2: the v2 iOS build ships in the same session (19.3 step 0). | The metric's `timeout` share; a share-sheet add of a deliberately bad URL in the cutover smoke list (19.3 step 6b). |
| R25 | **A StreamingCommunity URL legacy could still handle now fails.** Legacy detected by host but dispatched by path and fell through to yt-dlp for anything else. | M | M | `matches()` is host-**and**-path (§10.2), so a non-dispatchable path is `Match::No` and yt-dlp gets it exactly as before; the runner-up retry of §6.4 covers a scrape that resolves nothing. Both are recorded (Appendix A.9, B C46) and both have tests (PLAN WP-08, WP-12). | An SC-host URL failing with `unsupported_url` instead of being handled by yt-dlp; `aulos_resolve_fallthrough_total`. |
| R23 | **The reverse proxy strips the WebSocket `Upgrade`** for `<prefix>ws`. | M | M | A pre-flight check in the runbook (19.1 step 5); `AULOS_API_TOKEN` accepted via `Sec-WebSocket-Protocol` or `?token=`; and `GET api/v2/state?since=` is a fully functional polling fallback that needs no upgrade at all. | The WS handshake failing in the shadow run. |

---

## 23. Resolved decisions

Every open question raised by the three candidate proposals, decided. There are no TODOs.

| # | Question | **Decision** |
|---|---|---|
| 38 | Add `pause`/`resume`, the half of iOS ask 11 that C35 did not cover? | **Yes — as a `pause` action, not a ninth status** (§8.7). `queued(auto_start=true) → auto_start=false` un-schedules; a running job is killed but keeps its `.part` so `start` resumes it, with `attempt` unchanged. The action set becomes `start \| pause \| cancel \| retry \| delete`. "Paused" and "never started" are the same thing to the scheduler, the v1 shim and the client, so the closed 8-value vocabulary of BRIEF §6 is untouched (Appendix B, C41). |
| 39 | Where does the `DomainEvent` fan-out live, and what type is it? | **An explicit `EventRouter` task in `aulos-core::event`, owning the single receiver and issuing one bounded `EventInbox` per subscriber** (§2.2.1). Not a `broadcast` channel: it cannot express per-subscriber capacity or drop policy, and it drops the *oldest* message for a slow reader, which for a `Completed` event silently skips a hook. Not four consumers each taking "the" `mpsc::Receiver`, which does not compile. |
| 40 | v1 `GET <p>history` `done[]`: the in-memory window or the whole store? | **The whole store**, `AULOS_V1_HISTORY_MAX=0` by default (§11.4). v1 has no `truncated`, no `done_total` and no paging, and `HistoryResponse` declares all three arrays non-optional, so a window would silently drop thousands of rows out of the shipped client at cutover. v2 stays windowed and honest. |
| 1 | A Socket.IO shim for the overlap window? | **No.** BRIEF §8 says Socket.IO is not provided. `<p>socket.io` returns 501 with a pointer; the v2 iOS build ships in the same session (§11.6, R2). |
| 2 | Should the v1 shim synthesise a parent row for a group? | **No.** Groups are omitted from `/history`; only children appear. A parent row that never progresses is worse than nothing in the old client (§11.4). |
| 3 | `canceled` in v1 `/history`? | **Omitted entirely.** The shipped `DownloadStatus` has no `canceled` case and maps unknown → `.pending`, so a cancelled row would be stuck in "In Progress" forever. Legacy made cancels vanish; this is faithful (§11.4). |
| 4 | v1 `id`: the ULID or the legacy extractor id? | **`media_id` when present, else the ULID**, with the `<prefix>.<id>` prefixing reproduced. The shipped client keys deletes on `url ?? id`, and the resolution ladder (§11.3) accepts all three tokens, so both work (§11.4). |
| 5 | `SubId` representation for imported subscriptions? | **A validated string newtype**, not a `Ulid`, and not an enum with two variants. Imported UUIDs are kept verbatim; new ids are ULIDs. One representation, no `legacy_id` column, no join (§4.1). |
| 6 | Delete the `.info.json` after NFO generation? | **No** — `AULOS_NFO_DELETE_INFO_JSON=false` by default. Legacy's CLI always deleted it, but users' own `Exec` postprocessors may consume it, and changing that on the wrong side of a cutover is the wrong risk (§13.2). |
| 7 | SC output naming? | **Kept** as `<sanitised title>.mp4` + `.info.json`, ignoring `OUTPUT_TEMPLATE*`. Existing Jellyfin libraries depend on those paths. `AULOS_SC_USE_OUTPUT_TEMPLATE=true` opts in (§10.5). |
| 8 | Keep the `[0, 99.9]` percent clamp? | **Keep it.** `percent` is the download number; the real `postprocessing` status plus `phase`/`phase_percent` now carry what the clamp used to hide (§4.7). |
| 9 | `quality: "worst"` — fix the selector or keep the quirk? | **Keep the quirk**, byte-identical to legacy, and make it honest: the catalog carries `notice: "This selector currently resolves to the best available stream"` (§6.6). No hidden env flag. |
| 10 | Auth beyond the proxy? | **Optional `AULOS_API_TOKEN` bearer**, plus optional `AULOS_TRUSTED_PROXY_AUTH_HEADER`. Cookie passthrough remains the primary path. No credential store, no user model (§16.6). |
| 11 | WS auth ticket endpoint? | **No.** Cookies flow on the upgrade; where the proxy cannot forward them, the same `AULOS_API_TOKEN` works via `Sec-WebSocket-Protocol` or `?token=`. A ticket endpoint is a third auth path for one hypothetical (§16.6). |
| 12 | APNs / device tokens / a webhook notifier now? | **No.** The `Notifier` trait is the seam (§12.6); webhooks are already covered by the `[[hook]]` manifest (§13.4). A push service is a separate deployment decision. |
| 13 | Per-group slot cap, so one playlist cannot own every slot? | **No cap. `Priority` classes instead** (§8.2). A pasted link enters at `Interactive` and starts before 497 `Bulk` children, which is the actual user complaint; a cap would just be a second, weaker mechanism. |
| 14 | Prometheus `/metrics`? | **Yes, opt-in**: `AULOS_METRICS_ENABLED=false` by default. The metric names, types and labels are enumerated in **§16.7**; `healthz`'s short JSON field names and `metrics`' flat `aulos_*` names are two deliberately different surfaces, reconciled in that table (§16.3). |
| 15 | Pin `bgutil-pot`? | **Yes**, plus a weekly `update-sidecars.yml` PR. Resolving `latest` at build time made the image non-reproducible (§18.1, §18.4). |
| 16 | Where does the DB live? | **`<STATE_DIR>/aulos.db`** by default, so cutover needs no compose change (T1). `AULOS_DB_PATH` moves it for anyone who wants it off the media volume (§17.3). |
| 17 | Done-window size and whether the snapshot includes it? | **`AULOS_MEM_DONE_ITEMS=500`, included by default**, with `truncated.done` telling the truth and `?done=false` available. History beyond the window is paged from SQLite (§15.2). |
| 18 | Delta of `chapter_files`/`subtitle_files`? | **Always the whole array.** They only grow and are almost always ≤ 3 elements; appended-entry semantics is protocol complexity for nothing. |
| 19 | Group representation: promote the anchor into a group row, or delete it and add a group record? | **Promote in place.** One `items` table, one wire type, the same `id` and the same `ord`, `kind` flips. No removal frame at all, so the row cannot blink or jump (§8.4). |
| 20 | Dedupe as a database constraint? | **No.** In-memory engine policy over `(canonical_key, selection)` for user adds only, with a non-unique index. A partial unique index would abort a 500-child transaction the first time a channel lists a video twice, and would block "the mp3 of what I'm already pulling as mp4" (§8.5). |
| 21 | Delta derivation: field mask or diff? | **Diff against `last_sent`.** A missed mask bit is a permanently stale field on the client with no error anywhere, guarded only by discipline; a diff cannot have that failure mode (§15.1). |
| 22 | Progress cell: seqlock or channel? | **Channel with latest-wins coalescing.** The workload is ~240 messages/second total at the default 3+1 slots; a hand-rolled seqlock is optimisation theatre with a real memory-ordering hazard, and there is no second gated implementation for a load this box will never see (§2.3). |
| 23 | `deny_unknown_fields` on the v2 add request? | **No.** An App Store rollout runs mixed client and server versions for weeks; the first client sending a new optional field must not get a 400 on the whole add. Unknown fields are ignored and echoed in a `warnings` array (see PROTOCOL.md). |
| 24 | Persist a frame/event log in SQLite? | **No.** The replay ring is in memory and bounded. Persisting delta frames would be ~345 k rows/day of pure progress churn on a DB inside the media volume, and would throw away the one genuinely good legacy property. |
| 25 | Stack `teloxide::Throttle` on top of `governor`? | **No.** One limiter, the one that knows about `last_rendered` and the per-chat interval (§12.4). |
| 26 | `AULOS_TELEGRAM_WATCH_ALL` default? | **`true`.** On a single-user box, web and subscription downloads being invisible to the bot is a legacy bug, not a feature. `false` restores it exactly (§12.6). |
| 27 | Serve a status page and a directory index? | **No HTML.** `GET <p>` returns a small JSON identity document; `DOWNLOAD_DIRS_INDEXABLE=true` serves a JSON listing. BRIEF parked the status page, and a web UI is out of scope. |
| 28 | CI performance gates? | **No latency gates.** `criterion` benchmarks are tracked in release notes; CI gates only runner-speed-independent assertions: frame counts, transaction counts, byte sizes, zero leaked processes, zero `Lagged` (§20). |
| 29 | CI target architectures? | **`linux/amd64` only** (BRIEF §16). The Dockerfile stays `TARGETARCH`-parametrised so arm64 is a one-line change later (§18.1). |
| 30 | `natord` for the gapless mux sort? | **No.** A ~30-line numeric-aware comparator with a `proptest` — the crate has been effectively unmaintained since ~2015 and would trip `cargo deny` (§10.5). |
| 31 | Does `CLEAR_COMPLETED_AFTER` apply to items aged out of memory? | **Yes.** `ClearScheduler` queries SQLite, not the in-memory window (§8.10). |
| 32 | Multi-user / per-user queues? | **Out of scope.** Single-tenant. `SourceRef` is the natural discriminator if it is ever wanted. |
| 33 | Resolution result caching? | **No positive caching** (stale titles are worse than a re-fetch). Negative results (`unsupported_url`, `unavailable`) are cached for 60 s so a Telegram double-paste does not re-hit the site twice. |
| 34 | Retry exhaustion as a distinct status? | **No.** The status vocabulary is closed at eight values (BRIEF §6). An exhausted item is `error` with `attempt` on the wire, which is enough for a client to offer "retry all". |
| 35 | Plugin installation over HTTP (`POST providers/install`)? | **No.** A directory drop is the interface. A supply-chain surface with signature checking is not justified for a single-operator box. |
| 36 | Plugin sandboxing beyond rlimits (bubblewrap, a dedicated uid)? | **No**, and documented as such in bold. Packaging cost plus a "why can't my plugin see /media" support load, for a directory only the operator can write to. |
| 37 | `DELETE_FILE_ON_TRASHCAN` scope? | **All artifacts** — the primary file, chapter and subtitle files, the SC `.info.json` and `.nfo`. The default stays `false`, exactly as legacy; only the scope of "delete" changes, and legacy's scope orphaned files (Appendix B, C19). |

### 23.1 Deviations from the BRIEF

`docs/BRIEF.md` wins wherever this document conflicts with it, so a deliberate override must be
declared rather than left implicit. There are exactly **five**, all of them naming, transport or
default-value details rather than architecture, and each needs a one-line BRIEF amendment to close.
The test for inclusion is the one B2 states: *BRIEF is binding on names and values it states
explicitly, and an implementer following BRIEF and one following DESIGN must not ship different
public APIs or different observable behaviour.*

| # | BRIEF says | This design does | Why | Amendment needed |
|---|---|---|---|---|
| B1 | §9: the `ytdlp` shim "streams JSON lines … **on stdout**" | the protocol is on **fd 3**; the shim redirects its own stdout to devnull for the whole run (§9.1) | The BgUtils POT plugin, `yt-dlp-ejs` and its `deno` grandchildren print to stdout. A yt-dlp `logger` object silences yt-dlp but not a plugin or a grandchild, so stdout risks silent, intermittent stream corruption — the most expensive class of bug this system can have, and one legacy could not have because it used a pickled queue. | BRIEF §9: "on stdout" → "on a dedicated pipe (fd 3)". |
| B2 | §13: "a **`CompletionHook`** trait in `aulos-hooks` (`on_event(&JobEvent)` …, with debounce support)" | the trait is **`Hook`** with `id()` / `ordering()` / `applies(&Item)` / `run(HookCtx)` (§13) | `ordering()` is what makes the built-in sequence deterministic (audio-sync rewrites the file, so NFO and the Jellyfin scan must follow it), `applies()` keeps the filter out of every hook body, and debounce is a dispatcher concern rather than a per-hook one. Functionally a superset; the name and the method set differ. Recorded here because BRIEF is binding on names it states explicitly, and an implementer following BRIEF and one following DESIGN would otherwise ship different public APIs. | BRIEF §13: `CompletionHook`/`on_event` → `Hook`/`applies`+`run`. |
| B3 | §15: "New vars are prefixed `AULOS_`" | `PLUGINS_DIR` (introduced by BRIEF §9 itself) stays **un-prefixed**, with `AULOS_PLUGINS_DIR` defaulting to `${PLUGINS_DIR:-/config/plugins}` (§17.3) | BRIEF §9 names `PLUGINS_DIR` as the discovery variable and the community plugin format is described in terms of it, so honouring both names costs nothing and honouring only the prefixed one would break the BRIEF's own text. It is *not* a legacy variable — it exists nowhere in the Python source — so its §17.3 legend is **N\*** ("new, behaviour note"), not **L\***. | BRIEF §15: note `PLUGINS_DIR` as the one grandfathered un-prefixed new name. |
| B4 | §9: the `Provider` trait is `id()`, `matches(&Url) -> Match`, **`resolve(url, opts) -> Vec<MediaEntry>`**, **`download(entry, request, ProgressSink, CancellationToken) -> Result<Outcome>`** | the two parameter lists are bundled into **`ResolveCtx`/`DownloadCtx`** (which also carry `ytdl_options`, `paths`, `out_dir`, `tmp_dir`, `outtmpl`, `deadline`, `flat`, `playlist_end`), and the trait gains **`catalog()`**, **`own_slots()`** and **`probe()`** (§6.1) | Five positional parameters that must grow every time a provider needs one more piece of context is the signature that forces a breaking change on every plugin author; a borrowed context struct is additive. The three extra methods are load-bearing elsewhere in this document: `catalog()` is what makes validation catalog-driven and the per-URL picker possible (§6.6), `own_slots()` is how `SC_MAX_CONCURRENT_DOWNLOADS` bypasses the global semaphore exactly as legacy did (§8.7), and `probe()` is what `healthz` and the `Degraded` gate read (§6.4). Functionally a superset, but the public API differs, which is exactly the B2 test. | BRIEF §9: `resolve(url, opts)` / `download(entry, request, sink, token)` → `resolve(&Url, ResolveCtx)` / `download(DownloadCtx, ProgressSink)`, plus `catalog()`, `own_slots()`, `probe()`. |
| B5 | §16: "`PUID/PGID/UMASK` entrypoint semantics **preserved** (`CHOWN_DIRS` honoured)" | `CHOWN_DIRS=true` chowns **only the directories themselves plus the state dir**, not the volume recursively; `CHOWN_DIRS=recursive` is the new value that restores the legacy walk (§18.2, Appendix B C34) | Legacy `chown -R`'d `/app` and the entire downloads volume on *every* container start, which on a multi-TB library takes minutes and is why the user already sets `CHOWN_DIRS=false`. Keeping the name and the truthy token set while changing what `true` *does* is an observable behaviour change, so "preserved" is not accurate as written; C34 records the change but Appendix B is not where a BRIEF conflict is declared. `PUID`/`PGID`/`UID`/`GID` precedence and `UMASK` are untouched. | BRIEF §16: note that `CHOWN_DIRS=true` is non-recursive and `CHOWN_DIRS=recursive` reproduces the legacy behaviour. |

---

## Appendix A — Legacy behaviour → where it lives in v2

Covers `docs/reference/legacy-backend-spec.md` §1–§12 exhaustively. Legend:
**K** kept identical · **K\*** kept, implementation differs · **Δ** intentional change (the `C`
number is the row in Appendix B) · **✗** dropped, BRIEF out of scope.

### A.1 Config (spec §1)

| Legacy behaviour | Lives in | |
|---|---|---|
| `_DEFAULTS` table; every value starts as a string | `core::config::RawEnv`, §17.3 | K |
| `%%KEY` indirection | `config::resolve_indirections`, cycle-checked | K\* |
| Boolean token set + truthy set; exit 1 on a bad token | `config::parse_bool` | K |
| `URL_PREFIX` trailing `/` | `config::Prefix` | Δ C16 (also adds a leading `/`) |
| `PUBLIC_HOST_*` trailing `/` only if non-empty | same | K |
| `.`-prefixed option-file paths canonicalised | same | K |
| `load_ytdl_options()` order (env, then file over env) and the exact error strings | `core::ytdl_options`, §17.2 | K |
| Presets `dict[str, dict]` invariant and error strings | same | K |
| Runtime overrides (`cookiefile`) re-applied after each reload | `kv` table + `YtdlOptions::overrides` | K\* |
| `watchfiles` hot reload of `YTDL_OPTIONS_FILE` with the `samefile` filter and `{modified, added, deleted}` | `ConfigWatcher`, §17.2 (directory watch, debounce, poll fallback, manual reload) | K\* / Δ C20 |
| Presets file **not** watched | §17.2 | Δ C20 |
| A reload failure silently discarding the file's contribution | last-good options kept | Δ C20 |
| `frontend_safe()` 8 keys, two emitted as strings | `api/v2/capabilities.config` (numbers) + the v1 shim (strings) | K\* / Δ C1 |
| Env vars outside `_DEFAULTS` (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_ALLOWED_CHAT_IDS`, `METUBE_VERSION`, `PUID`…) | §17.3 | K |
| `JELLYFIN_LIBRARY_ID` / `*_REFRESH_MODE` silently ignored | `hooks::jellyfin`, §13.1 | Δ C11 |
| Pre-config `basicConfig`, third-party dampening, DEBUG ⇒ yt-dlp verbose | §16.5; DEBUG sets `verbose:true` in the runner job | K\* |
| 5 s memoised `get_custom_dirs()` recursive glob on the event loop | `api/v2/custom-dirs`: same exclusion regex, walk on `spawn_blocking`, 30 s cache, `AULOS_CUSTOM_DIRS_MAX_DEPTH` | Δ C24 |

### A.2 REST (spec §2)

| Legacy behaviour | Lives in | |
|---|---|---|
| Every route under `URL_PREFIX` | `api::router` + the `Prefix` newtype | K |
| `text/plain` bodies for JSON | `api::error` and every handler | Δ C7 (`application/json` everywhere) |
| Route table (`add`, `presets`, `cancel-add`, `subscribe`, `subscriptions*`, `delete`, `start`, cookies, `history`, `version`, `robots.txt`, static, OPTIONS) | `api::v1`, §11.1 | K |
| `GET <p>` = Angular index + `metube_theme` cookie | a small JSON identity document | ✗ / Δ C27 |
| `GET /` → 302 `URL_PREFIX` | `api::v1` | K |
| `<p>*` static frontend assets | dropped (no web UI) | ✗ |
| `<p>download/*`, `<p>audio_download/*` static with `show_index` | `api::files`, component-wise containment, `Range`/`If-Range`/`ETag`/`Last-Modified`, JSON index | K\* / Δ C16, C27 |
| CORS `on_response_prepare` reflection | `api::cors`, §11.6 | K (+ methods on v2) |
| `parse_download_options` validation matrix and messages | `core::request::validate` + the catalog; every string quoted in §11.7 | K |
| `parse_download_options` leniencies: singular `ytdl_options_preset`, a bare string in `ytdl_options_presets`, `ytdl_options_overrides` as a JSON *string*, int-ish `playlist_item_limit` and `check_interval_minutes` | `api::v1`, §11.2.1 — reproduced in the shim only; v2 is strict | K |
| `POST add` runs extraction **synchronously** and reports resolution failures as `{"status":"error","msg":…}` at HTTP 200 (playlist children joined with `", "`) | the v1 shim's bounded pre-resolve, §11.2 step 6 — same body contract, now capped by `AULOS_V1_ADD_RESOLVE_WAIT_MS`; v2 is unconditionally async (BRIEF §5) | K\* / Δ C45 |
| `_migrate_legacy_request` table | `api::v1::legacy_request`, §11.2 | K |
| Positional `dqueue.add(...)` argument order | irrelevant (a typed struct); a test mirrors the legacy assertion | K\* |
| Business errors as HTTP 200 `{"status":"error"}` | the v1 shim only | K (v2 uses 4xx) |
| Validation errors as a bare-reason 400 | the v1 shim returns the same text inside the JSON envelope | K\* |
| `supports_reuse_port()` | `SO_REUSEPORT` when available | K |
| Startup / cleanup hook order | §16.1 / §16.4 | K\* |
| `POST cancel-add` = a generation counter checked between entries, body ignored | `EngineCmd::CancelResolve { scope: CancelScope::All }` (§8.1) — the body stays ignored because a legacy client has no generation to send; it also aborts in-flight resolution | Δ C28 |
| `GET history` returns the **entire** `completed` collection | the v1 shim queries the store, uncapped by default (`AULOS_V1_HISTORY_MAX=0`); only v2 is windowed and says so | K (§11.4, C42) |
| `upload-cookies` (field `cookies`, cap **1 000 000 bytes** decimal), `delete-cookies`, `cookie-status`, all messages | `api::v2::cookies` + the v1 shim, §16.6, §11.7 | K |

### A.3 Socket.IO (spec §3)

| Legacy | Replacement | |
|---|---|---|
| `socketio.AsyncServer` at `<p>socket.io`, default namespace, JSON strings inside the event argument | `<p>ws`, one JSON object per frame | ✗ / Δ C27 |
| `all` = `[[[key,info],…],[[key,info],…]]` | `snapshot` with flat items in **the same shape as REST** | Δ C4 |
| `added` / `updated` / `completed`, full object, unthrottled, broadcast per progress hook | `added` / `delta` (changed fields, 250 ms, serialised once) / `completed` | Δ C4 |
| `canceled` / `cleared` = a bare URL string, one emit per id | `completed{items:[…]}` with `status:"canceled"` / `removed{ids, reason}`, one frame per reason (§15.1, PROTOCOL §5.6/§5.7) | Δ C5 |
| `configuration`, `custom_dirs` on connect | `api/v2/capabilities`, `api/v2/custom-dirs` | Δ |
| `ytdl_options_changed` | the `ytdl_options` frame, identical payload — **and, as legacy did on connect, the current reload state is included in the `snapshot`** (`snapshot.ytdl_options`, alongside `snapshot.health`), so a client that connects while the options file is broken or the POT sidecar is down learns it from the socket instead of having to poll two REST endpoints (§15.4 step 2, PROTOCOL §5.3) | K\* |
| `subscriptions_all`, `subscription_added/updated/removed` | `snapshot.subscriptions`, `subscription` (`{t, seq, subscription}`), `subscription_removed` (`{t, seq, ids}` — an array, where legacy emitted one bare id string per deletion); both shapes are given in PROTOCOL §5.9 | K\* |
| `formats` event (documented by the client, **never emitted** by the server) | `api/v2/capabilities.formats` + `api/v2/catalog?url=` | Δ C21 |

### A.4 Download model (spec §4)

| Legacy | Here | |
|---|---|---|
| `url` is the primary key everywhere | `id` (ULID) is; `url` is indexed data | Δ C5 |
| `id` = `entry['id']`, prefixed `"<prefix>.<id>"` | `media_id`, same prefixing; the v1 `id` projects from it | K |
| `timestamp` = `time.time_ns()` | `created_at` in ms; the v1 shim multiplies by 1e6 | K\* |
| `entry` (the full sanitised info dict) on the wire | not on the wire at all | Δ C22 |
| `filename` / `chapter_files` created lazily, so keys are sometimes absent | always present (`null` / `[]`) | Δ C20a |
| `percent` clamp `[0,99.9]`, monotonic per progress source, `100.0` on finish | `core::progress::Normalizer`, ported + golden tests | K |
| `subtitle_files` never persisted | persisted | Δ C19 |
| Status enum `pending/preparing/downloading/finished/error` | the closed 8-value v2 vocabulary; v1 mapping §11.5 | Δ C2 |
| No `postprocessing` status (a frozen UI during ffmpeg) | `postprocessing` + `msg` + `phase`/`phase_percent` | Δ C2 |
| Transition table including the cancel/clear paths | §4.2, §8.7, §8.10 | K\* |
| Every `DownloadInfo` field | `Item`/`ItemView` §4.5–4.6; v1 projection §11.4 | K\* |

### A.5 Queue mechanics (spec §5)

| Legacy | Here | |
|---|---|---|
| Three `PersistentQueue`s (`queue`, `pending`, `completed`) | one `items` table; `pending` ≡ `queued && !auto_start`, `done` ≡ terminal | K\* |
| `AtomicJsonStore` v2, tempfile+fsync+rename, `.invalid.<ts>` quarantine | SQLite WAL; the importer reads v1/v2 JSON and never quarantines | Δ C17 |
| A whole-file rewrite per put/delete | batched transactions | Δ C17 |
| Transient progress fields not persisted | still not persisted | K |
| `entry` compaction rules, including the whole-SC-entry exception | §7.5 | K |
| Legacy shelf (pickle) import | detected and reported | ✗ |
| `initialize()` re-adds all of `queue.json` at once | boot recovery re-queues; the scheduler admits `MAX_CONCURRENT_DOWNLOADS` at a time | Δ C1 |
| `get()` 2-tuple of `[key, info]` pairs | `snapshot` / `history` | Δ C4 |
| Global semaphore + the SC semaphore acquired **outside** it | `global` + `provider_slots`; `own_slots()` bypasses global | K |
| Both entry points re-check `canceled` before start | the engine checks the token before spawning (same regression test) | K |
| `multiprocessing.Process` + a Manager queue + 2 threads per download | one child process, fd-3 JSON lines, one async reader | Δ C3 |
| Child `ytdl_params` construction with user-opts-last | `ytdlp::opts`, same precedence | K |
| `put_status` key allow-list | the runner `progress` frame fields | K |
| `put_status_postprocessor` MoveFiles / SplitChapters semantics | the runner `pp` frame + `run` handling | K |
| Caption extension filtering and `.srt`→`.txt` conversion | shim `policy`, same allow-list and stripping | K |
| Thumbnail `.webm`→`.jpg` rewrite | same | K |
| `progress_source` change resets the monotonic clamp | `ProgressCell.source_tag`, now from the explicit `stream` field | K\* |
| `tmpfilename` overwritten by every message | only updated when the frame carries it | Δ C18 |
| `cancel()` = `proc.kill()` (SIGKILL, orphans ffmpeg) | SIGTERM to the pgid → grace → SIGKILL | Δ C14 |
| `_post_download_cleanup` runs inside the semaphore | the permit is released before hooks run | Δ C13 |
| Delete only `tmpfilename` on a non-finished item | full partial cleanup (`.part`, `.ytdl`, tmp dir, SC segment dir) | Δ C18 |
| `CLEAR_COMPLETED_AFTER` timer lost on restart | persisted `clear_after`, SQLite-driven | Δ C12 |
| Add recursion guard (`already`), `_canceled_urls`, `_add_generation` | `dedupe`, the cancel registry, `add_generation`, plus a depth cap | K\* |
| `__extract_info` MeTube-keys-after-user-opts + the strict-retry rule | `ytdlp` resolve + the shim's strict retry | K |
| Playlist/channel field injection (`{etype}_index` zero-padded, `_count`, `_autonumber`, `n_entries`, `__last_playlist_index`, copied parent props) | `ytdlp::outtmpl` + engine expansion, §21.2 | K |
| `_resolve_outtmpl_fields` via yt-dlp's own `evaluate_outtmpl`, `_sanitize_path_component` first | the shim's `mode=outtmpl`, so full template syntax still works | K\* |
| `playlist_item_limit` applied twice (slice + `playlistend`) | both preserved | K |
| Dedupe checks only `queue` | checks every non-terminal item, by `(canonical_key, selection)` | Δ C15 |
| `__calc_download_path` messages + `startswith` containment | the same messages, component-wise containment | Δ C16 |
| Chapter template: global at construction, per-download only when `split_by_chapters` | same | K |
| `_build_ytdl_options` layering, `null` preserved | `YtdlOptions::layer` | K |
| `impersonate` string → `ImpersonateTarget` | the shim's `coerce` | K\* |
| `auto_start is True` comparison | accepts booleans and boolean strings | Δ C10 |
| `start_pending` / `cancel` / `clear` semantics | `Start` / `Cancel` / `Delete` | K\* |
| `DELETE_FILE_ON_TRASHCAN` deletes only `filename` | also chapter/subtitle/`.info.json`/`.nfo` | Δ C19 |
| Jellyfin fired once per finished download | debounced, capped, optionally targeted | Δ C11 |
| ffmpeg PP timeout `max(600, ceil(dur/2))`, 1800 unknown | `hooks::audio_sync` | K |

### A.6 `dl_formats` (spec §6)

`aulos-provider-ytdlp::formats` / `::opts` are a literal port with golden tests (§9.8):
`AUDIO_FORMATS`, `CAPTION_MODES`, `CODEC_FILTER_MAP`, the whole `get_format` decision table
(including `custom:` checked first, the `ios` selector chain, `best_remux` →
`bestvideo+bestaudio/best`, and the quirk that `quality == "worst"` emits no `worst*` selector),
and the whole `get_opts` branch table (the audio postprocessor chain with the `writethumbnail`
guard and the **string** `preferredquality`; thumbnail `skip_download` + `writethumbnail` +
`FFmpegThumbnailsConvertor`; `best_remux`'s `opts.pop("format")` + `merge_output_format` +
`FFmpegVideoConvertor`; the per-mode caption `subtitleslangs` ordering). **K**, with two deltas:
the late `Exec` postprocessor becomes the in-process `audio_sync` hook (Δ C9), and the catalog now
labels `worst` honestly (Δ C21).

### A.7 Subscriptions (spec §7)

| Legacy | Here | |
|---|---|---|
| Data model and `to_public_dict()` 13 keys | §14.1 (the v1 shim emits exactly 13) | K |
| `subscriptions.json` whole-file rewrite; legacy shelf import | `subscriptions` + `subscription_seen` tables; the JSON importer | Δ C17 |
| `timestamp` in memory only, not persisted | same | K |
| 60 s tick, first check at +60 s, sequential checks | per-subscription timers, first check ~+10 s with jitter, bounded concurrency | Δ C8 |
| `extract_flat_playlist` applying user options **last** | the same order as normal adds | Δ C23 |
| `_is_media_entry`, `_entry_id`, `_entry_video_url`, tab recursion depth 1 / first 5 children | ported verbatim | K |
| `add_subscription` duplicate-URL and in-flight guards; `Missing URL`, `Could not resolve URL`, `VIDEO_ONLY_MSG` | ported verbatim (a unique index plus an in-flight set) | K |
| Backfill suppression except `is_upcoming` | ported verbatim | K |
| New = unseen + already-seen `is_live` | ported verbatim | K |
| Failed entries not marked seen; the first 3 errors joined | ported verbatim | K |
| `seen_ids` dedupe + truncate newest-first | `subscription_seen` + `PruneSeen` | K\* |
| An extraction failure leaving `last_checked` untouched (a 60 s hot-retry loop forever) | `last_checked` always updated; exponential backoff to 6 h | Δ C8 |
| `update_subscription`: only `enabled`/`interval`/`name`; `ValueError` → 500 | the same fields; 400 on bad input | Δ C25 |
| `delete_subscriptions` always ok, emits per id | same | K |
| `folder == ""` → `None` | same | K |
| The scheduler is a `Notifier` caller | the scheduler is a **producer only** into the `EventRouter` (§2.2.1); it registers no inbox | K\* |
| `POST subscriptions/check` awaits every check | 202 with a `job_id` | Δ C8 |
| Subscriptions are yt-dlp only | routed through the provider registry | Δ C26 |

### A.8 Telegram (spec §8)

All of spec §8 is **K** — the exact texts, the `cfg:` callback grammar, the `[0,1,5,10,20]` limit
keyboard, the URL regex and its trailing-punctuation set, the max-URLs message, the SSRF guard,
`_normalize_download_selection`, the ✅/❌ completion messages and both watchdog messages —
except:

| Legacy | Here | |
|---|---|---|
| Attribution via a `contextvars` chat id read in `on_added` | `SourceRef` on the job, so playlist children **and** optionally web/subscription jobs are covered | Δ C6 |
| A 15 s monitor poll loop with its own lock | a 1 Hz tick driven off the progress cells | K\* |
| Per-chat config in `telegram_bot_config.json` | `telegram_chats` in SQLite, imported once | K\* |
| **No** progress reporting or message editing at all | the live board, §12.4 | Δ C6 |
| `get_available_formats()` list passed only to the bot | `FormatCatalog::bot_formats()` — a documented projection of the one shared `ytdlp` catalog that reproduces the legacy nine entries exactly, including the `thumbnail`→`jpg` alias, the `any`/`audio` pseudo-quality and `mp4`/`best_remux`, and offering `ios` only `best` even though the API catalog now offers nine heights for it (§12.2). The `cfg:` grammar is therefore unchanged and captions stay unreachable from the keyboard, as in legacy | K\* |

### A.9 StreamingCommunity (spec §9)

All of spec §9 is **K** (§10 lists the module per step) except:

| Legacy | Here | |
|---|---|---|
| `curl_cffi` `impersonate="chrome"` | `wreq` behind `ScHttp`, with a `reqwest` fallback and `AULOS_SC_HTTP` | K\* |
| Detection by **host** (`can_extract`) but dispatch by **path** (`extract()`: `/season-`, `/watch/`, `/titles/`, else log + `None`), so any other SC URL — and any scrape exception — fell through to yt-dlp in `__extract_info` | `matches()` is host-**and**-path and returns `Match::No` for a non-dispatchable path, so yt-dlp gets those URLs exactly as before (§10.2); a scrape that resolves nothing raises `ProviderError::Unsupported` and the engine retries once through the runner-up (§6.4, §8.4). A host-only `Strong(200)` would have terminated both cases with `unsupported_url` | K\* / Δ C46 |
| `extract_season` doing 3+ round trips per episode (~60 for 20 episodes) to obtain an m3u8 it discards | zero embed/stream requests during resolution: 2 requests total | Δ C29 |
| A debug-only full `GET` of the m3u8 on every download | removed | Δ C29 |
| `SC_THREAD_COUNT` / `SC_USE_FFMPEG` re-read from `os.environ` inside the child | read from `Config`; one source of truth | Δ C30 |
| Segment counts written into `downloaded_bytes`/`total_bytes` | written to `fragment_index`/`fragment_count`; byte fields stay `null` until real | Δ C31 |
| Inertia version cached forever per instance; no retry on drift | 30 min TTL, single-flight, one retry on 403/404/409 | Δ C29 |
| Output naming `<sanitised title>.mp4` + `.info.json`, ignoring `OUTPUT_TEMPLATE*` | **kept**; opt-in template naming | K |
| The N_m3u8DL-RE argv, the ffmpeg fallback, the ANSI last-match-wins parser, the gapless natural-order concat (never `-f concat`) | ported verbatim | K |

### A.10 Jellyfin / NFO / audio-sync (spec §10)

| Legacy | Here | |
|---|---|---|
| `POST {base}/Library/Refresh`, `MediaBrowser Token`, no body, the exact error message shapes | `hooks::jellyfin` | K |
| Refreshes all libraries; `JELLYFIN_LIBRARY_ID` inert | targeted `Items/{id}/Refresh` when set, with a one-shot fallback and a WARN | Δ C11 |
| One refresh per finished download | 30 s debounce with a 300 s cap | Δ C11 |
| `jellyfin_nfo_generator.py`: an unwired CLI that deletes the `.info.json` it consumes | an in-process hook for SC items, reading the in-memory entry; deletion opt-in | Δ C9 |
| `audio_sync_fix.py` as an `Exec` PP at a hard-coded container path | an in-process hook with a `postprocessing` status, real progress, and a failure that does not fail the item | Δ C9 |
| ffprobe-derived timeout `max(600, ceil(dur/2))`, `1800` unknown | same | K |

### A.11 BgUtils POT (spec §11)

| Legacy | Here | |
|---|---|---|
| Sidecar binary at `/usr/local/bin/bgutil-pot`, arch-matched, from the **latest** release | same, but the tag is **pinned** via a build arg with a weekly bump PR | Δ C32 |
| The yt-dlp plugin zip unpacked into site-packages | same; the runner reports the discovered plugin list in `hello` and `healthz` shows it | K\* |
| Started by the entrypoint as an unsupervised `&` child | supervised by the server: backoff, a health probe, a force-restart after 3 failed probes, `failed` after 10 restarts in 10 min | Δ C33 |
| `deno` installed for `yt-dlp[deno]` / `yt-dlp-ejs` | same | K |
| `extractor_args` supplied through `YTDL_OPTIONS` | same; option dicts pass through untouched | K |

### A.12 Process / deploy (spec §12)

Preserved: PUID/PGID/UID/GID precedence, `umask`, `mkdir -p` of the directories, `CHOWN_DIRS`
gating, the root warning, the `gosu` privilege drop, `tini -g` as PID 1, the `/downloads` volume,
`EXPOSE 8081`, every `ENV` default, and the `VERSION` → `METUBE_VERSION` build arg. Changed: the
healthcheck hits `healthz` and honours `URL_PREFIX` (Δ C33); the Node/Angular stage is gone (Δ
C27); Python exists only for the runner shim and yt-dlp; `CHOWN_DIRS=true` is O(1) instead of
walking the whole volume (Δ C34); CI builds amd64 only for now (BRIEF §16); `bgutil-pot` is
started by the server, not the entrypoint (Δ C33). CI workflows are ported one-for-one with the
yt-dlp bump automation hardened (§18.5).

---

## Appendix B — Intentional changes, each with a user-visible reason

| # | Change | Why it is better for the user |
|---|---|---|
| C1 | `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` are numbers in v2 (strings preserved in v1). | The client stops writing three-branch flexible decoders. |
| C2 | New statuses `resolving`, `postprocessing`, `canceled`; `phase`/`phase_percent`. | The two states the old UI lied about — "nothing is happening yet" and "ffmpeg is remuxing a 4 GB file" — are now visible. Progress bars stop looking frozen. |
| C3 | One child process per job speaking a documented JSON-lines protocol over fd 3, instead of a fork plus a pickled Manager queue and two thread-pool threads per download. | Metadata extraction can no longer starve downloads; the box does not fork an interpreter per job; RSS drops from 250–400 MB to under 90 MB. |
| C4 | Progress is batched at 250 ms, delta-encoded (changed fields only), and serialised **once** into shared bytes; `added`/`completed`/`removed` are prompt at 25 ms; an idle server sends nothing. | The single biggest snappiness fix. The app deletes its own throttle, its `pendingUpdates` map and the index-0 insertion that reordered rows; the radio stays asleep when nothing is happening. |
| C5 | One immutable server-assigned ULID used by every endpoint and every frame; `removed` carries ids. | Deletes the client's `id → url → UUID` fallback chain, the dual-key `removeItems`, the `url ?? id` delete key and the "skip items with no url" bug in *Clear completed*. |
| C6 | `POST add` returns `202` before any metadata extraction. | Deletes the whole background-upload / app-group / staging / dedup / sweeper machinery in the share extension (~900 lines and 3 follow-up bug fixes) and makes the share sheet honest. |
| C7 | Honest HTTP: 4xx for client errors, `401` never a redirect, `202` on add, `application/json` everywhere, one error envelope with a closed code taxonomy. | The client stops treating "2xx with an HTML body" as an expired session, and branches on `bot_check` instead of regex-matching prose. |
| C8 | Subscriptions: per-subscription timers, first check ~10 s after boot with jitter, bounded concurrency, `last_checked` always updated, exponential backoff to 6 h, `check` returns immediately. | A dead feed stops hammering YouTube every 60 seconds forever; a fresh boot picks up new videos immediately instead of a minute later; one slow feed can no longer hang an HTTP request or block every other subscription. |
| C9 | NFO generation and the audio-sync re-encode run in-process, ordered, with a `postprocessing` status and progress; a hook failure never fails the item. | NFOs actually get written (the legacy script was never wired), and a failed audio-sync no longer makes a perfectly good download report as an error. |
| C10 | `auto_start` accepts boolean strings. | An iOS Shortcut sending `"true"` no longer has its download silently parked in *pending*. |
| C11 | Jellyfin: 30 s debounce with a 300 s cap, and a targeted library refresh when `JELLYFIN_LIBRARY_ID` is set; the three documented-but-inert env vars now work. | A 500-item playlist triggers a handful of scans instead of 500 full-library ones, and only rescans the library that changed — minutes instead of hours of Jellyfin CPU. |
| C12 | `CLEAR_COMPLETED_AFTER` survives restarts and applies to items aged out of memory. | The setting finally means what it says. |
| C13 | Terminal handling and hooks run outside the download slot. | The next download starts immediately instead of waiting for a disk-touching cleanup. |
| C14 | Cancel is SIGTERM to the process group, then SIGKILL, with full partial cleanup. | No more orphaned ffmpeg / N_m3u8DL-RE processes eating CPU after a cancel, and no more stray `.part` files. |
| C15 | Dedupe covers every non-terminal item and is keyed on a canonical URL/media id plus the selection; playlist children are exempt. | Re-adding a URL that is already pending no longer silently replaces the first entry, `youtu.be/x` and `watch?v=x&t=30` collapse, and you can still grab the mp3 of something you are pulling as mp4. |
| C16 | Path containment is component-wise on canonicalised paths, symlink escape is rejected, and `URL_PREFIX` gains a leading `/`. | `/downloads-evil` no longer passes as inside `/downloads`, and `URL_PREFIX=metube` no longer produces routes like `metubeadd`. |
| C17 | SQLite WAL with batched transactions replaces four whole-file JSON stores. | Adding a 500-item playlist is ~6 transactions instead of 500 full-file rewrites and 1000 `fsync`s; a subscription check writes only the new seen ids instead of rewriting a 50 000-element array. |
| C18 | `tmpfilename` is only updated when a frame carries it; cleanup removes every partial. | Partial-file cleanup actually finds the partial file. |
| C19 | `DELETE_FILE_ON_TRASHCAN` also removes chapter, subtitle, `.info.json` and `.nfo` siblings; `subtitle_files` is persisted. | No orphaned files after a delete, and caption downloads keep their file list across a restart. |
| C20 | `YTDL_OPTIONS_FILE` reload keeps the last-good config on failure, watches the directory, debounces, has a poll fallback and a manual reload endpoint; the presets file is watched too. | A typo in the options file no longer silently changes download behaviour, and atomic-replace edits (vim, Ansible, `docker cp`) and `/config` on NFS are actually detected. |
| C20a | Every optional field is always serialised (`null`, never absent); `percent` is always a number; `eta` is always integer seconds. | The whole "the Swift type says non-optional but the key is missing" bug class disappears. |
| C21 | A provider-aware format catalog, plus `GET api/v2/catalog?url=` and a flat legacy-shaped `capabilities.formats`. | The quality picker stops offering 2160p for a StreamingCommunity item that has one rendition, `worst` stops lying, and the app's dead `formats` code path goes live with zero new models. |
| C22 | `entry` (the full yt-dlp info dict) is no longer sent to clients. | Payloads shrink by 10–100×; the client's own comments say logging them stalls the UI. |
| C23 | Subscription extraction uses the same option precedence as normal adds. | A `YTDL_OPTIONS` that works for downloads can no longer silently break every subscription. |
| C24 | The custom-dirs listing is a bounded, background-refreshed cache computed off the event loop. | A multi-TB library no longer stalls the server on every client connect. |
| C25 | `subscriptions/update` returns 400 instead of leaking a 500. | Actionable errors. |
| C26 | Subscriptions run through the provider registry. | You can subscribe to a StreamingCommunity series or a plugin feed, not only a yt-dlp source. |
| C27 | Socket.IO, the Angular UI, the `metube_theme` cookie, the HTML directory index and pickle/shelve import are dropped. | BRIEF scope. The one real consequence — no live updates for the *currently installed* iOS build — is handled by shipping the v2 client in the same cutover session. |
| C28 | `cancel-add` aborts in-flight resolution, not just the gap between entries. | Cancelling a 500-item add stops it now, instead of after the next entry finishes extracting. |
| C29 | SC season resolution does 2 requests instead of ~60, caches the Inertia version with a TTL and retries once on version drift, and drops the debug-only m3u8 fetch. | Adding a 20-episode season takes seconds instead of a minute and is far less likely to be rate-limited or to break on a site deploy. |
| C30 | `SC_THREAD_COUNT` and `SC_USE_FFMPEG` come from the config, not from a child re-reading the environment. | One source of truth; `check-config` shows what will actually be used. |
| C31 | SC segment counts go to `fragment_*` and byte fields stay `null` until real sizes are known. | The byte counters stop lying and the progress bar stays monotonic. |
| C32 | `bgutil-pot`, `N_m3u8DL-RE` and yt-dlp are all pinned build args with separate auto-bump PRs, and the yt-dlp bump PR must build the image and run a real extraction before auto-merging. | A bad nightly fails in CI instead of on the VPS, and a regression is bisectable to one PR. |
| C33 | The POT sidecar is supervised with backoff, health-probed, force-restarted when wedged, and surfaced in `healthz`; the Docker healthcheck hits `healthz` and honours `URL_PREFIX`. | "YouTube suddenly wants a login" becomes a visible red component instead of a mystery. |
| C34 | `CHOWN_DIRS=true` chowns the directories themselves plus the state dir; `CHOWN_DIRS=recursive` restores the exact legacy walk. | Container start stops taking minutes on a multi-TB library. |
| C35 | Automatic retry for retryable errors only (max 2), plus a real retry action reachable from v1 `POST /start`. | Transient failures self-heal, and a failed item is one call from retrying instead of delete-and-re-share. |
| C36 | A bounded resolution pool separate from download slots, plus priority classes. | A 500-item playlist can never starve downloads or the API, and a link you paste now starts next instead of behind 497 children. |
| C37 | Telegram gains a rate-limit-aware live progress board and (by default) watches web and subscription jobs too. | The bot finally answers "how far along is it?" without spamming, and reports downloads it used to be blind to. |
| C38 | `?since=<seq>` with `boot_id`, merged resume, and `ETag`/`304` on state. | Pull-to-refresh becomes a 304 instead of a full history payload, no longer needs to tear down the socket, and can never silently drop frames across a server restart. |
| C39 | A bounded connect payload: a 500-item done window, a group-children threshold, an explicit `truncated` block, and paged history. | Foregrounding the app costs the same after a year of use as it does on day one. |
| C40 | `ItemView` echoes the **whole** request — `selection` and `folder` plus a nested `request` object with the other eight fields (§4.6.1), secret-looking override *values* redacted. | "Download this again with the same options" and a request inspector become possible in a v2-only client. v1 already projected all of these, so echoing only `selection` would have made v2 a strictly narrower payload than the API it replaces. |
| C41 | A `pause` action: `queued(auto_start=true)` un-schedules, and a running job is killed but keeps its partial file so `start` resumes it (§8.7, §23 decision 38). | The other half of iOS ask 11. Parking a download no longer means cancelling it and losing the bytes already on disk, and the status vocabulary stays closed at eight values. |
| C42 | `AULOS_V1_HISTORY_MAX` (default `0` = unlimited) caps the v1 `history` `done[]`; v2 windows the same data and advertises `done_total`/`truncated` instead. | The shipped client's "Completed" section keeps showing every record at cutover instead of silently shrinking to the most recent 500, while a v2 client gets a connect payload that does not grow forever. |
| C43 | `video`/`ios` keeps the full nine-height quality list in the catalog (§6.6), and the caption catalog advertises all seven legacy formats (`srt, txt, vtt, ttml, sbv, scc, dfxp`) everywhere it is published. | `{video, ios, 1080}` and a `.dfxp` caption download were both legal in legacy and both produce real selectors; a narrower catalog would reject requests the old server accepted and would quietly delete four caption formats from every client's picker. |
| C45 | v1 `POST <p>add` keeps reporting resolution failures in its body by waiting for resolution for up to `AULOS_V1_ADD_RESOLVE_WAIT_MS` (default 10 s) before answering; v2's add is unconditionally async (§11.2 step 6). On a failure the item is **kept** as an `error` row, where legacy created nothing. | The shipped share extension's *only* failure path is parsing that body, so without the wait a mistyped or geo-blocked link would report "queued" and the "Couldn't add to Aulos" notification would never fire. The wait is also strictly shorter than legacy's, which blocked for the whole extraction with no ceiling. Keeping the failed row means the user can see *why* it failed in the queue instead of only in a notification they may have missed. |
| C46 | `streamingcommunity` matches on **host and path** (`/watch/`, `/titles/`, `/season-`) and returns `Match::No` otherwise; a healthy provider answering `Unsupported` is retried **once** through the runner-up (§6.4, §8.4, §10.2). | Legacy detected SC by hostname but dispatched by path and handed everything else to yt-dlp. A search page, a browse page or a mirror's homepage on an SC host therefore keeps working exactly as it did, instead of dying with `unsupported_url`. |
| C44 | An upcoming livestream (and any entry-level pre-download `msg`) is `queued` with `auto_start = false` and a populated `error`, not a terminal `error` (§8.4). | Legacy's behaviour restored: the row sits in "In Progress"/`pending[]` with its scheduled-start text, is one tap from starting, and is re-queued by its subscription when the stream goes live — instead of appearing in **Failed** and never starting. |

**Kept on purpose (K1).** SC output naming and its `.info.json`; the `[0, 99.9]` percent clamp and
its monotonic-per-source rule; `preferredquality` as a string; the exact N_m3u8DL-RE argv; the
gapless natural-order concat and the refusal to use `-f concat`; the `format.startswith("custom:")`
escape hatch; `null` in a preset clearing a key; presets applied in request order; `best_remux`
popping a user `format`; `writethumbnail` only added when the user has not set it; caption `txt`
mapping to `srt` plus post-conversion; the `worst` selector's actual behaviour; subscription
backfill suppression except `is_upcoming`; re-queueing already-seen `is_live` entries; every legacy
validation message and every legacy error string; the whole legacy validation matrix; and
`SC_MAX_CONCURRENT_DOWNLOADS` being acquired outside the global slot. Existing Jellyfin libraries,
user scripts and muscle memory depend on these. **Added to K1 by the same reasoning:** the
the v1 `POST add` body contract for resolution failures, including the `", "` child-error joiner
(§11.2, C45); the five `parse_download_options` leniencies (§11.2.1); the decimal 1 000 000-byte
cookie cap and its message (§16.6); the SC host-plus-path dispatch and its fall-through to yt-dlp
(§10.2, C46); the
legacy nine-entry Telegram format list and its `cfg:` grammar (§12.2); the SC `.info.json` written
in the legacy flat key shape, `_sc_*` names included (§10.3); the full nine-height quality list for
`video`/`ios` and all seven caption formats (§6.6); the pre-download-problem item staying
non-terminal with a populated `error` (§8.4); and the v1 `history` `done[]` being the whole
completed set (§11.4).
