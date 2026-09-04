# Aulos Server — Architecture Proposal (candidate: **REALTIME & PERFORMANCE FIRST**)

Author lens: make the iOS client feel *instantaneous*. Every structural decision below is
justified against a latency, allocation, syscall, or byte budget. Where the BRIEF is binding I
follow it; where it is silent I decide and say why.

Status: design candidate. No Rust written yet. Companion docs this would become:
`docs/DESIGN.md` (this, edited down), `docs/PROTOCOL.md` (§5–§7 extracted verbatim),
`docs/PLAN.md` (§22 milestones).

---

## 0. Reading guide

| § | Content | Depth |
|---|---|---|
| 1 | Performance thesis + the ten invariants | headline |
| 2 | Workspace and crate/module map | full |
| 3 | `aulos-core`: domain types, ids, status, config | full |
| 4 | **In-memory state layout** (the Hub, HotCell, published snapshot) | deepest |
| 5 | **Realtime protocol v2**: frames, seq, resume, backpressure | deepest |
| 6 | **Delta batching engine** | deepest |
| 7 | REST v2 surface + v1 shim | full |
| 8 | `aulos-store`: SQLite DDL, write batching, importer | deep |
| 9 | `aulos-provider`: trait, sink, registry, command plugins | full |
| 10 | `aulos-provider-ytdlp`: shim protocol, **process/kill semantics** | deep |
| 11 | `aulos-provider-sc` | full |
| 12 | `aulos-queue`: scheduler, slots, resolver pool, groups | deep |
| 13 | `aulos-hooks`, `aulos-telegram`, `aulos-subscriptions` | full |
| 14 | `aulos-server`: wiring, supervision, shutdown | full |
| 15 | Channel/backpressure inventory | deep |
| 16 | Memory bounds | deep |
| 17 | Sequence walkthroughs (8 required scenarios) | full |
| 18 | Legacy behaviour map, spec §1–§12 | full |
| 19 | Intentional divergences from legacy | full |
| 20 | Dependencies | full |
| 21 | Performance budgets, benchmarks, observability | full |
| 22 | Risk register + milestones | full |
| 23 | Open questions | — |

---

## 1. Performance thesis

The legacy backend's snappiness problem is not CPU. It is **fan-out amplification** and
**write amplification**:

| Legacy amplifier | Measured shape | Cost at 3 concurrent downloads, 2 clients |
|---|---|---|
| One Socket.IO broadcast per yt-dlp progress hook | ~20–30 msgs/s/job, full `DownloadInfo` (~1.5–3 KB JSON, plus double-encoding) | ~60–90 frames/s, **180–540 KB/s per client**, 2 JSON encodes + 2 parses each |
| One whole-file JSON rewrite + 2 `fsync` per enqueue | 500-item playlist ⇒ 500 rewrites, O(n²) bytes | ~63 MB written, 1000 fsyncs, ~10–40 s of blocked event loop |
| One forked process + one `multiprocessing.Manager` proxy queue per job | 2 pool threads/job, every status pickled twice over a socket | executor starvation; `__extract_info` queues behind `proc.join` |
| Synchronous metadata extraction inside `POST /add` | 1–8 s (worse with POT) | drove the entire iOS background-upload/app-group/staging/notification stack |

The design below attacks each with a specific mechanism:

| Mechanism | § | Effect |
|---|---|---|
| Single-owner in-memory state Hub, no locks on the hot path | 4 | mutation cost ~120 ns, no contention, trivially correct `seq` order |
| Per-job cache-line-isolated `HotCell` of atomics, **pull-sampled at tick** | 4.4 | progress ingestion = 8 relaxed stores, **zero allocations, zero channel traffic** |
| Field-masked delta frames, serialized **once** into shared `Bytes` | 6 | ~0.9 KB/s/client steady state (**~120–500× reduction**) |
| Durable monotonic `seq` + replay ring + `?since=` | 5.5 | reconnect costs one small delta, not a full `/history` |
| Async add returning `202` before any extraction | 7.2 | deletes ~900 lines of iOS machinery (client ref §7.1) |
| Bounded resolver pool separate from download slots | 12.3 | a 500-item playlist never starves downloads |
| Batched single-transaction SQLite writes, progress never persisted | 8.4 | 500-item add = **1 transaction, ~2 ms, ~180 KB** |
| Process-group SIGTERM→SIGKILL | 10.4 | no orphaned ffmpeg, `.part` files cleaned |
| Server-computed group aggregates | 12.5 | whole-playlist progress in O(1) per child change |

### 1.1 The ten invariants

These are testable properties. Any change that breaks one is a regression.

| # | Invariant | Enforced by |
|---|---|---|
| I1 | Steady-state WS byte rate is **O(active downloads)**, not O(queue size) | delta frames only include dirty items; queued items are static |
| I2 | An item's `id` is assigned once and never changes, including across playlist promotion and restart | ULID minted in the API handler before any I/O; §12.5 |
| I3 | Every item carries `seq` (creation order); the client **never sorts** | `seq_created` from the durable hi/lo allocator; §8.3 |
| I4 | `percent` is monotone non-decreasing per item per progress source | port of `_calculate_progress_percent`; §10.5 |
| I5 | No progress datum ever reaches SQLite | `StoreCmd` has no progress variant; §8.4 |
| I6 | No client can slow down the server or another client | per-client writer task + `broadcast` lag → snapshot resync; §5.7 |
| I7 | The Hub never awaits anything that can block (no I/O, no locks) | `Hub::run` only touches memory + `try_send`; §4.2 |
| I8 | Cancel kills the whole process group within `AULOS_KILL_GRACE_MS + ε` | `process_group(0)` + `killpg`; §10.4 |
| I9 | Resident memory is bounded independent of lifetime queue size | done-window cap, replay-ring byte cap, entry-JSON compaction; §16 |
| I10 | A `202` from `POST api/v2/downloads` means the row is in the WAL | `Durability::Sync` on add; §8.4 |

---

## 2. Workspace and module map

```
aulos_server/
  Cargo.toml                      # [workspace] resolver = "3", edition 2024, rust-version 1.95
  Cargo.lock                      # committed (binary crate)
  rust-toolchain.toml             # channel = "1.95"
  crates/
    aulos-core/       aulos-store/       aulos-provider/
    aulos-provider-ytdlp/            aulos-provider-sc/
    aulos-queue/      aulos-api/         aulos-telegram/
    aulos-subscriptions/             aulos-hooks/        aulos-server/
  docker/  plugins/examples/  docs/  .github/workflows/
```

Dependency direction (no cycles):

```
                aulos-core  ◄──────────────────────────────────┐
                    ▲                                          │
   aulos-store ─────┤                                          │
   aulos-provider ──┤◄── aulos-provider-ytdlp                  │
                    │◄── aulos-provider-sc                     │
   aulos-hooks ─────┤                                          │
                    │                                          │
   aulos-queue ─────┴── (core, store, provider, hooks)         │
        ▲                                                      │
        ├── aulos-api          (core, store, queue)            │
        ├── aulos-telegram     (core, queue) ──── notifier ─────┘
        ├── aulos-subscriptions(core, store, queue)
        └── aulos-server       (everything)
```

### 2.1 Module lists

**`aulos-core`** — zero I/O, zero tokio-runtime assumptions (only `tokio::sync` types).

| Module | Contents |
|---|---|
| `id` | `ItemId`, `GroupId`, `SubId`, `BootId` (newtypes over `Ulid`), `Seq(u64)`, `SeqAllocator` trait |
| `status` | `Status` (closed enum), `Phase`, `TerminalKind`, v1 projection |
| `item` | `ItemView`, `GroupView`, `Progress`, `OutputFile`, `Source`, `FieldMask` (bitflags) |
| `request` | `DownloadRequest`, `DownloadType`, `Codec`, `Container`, `Quality`, `SubtitleMode`, validation |
| `formats` | port of `dl_formats.py`: `get_format`, `get_opts`, `FormatCatalog` |
| `config` | `Config`, `EnvSource`, `BoolToken`, `YtdlOptions`, `Presets`, `ConfigError` |
| `event` | `Mutation`, `Frame`, `FrameKind`, `WireFrame`, `DeltaBatch`, `ItemDelta`, `GroupDelta` |
| `error` | `CoreError` (thiserror), `ApiError`, `ErrorCode` (closed string enum) |
| `paths` | `DownloadRoots`, `SafeRelPath` (containment check done right), out-template resolution helpers |
| `progress` | `calc_percent` (port), `ProgressSourceKey`, monotonic clamp |
| `notify` | `Notifier` trait (Telegram is impl #1; APNs hook) |
| `metrics` | metric name constants + registration helpers |

**`aulos-store`**

| Module | Contents |
|---|---|
| `lib` | `StoreHandle` (cheap clone), `Store::spawn(cfg) -> StoreHandle` |
| `actor` | dedicated OS thread, `recv_many` batching, transaction assembly |
| `cmd` | `StoreCmd`, `Durability`, `StoreQuery` |
| `schema` | DDL, `migrate()`, `user_version` ladder |
| `sql` | `prepare_cached` statement text constants |
| `read` | read-only connection pool (WAL snapshot reads) |
| `seq` | hi/lo `SeqAllocator` impl backed by `meta` |
| `import` | legacy `queue.json`/`pending.json`/`completed.json`/`subscriptions.json`/`telegram_bot_config.json` (schema_version 1 & 2), quarantine handling |
| `model` | row structs ⇄ core types |

**`aulos-provider`**

| Module | Contents |
|---|---|
| `lib` | `Provider` trait, `Match`, `ProviderLimits`, `ProviderError`, `Outcome`, `Resolution`, `MediaEntry`, `Job` |
| `sink` | `ProgressSink`, `HotCell`, `HotProgress`, `SinkFactory` |
| `registry` | `Registry` (score-ordered `Vec<Arc<dyn Provider>>`), selection, per-provider semaphores |
| `proc` | `spawn_group`, `KillSwitch`, `LineReader` (reusable-buffer NDJSON), `StderrRing` |
| `command` | `plugin.toml` model, discovery, `CommandProvider`, `RegexProgressParser`, `JsonLinesParser` |
| `ansi` | ANSI/OSC stripper, "last match wins" frame scanner |

**`aulos-provider-ytdlp`** — `lib`, `opts` (option layering), `shim` (NDJSON protocol types),
`runner` (process lifecycle), `extract` (resolve mode), `download`, `percent`, `python/ytdlp_runner.py`.

**`aulos-provider-sc`** — `lib`, `http` (impersonating client + fallback), `inertia`, `scrape`
(watch/season/title), `m3u8` (just-in-time re-extraction), `nm3u8dlre`, `ffmpeg`, `gapless` (natural-order
segment mux), `progress`.

**`aulos-queue`**

| Module | Contents |
|---|---|
| `hub/` | `mod` (`Hub::run`), `state` (`ItemState`, `GroupState`), `publish` (`Published`, `ArcSwap`), `delta` (mask→JSON), `ring` (`ReplayRing`), `bus` |
| `scheduler` | ready heap, global + per-provider slots, `JoinSet` of job tasks, restart policy |
| `resolver` | bounded resolve pool, group expansion, dedupe |
| `job` | `JobTask` (spawn → sink → outcome → hooks), stall/timeout watchdog |
| `cancel` | `CancelRegistry` (`DashMap<ItemId, CancelHandle>`) |
| `dedupe` | `canonical_key` computation + active-key index |
| `handle` | `QueueHandle` — the only thing `aulos-api`/`telegram`/`subscriptions` see |

**`aulos-api`** — `lib` (router), `v2/{downloads, items, groups, history, state, subs, config, cookies, dirs, capabilities}`,
`ws/{mod, session, protocol}`, `v1/{mod, add, history, delete, start, version, subscribe}`, `health`,
`static_files`, `error` (`IntoResponse` for `ApiError`), `mw/{request_id, trace, cors, auth_passthrough, body_limit}`.

**`aulos-telegram`** — `lib`, `bot` (teloxide dispatcher), `config_ui` (inline keyboards), `urls`
(extraction + SSRF guard), `progress` (per-chat live message editor), `notifier` (impl `Notifier`).

**`aulos-subscriptions`** — `lib`, `model`, `scheduler` (per-sub timers + jitter + backoff), `check`,
`projection` (public dict).

**`aulos-hooks`** — `lib` (`HookDispatcher`), `jellyfin` (debounced refresh), `nfo` (quick-xml), `audiosync`
(ffprobe/ffmpeg), `debounce`.

**`aulos-server`** — `main`, `wire` (build all actors), `supervise/pot`, `signals`, `shutdown`,
`log` (tracing init + third-party dampening), `entry` (PUID/PGID assertions, dir prep).

---

## 3. `aulos-core`

### 3.1 Identity and ordering

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(Ulid);
pub struct GroupId(Ulid);   // same namespace as ItemId: a record is an item XOR a group

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(pub u64);

/// Durable, monotonic, gap-tolerant. Hi/lo: reserve 1024 at a time in `meta`.
pub trait SeqAllocator: Send + Sync {
    fn next(&self) -> Seq;
    fn current(&self) -> Seq;
}
```

Rationale for **two** ordering keys:

* `id: ItemId` (ULID string) — the BRIEF's immutable identity. Human-pasteable, sorts by
  creation time, no coordination needed to mint (the API handler mints it *before* touching the
  store, which is what makes `202` fast).
* `seq: u64` — the client's sort key and the protocol's cursor. A `u64` compares in one
  instruction, survives JSON as a number, and is what `?since=` and the replay ring key on.
  It must be **durably monotonic** across restarts, otherwise a client resuming with
  `since=90210` after a restart could be handed frame `seq=12` and silently drop everything.
  Hi/lo allocation writes one row per 1024 ids; a crash skips ≤1023 values, which is harmless
  because only monotonicity is contracted.

`Seq` is used for *both* per-item creation order (`item.seq`) and per-frame order (`frame.seq`)
from the same allocator. One counter, one meaning: "server-side happens-before".

### 3.2 Status (closed, per BRIEF §6)

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued, Resolving, Preparing, Downloading, Postprocessing,
    Finished, Error, Canceled,
}
impl Status {
    pub const fn is_terminal(self) -> bool { matches!(self, Finished | Error | Canceled) }
    pub const fn is_active(self) -> bool { matches!(self, Resolving | Preparing | Downloading | Postprocessing) }
    /// Only these produce a HotCell / hold a slot.
    pub const fn is_running(self) -> bool { matches!(self, Preparing | Downloading | Postprocessing) }
    pub const fn v1(self) -> &'static str { /* §7.6 */ }
}
```

Legal transitions (anything else is a bug and `debug_assert!`s):

```
Queued ──► Resolving ──► Queued            (resolve produced a single entry / children)
Queued ──► Preparing ──► Downloading ──► Postprocessing ──► Finished
                     └────────────────────────────────────► Error
any non-terminal ──► Canceled
Error|Canceled ──► Queued                  (explicit retry, attempt += 1)
Finished ──► (removed)                     (delete / CLEAR_COMPLETED_AFTER)
```

`Postprocessing` is new (legacy had none) and is the single biggest *perceived*-latency fix
after batching: a 4-minute ffmpeg remux currently shows the user a frozen `downloading` bar at
99.9 %. See §19.

`Queued` carries `auto_start: bool`; `auto_start = false` is the legacy `pending` bucket. One
status, one flag — the client renders "Queued" vs "Paused" from the flag, and the v1 shim
projects the two onto the legacy `queue`/`pending` arrays.

### 3.3 Item and group views

`ItemView` is exactly the REST/snapshot JSON shape (BRIEF §3: "Snapshot and REST item shapes are
identical"). It is `Arc`-shared and never mutated; the Hub replaces the whole `Arc` when a field
changes. All `Arc<str>`, so a replacement copies pointers, not text.

```rust
#[derive(Serialize)]
pub struct ItemView {
    pub id: ItemId,
    pub seq: Seq,
    pub group_id: Option<GroupId>,
    pub group_index: Option<u32>,
    pub url: Arc<str>,
    pub title: Arc<str>,
    pub status: Status,
    pub auto_start: bool,
    // progress — always a number or null, never a string (client ref §7.7)
    pub percent: Option<f64>,          // 0.0..=100.0
    pub speed: Option<f64>,            // bytes/s
    pub eta: Option<i64>,              // whole seconds
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>,
    pub fragment_count: Option<u32>,
    pub phase: Option<Phase>,          // finer-grained than status, purely cosmetic
    // text
    pub msg: Option<Arc<str>>,
    pub error: Option<Arc<str>>,
    // outputs
    pub filename: Option<Arc<str>>,    // relative to its download root
    pub download_url: Option<Arc<str>>,// PUBLIC_HOST_URL/PUBLIC_HOST_AUDIO_URL + filename
    pub size: Option<u64>,
    pub files: Arc<[OutputFile]>,      // chapters + subtitles, unified
    // request echo (stable for the item's life)
    pub provider: &'static str,
    pub download_type: DownloadType,
    pub format: Arc<str>,
    pub quality: Arc<str>,
    pub codec: Codec,
    pub folder: Option<Arc<str>>,
    pub source: Source,
    // times, unix millis
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub attempt: u16,
}

#[derive(Serialize)]
pub struct OutputFile { pub kind: FileKind, pub filename: Arc<str>, pub size: Option<u64>,
                        pub download_url: Option<Arc<str>>, pub lang: Option<Arc<str>> }
#[derive(Serialize)] #[serde(rename_all="snake_case")]
pub enum FileKind { Primary, Chapter, Subtitle, Thumbnail, Nfo, InfoJson }

#[derive(Serialize)] #[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Api, V1, Telegram { chat_id: i64 }, Subscription { id: SubId }, Restart, Retry,
}
```

`GroupView` is the cheap whole-playlist progress record (BRIEF §5):

```rust
#[derive(Serialize)]
pub struct GroupView {
    pub id: GroupId, pub seq: Seq,
    pub kind: GroupKind,               // playlist | channel | season | batch
    pub title: Arc<str>, pub url: Arc<str>, pub provider: &'static str,
    pub total: u32,                    // declared child count
    pub resolved: u32,                 // children actually created
    pub counts: StatusCounts,          // [u32; 8], one per Status
    pub status: Status,                // derived roll-up
    pub percent: Option<f64>,          // byte-weighted when totals known, else count-weighted
    pub speed: Option<f64>,            // sum over running children
    pub eta: Option<i64>,              // conservative: bytes_remaining / speed_sum
    pub downloaded_bytes: u64,
    pub total_bytes_estimate: Option<u64>,
    pub children_inline: bool,         // false ⇒ fetch via GET groups/{id}/items
    pub created_at: i64, pub finished_at: Option<i64>,
}
```

`FieldMask` (bitflags 2) has one bit per wire field of `ItemView`/`GroupView`. 64 bits is enough
for both with room to spare.

### 3.4 Config

`Config::from_env() -> Result<Arc<Config>, ConfigError>` — a hand-written loader, deliberately
**not** `figment`/`config`, because BRIEF §15 demands byte-exact legacy names, defaults, `%%`
indirection and the legacy boolean token set. A `env_table!` macro generates the field, the
default, the parser and a `--print-config` dump so drift is visible in CI.

Rules ported verbatim from spec §1:

1. Read `os.environ.get(KEY, default)` — everything starts as a string.
2. `%%OTHER` ⇒ indirection to another key (topologically resolved; a cycle is a hard error).
3. `_BOOLEAN` keys accept exactly `true|false|True|False|on|off|1|0`; truthy = `{true,True,on,1}`;
   anything else ⇒ `ConfigError::BadBool` ⇒ `exit(1)` with the offending key and value.
4. `URL_PREFIX` gains a trailing `/` (default `""` ⇒ `"/"`).
5. `PUBLIC_HOST_URL` / `PUBLIC_HOST_AUDIO_URL` gain a trailing `/` **only if non-empty**.
6. `YTDL_OPTIONS_FILE` / `YTDL_OPTIONS_PRESETS_FILE` starting with `.` are canonicalised.
7. `YTDL_OPTIONS`, `YTDL_OPTIONS_PRESETS` parsed as JSON objects; failure ⇒ `exit(1)`.

New `AULOS_*` keys (all with defaults chosen so a stock legacy compose file behaves identically):

| Key | Default | Purpose | § |
|---|---|---|---|
| `AULOS_DB_PATH` | `<STATE_DIR>/aulos.db` | SQLite file | 8 |
| `AULOS_DB_FLUSH_MS` | `200` | batched-write flush window | 8.4 |
| `AULOS_WS_BATCH_MS` | `250` | delta tick | 6.1 |
| `AULOS_WS_URGENT_MS` | `25` | coalescing floor for prompt frames | 6.2 |
| `AULOS_WS_MAX_DELTAS_PER_FRAME` | `200` | frame split threshold | 6.4 |
| `AULOS_WS_REPLAY_FRAMES` | `512` | replay ring depth | 5.5 |
| `AULOS_WS_REPLAY_BYTES` | `4194304` | replay ring byte cap | 5.5 |
| `AULOS_WS_CLIENT_BUFFER` | `64` | per-client outbound queue depth | 5.7 |
| `AULOS_WS_MAX_CLIENTS` | `64` | hard cap on concurrent sockets | 5.7 |
| `AULOS_RESOLVE_CONCURRENCY` | `4` | resolver pool size | 12.3 |
| `AULOS_RESOLVE_TIMEOUT_S` | `120` | per-resolve wall clock | 12.3 |
| `AULOS_KILL_GRACE_MS` | `3000` | SIGTERM→SIGKILL grace | 10.4 |
| `AULOS_JOB_TIMEOUT_S` | `0` | 0 = off, hard per-job wall clock | 10.6 |
| `AULOS_STALL_TIMEOUT_S` | `%%TELEGRAM_STALL_TIMEOUT_SECONDS` | no-progress warning | 10.6 |
| `AULOS_STALL_ACTION` | `warn` | `warn` \| `cancel` | 10.6 |
| `AULOS_MEM_DONE_ITEMS` | `500` | in-memory completed window | 16 |
| `AULOS_SNAPSHOT_GROUP_INLINE` | `50` | inline children up to this group size | 12.5 |
| `AULOS_RESTART_POLICY` | `resume` | `resume` \| `pause` in-flight items on boot | 17.5 |
| `AULOS_PLUGINS_DIR` | `/config/plugins` | command plugins | 9.5 |
| `AULOS_POT_ENABLED` | `true` | supervise `bgutil-pot server` | 14.2 |
| `AULOS_POT_ARGS` | `server` | argv for the sidecar | 14.2 |
| `AULOS_POT_HEALTH_URL` | `http://127.0.0.1:4416/ping` | liveness probe | 14.2 |
| `AULOS_METRICS_ENABLED` | `false` | expose `<prefix>metrics` | 21.3 |
| `AULOS_JELLYFIN_DEBOUNCE_S` | `30` | one refresh per N s of completions | 13.3 |
| `AULOS_ALLOC` | `system` | `system` \| `mimalloc` (compile-time feature gate) | 21.4 |
| `AULOS_SUBSCRIPTION_CONCURRENCY` | `4` | parallel subscription checks | 13.2 |
| `AULOS_MAX_BATCH_URLS` | `500` | cap on `{"urls":[…]}` per request (413 above) | 7.2 |
| `AULOS_V1_ENABLED` | `true` | serve the v1 compatibility shim | 7.6 |
| `AULOS_DB_SYNCHRONOUS` | `NORMAL` | `NORMAL` \| `FULL` — SQLite durability | 8.4 |
| `AULOS_HOT_DIRTY_THRESHOLD` | `2048` | switch progress ingestion to the dirty-bit variant above this many running jobs | 4.4 |
| `AULOS_AUTO_RETRY` | `0` | auto-retries for classified-transient failures (0 = off) | 23.5 |

`JELLYFIN_LIBRARY_ID`, `JELLYFIN_METADATA_REFRESH_MODE`, `JELLYFIN_IMAGE_REFRESH_MODE` become
**real** (spec §1.2 notes they are inert today).

Hot state that config changes must swap atomically lives in `ArcSwap`:

```rust
pub struct Live {
    pub ytdl_options: ArcSwap<YtdlOptions>,   // YTDL_OPTIONS ∪ YTDL_OPTIONS_FILE ∪ runtime overrides
    pub presets:      ArcSwap<Presets>,
    pub custom_dirs:  ArcSwap<CustomDirs>,    // 5 s memo, refreshed off-thread
    pub ytdl_state:   ArcSwap<YtdlOptionsState>, // {ok, msg, mtime} for the `config` frame
}
```

---

## 4. In-memory state layout — the Hub

This is the core of the proposal. Everything the client sees is derived from one data structure
owned by one task.

### 4.1 Why single-owner, not `DashMap` or `RwLock<HashMap>`

The workload is not "many independent readers and writers of unrelated keys". It is
**read-modify-write with change tracking, plus a consistent whole-collection snapshot**:

* Every mutation must OR a bit into that item's `FieldMask` *and* register the item in a
  process-wide dirty set. With `DashMap` that dirty set is a second contended structure, and the
  mask update and the dirty insert are not atomic together.
* `seq` must be assigned in a total order that matches the order mutations were applied,
  otherwise `?since=` replay is wrong. A shared map gives no such order without an extra lock.
* The snapshot handed to a connecting client must be **consistent** (no item at `finished` while
  the group aggregate still says `downloading`). Iterating `DashMap`'s shards gives a torn read.
* `RwLock<HashMap>` serialises writers anyway, and every writer would hold the lock across
  `Arc<ItemView>` construction.

Measured budget: the Hub applies a mutation in ~120 ns (`IndexMap` lookup + a few stores + a
mask OR). Even a pathological 50 000 mutations/s is 0.6 % of one core. The mutation rate this
design actually produces is **`dirty_items × tick_rate`** for progress (≈ 3 × 4 = 12/s) plus a
handful of structural events. Single-owner is not a bottleneck by three orders of magnitude, and
it buys correctness for free.

`DashMap` *is* used, but only where the access pattern genuinely is disjoint-key and off the
state path: `CancelRegistry` (`DashMap<ItemId, CancelHandle>`, written by the scheduler, read by
the API handler that wants to cancel) and `chat_state` in the Telegram bot.

### 4.2 The Hub task

```rust
// crates/aulos-queue/src/hub/mod.rs
pub struct Hub {
    // ---- authoritative state, exclusively owned, never behind a lock ----
    items:  IndexMap<ItemId, ItemState>,      // insertion order == seq order
    groups: IndexMap<GroupId, GroupState>,
    done:   VecDeque<ItemId>,                 // FIFO window, cap AULOS_MEM_DONE_ITEMS
    done_total: u64,                          // count in SQLite, incl. evicted

    // ---- change tracking ----
    running: SmallVec<[ItemId; 32]>,          // items whose HotCell must be sampled
    dirty_items:  IndexSet<ItemId>,
    dirty_groups: IndexSet<GroupId>,
    pending_added:     Vec<ItemId>,
    pending_completed: Vec<ItemId>,
    pending_removed:   Vec<(RecordId, RemoveReason)>,
    urgent: bool,                             // a status change is pending ⇒ flush early

    // ---- publication ----
    seq: Arc<dyn SeqAllocator>,
    published: Arc<ArcSwap<Published>>,       // lock-free reads for REST + WS connect
    ring: ReplayRing,
    bus: broadcast::Sender<Arc<WireFrame>>,
    scratch: Vec<u8>,                         // reused serialization buffer

    // ---- collaborators ----
    inbox: mpsc::Receiver<Mutation>,          // capacity 4096
    store: StoreHandle,
    hooks: HookHandle,
    notifier: Arc<dyn Notifier>,
    sched: SchedulerHandle,

    tick: Interval,                           // AULOS_WS_BATCH_MS, MissedTickBehavior::Delay
    urgent_at: Option<Instant>,
}

impl Hub {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                // 1. drain structural mutations in bulk
                n = self.inbox.recv_many(&mut self.batch, 256) => {
                    if n == 0 { break }                   // all senders dropped ⇒ shutdown
                    for m in self.batch.drain(..) { self.apply(m); }
                }
                // 2. urgent flush (status change, added, completed, removed)
                _ = sleep_until_opt(self.urgent_at), if self.urgent_at.is_some() => {
                    self.flush(FlushKind::Urgent);
                }
                // 3. periodic progress tick
                _ = self.tick.tick() => {
                    self.sample_hot();                    // pull-based, O(running)
                    self.flush(FlushKind::Periodic);
                }
            }
        }
    }
}
```

**I7** (the Hub never blocks) is enforced structurally: `apply` and `flush` are `fn`, not
`async fn`. Everything the Hub needs to hand off (store writes, hook dispatch, notifier calls,
scheduler pokes) goes out via `try_send` on a bounded channel with a documented overflow policy
(§15). `recv_many` amortises the select overhead over up to 256 mutations, which matters on the
500-item playlist path.

### 4.3 `ItemState`

```rust
pub struct ItemState {
    pub id: ItemId,
    pub seq: Seq,                        // creation order, immutable — I3
    pub group: Option<(GroupId, u32)>,
    pub ident: Arc<ItemIdentity>,        // url, provider, request, source, created_at — immutable
    pub title: Arc<str>,
    pub status: Status,
    pub auto_start: bool,
    pub prog: Progress,                  // last *sampled* values (plain, not atomic)
    pub prog_src: Option<ProgressSourceKey>, // resets the monotonic clamp on stream switch
    pub msg: Option<Arc<str>>,
    pub error: Option<Arc<str>>,
    pub filename: Option<Arc<str>>,
    pub size: Option<u64>,
    pub files: Arc<[OutputFile]>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub attempt: u16,
    pub hot: Option<Arc<HotCell>>,       // Some ⟺ status.is_running()
    pub view: Arc<ItemView>,             // last published; reused verbatim when clean
    pub mask: FieldMask,                 // changed since the last frame that carried this item
}
```

Two properties worth calling out:

* **`ident` is one `Arc` shared with the `ItemView`.** The immutable half of an item (url,
  request options, source, timestamps) is ~200 bytes of strings; rebuilding the view clones one
  pointer, not the strings.
* **`view` is memoised.** `flush` only calls `rebuild_view(&ItemState)` for items in
  `dirty_items`. A snapshot of 500 items where 3 are moving allocates 3 `Arc<ItemView>` per
  tick, not 500.

### 4.4 `HotCell` — the lock-free-ish progress path

The single hottest path in the system: a yt-dlp progress hook line, or an N_m3u8DL-RE repaint
frame, arriving 20–60 times a second per job.

```rust
/// One per running job. Exactly one writer (the job task), one reader (the Hub).
/// 64-byte aligned so two concurrent jobs never share a cache line (false-sharing kills
/// this pattern otherwise).
#[repr(align(64))]
pub struct HotCell {
    /// Seqlock counter: odd ⇒ a write is in progress.
    gen: AtomicU64,
    percent_centi:    AtomicU32,   // 0..=10_000; u32::MAX == None
    speed_milli:      AtomicU64,   // bytes/s × 1000; u64::MAX == None
    eta_secs:         AtomicI64,   // -1 == None
    downloaded_bytes: AtomicU64,   // u64::MAX == None
    total_bytes:      AtomicU64,
    total_estimate:   AtomicU64,
    fragment_index:   AtomicU32,
    fragment_count:   AtomicU32,
    phase:            AtomicU8,    // Phase discriminant
    /// Bumped by the writer when a *structural* field (filename, chapter file, msg) changed and
    /// a Tier-B message was sent; lets the Hub cheaply assert ordering in debug builds.
    epoch: AtomicU32,
}

impl HotCell {
    #[inline]
    pub fn write(&self, p: &HotProgress) {
        self.gen.fetch_add(1, Ordering::Release);              // → odd
        self.percent_centi.store(enc_pct(p.percent), Relaxed);
        self.speed_milli.store(enc_speed(p.speed), Relaxed);
        self.eta_secs.store(p.eta.unwrap_or(-1), Relaxed);
        self.downloaded_bytes.store(opt(p.downloaded), Relaxed);
        self.total_bytes.store(opt(p.total), Relaxed);
        self.total_estimate.store(opt(p.estimate), Relaxed);
        self.fragment_index.store(opt32(p.frag_index), Relaxed);
        self.fragment_count.store(opt32(p.frag_count), Relaxed);
        self.phase.store(p.phase as u8, Relaxed);
        self.gen.fetch_add(1, Ordering::Release);              // → even
    }

    /// Bounded-retry seqlock read. Progress is advisory, so after 3 failed attempts we accept a
    /// possibly-torn read: the worst case is one tick showing a percent from t and a speed from
    /// t+8 ms, which is invisible.
    #[inline]
    pub fn read(&self) -> (u64, HotProgress) { /* … */ }
}
```

**Why pull, not push.** The obvious design is a channel or a `watch` per item. Both are worse:

| Option | Cost per progress datum | Why rejected |
|---|---|---|
| `mpsc::Sender<Mutation>` per datum | 1 alloc (`Arc<str>`/enum), 1 atomic push, wakes the Hub | 60 msg/s × N jobs of Hub wakeups purely to overwrite a value that will be coalesced anyway; also needs an overflow policy for a value we don't care about losing |
| `tokio::sync::watch` per item | 1 lock + 1 notify | Hub must `select!`/`JoinSet` over N receivers; 500 channels for a 500-item playlist; teardown churn |
| `DashMap<ItemId, Progress>` | 1 shard lock | contention between jobs; Hub still needs a dirty set |
| **`HotCell` + tick sampling** | **8 relaxed stores + 2 release fetch_adds, 0 allocations, 0 wakeups** | chosen |

The Hub samples once per tick:

```rust
fn sample_hot(&mut self) {
    for i in 0..self.running.len() {
        let id = self.running[i];
        let Some(st) = self.items.get_mut(&id) else { continue };
        let Some(hot) = st.hot.as_ref() else { continue };
        let (gen, raw) = hot.read();
        if gen == st.prog.last_gen { continue }              // nothing new since last tick
        st.prog.last_gen = gen;
        let mut mask = FieldMask::empty();
        // monotonic clamp + fragment bounding, ported from _calculate_progress_percent
        let pct = calc_percent(&raw, st.prog.percent, st.prog_src.as_ref());
        if neq_f(pct, st.prog.percent)          { st.prog.percent = pct;  mask |= F::PERCENT }
        if neq_f(raw.speed, st.prog.speed)      { st.prog.speed = raw.speed; mask |= F::SPEED }
        if raw.eta != st.prog.eta               { st.prog.eta = raw.eta;  mask |= F::ETA }
        // … bytes, fragments, phase …
        if !mask.is_empty() { st.mask |= mask; self.dirty_items.insert(id); }
    }
}
```

`self.running` is a `SmallVec<[ItemId; 32]>` maintained by status transitions. Its length is
bounded by `MAX_CONCURRENT_DOWNLOADS + SC_MAX_CONCURRENT_DOWNLOADS` (default 3 + 1 = 4). So the
whole progress subsystem is **O(4) work, 4 times a second**, regardless of a 500-item queue.
That is invariant **I1**.

> **Scaling note.** If someone sets `MAX_CONCURRENT_DOWNLOADS=256`, per-tick sampling becomes
> 256 seqlock reads (~5 µs) — still fine. Above ~2000 the pull loop should switch to a
> dirty-bit variant: `HotCell.dirty: AtomicBool`, and the writer does
> `if !dirty.swap(true, AcqRel) { let _ = tx.try_send(Mutation::HotDirty(id)); }`, with the Hub
> clearing the flag at sample time and a full sweep every 2 s to cover `try_send` failures.
> Implement the simple version; gate the variant behind `AULOS_HOT_DIRTY_THRESHOLD` and a bench.

### 4.5 The published snapshot — lock-free reads

REST handlers and newly-connecting WS sessions must read the whole state without touching the
Hub (otherwise a burst of connects would queue behind progress work).

```rust
pub struct Published {
    pub seq: Seq,                              // frame seq this view corresponds to
    pub boot_id: BootId,
    pub items:  Vec<Arc<ItemView>>,            // in seq order — I3
    pub groups: Vec<Arc<GroupView>>,
    pub by_id:  Arc<HashMap<RecordId, u32>>,   // shared across ticks unless membership changed
    pub done_total: u64,
    pub counts: StatusCounts,
}

// crates/aulos-queue/src/hub/publish.rs
pub struct StateView(Arc<ArcSwap<Published>>);
impl StateView {
    pub fn load(&self) -> arc_swap::Guard<Arc<Published>> { self.0.load() }   // ~2 ns, no lock
}
```

Publication cost per tick:

| Work | Cost with 500 items |
|---|---|
| clone `Vec<Arc<ItemView>>` | one 4 KB memcpy of pointers, ~150 ns |
| clone `Arc<HashMap>` | pointer bump (rebuilt **only** when membership changes) |
| rebuild dirty `Arc<ItemView>`s | 3 allocations |
| `ArcSwap::store` | one RCU-style swap, readers never block |

`ArcSwap` rather than `RwLock<Arc<_>>`: readers are wait-free and never contend with the writer,
and a reader holding a `Guard` across an `await` (which a streaming REST handler may do) cannot
stall the Hub.

### 4.6 Group aggregates in O(1)

Whole-playlist progress is maintained incrementally, never recomputed:

```rust
pub struct GroupState {
    pub id: GroupId, pub seq: Seq, pub kind: GroupKind,
    pub title: Arc<str>, pub url: Arc<str>, pub provider: &'static str,
    pub total: u32, pub resolved: u32,
    pub counts: StatusCounts,               // [u32; 8]
    pub acc: GroupAcc,                      // running sums
    pub view: Arc<GroupView>, pub mask: FieldMask,
}
pub struct GroupAcc {
    downloaded: u64,          // Σ child downloaded_bytes
    total_known: u64,         // Σ child total_bytes where known
    total_est: u64,           // Σ child best-effort total (bytes or estimate)
    n_with_total: u32,
    speed_milli: u64,         // Σ running child speed
    finished_bytes: u64,      // Σ size of finished children (so completed work is never lost)
}
```

Every child mutation that touches a summed field applies the delta to its parent's `acc` and
marks the parent dirty:

```rust
fn bump_group(&mut self, gid: GroupId, d: AccDelta) {
    if let Some(g) = self.groups.get_mut(&gid) {
        g.acc.apply(d);
        g.mask |= d.mask();
        self.dirty_groups.insert(gid);
    }
}
```

`percent` roll-up rule (documented so the client never has to compute it):

```
if n_with_total == resolved and total_est > 0:
    percent = 100 * (finished_bytes + downloaded) / total_est          # byte-weighted
else:
    percent = 100 * (counts.finished + Σ_active(child.percent/100)) / max(total,1)   # count-weighted
```

`status` roll-up: `Downloading` if any child is running; else `Queued` if any child is
`Queued|Resolving`; else `Error` if any child errored; else `Canceled` if all canceled; else
`Finished`.

Consequence: a client rendering a collapsed 500-item playlist row receives **one `GroupDelta` of
~120 bytes per tick** and nothing else. That is the answer to "the client must be able to show
progress for the whole group cheaply".

### 4.7 The `Mutation` enum (Tier B)

```rust
pub enum Mutation {
    // creation
    AddItems { items: Box<[NewItem]>, group: Option<Box<NewGroup>>, replace: Option<RecordId> },
    // lifecycle
    Status  { id: ItemId, status: Status, at: i64 },
    Phase   { id: ItemId, phase: Phase },
    Message { id: ItemId, msg: Option<Arc<str>> },
    Failed  { id: ItemId, error: Arc<str>, retryable: bool },
    Output  { id: ItemId, file: OutputFile, primary: bool },
    Title   { id: ItemId, title: Arc<str> },
    Attach  { id: ItemId, hot: Arc<HotCell> },      // job spawned
    Detach  { id: ItemId },                          // job ended, stop sampling
    // removal
    Remove  { ids: Box<[RecordId]>, reason: RemoveReason, delete_files: bool },
    // side channels that still need a seq'd frame
    Subs    { upsert: Box<[SubView]>, removed: Box<[SubId]> },
    ConfigChanged { ytdl: Arc<YtdlOptionsState> },
    Notice  { level: Level, code: &'static str, id: Option<RecordId>, message: Arc<str> },
    // control
    Ack { seq: Seq },                                // from a WS session, trims the ring
}
```

`Box<[T]>` rather than `Vec<T>` in the variants keeps `Mutation` at 40 bytes so the 4096-slot
mpsc ring is 160 KB, not megabytes.

`AddItems { replace: Some(id) }` is the atomic playlist-promotion primitive (§12.5): it removes
`id` and inserts the group + children **in the same `apply` call**, so they land in the same
frame and the client can render a morph instead of a delete-then-insert.

---

## 5. Realtime protocol v2 (WebSocket)

Endpoint: `GET <URL_PREFIX>ws` (axum `WebSocketUpgrade`). Text frames, one JSON object per frame,
envelope `{"t": "<type>", "seq": <u64>, …}` per BRIEF §3.

Query parameters:

| Param | Type | Meaning |
|---|---|---|
| `since` | u64 | resume from this seq; omit for a fresh snapshot |
| `boot` | ULID | the `boot_id` the client's `since` came from; a mismatch forces a snapshot |
| `done` | bool, default `true` | include the completed window in the snapshot and its deltas |
| `groups` | csv of GroupId | pre-subscribe to these groups' children |

### 5.1 Server→client frame catalogue

| `t` | Cadence | Payload |
|---|---|---|
| `snapshot` | once, on connect (or after an unresumable gap) | full state, same item shape as REST |
| `resume` | once, instead of `snapshot`, when `since` is in the ring | `{from, to}` then the merged delta |
| `delta` | every `AULOS_WS_BATCH_MS`, or `AULOS_WS_URGENT_MS` after a status change | changed fields only |
| `added` | prompt | full `ItemView`s / `GroupView`s |
| `completed` | prompt | full `ItemView`s, terminal |
| `removed` | prompt | ids + reason |
| `subs` | prompt | subscription upserts/removals |
| `config` | prompt | `YTDL_OPTIONS_FILE` reload result, capability changes |
| `notice` | prompt | stall/timeout/POT warnings, human-readable |
| `pong` | on demand | RTT probe echo |
| `error` | on demand | protocol/auth error, then close |

`seq` is strictly increasing across **all** frame types from one server boot. A client that sees
`seq` jump by more than 1 has lost nothing — `seq` counts frames, and every frame is delivered
in order on a single socket. `seq` gaps only appear across a reconnect.

### 5.2 `snapshot`

```json
{
  "t": "snapshot",
  "seq": 90210,
  "boot_id": "01JBQ8YQ2E0000000000000000",
  "server": { "version": "2026.09.04", "yt_dlp": "2026.8.30.232658.dev0",
              "url_prefix": "/", "started_at": 1767225000000 },
  "protocol": { "batch_ms": 250, "urgent_ms": 25, "replay_frames": 512,
                "delta_semantics": "absent-key-means-unchanged" },
  "config": { "custom_dirs": true, "create_custom_dirs": true,
              "output_template_chapter": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
              "public_host_url": "download/", "public_host_audio_url": "audio_download/",
              "default_option_playlist_item_limit": 0,
              "subscription_default_check_interval": 60,
              "allow_ytdl_options_overrides": false,
              "delete_file_on_trashcan": false,
              "clear_completed_after": 0 },
  "capabilities": {
    "formats": [ { "id": "mp4", "text": "MP4",
                   "qualities": [{"id":"best","text":"Best"},{"id":"best_remux","text":"Best (remux)"},
                                 {"id":"1080","text":"1080p"}] } ],
    "download_types": ["video","audio","captions","thumbnail"],
    "codecs": ["auto","h264","h265","av1","vp9"],
    "subtitle_modes": ["auto_only","manual_only","prefer_manual","prefer_auto"],
    "presets": ["archive","music"],
    "features": ["retry","cancel","groups","subscriptions","since_resume","file_serving",
                 "batch_add","postprocessing_status"]
  },
  "counts": { "queued": 486, "resolving": 0, "preparing": 0, "downloading": 3,
              "postprocessing": 0, "finished": 10, "error": 1, "canceled": 0 },
  "done_total": 4211,
  "groups": [ { "id": "01JBQ8Z1", "seq": 90180, "kind": "playlist",
                "title": "Some Playlist", "url": "https://…/playlist?list=PL…",
                "provider": "ytdlp", "total": 500, "resolved": 500,
                "counts": {"queued":486,"downloading":3,"finished":10,"error":1,
                           "resolving":0,"preparing":0,"postprocessing":0,"canceled":0},
                "status": "downloading", "percent": 2.4, "speed": 3145728.0, "eta": 4210,
                "downloaded_bytes": 123456789, "total_bytes_estimate": 5100000000,
                "children_inline": false, "created_at": 1767225501000, "finished_at": null } ],
  "items": [
    { "id": "01JBQ8Z2AAAA0000000000000A", "seq": 90183, "group_id": "01JBQ8Z1", "group_index": 11,
      "url": "https://www.youtube.com/watch?v=abc", "title": "Episode 11",
      "status": "downloading", "auto_start": true,
      "percent": 42.5, "speed": 1048576.0, "eta": 37,
      "downloaded_bytes": 12345678, "total_bytes": 29000000,
      "total_bytes_estimate": null, "fragment_index": null, "fragment_count": null,
      "phase": "video", "msg": null, "error": null,
      "filename": null, "download_url": null, "size": null, "files": [],
      "provider": "ytdlp", "download_type": "video", "format": "mp4", "quality": "1080",
      "codec": "auto", "folder": null, "source": {"kind":"api"},
      "created_at": 1767225501200, "started_at": 1767225503900, "finished_at": null,
      "attempt": 0 }
  ],
  "done": [ /* most recent AULOS_MEM_DONE_ITEMS finished/error items, same shape */ ],
  "truncated": { "done": true, "groups": ["01JBQ8Z1"] }
}
```

Notes that matter to the client (and delete code in it):

* `items` and `done` use **the identical object shape as `GET api/v2/items`** — one decoder, no
  `[key, info]` pairs, no `AnyCodable`, no four fallback parsers (client ref §6.1, §7.4).
* every numeric field is a number or `null`; `eta` is always integer seconds (client ref §7.7).
* `truncated.groups` lists groups whose children were not inlined; `truncated.done` says the
  completed window is a window.
* `capabilities.formats` is the catalogue the iOS app already tries to consume via a `formats`
  event that **the legacy server never emits** (spec §3.1 has no such event, so the app always
  falls back to its hard-coded list). Shipping it in the snapshot makes that code path live.

### 5.3 `delta`

```json
{ "t": "delta", "seq": 90211, "ts": 1767225601123,
  "items": [
    { "id": "01JBQ8Z2AAAA0000000000000A", "percent": 43.9, "speed": 1002401.0, "eta": 35,
      "downloaded_bytes": 12740000 },
    { "id": "01JBQ8Z2AAAA0000000000000B", "status": "postprocessing", "percent": 100.0,
      "speed": null, "eta": null, "phase": "remux" }
  ],
  "groups": [
    { "id": "01JBQ8Z1", "percent": 2.6, "speed": 2098000.0, "eta": 4100,
      "downloaded_bytes": 132000000 }
  ] }
```

**Delta semantics (contractual):**

| Wire | Meaning |
|---|---|
| key absent | field unchanged since the last frame that mentioned this id |
| key present, non-null | new value |
| key present, `null` | field changed **to** null (e.g. speed cleared when a job leaves `downloading`) |

This is why `absent ≠ null` is stated in `protocol.delta_semantics` — it is the one place a JSON
protocol can bite a client that uses `Optional<Optional<T>>`-free decoding. The iOS client's
`QueueItem` merge becomes: for each key present in the delta, overwrite; leave the rest.

### 5.4 `added` / `completed` / `removed`

```json
{ "t": "added", "seq": 90184,
  "groups": [ { "id": "01JBQ8Z1", "…": "full GroupView" } ],
  "items":  [ { "id": "01JBQ8Z2…A", "…": "full ItemView" } ] }
```

```json
{ "t": "completed", "seq": 90260,
  "items": [ { "id": "01JBQ8Z2…B", "status": "finished", "percent": 100.0,
               "filename": "Some Playlist/Episode 12.mp4",
               "download_url": "download/Some%20Playlist/Episode%2012.mp4",
               "size": 288314112, "finished_at": 1767225998000,
               "files": [ { "kind": "primary", "filename": "Some Playlist/Episode 12.mp4",
                            "size": 288314112,
                            "download_url": "download/Some%20Playlist/Episode%2012.mp4",
                            "lang": null } ],
               "…": "the rest of the full ItemView" } ] }
```

```json
{ "t": "removed", "seq": 90261, "ids": ["01JBQ8Z2…C"], "reason": "canceled" }
```

`reason ∈ {deleted, cleared, canceled, expanded, expired, auto_cleared}`.

**The `expanded` promotion rule.** When a provisional `resolving` item turns out to be a
playlist, one frame carries both the removal and the group with **the same id**:

```json
{ "t": "added", "seq": 90180,
  "removed": { "ids": ["01JBQ8Z1"], "reason": "expanded" },
  "groups":  [ { "id": "01JBQ8Z1", "seq": 90180, "kind": "playlist", "…": "…" } ],
  "items":   [ /* first AULOS_SNAPSHOT_GROUP_INLINE children */ ] }
```

Because it is one frame, and because the group inherits both the `id` **and** the `seq` of the
provisional item, the row neither moves nor blinks: the client replaces an item row with a group
row in place. `reason: "expanded"` tells it to animate a morph.

### 5.5 `seq`, the replay ring, and resume

```rust
pub struct ReplayRing {
    frames: VecDeque<RingEntry>,          // cap AULOS_WS_REPLAY_FRAMES
    bytes: usize,                          // cap AULOS_WS_REPLAY_BYTES
    floor: Seq,                            // lowest seq still replayable
}
pub struct RingEntry {
    seq: Seq,
    wire: Arc<WireFrame>,                  // pre-serialized, shared with live subscribers
    batch: Option<Arc<DeltaBatch>>,        // structured form, only for `delta` frames
}
```

Two representations because they serve two jobs:

* `wire` — what live subscribers get: `Arc<WireFrame { seq, kind, text: Utf8Bytes }>`. Serialized
  **once**, cloned by pointer to every socket. With 5 clients that is 1 serde pass, not 5.
* `batch` — the structured `DeltaBatch` so a resume can **merge** N frames into one. Replaying
  180 raw delta frames to a client that was away for 45 s would send 180 frames of mostly-stale
  numbers; merging them yields one frame with the latest value per (id, field).

```rust
pub fn resume(&self, since: Seq, boot: Option<BootId>) -> Resume {
    if boot.is_some_and(|b| b != self.boot_id) { return Resume::Snapshot }
    if since >= self.ring.head()             { return Resume::UpToDate }
    if since <  self.ring.floor              { return Resume::Snapshot }   // gap too old
    Resume::Merged { from: since, to: self.ring.head(), frame: self.ring.merge_after(since) }
}
```

`merge_after` folds the tail of the ring:

| Frame kind in the window | Merge rule |
|---|---|
| `delta` | per (id, field), last value wins |
| `added` | accumulate; if the id is later `removed`, drop both |
| `completed` | accumulate as a full object; supersedes any earlier delta for that id |
| `removed` | accumulate; drops any earlier `added`/`delta` for that id |
| `subs`, `config`, `notice` | accumulate in order (rare, small) |

The merged result is emitted as a `resume` frame followed by one `added`, one `completed`, one
`removed` and one `delta` frame (in that order — additions before updates before removals is the
only order that never references an unknown id):

```json
{ "t": "resume", "seq": 90261, "from": 90200, "to": 90261,
  "merged": { "added": 2, "completed": 1, "removed": 1, "delta_items": 3, "delta_groups": 1 } }
```

Ring eviction advances `floor` and is bounded by frames **and** bytes, so a burst of large
`added` frames (a 500-item playlist) cannot blow memory (**I9**).

`GET <prefix>api/v2/state?since=<seq>` is the same logic over HTTP (BRIEF §3), for clients that
prefer a plain fetch on foreground:

```json
{ "seq": 90261, "boot_id": "01JBQ8YQ…", "from": 90200, "full": false,
  "added": [...], "completed": [...], "removed": {"ids": [...], "reason": "deleted"},
  "delta": { "items": [...], "groups": [...] } }
```
…or, when unresumable, exactly the `snapshot` body with `"full": true`. Response carries
`ETag: W/"90261"` and honours `If-None-Match` with `304` (client ref §7.9), so a foreground poll
that finds nothing changed costs ~200 bytes.

### 5.6 Client→server frames

Minimal, and all of them optional — a client that only reads is fully functional.

| Frame | Purpose |
|---|---|
| `{"t":"ping","c":1767225601000}` | RTT probe → `{"t":"pong","seq":N,"c":1767225601000}` |
| `{"t":"ack","seq":90261}` | lets the server trim the ring and export a per-client lag metric |
| `{"t":"watch","groups":["01JBQ8Z1"],"done":true}` | subscribe to a collapsed group's children |
| `{"t":"unwatch","groups":["01JBQ8Z1"]}` | stop receiving them |

`watch` is what keeps the snapshot small without hurting UX: expanding a 500-item playlist in
the UI sends one `watch`, and the server replies with an `added` frame containing that group's
children (paged at 250 per frame) and starts including them in subsequent deltas. Mutations are
**never** sent over the socket — they stay on REST (BRIEF §7), which keeps auth, idempotency and
error envelopes in one place.

### 5.7 Per-client fan-out and backpressure

```
Hub ──broadcast::Sender<Arc<WireFrame>> (cap 256)──┬──► Session A: rx.recv() → ws_tx.send()
                                                   ├──► Session B
                                                   └──► Session C
```

Each session is **two tasks**: a reader (client frames, ping/pong, close) and a writer
(`broadcast::Receiver` → socket). They share a `CancellationToken`; either ending kills both.

```rust
async fn writer(mut rx: broadcast::Receiver<Arc<WireFrame>>, mut ws: SplitSink<WebSocket, Message>,
                state: StateView, cancel: CancellationToken) {
    loop {
        match rx.recv().await {
            Ok(f)  => { if ws.send(Message::Text(f.text.clone())).await.is_err() { break } }
            Err(RecvError::Lagged(n)) => {
                metrics::counter!("aulos_ws_lagged_frames").increment(n);
                // Recover by re-snapshotting from the lock-free published view.
                let snap = build_snapshot(&state.load(), &opts);
                if ws.send(Message::Text(snap)).await.is_err() { break }
                if lag_budget.record(n).exceeded() {
                    let _ = ws.send(Message::Close(Some(CloseFrame {
                        code: 1013, reason: "client too slow".into() }))).await;
                    break
                }
            }
            Err(RecvError::Closed) => break,
        }
    }
    cancel.cancel();
}
```

Properties:

| Concern | Handling |
|---|---|
| Slow client | its `broadcast::Receiver` lags; `Lagged(n)` → one snapshot resync. It degrades to coarser updates and **never** slows the Hub or other clients (**I6**) |
| Pathologically slow client | `lag_budget` = >8 lags in 60 s ⇒ close 1013. Metric `aulos_ws_slow_disconnects` |
| Dead TCP (no FIN) | server-side ping every 20 s, close if no pong/data within 60 s |
| Too many clients | `AULOS_WS_MAX_CLIENTS` (64); the 65th gets `{"t":"error","error":{"code":"too_many_clients"}}` and close 1013 |
| Frame >1 MiB from client | reject and close 1009 |
| Head-of-line blocking on `broadcast` | none: `broadcast` never blocks the sender; the sender overwrites the oldest slot and readers observe `Lagged` |
| Compression | `permessage-deflate` **off**. Frames are ~200–900 B; deflate would add per-frame CPU and, worse, per-connection dictionary memory (~300 KB each). Payloads are already field-masked, which is a better compressor. |

`broadcast` capacity 256 = ~64 s of buffer at the default tick, so a client that goes to sleep
for a minute still resumes without a snapshot.

### 5.8 Why native WebSocket and not Socket.IO

Beyond BRIEF §3 mandating it: Socket.IO's payload here is a **JSON string inside a JSON frame**
(spec §3, "double encoding"). Removing that is two fewer serde passes per frame on the server and
two fewer `JSON.parse`es on the client, plus it deletes `SocketAllResponse`, `AnyCodable`,
`handleAllEventFromArray` and `parseAllEventManually` (~180 lines, client ref §6.1). The
handshake also drops from HTTP-poll-then-upgrade to a single upgrade (the iOS client already
forces `forceWebsockets(true)`), and reconnect no longer needs `/history` on top of the socket
handshake.

---

## 6. Delta batching engine

### 6.1 Flush

```rust
fn flush(&mut self, kind: FlushKind) {
    self.emit_removed();        // order: removed → added → completed → delta
    self.emit_added();          // (an `added` may carry an inline `removed` for `expanded`)
    self.emit_completed();
    self.emit_delta(kind);
    self.republish();           // ArcSwap::store of the new Published
    self.urgent = false;
    self.urgent_at = None;
}

fn emit_delta(&mut self, _kind: FlushKind) {
    if self.dirty_items.is_empty() && self.dirty_groups.is_empty() { return }
    self.scratch.clear();
    let mut w = DeltaWriter::new(&mut self.scratch, self.seq.next());
    let mut n = 0;
    while let Some(id) = self.dirty_items.pop_front_cursor(&mut self.cursor) {
        let st = &mut self.items[&id];
        w.item(st.id, &st.view_after_rebuild(), st.mask);   // mask-driven field emission
        st.mask = FieldMask::empty();
        n += 1;
        if n == self.max_deltas_per_frame { break }         // fairness: cursor persists
    }
    /* groups likewise */
    let frame = w.finish();                                  // Arc<WireFrame> + Arc<DeltaBatch>
    self.ring.push(frame.clone());
    let _ = self.bus.send(frame);                            // never blocks
}
```

Serialization writes directly into `self.scratch` (a `Vec<u8>` that reaches steady-state
capacity after a few ticks) and then `Utf8Bytes::from(String::from_utf8_unchecked(take))` — the
buffer is validated by construction because every writer is `serde_json`. **Zero allocations per
tick beyond the one `Arc<WireFrame>` and the dirty items' views.**

`DeltaWriter::item` is hand-written rather than `#[derive(Serialize)]` + `skip_serializing_if`,
because the skip predicate is a *runtime* mask, not a per-field `Option`:

```rust
impl<'a> DeltaWriter<'a> {
    fn item(&mut self, id: ItemId, v: &ItemView, m: FieldMask) {
        self.obj_start(); self.key_str("id", id);
        if m.contains(F::STATUS)  { self.key("status", &v.status) }
        if m.contains(F::PERCENT) { self.key_f64_opt("percent", v.percent) }
        if m.contains(F::SPEED)   { self.key_f64_opt("speed", v.speed) }
        /* … one line per wire field … */
        self.obj_end();
    }
}
```

### 6.2 Promptness vs coalescing

| Event class | Latency target | Mechanism |
|---|---|---|
| numeric progress | ≤ `batch_ms` (250 ms) | periodic tick |
| status transition (`preparing`, `downloading`, `postprocessing`) | ≤ `urgent_ms` (25 ms) | `urgent_at = now + 25 ms` on any `Mutation::Status`; the tick is not reset |
| `added` / `completed` / `removed` | ≤ 25 ms | same urgent deadline (BRIEF §3: "delivered promptly, not batched with progress") |
| `config`, `notice`, `subs` | ≤ 25 ms | same |

The 25 ms floor is what keeps a 500-item playlist from producing 500 frames: the resolver emits
children as it materialises them, and the Hub coalesces everything arriving within one 25 ms
window into one `added` frame. Measured shape on the 500-item path: ~12–20 frames of 25–60
children each (§17.2).

Why not 0 ms? Because at 0 ms the frame count is bounded only by the producer's message rate, and
`broadcast` capacity 256 would then be ~2 s of buffer instead of 64 s. 25 ms is below the
human flicker threshold and below one iOS display frame at 30 fps.

### 6.3 What the client can now delete

Server-side batching lets the iOS client drop its 250 ms throttle, its `pendingUpdates` map, and
`applyPendingUpdates`' index-0 insertion — which is the documented cause of nondeterministic row
reordering (client ref §6.9). Deltas arrive pre-coalesced, in `seq` order, and every item carries
its stable `seq` sort key, so the client's apply step is a dictionary merge with no sorting and
no insertion-position decision.

### 6.4 Overload behaviour

| Condition | Response |
|---|---|
| dirty items > `AULOS_WS_MAX_DELTAS_PER_FRAME` | split across consecutive frames with a persistent round-robin cursor (no item can be starved) |
| splitting persists for > 4 ticks | effective tick backs off to `batch_ms × ceil(dirty / max_per_frame)`, capped at 2 s; logged once at WARN, exported as `aulos_ws_tick_backoff_ms` |
| Hub inbox (4096) full | producers `try_send` → on failure, structural mutations fall back to `send().await` (backpressure on the *producer*, which is correct: a job task waiting 1 ms to report a status change is free); `Mutation::Notice` is dropped with a counter |
| ring byte cap hit | oldest frames evicted, `floor` advances; affected resumers get a snapshot |

---

## 7. REST surface

Base = `URL_PREFIX` (default `/`). `Content-Type: application/json` everywhere. Error envelope
(BRIEF §7):

```json
{ "error": { "code": "invalid_request", "message": "quality must be one of [best, worst, 2160, …]",
             "field": "quality", "request_id": "01JBQ8ZA…" } }
```

`ErrorCode` is a closed enum so clients can branch: `invalid_request`, `unauthorized`,
`not_found`, `conflict`, `unsupported_url`, `provider_error`, `resolve_failed`, `disabled`,
`too_large`, `rate_limited`, `internal`.

Status codes: `200` reads, `202` accepted async work, `204` no-content mutations, `400`
validation, `401` auth (never a redirect — client ref §7.2), `404`, `409` conflict (duplicate
active URL when `?dedupe=strict`), `413` too large, `429`, `503` not-ready.

### 7.1 Route table (v2)

| Method | Path | Body / query | Success | Notes |
|---|---|---|---|---|
| POST | `api/v2/downloads` | §7.2 | `202 {"id"}` or `202 {"ids":[…]}` | never blocks on extraction |
| GET | `api/v2/state` | `?since=&boot=` | `200` delta-or-snapshot | `ETag`/`304` |
| GET | `api/v2/items` | `?status=&group_id=&source=&limit=&cursor=&order=` | `200 {"items":[…],"next_cursor":…}` | served from `ArcSwap`, no SQLite |
| GET | `api/v2/items/{id}` | — | `200 ItemView` | includes `entry` when `?verbose=1` |
| POST | `api/v2/items/{id}/start` | — | `204` | `Queued{auto_start:false}` → `true` |
| POST | `api/v2/items/{id}/cancel` | — | `204` | idempotent |
| POST | `api/v2/items/{id}/retry` | `{"reset_options":bool}` | `202 {"id"}` | same id, `attempt+1` |
| DELETE | `api/v2/items/{id}` | `?delete_files=bool` | `204` | cancels first if running |
| POST | `api/v2/items/actions` | `{"action":"cancel\|start\|retry\|delete","ids":[…],"delete_files":false}` | `200 {"ok":[…],"failed":[{"id","code","message"}]}` | one store txn |
| GET | `api/v2/groups/{id}` | — | `200 GroupView` | |
| GET | `api/v2/groups/{id}/items` | `?limit=&cursor=` | `200 {"items":[…],"next_cursor"}` | default limit 250 |
| POST | `api/v2/groups/{id}/actions` | as items | `200` | applies to all children |
| DELETE | `api/v2/groups/{id}` | `?delete_files=` | `204` | cancels + removes children |
| GET | `api/v2/history` | `?before=<seq>&limit=&status=` | `200 {"items":[…],"next_cursor"}` | paged from SQLite beyond the memory window |
| GET | `api/v2/capabilities` | — | `200` | the `capabilities` block from §5.2 |
| GET | `api/v2/presets` | — | `200 {"presets":["a","b"]}` | sorted |
| GET | `api/v2/config/ytdl-options` | — | `200 {"ok":true,"msg":"","mtime":1767…,"source":"file"}` | |
| GET | `api/v2/dirs` | — | `200 {"download_dir":[…],"audio_download_dir":[…]}` | from `ArcSwap`, refreshed off-thread |
| POST | `api/v2/cookies` | `multipart` field `cookies` | `200 {"bytes":1234}` | 1 MB cap |
| DELETE | `api/v2/cookies` | — | `204` | |
| GET | `api/v2/cookies` | — | `200 {"has_cookies":true,"managed":true}` | |
| GET | `api/v2/subscriptions` | — | `200 {"subscriptions":[…]}` | |
| POST | `api/v2/subscriptions` | add body + `check_interval_minutes` | `202 {"id":…}` | resolution in background |
| PATCH | `api/v2/subscriptions/{id}` | `{"enabled","check_interval_minutes","name",…}` | `200 SubView` | `400` on bad types, never 500 |
| DELETE | `api/v2/subscriptions/{id}` | — | `204` | |
| POST | `api/v2/subscriptions/{id}/check` | — | `202 {"job_id":…}` | returns immediately (BRIEF §12) |
| POST | `api/v2/subscriptions/check` | `{"ids":[…]}` or `{}` | `202 {"job_id":…}` | |
| GET | `api/v2/version` | — | `200 {"version","yt_dlp","url_prefix","boot_id","protocol":2}` | |
| GET | `healthz` | — | `200`/`503` §14.3 | |
| GET | `readyz` | — | `200` once the store is open and state loaded | |
| GET | `metrics` | — | Prometheus text | gated by `AULOS_METRICS_ENABLED` |
| GET | `ws` | upgrade | `101` | §5 |
| GET | `download/*`, `audio_download/*` | — | file, `Accept-Ranges: bytes` | `tower_http::services::ServeDir`, `show_index = DOWNLOAD_DIRS_INDEXABLE` |
| GET | `robots.txt` | — | `ROBOTS_TXT` file or the legacy default text | |

`GET api/v2/items` reads the `ArcSwap` snapshot — **no database round trip, no lock**, ~30 µs for
500 items including serialization. Only `history` beyond `AULOS_MEM_DONE_ITEMS` touches SQLite,
through the read pool.

### 7.2 `POST api/v2/downloads` — async add

```json
{ "url": "https://www.youtube.com/playlist?list=PLabc",
  "download_type": "video", "codec": "auto", "format": "mp4", "quality": "1080",
  "folder": "Series/Foo", "custom_name_prefix": "", "playlist_item_limit": 0,
  "auto_start": true, "split_by_chapters": false, "chapter_template": null,
  "subtitle_language": "en", "subtitle_mode": "prefer_manual",
  "ytdl_options_presets": ["archive"], "ytdl_options_overrides": {},
  "dedupe": "active", "source": {"kind":"api"} }
```

Batch form: `{"urls":[…], …same options…}` → `202 {"ids":["01J…A","01J…B"]}`.

Response, before any network I/O has happened:

```json
{ "id": "01JBQ8Z1", "status": "resolving", "seq": 90180 }
```

Handler, in order (target p99 **< 3 ms**):

1. Validate the request against the same tables as legacy `parse_download_options` (spec §2.2),
   including the per-`download_type` format/quality matrices and the `..`/leading-separator
   checks on `custom_name_prefix` and `chapter_template`. Failures are `400` with `field` set.
2. Resolve `folder` against the right root with a **component-wise containment check**
   (`SafeRelPath`), not `starts_with` on a canonicalised string (fixes spec §13.21).
3. Validate preset names against the live `Presets`; reject non-empty `ytdl_options_overrides`
   when `ALLOW_YTDL_OPTIONS_OVERRIDES` is false.
4. Mint `ItemId` (ULID) and `Seq`.
5. Compute `canonical_key` (§12.4) and check the active-key index for `dedupe`
   (`off | active | strict`). `active` (default) skips a URL already active and returns the
   existing id with `"deduped": true`; `strict` returns `409`.
6. `store.send(StoreCmd::InsertItems { …, durability: Sync }).await` — one transaction; the
   `202` is not written until the row is in the WAL (**I10**).
7. `hub.send(Mutation::AddItems { … }).await` → `added` frame within 25 ms.
8. `resolver.submit(ResolveJob { id, url, req })` — bounded pool, may queue.

Steps 6 and 7 are the only awaits; both are sub-millisecond. Notably **`POST` does not wait for
step 8**, which is the whole point (client ref §7.1) and deletes `BackgroundAddService`,
`BackgroundAddUploader`, `BackgroundAddCompletionHandler`, `AddResultClassifier`,
`AddNotificationPresenter`, the app-group payload staging and the 24 h orphan sweeper from the
iOS app.

### 7.3 Auth

Aulos itself is unauthenticated (as today); Authelia sits in front. Two rules matter:

* Never redirect an API request. A reverse proxy that 302s to a login page is out of our control,
  but for anything we generate, `401` + the JSON envelope (client ref §7.2, and the classifier
  hack it forces).
* `GET ws` must fail the upgrade with `401` (not `101` then close) so the client can distinguish
  "session expired" from "server down".

Optional, off by default: `AULOS_API_TOKEN`, checked as `Authorization: Bearer` on `api/v2/*`
and `ws`, for people exposing Aulos without an SSO layer.

### 7.4 Static file serving of downloads

`ServeDir::new(DOWNLOAD_DIR).precompressed_br(false)` under `download/`, plus
`AUDIO_DOWNLOAD_DIR` under `audio_download/`. `tower-http` gives ranged requests, `Last-Modified`,
`If-Range` and `HEAD` for free, which is what unlocks the iOS client's missing
"open/stream/share a finished file" (client ref §7.14). Every finished item's `download_url` is
`PUBLIC_HOST_URL + percent_encode(filename)`, computed server-side so the client stops
concatenating.

Path traversal: `ServeDir` rejects `..`; we additionally refuse symlinks that escape the root
(`follow_symlinks(false)`).

### 7.5 Middleware stack (outermost first)

| Layer | Purpose |
|---|---|
| `SetRequestIdLayer` + `PropagateRequestIdLayer` | ULID request id, echoed in `x-request-id` and in every log line and error body |
| `TraceLayer` | one span per request; `ENABLE_ACCESSLOG` toggles the INFO access event |
| `CorsLayer` | `CORS_ALLOWED_ORIGINS`, `*` allowed; adds `Allow-Methods`/`Allow-Headers` (legacy omitted them — spec §2.1) |
| `DefaultBodyLimit` | 1 MiB, raised to 8 MiB for `api/v2/cookies` |
| `TimeoutLayer` | 30 s on `api/v2/*`; **excluded** for `ws` and `download/*` |
| `ConcurrencyLimitLayer` | 256 in-flight requests |
| `SetResponseHeaderLayer` | `Content-Type: application/json`, `X-Content-Type-Options: nosniff` |

### 7.6 v1 compatibility shim (BRIEF §8)

A thin translation layer, `crates/aulos-api/src/v1/`, with no state of its own.

| v1 route | Maps to | Translation notes |
|---|---|---|
| `POST add` | `POST api/v2/downloads` | runs `migrate_legacy_request` (spec §2.3) first; always `200 {"status":"ok"}` even on business errors, `{"status":"error","msg":…}` otherwise; validation failures keep the legacy `HTTPBadRequest` reason string |
| `GET history` | snapshot projection | `{"done":[…],"queue":[…],"pending":[…]}` — **flat arrays of items**, `queue` = non-terminal with `auto_start`, `pending` = non-terminal without, `done` = finished/error window. `canceled` items are omitted (legacy drops them) |
| `POST delete` | `items/actions` | `{"ids":[…],"where":"queue"\|"done"}`; ids are matched against **`id` first, then `url`** so the current iOS build (which sends `url ?? id`) keeps working |
| `POST start` | `items/{id}/start` | missing ids logged, always `{"status":"ok"}` |
| `GET version` | `api/v2/version` | `{"yt-dlp":…,"version":…}` **plus** `"url_prefix"` (client ref §7.15) |
| `POST subscribe` | `POST api/v2/subscriptions` | must return the resolved subscription, so this one **does** await resolution (bounded by `AULOS_RESOLVE_TIMEOUT_S`); documented as the single blocking v1 route |
| `GET subscriptions` | projection | legacy 13-key public dict (spec §7.1) |
| `POST subscriptions/{update,delete,check}` | v2 equivalents | `update` returns `400` (not `500`) on a bad `enabled` (fixes spec §13.25); `check` returns `{"status":"ok"}` **immediately** now |
| `POST cancel-add` | cancels every `resolving` item created by this caller in the last 60 s | legacy's generation counter has no analogue; this is strictly more useful |
| `GET presets`, `POST upload-cookies`, `DELETE`/`GET cookie-status` | v2 equivalents | legacy shapes |

Legacy status projection: `queued|resolving → "pending"`, `preparing → "preparing"`,
`downloading|postprocessing → "downloading"`, `finished → "finished"`, `error → "error"`,
`canceled → omitted`. The v1 `filename` key is present only when known (legacy also omitted it
lazily), and `percent`, `speed`, `eta`, `downloaded_bytes`, `total_bytes`,
`total_bytes_estimate`, `fragment_index`, `fragment_count`, `msg`, `title`, `url`, `id`,
`quality`, `format` are all emitted with legacy names and types.

No Socket.IO. The Angular UI is not a target (BRIEF §8), and the current iOS build's socket
simply fails to connect — which is why cutover order is: ship v2 server → ship the iOS build
that speaks v2 → drop the shim.

---

## 8. `aulos-store` — SQLite

### 8.1 Topology

```
 API / Hub / Subs / Telegram ──StoreCmd──► [mpsc 1024] ──► Store thread (1 OS thread, sync rusqlite)
                                                              │  write conn: WAL, NORMAL
 REST history / boot load ──StoreQuery──► [deadpool, 2 conns] ─┘  read conns: WAL snapshots
```

One writer (SQLite allows exactly one anyway; making it explicit removes `SQLITE_BUSY` entirely)
and a tiny read pool so a paged `history` query never queues behind a write batch. The store runs
on a **dedicated OS thread**, not `spawn_blocking`, so it never competes for the blocking pool
with ffprobe/ffmpeg calls.

Pragmas on the write connection:

```sql
PRAGMA journal_mode   = WAL;
PRAGMA synchronous    = NORMAL;      -- fsync at checkpoint, not per commit
PRAGMA foreign_keys   = ON;
PRAGMA busy_timeout   = 5000;
PRAGMA temp_store     = MEMORY;
PRAGMA cache_size     = -16000;      -- 16 MiB page cache
PRAGMA mmap_size      = 268435456;   -- 256 MiB
PRAGMA wal_autocheckpoint = 1000;    -- ~4 MB WAL
PRAGMA optimize;                     -- on close
```

Read connections: `journal_mode` inherited, `query_only = ON`.

### 8.2 DDL

```sql
-- migrations/0001_init.sql        (user_version = 1)

CREATE TABLE meta (
  k TEXT PRIMARY KEY NOT NULL,
  v TEXT NOT NULL
) STRICT;
-- rows: schema_version, seq_hwm, boot_id, imported_legacy_at, yt_dlp_version

CREATE TABLE item_group (
  id            TEXT PRIMARY KEY NOT NULL,       -- ULID, shares the namespace with item.id
  seq           INTEGER NOT NULL UNIQUE,
  kind          TEXT NOT NULL,                    -- playlist|channel|season|batch
  title         TEXT NOT NULL,
  url           TEXT NOT NULL,
  provider      TEXT NOT NULL,
  total         INTEGER NOT NULL DEFAULT 0,
  resolved      INTEGER NOT NULL DEFAULT 0,
  request_json  TEXT NOT NULL,
  source_json   TEXT NOT NULL,
  created_at    INTEGER NOT NULL,
  finished_at   INTEGER
) STRICT;

CREATE TABLE item (
  id            TEXT PRIMARY KEY NOT NULL,        -- ULID — immutable identity (I2)
  seq           INTEGER NOT NULL UNIQUE,          -- creation order / client sort key (I3)
  group_id      TEXT REFERENCES item_group(id) ON DELETE CASCADE,
  group_index   INTEGER,
  url           TEXT NOT NULL,
  canonical_key TEXT NOT NULL,                    -- dedupe key, §12.4
  provider      TEXT NOT NULL,
  media_id      TEXT,                             -- provider-native id (yt-dlp `id`, `sc_123_456`)
  title         TEXT NOT NULL,
  status        TEXT NOT NULL,
  auto_start    INTEGER NOT NULL DEFAULT 1,       -- 0 == legacy `pending`
  attempt       INTEGER NOT NULL DEFAULT 0,
  msg           TEXT,
  error         TEXT,
  filename      TEXT,                             -- relative to the item's download root
  size_bytes    INTEGER,
  request_json  TEXT NOT NULL,                    -- DownloadRequest, incl. presets + overrides
  entry_json    TEXT,                             -- compacted provider entry, §8.5
  source_json   TEXT NOT NULL,                    -- Source (api|v1|telegram|subscription|…)
  download_root TEXT NOT NULL,                    -- 'video' | 'audio' (which base dir)
  folder        TEXT,
  created_at    INTEGER NOT NULL,                 -- unix ms
  started_at    INTEGER,
  finished_at   INTEGER,
  CHECK (status IN ('queued','resolving','preparing','downloading','postprocessing',
                    'finished','error','canceled'))
) STRICT;

CREATE INDEX item_status_seq  ON item(status, seq);
CREATE INDEX item_group_ix    ON item(group_id, group_index);
CREATE INDEX item_finished    ON item(finished_at DESC) WHERE finished_at IS NOT NULL;
CREATE UNIQUE INDEX item_active_key ON item(canonical_key)
  WHERE status IN ('queued','resolving','preparing','downloading','postprocessing');

CREATE TABLE item_file (
  item_id  TEXT NOT NULL REFERENCES item(id) ON DELETE CASCADE,
  kind     TEXT NOT NULL,                          -- primary|chapter|subtitle|thumbnail|nfo|info_json
  filename TEXT NOT NULL,
  size     INTEGER,
  lang     TEXT,
  PRIMARY KEY (item_id, kind, filename)
) STRICT, WITHOUT ROWID;

CREATE TABLE subscription (
  id            TEXT PRIMARY KEY NOT NULL,
  name          TEXT NOT NULL,
  url           TEXT NOT NULL UNIQUE,
  enabled       INTEGER NOT NULL DEFAULT 1,
  check_interval_minutes INTEGER NOT NULL DEFAULT 60,
  request_json  TEXT NOT NULL,                     -- the full stored download settings
  last_checked  INTEGER,                            -- unix ms
  next_check_at INTEGER,                            -- unix ms, jittered; drives the scheduler
  consecutive_failures INTEGER NOT NULL DEFAULT 0,
  error         TEXT,
  created_at    INTEGER NOT NULL
) STRICT;
CREATE INDEX subscription_due ON subscription(enabled, next_check_at);

CREATE TABLE subscription_seen (
  sub_id   TEXT NOT NULL REFERENCES subscription(id) ON DELETE CASCADE,
  entry_id TEXT NOT NULL,
  seen_at  INTEGER NOT NULL,
  PRIMARY KEY (sub_id, entry_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX subscription_seen_age ON subscription_seen(sub_id, seen_at DESC);

CREATE TABLE telegram_chat (
  chat_id     INTEGER PRIMARY KEY NOT NULL,
  config_json TEXT NOT NULL,
  updated_at  INTEGER NOT NULL
) STRICT;

CREATE TABLE runtime_override (                     -- currently only `cookiefile`
  k TEXT PRIMARY KEY NOT NULL, v TEXT NOT NULL
) STRICT;
```

Design notes:

* **`STRICT` tables** — typed columns, so a bad write is an error, not a silently stored string
  (legacy stored `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` as a string all the way to the client;
  spec §1.4).
* **`item_active_key` is a partial unique index** — the database itself enforces "one active
  download per canonical URL", which is what legacy tried to do with an in-memory
  `queue.exists(key)` check that ignored `pending` and `done` (spec §13.18). A duplicate insert
  fails with `SQLITE_CONSTRAINT` and the API turns it into the dedupe response.
* **`subscription_seen` is a table, not a JSON array.** Legacy rewrites a 50 000-element JSON
  list on every check. Here a check is `INSERT OR IGNORE` of the new ids plus one
  `DELETE … WHERE seen_at < (SELECT seen_at … LIMIT 1 OFFSET SUBSCRIPTION_MAX_SEEN_IDS)` trim.
* **`next_check_at` lives in the DB**, so the per-subscription schedule (with jitter and
  exponential backoff) survives a restart instead of resetting to "+60 s" (spec §13.17).
* No `progress` columns anywhere — **I5** is enforced by the schema.

### 8.3 `seq` allocator

```sql
-- allocate a block
BEGIN IMMEDIATE;
  UPDATE meta SET v = CAST(CAST(v AS INTEGER) + 1024 AS TEXT) WHERE k = 'seq_hwm';
  SELECT CAST(v AS INTEGER) FROM meta WHERE k = 'seq_hwm';
COMMIT;
```

`SeqAllocator` hands out `[hwm-1023 … hwm]` from an `AtomicU64`; when exhausted it asks the store
for another block (a ~60 µs blocking call, once per 1024 ids). Crash ⇒ up to 1023 skipped values,
which is fine (only monotonicity is contracted, §3.1).

### 8.4 Write batching

```rust
pub enum StoreCmd {
    InsertItems  { items: Box<[ItemRow]>, group: Option<Box<GroupRow>>, remove: Option<RecordId> },
    SetStatus    { id: ItemId, status: Status, at: i64, msg: Option<Arc<str>>, error: Option<Arc<str>> },
    SetTitle     { id: ItemId, title: Arc<str>, media_id: Option<Arc<str>>, entry_json: Option<Arc<str>> },
    SetOutput    { id: ItemId, filename: Arc<str>, size: Option<u64>, files: Box<[OutputFile]> },
    AddFile      { id: ItemId, file: OutputFile },
    BumpAttempt  { id: ItemId },
    Remove       { ids: Box<[RecordId]> },
    UpsertSub    { sub: Box<SubRow> },
    MarkSeen     { sub: SubId, ids: Box<[Arc<str>]>, trim_to: u32 },
    UpsertChat   { chat_id: i64, config_json: Arc<str> },
    SetOverride  { k: &'static str, v: Option<Arc<str>> },
}
pub struct Envelope { cmd: StoreCmd, durability: Durability, ack: Option<oneshot::Sender<StoreResult>> }
pub enum Durability { Batched, Sync }
```

The store loop:

```rust
loop {
    let n = rx.recv_many(&mut buf, 256).await;      // wakes on the first message
    if n == 0 { break }
    let mut want_sync = buf.iter().any(|e| e.durability == Durability::Sync);
    // opportunistically extend the batch until the flush window expires or 256 is reached
    if !want_sync {
        let deadline = Instant::now() + flush_window;             // AULOS_DB_FLUSH_MS
        while buf.len() < 256 {
            match timeout_at(deadline, rx.recv()).await {
                Ok(Some(e)) => { want_sync |= e.durability == Durability::Sync; buf.push(e) }
                _ => break,
            }
        }
    }
    let tx = conn.transaction()?;                                  // ONE transaction
    for e in &buf { apply(&tx, &e.cmd)?; }                         // prepare_cached everywhere
    tx.commit()?;
    for e in buf.drain(..) { if let Some(a) = e.ack { let _ = a.send(Ok(())) } }
}
```

So: a `Sync` write commits on the next loop iteration (µs, no wait); `Batched` writes ride along
in the same transaction if they arrive within the same window. `recv_many` plus the extension
window means a 500-child insert that arrives as one `InsertItems` is one `INSERT INTO item`
executed 500 times inside one transaction — **~2 ms, one commit, ~180 KB written** versus
legacy's 500 whole-file rewrites (~63 MB, 1000 fsyncs).

Durability policy:

| Write | Durability | Rationale |
|---|---|---|
| item insert (single or batch) | `Sync` + `ack` awaited | **I10**: `202` means it is persisted |
| `resolving → queued` promotion + children | `Sync` | the expensive extraction result must not be lost |
| non-terminal status transitions | `Batched` | re-derivable; losing 200 ms of "it moved to downloading" is invisible |
| terminal status + filename + size | `Sync` | the one thing users hate losing |
| `MarkSeen` | `Sync` | otherwise a crash re-downloads a channel |
| Telegram chat config | `Sync` | user-visible setting |
| `Remove` | `Sync` | a deleted item must not come back |

With `synchronous = NORMAL`, `Sync` means "in the WAL", not "fsynced". The exposure is a
**host power loss** (not a process crash) losing up to one checkpoint interval. That is the
right trade for a media downloader; `AULOS_DB_SYNCHRONOUS=FULL` is available for the paranoid and
costs ~1 ms per commit.

### 8.5 Entry compaction

`entry_json` is the provider's metadata blob. Legacy persists it for queue/pending only, keeping
`playlist*`/`channel*`/`n_entries`/`__last_playlist_index` — except StreamingCommunity entries,
which are kept whole (spec §5.2). Ported exactly, plus:

* a hard 64 KiB cap per entry; oversized blobs are truncated to the keep-list only and a
  `entry_truncated` flag is recorded.
* the blob is dropped on terminal status **unless** the provider declares
  `Provider::retains_entry() == true` (SC does, for NFO generation).

### 8.6 Legacy importer (BRIEF §2)

Runs automatically when `AULOS_DB_PATH` does not exist and `STATE_DIR` contains legacy files.
Handles `schema_version` 1 and 2 JSON only (pickle/shelve is out of scope, BRIEF "out of scope").

| Source | Target | Mapping |
|---|---|---|
| `queue.json` | `item` rows, `status='queued'`, `auto_start=1` | key (url) → `url`; a fresh ULID is minted as `id`; `seq` in `timestamp` order |
| `pending.json` | `item`, `status='queued'`, `auto_start=0` | same |
| `completed.json` | `item`, `status = 'finished'` or `'error'` | `finished_at = timestamp/1e6` |
| `subscriptions.json` | `subscription` + `subscription_seen` | `seen_ids[]` → rows with synthetic descending `seen_at`; `ytdl_options_preset` → `presets` |
| `telegram_bot_config.json` | `telegram_chat` | JSON object keyed by chat id |
| `cookies.txt` | `runtime_override('cookiefile')` | detected at boot, as legacy |

Order matters: subscriptions before items, so a `Source::Subscription` reference resolves.
The whole import is **one transaction**; on any error it rolls back, quarantines the offending
file to `<name>.invalid.<ts>` (legacy's own convention) and continues with the rest. Legacy files
are then renamed `<name>.imported.<ts>` — never deleted, so a rollback to the Python image is
possible by renaming them back. A marker row `meta.imported_legacy_at` prevents a re-import.

`--import-only` and `--dry-run-import` CLI flags let this be rehearsed on the VPS before cutover.

---

## 9. `aulos-provider`

### 9.1 The trait

```rust
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    /// 0 = no match. Highest wins; ties broken by registration order. ytdlp returns 1.
    fn matches(&self, url: &Url) -> Match;
    fn limits(&self) -> ProviderLimits;
    /// Whether entry_json must survive terminal status (SC: yes, for NFO).
    fn retains_entry(&self) -> bool { false }

    async fn resolve(&self, url: &Url, req: &ResolveRequest, cancel: &CancellationToken)
        -> Result<Resolution, ProviderError>;

    async fn download(&self, job: &Job, sink: ProgressSink, cancel: CancellationToken)
        -> Result<Outcome, ProviderError>;

    async fn health(&self) -> ProviderHealth { ProviderHealth::Ok }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)] pub struct Match(pub u8);
impl Match { pub const NO: Self = Match(0); pub const FALLBACK: Self = Match(1);
             pub const HOST: Self = Match(50); pub const EXACT: Self = Match(100); }

pub struct ProviderLimits { pub max_concurrent: usize, pub holds_global_slot: bool }

pub struct Resolution { pub entries: Vec<MediaEntry>, pub kind: ResolutionKind }
pub enum ResolutionKind { Single, Group { kind: GroupKind, title: String, total: u32 } }

pub struct MediaEntry {
    pub media_id: String, pub title: String, pub url: Url,
    pub webpage_url: Option<Url>,
    pub index: Option<u32>, pub count: Option<u32>,
    pub live: Option<LiveStatus>, pub release_at: Option<i64>,
    pub duration_s: Option<f64>, pub ext: Option<String>,
    pub pre_error: Option<String>,      // e.g. "Live stream is scheduled to start at …"
    pub raw: Option<Box<RawEntry>>,     // provider blob → entry_json
    pub tmpl_fields: TemplateFields,    // pre-resolved playlist*/channel* outtmpl values
}

pub struct Job<'a> {
    pub id: ItemId, pub attempt: u16,
    pub entry: &'a MediaEntry,
    pub request: &'a DownloadRequest,
    pub paths: &'a JobPaths,            // home, temp, resolved outtmpl, chapter tmpl
    pub ytdl: Arc<YtdlOptions>,         // merged AT SPAWN TIME (§19.6)
    pub debug: bool,
}

pub enum Outcome {
    Finished { primary: OutputFile, extra: Vec<OutputFile> },
    Skipped  { reason: String },
    Canceled,
}
```

Deviation from BRIEF §9's literal signature: `download` takes `&Job` bundling
`entry + request + paths + merged options` instead of `(entry, request)`. Reason: path/outtmpl
resolution and option layering are queue concerns done once, and a struct keeps the signature
stable as fields are added. `ProgressSink` and `CancellationToken` are as specified.

### 9.2 `ProgressSink`

A concrete struct, not a trait — this is the hot path and dynamic dispatch on 60 calls/s/job is
avoidable, but more importantly a concrete type lets `hot()` be `#[inline]` and allocation-free.

```rust
pub struct ProgressSink {
    id: ItemId,
    hot: Arc<HotCell>,                    // Tier A — §4.4
    tx: mpsc::Sender<Mutation>,           // Tier B — Hub inbox
}
impl ProgressSink {
    /// Tier A. Non-blocking, non-allocating, infallible. Call as often as you like.
    #[inline] pub fn progress(&self, p: &HotProgress) { self.hot.write(p) }
    #[inline] pub fn phase(&self, ph: Phase) { self.hot.set_phase(ph) }

    /// Tier B. Rare, must not be lost — awaits if the Hub inbox is full.
    pub async fn status(&self, s: Status) { let _ = self.tx.send(Mutation::Status{..}).await }
    pub async fn message(&self, m: impl Into<Arc<str>>) { … }
    pub async fn output(&self, f: OutputFile, primary: bool) { … }
    pub async fn title(&self, t: impl Into<Arc<str>>) { … }
}
```

The split is the contract every provider must respect: **numbers go to Tier A, state goes to
Tier B**. A provider that reports a status change per progress line (the SC ffmpeg path is
tempted to) would defeat the batching, so `status()` debounces internally: identical consecutive
statuses are dropped.

### 9.3 Registry and selection

```rust
pub struct Registry {
    providers: Vec<Arc<dyn Provider>>,               // sorted by Match descending at build time
    slots: HashMap<&'static str, Arc<Semaphore>>,    // per-provider limits
}
impl Registry {
    pub fn select(&self, url: &Url) -> Arc<dyn Provider> { /* highest score, ties by order */ }
}
```

Registration order: command plugins (from `AULOS_PLUGINS_DIR`) → `streamingcommunity` → `ytdlp`.
`ytdlp` returns `Match::FALLBACK` for every `http(s)` URL, so it always wins by default and never
beats a specific provider. Command plugins are listed first so a user can shadow a built-in.

### 9.4 Process helpers (`proc`)

```rust
pub struct SpawnSpec { pub program: PathBuf, pub args: Vec<OsString>, pub env: Vec<(OsString, OsString)>,
                       pub cwd: Option<PathBuf>, pub stdin: Option<Vec<u8>> }
pub struct Child { pub pgid: Pid, inner: tokio::process::Child, pub stdout: ChildStdout,
                   pub stderr_ring: Arc<StderrRing> }
pub fn spawn_group(spec: SpawnSpec) -> io::Result<Child>;   // sets .process_group(0)
pub async fn kill_group(pgid: Pid, grace: Duration) -> io::Result<ExitStatus>;
```

`LineReader` — the NDJSON/ANSI reader used by every child-process provider:

```rust
pub struct LineReader<R> { r: BufReader<R>, buf: Vec<u8>, max: usize /* 1 MiB */ }
impl<R: AsyncRead + Unpin> LineReader<R> {
    /// Borrows into a reusable buffer: no String allocation per line.
    pub async fn next_line(&mut self) -> io::Result<Option<&[u8]>>;
}
```
Callers then `serde_json::from_slice::<ShimMsg<'_>>(line)` with `#[serde(borrow)]` `&str` fields —
so a progress line costs one `read_until` and zero heap allocations, and the values are copied
only into the `HotCell` (integers) or, rarely, into an `Arc<str>`.

`StderrRing` — a `Mutex<VecDeque<Box<str>>>` capped at 64 lines / 32 KiB, drained by a dedicated
task. **Draining stderr is mandatory**, not optional: a full 64 KiB pipe blocks the child
forever, and yt-dlp in verbose mode fills it in seconds. Legacy got away with this only because
`multiprocessing` had no pipe.

### 9.5 Command plugins (BRIEF §9)

`<AULOS_PLUGINS_DIR>/<name>/plugin.toml`:

```toml
name = "example"
version = "1.0.0"
priority = 60                            # Match score

[match]
hosts = ['^(www\.)?example\.(com|net)$']  # regex, anchored, matched against url.host_str()
paths = ['^/video/\d+']                   # optional, all must match

[resolve]                                 # optional; omit for single-video-only providers
command = ["/usr/bin/env", "python3", "resolve.py", "{url}"]
timeout_s = 60
# stdout: one JSON object per line, or one JSON array:
#   {"media_id":"1","title":"Ep 1","url":"https://…","index":1,"count":12}
# a line {"_type":"group","title":"Season 1","total":12} declares a group

[download]
command = ["/usr/bin/env", "bash", "dl.sh", "{url}", "{out_dir}", "{out_name}", "{tmp_dir}"]
timeout_s = 0
# {url} {out_dir} {out_name} {out_path} {tmp_dir} {referer} {cookies_file} {media_id} {title}
[download.env]
EXAMPLE_TOKEN = "$EXAMPLE_TOKEN"          # "$X" reads X from the server's environment

[download.headers]
Referer = "{referer}"

[progress]
kind = "regex"                             # "json_lines" | "regex"
stream = "both"                            # stdout | stderr | both
strip_ansi = true
last_match_wins = true                     # Spectre.Console-style repaint frames
pattern = '(?P<percent>[\d.]+)%\s+(?P<downloaded>\d+)/(?P<total>\d+)\s+(?P<speed>[\d.]+)([KMG]i?B)/s\s+ETA\s+(?P<eta>\d+:\d+)'
[progress.units]
speed = "auto"                             # auto|bytes|kib|mib
eta   = "clock"                            # clock (HH:MM:SS / MM:SS) | seconds

[output]
# how to find the produced file when the command does not print one
glob = "{out_name}.*"
prefer_ext = ["mp4", "mkv", "webm", "m4a"]
```

Named groups recognised: `percent | downloaded | total | speed | eta | status | filename | msg`.
A `json_lines` parser accepts the same key names directly. Unknown keys are ignored. Every
plugin runs in its own process group with the same kill semantics as yt-dlp, `stdin` closed, and
`PATH`/`HOME` sanitised. Discovery is at boot plus on `notify` events in `AULOS_PLUGINS_DIR`
(hot-add without restart); a malformed `plugin.toml` is logged and skipped, never fatal.

An example plugin ships in `plugins/examples/` and is exercised by the integration suite.

---

## 10. `aulos-provider-ytdlp`

### 10.1 Shim protocol

Rust ↔ `ytdlp_runner.py` over stdin (one JSON job) / stdout (NDJSON) / stderr (log ring).
This preserves 100 % `YTDL_OPTIONS` compatibility because the options are a **Python dict**, not
CLI flags (BRIEF §9).

Job in:

```json
{ "v": 1, "mode": "download",
  "url": "https://www.youtube.com/watch?v=abc",
  "opts": { "format": "bestvideo[height<=1080][ext=mp4]+bestaudio[ext=m4a]/best[height<=1080][ext=mp4]",
            "paths": {"home": "/downloads/Series/Foo", "temp": "/downloads"},
            "outtmpl": {"default": "%(title)s.%(ext)s",
                        "chapter": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s"},
            "socket_timeout": 30, "ignore_no_formats_error": true,
            "quiet": true, "verbose": false, "no_color": true,
            "merge_output_format": "mp4",
            "postprocessors": [{"key":"FFmpegVideoConvertor","preferedformat":"mp4"}],
            "impersonate": "chrome" },
  "extra": { "download_type": "video", "caption_ext_allow": [".srt",".vtt"],
             "thumbnail_rewrite": true } }
```

Messages out (`{"t": …}`, one per line):

| `t` | Fields | Rust action |
|---|---|---|
| `ready` | `yt_dlp_version`, `pid` | log; start the stall watchdog |
| `progress` | `status`, `downloaded_bytes`, `total_bytes`, `total_bytes_estimate`, `fragment_index`, `fragment_count`, `speed`, `eta`, `tmpfilename`, `filename` | Tier A `HotCell::write` after `calc_percent` |
| `pp` | `postprocessor`, `status`, `filepath`, `finaldir` | Tier B: `Postprocessing` status, `MoveFiles`→primary output |
| `file` | `kind` (`chapter`\|`subtitle`), `path` | Tier B `output()` |
| `info` | `id`, `title`, `entry` (compacted) | Tier B `title()` + `entry_json` |
| `entries` | `[MediaEntry]`, `kind`, streamed in chunks of 100 | resolve mode: yielded to the resolver incrementally |
| `log` | `level`, `msg` | tracing at the mapped level |
| `done` | `ok`, `code`, `msg` | terminal |
| `error` | `msg`, `kind` (`YoutubeDLError`\|`other`) | terminal error |

The shim forwards **only** the legacy key set from the progress hook (spec §5.3) plus
`postprocessor_hooks`, so behaviour is bit-identical to today. It is ~200 lines of Python with no
dependencies beyond `yt_dlp`, and it is the *only* Python in the image.

### 10.2 Resolve mode

`mode: "extract"` with `extract_flat: true, noplaylist: true, ignore_no_formats_error: true`
applied **after** user options (legacy ordering, spec §5.4 — presets must not be able to break
flat extraction), and the `__needs_strict_extract_retry` heuristic ported verbatim (`_type ==
"video"`, `formats == []`, has `id|url|webpage_url` ⇒ retry with `extract_flat: false,
ignore_no_formats_error: false` so geo-blocks surface as real errors).

Entries stream out in chunks of 100 (`entries` messages) rather than one big blob, so the
resolver can start creating children while yt-dlp is still walking a 5 000-item channel — the
first `added` frame lands within a few hundred ms even for a huge playlist.

### 10.3 Option layering and format selection

`get_format` / `get_opts` are a direct port of `dl_formats.py` (spec §6), tables and all,
including `custom:` escape hatch, `best_remux` popping the user `format`, the `writethumbnail`
guard for audio, the caption-mode language lists, and the `late_postprocessors` position.

Layering (spec §5.5): `YTDL_OPTIONS` (env) → `YTDL_OPTIONS_FILE` (file wins) → presets in
request order → per-request `ytdl_options_overrides`. `null` values are preserved so a preset can
clear a global key. Implemented as a fold over `serde_json::Map` with `Value::Null` retained.

### 10.4 Process and kill semantics (BRIEF §10)

```rust
let mut cmd = Command::new(&python);
cmd.arg(&shim_path)
   .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
   .process_group(0)                 // ← child becomes its own process-group leader
   .kill_on_drop(true)
   .env_clear()
   .envs(sanitised_env());           // PATH, HOME, LANG, TMPDIR, DOTNET_…, POT vars
let child = cmd.spawn()?;
let pgid = Pid::from_raw(child.id().unwrap() as i32);
```

Cancel:

```rust
async fn kill_group(pgid: Pid, grace: Duration) -> io::Result<ExitStatus> {
    signal::killpg(pgid, Signal::SIGTERM)?;                       // reaches ffmpeg, aria2c, deno
    match timeout(grace, child.wait()).await {
        Ok(st) => st,
        Err(_) => { signal::killpg(pgid, Signal::SIGKILL)?; child.wait().await }
    }
}
```

| Property | Legacy | Here |
|---|---|---|
| signal | `SIGKILL` to the forked process only | `SIGTERM` to the **process group**, `SIGKILL` after `AULOS_KILL_GRACE_MS` |
| ffmpeg / N_m3u8DL-RE grandchildren | orphaned (spec §13.10) | killed with the group |
| `.part` / `.ytdl` cleanup | one `tmpfilename`, often stale (spec §13.23) | the shim's SIGTERM handler raises into yt-dlp so its own cleanup runs; Rust additionally removes every path the shim reported as `tmpfilename` (tracked as a set, not a single overwritten field) |
| zombie reaping | thread blocked in `proc.join` per job | `child.wait()` inside the job task; `tini` as PID 1 catches anything escaping |
| shutdown | none | on SIGTERM the supervisor `kill_group`s every entry in `CancelRegistry`, in parallel, bounded by the same grace |

The shim installs `signal.signal(SIGTERM, raise KeyboardInterrupt)` and exits `130`; Rust maps
exit `130`/`143` plus a set `CancellationToken` to `Outcome::Canceled` rather than an error.

### 10.5 Percent normalisation

`calc_percent` is a line-by-line port of `_calculate_progress_percent` (spec §5.3), including:
`finished ⇒ 100.0`; exact `total_bytes` ⇒ ratio; else estimate bounded by
`[idx/count, min((idx+1)/count, 99.9)]` when fragment counts exist; the "ignore the bogus
1 KiB/1 KiB HLS estimate" rule; `None ⇒ keep previous`; clamp `[0.0, 99.9]`; never decrease.
Plus the progress-source reset: `progress_src = filename ?? tmpfilename`, and a change resets the
monotonic floor (so a video→audio stream switch during a merge restarts at 0 instead of pinning
at 99.9). This is covered by a `proptest` that asserts monotonicity over arbitrary status
sequences — invariant **I4**.

### 10.6 Watchdogs

One `JobWatchdog` per running job, driven off the same `HotCell` the Hub samples (no extra
channel):

| Timer | Default | Action |
|---|---|---|
| stall | `AULOS_STALL_TIMEOUT_S` (=`TELEGRAM_STALL_TIMEOUT_SECONDS`, 180) | `Mutation::Notice{code:"stalled"}` → WS `notice` + Telegram message, **once**. If `AULOS_STALL_ACTION=cancel`, cancel. |
| hard timeout | `AULOS_JOB_TIMEOUT_S` (0 = off) | `Notice{code:"job_timeout"}`; legacy parity = warn only |
| Telegram hard warn | `TELEGRAM_HARD_TIMEOUT_SECONDS` (7200) | `Notice{code:"slow"}`, once per chat |

Legacy polls every 15 s from the Telegram bot with its own lock (spec §8). Here the watchdog is a
`tokio::time::sleep_until` per job, recomputed when `HotCell.gen` advances — no polling, no lock,
and it works for web/subscription downloads too (legacy only watched Telegram-originated ones).

---

## 11. `aulos-provider-sc` (StreamingCommunity)

### 11.1 HTTP client decision (BRIEF §9)

**Choice: `wreq` (the maintained continuation of `rquest`) behind a default-on cargo feature
`sc-impersonate`, with `reqwest` as the compiled fallback.**

Rationale: legacy uses `curl_cffi` with `impersonate="chrome"`. The SC front-end sits behind
Cloudflare and fingerprints the TLS ClientHello (JA3/JA4) and the HTTP/2 SETTINGS frame order;
plain `reqwest` with a spoofed `User-Agent` gets challenged. `wreq` gives BoringSSL-backed
Chrome impersonation with a `reqwest`-shaped API, so the call sites are identical.

Risks and the fallback: `wreq` needs BoringSSL, which lengthens the Docker build and constrains
cross-compilation for arm64. Mitigation — one `trait ScHttp` with two impls:

```rust
#[async_trait] pub trait ScHttp: Send + Sync {
    async fn get(&self, url: &Url, hdrs: &HeaderMap) -> Result<ScResponse, ScError>;
    fn cookie_header(&self, url: &Url) -> Option<String>;
    fn impersonating(&self) -> bool;
}
```
`--no-default-features` builds `ReqwestScHttp` (rustls, hand-set Chrome header order and UA).
`healthz` reports `sc.impersonating`, and a startup probe of `GET {base}/it` that returns 403
logs a loud warning naming the feature flag. If `wreq` ever becomes unmaintained the fallback is
already the shipped code path.

HTML parsing: `scraper` (html5ever) for `div#app[data-page]` and the first `<iframe src>` — two
selectors, correctness over speed, ~1 ms on a 300 KB page. `tl` is the lighter alternative if the
dependency weight matters.

### 11.2 Scraping and download

Ported behaviour, unchanged from spec §9: Inertia `version` from `div#app[data-page]` (cached per
base URL with a 10-minute TTL), `x-inertia`/`x-inertia-version` headers, watch page →
`props.embedUrl` → first iframe → `window.streams` (prefer `active: true`) + `token`/`expires`
regexes, `h=1` only when `canPlayFHD`, existing query params preserved. `can_extract` = hostname
*contains* `streamingcommunity`, so mirrors match. `/season-`, `/watch/`, `/titles/` dispatch.

Only the **watch URL** is stored (tokens expire); the m3u8 is re-extracted just in time at
download start — but now in the same process, not a fork, and the pointless diagnostic `GET` of
the m3u8 (spec §13.27) is dropped.

Season/title extraction currently costs 3+ round trips per episode (~60 requests for a 20-episode
season, spec §9.3). Here the per-episode fetches run through a `FuturesUnordered` with
concurrency 4 and a shared connection pool, cutting a 20-episode resolve from ~30 s to ~4 s.

Download: N_m3u8DL-RE with the legacy argv, `SC_THREAD_COUNT` threads, ANSI-stripped progress
with **last-match-wins** frame scanning (`aulos-provider::ansi`), and on non-zero exit the
ffmpeg retry after `cleanup_partial`. The **gapless fallback mux** is ported exactly: collect
`.m4s/.ts/.mp4/.m4a/.aac` under the segment dir, sort by **natural numeric filename order** (not
mtime), binary-concatenate in 1 MiB chunks into `_merged.ts`, then
`ffmpeg -y -i _merged.ts -map 0 -c copy -bsf:a aac_adtstoasc -movflags +faststart <out>`.
The comment explaining why `-f concat` must not be used is carried over into the Rust source.

Concurrency: `ProviderLimits { max_concurrent: SC_MAX_CONCURRENT_DOWNLOADS, holds_global_slot: false }`
— acquired *without* a global slot, exactly as legacy (spec §5.3, BRIEF §10).

Naming: SC still bypasses `OUTPUT_TEMPLATE` and writes `<download_dir>/<sanitised title>.mp4`
plus `.info.json`, because the NFO hook consumes the latter. Sanitisation regex `[<>:"/\\|?*]`
→ `_`, then trim `". "`.

---

## 12. `aulos-queue`

### 12.1 Actor topology

```
                       ┌──────────────── QueueHandle (cheap clone) ───────────────┐
                       │  add() start() cancel() retry() remove() subscribe_ws()  │
                       └───────────────┬────────────────────┬────────────────────┘
                                       │                    │
   API / Telegram / Subs ──────────────┤                    │
                                       ▼                    ▼
                              ┌────────────────┐    ┌──────────────────┐
                              │   Resolver     │    │      Hub         │◄── HotCell sampling
                              │  Semaphore(4)  │    │  (single owner)  │
                              └───────┬────────┘    └───┬────────┬─────┘
                                      │ AddItems        │        │ broadcast
                                      └─────────────────┤        ▼
                                                        │    WS sessions
                              ┌────────────────┐        │
                              │   Scheduler    │◄───────┘ Mutation::{Status,Attach,Detach}
                              │ ready heap +   │
                              │ Semaphore(N)   │──spawn──► JobTask ──► Provider::download
                              └───────┬────────┘                          │ ProgressSink
                                      │ terminal                          ▼
                                      ▼                              HotCell (Tier A)
                              ┌────────────────┐
                              │ HookDispatcher │──► jellyfin (debounced) / nfo / audiosync
                              └────────────────┘
                              ┌────────────────┐
                              │     Store      │ (1 OS thread, batched txns)
                              └────────────────┘
```

### 12.2 Scheduler

```rust
pub struct Scheduler {
    ready: BinaryHeap<Ready>,                // key: (priority desc, seq asc) → FIFO within priority
    global: Arc<Semaphore>,                  // MAX_CONCURRENT_DOWNLOADS
    per_provider: HashMap<&'static str, Arc<Semaphore>>,
    jobs: JoinSet<JobResult>,
    cancels: Arc<DashMap<ItemId, CancelHandle>>,
    inbox: mpsc::Receiver<SchedCmd>,
}
struct Ready { priority: Priority, seq: Seq, id: ItemId, provider: &'static str }
enum Priority { Retry = 3, Interactive = 2, Subscription = 1, Bulk = 0 }
```

Slot acquisition per job, in this order (deadlock-free because the order is total):

1. per-provider permit (if `max_concurrent > 0`)
2. global permit — **skipped** when `holds_global_slot == false` (SC)

`Priority` is new and cheap: an item a human just added (`Interactive`) jumps ahead of the 486
remaining children of a bulk playlist (`Bulk`), and a retry jumps ahead of both. This is the
difference between "I pasted a link and it started" and "I pasted a link and it's behind 486
episodes". Within a priority class, `seq` order = FIFO = legacy behaviour.

The scheduler polls `jobs.join_next()` in the same `select!` as its inbox, so a completed job
releases its permit and starts the next item in the same loop iteration (~microseconds). Legacy
ran `_post_download_cleanup` — including a synchronous whole-file JSON rewrite — *inside* the
semaphore (spec §13.11); here cleanup is a `Mutation` and a `StoreCmd`, both `try_send`, and the
permit is dropped first.

### 12.3 Resolver pool

```rust
pub struct Resolver { sem: Arc<Semaphore> /* AULOS_RESOLVE_CONCURRENCY = 4 */, … }
```

Separate from download slots (BRIEF §10) so a 500-item playlist's metadata work cannot starve
downloads, and vice versa. Each resolve is one task: acquire permit → `provider.resolve()` under
`timeout(AULOS_RESOLVE_TIMEOUT_S)` and the item's `CancellationToken` → emit.

Entries stream in (§10.2), and the resolver batches them into `Mutation::AddItems` chunks of 100
with a 25 ms coalescing window, so the client sees the group grow smoothly.

Failure: `Status → Error` with the provider's message, `attempt` unchanged; `resolve_failed`
error code on the v1/v2 add path when the caller awaited it.

### 12.4 Dedupe

`canonical_key` = `provider_id || '\u{1f}' || normalised`, where `normalised` is:

* lower-cased scheme+host, default port stripped, trailing `/` stripped;
* provider-supplied canonical form when available (`ytdlp`: `media_id` once known, so
  `youtu.be/x`, `youtube.com/watch?v=x` and `…&t=30` collapse; `sc`: `sc_<title>_<ep>`);
* before resolution, the URL with a per-provider tracking-param denylist applied
  (`si, feature, pp, utm_*` for YouTube hosts — the same list the iOS share extension applies,
  which lets the client stop doing it).

The partial unique index (§8.2) makes "one active download per key" a database invariant. Modes:
`off`, `active` (default — return the existing id with `"deduped": true`), `strict` (`409`).
This closes spec §13.18 (legacy checked only `queue`, so a pending URL could be added twice and
silently overwrite itself).

### 12.5 Groups and the 500-item playlist

The provisional record's lifecycle:

```
POST → ItemState{status: Resolving, id: G}           (1 row, 1 added frame)
     → resolve() → ResolutionKind::Group{total:500}
     → Mutation::AddItems{ replace: Some(G),
                           group: Some(NewGroup{ id: G, seq: <G's seq> }),
                           items: [child_1 … child_100] }        ← one frame, in-place morph
     → 4 more AddItems chunks of 100                              ← 4 more frames
```

The group **reuses the provisional id and seq** (§5.4), so:

* the client's row does not move (same sort key) and does not blink (same id);
* the id returned by `POST` stays valid and now addresses the group;
* `GET api/v2/groups/{id}` works with the id the caller already has.

`ResolutionKind::Single` instead promotes in place: `Mutation::Title` + `Status → Queued`, no
membership change at all — no `added`/`removed`, just a delta. That is the common case (a single
video add) and it costs one 90-byte delta frame.

Snapshot economics: groups larger than `AULOS_SNAPSHOT_GROUP_INLINE` (50) are sent as the
aggregate only, with `children_inline: false`; the client fetches or `watch`es them on expand.
A 500-item playlist therefore adds **~380 bytes** to the connect snapshot, not 125 KB.

Delta economics: children in `Queued` are static, so they never appear in a delta. Per tick the
group contributes one `GroupDelta` and the 3 running children contribute one `ItemDelta` each.

### 12.6 Cancellation

`CancelHandle { token: CancellationToken, pgid: Arc<OnceLock<Pid>> }`, stored in
`DashMap<ItemId, CancelHandle>` — a genuinely disjoint-key, read-mostly structure, which is where
`DashMap` earns its place.

Cancel path: mark `Canceled` in the Hub (prompt frame, so the UI reacts in <25 ms) → cancel the
token → if a pgid is registered, `kill_group` with grace → job task observes `Canceled`,
skips hooks, deletes tracked partials → `Detach`. A cancel of a `Queued` item just removes it
from the ready heap. A cancel of a `Resolving` item aborts the resolve. All idempotent.

Group cancel = cancel every non-terminal child, one `Mutation`, one store transaction.

### 12.7 Restart behaviour

On boot (after the store load), items are triaged:

| DB status | `AULOS_RESTART_POLICY=resume` | `=pause` |
|---|---|---|
| `preparing`, `downloading`, `postprocessing` | `Queued{auto_start:true}`, `attempt+1`, `msg="resumed after restart"` | `Queued{auto_start:false}` |
| `resolving` | re-submitted to the resolver pool | `Queued{auto_start:false}` |
| `queued` | unchanged | unchanged |
| terminal | loaded into the done window (most recent `AULOS_MEM_DONE_ITEMS`) | same |

Crucially, resumed items go **through the scheduler**, so `MAX_CONCURRENT_DOWNLOADS` is
respected. Legacy re-`add`ed everything in `queue.json` at once, before any client connected
(spec §13.12) — with a 40-item queue that is 40 simultaneous yt-dlp processes.

Partial files: yt-dlp resumes HTTP downloads from `.part` by default, so temp files are left in
place. SC partials are deleted (the m3u8 token is dead anyway).

---

## 13. Peripheral crates

### 13.1 `aulos-telegram` (BRIEF §11)

`teloxide 0.13+` with `dptree` handlers, wrapped in `teloxide::adaptors::Throttle` so the
30 msg/s global and per-chat limits are handled by the library. Same command surface as legacy:
`/start`, `/config` with the inline keyboards (`cfg:menu:{main|format|quality|limit}`,
`cfg:toggle:split`, `cfg:set:format:{fmt}`, `cfg:set:quality:{q}`, `cfg:set:limit:{n}`), the
`TELEGRAM_ALLOWED_CHAT_IDS` gate with silent-ignore-plus-warn, `TELEGRAM_MAX_URLS_PER_MESSAGE`,
and the SSRF guard (reject non-`http(s)`, `localhost`, `*.local`, and private/loopback/link-local/
multicast/reserved/unspecified IPs).

Per-chat config moves from `telegram_bot_config.json` to the `telegram_chat` table, with the
same defaults and the same legacy→v2 selection normalisation (`audio` formats ⇒ `audio`,
`thumbnail`, `captions`, `quality=audio`, `quality=best_ios` ⇒ `format=ios`).

Two real upgrades:

**Attribution by `source`, not a contextvar.** Every item created by the bot carries
`Source::Telegram { chat_id }` persisted in `source_json`. Legacy read a `contextvars.ContextVar`
inside `on_added`, so web-UI and subscription downloads were invisible to the bot and the
attribution died across a restart (spec §8). Here a restart-resumed Telegram job still reports to
its chat.

**One live progress message per chat, edited at most every 3 s.** A single `ProgressEditor` task:

```rust
struct ChatProgress { message_id: Option<MessageId>, items: IndexSet<ItemId>,
                      last_edit: Instant, last_text_hash: u64, backoff: Duration }
```
Every 3 s it walks the chats with active items, renders from the **published snapshot** (no Hub
interaction), and edits only if the rendered text changed (`last_text_hash`). `RetryAfter(n)`
from Telegram sets `backoff = n` for that chat. When every item in a chat's set is terminal, the
message is edited one final time into the completion summary and the set cleared:

```
⬇️ 2 downloading · 1 queued
▸ Episode 11 ████████░░░░░░░░ 43 %  1.0 MB/s  ETA 0:35
▸ Episode 12 ██░░░░░░░░░░░░░░ 12 %  0.8 MB/s  ETA 2:10
✅ Episode 10  (275 MB)
```

`Notifier` impl: `on_added`, `on_status`, `on_terminal`, `on_notice` — the trait BRIEF "out of
scope" wants left open for APNs. Adding an `ApnsNotifier` later is a new impl plus a device-token
table; nothing else changes.

### 13.2 `aulos-subscriptions` (BRIEF §12)

Same data model and the same 13-key public projection as legacy (spec §7.1: `id, name, url,
enabled, check_interval_minutes, download_type, codec, format, quality, folder, last_checked,
seen_count, error`). Storage per §8.2.

Scheduler: **per-subscription `next_check_at`, not a global 60 s tick.**

```rust
loop {
    let due = store.next_due().await;                    // SELECT … ORDER BY next_check_at LIMIT 1
    select! {
        _ = sleep_until(due.at) => spawn_check(due.id),  // under Semaphore(4)
        _ = rx.changed() => continue,                    // a mutation rescheduled something
    }
}
```

| Legacy | Here |
|---|---|
| first check at boot **+60 s** | boot + `rand(5..30) s` (BRIEF §12) |
| 60 s tick granularity | exact wake at `next_check_at`, ±jitter of ±10 % of the interval |
| sequential checks; one slow feed blocks all | `Semaphore(AULOS_SUBSCRIPTION_CONCURRENCY = 4)` |
| failure does **not** update `last_checked` ⇒ hot retry every 60 s forever (spec §13.17) | `consecutive_failures += 1`, `next_check_at = now + min(interval × 2^failures, 6 h)` with jitter |
| `POST /subscriptions/check` awaits everything (minutes) | `202 {"job_id"}`, checks run in the background, progress via `subs` frames |

Behaviour preserved exactly: backfill suppression on subscribe (all currently visible ids marked
seen without queueing, **except** `live_status == "is_upcoming"`); re-queue of already-seen
`is_live` entries; `_is_media_entry` heuristics including the `ie_key ~ playlist|channel|tab`
extra-field requirement; the one-level recursion into up to 5 child URLs for channel-of-tabs
pages; `error` cleared only by a fully successful check; `folder: ""` → `None`;
`VIDEO_ONLY_MSG` for single-video URLs.

One asymmetry is fixed: legacy applies user `YTDL_OPTIONS` **last** in subscription extraction
(so `YTDL_OPTIONS` can break `extract_flat`) but **first** in `__extract_info` (spec §13.13).
Both paths now use the `__extract_info` ordering. Called out in §19.

### 13.3 `aulos-hooks` (BRIEF §13)

```rust
pub enum Hook { Completed { id: ItemId, view: Arc<ItemView> } }
pub struct HookDispatcher { rx: mpsc::Receiver<Hook>, jf: JellyfinDebouncer, … }
```

| Hook | Behaviour |
|---|---|
| Jellyfin refresh | debounced: the first completion arms a `AULOS_JELLYFIN_DEBOUNCE_S` (30 s) timer; further completions inside the window coalesce; one request fires at expiry. When `JELLYFIN_LIBRARY_ID` is set, `POST /Items/{id}/Refresh?metadataRefreshMode=…&imageRefreshMode=…` (making the three documented-but-inert env vars real); otherwise `POST /Library/Refresh`. `Authorization: MediaBrowser Token="…"`, `JELLYFIN_SYNC_TIMEOUT_SECONDS`. Failures log at WARN and never affect the item. |
| NFO generation | For SC items (and any item whose provider `retains_entry`), write `<base>.nfo` with `quick-xml` from the stored entry: `episodedetails` when `series\|season_number\|episode_number` is set, else `movie`; the same element set as `jellyfin_nfo_generator.py`, `uniqueid type="streamingcommunity"\|"youtube"`, ≤20 tags, `runtime` in whole minutes. Unlike legacy this is **wired in** (legacy's script was never called from any code path, spec §10) and it does **not** delete the `.info.json` — it is registered as an `OutputFile{kind: InfoJson}` instead. |
| `best_remux` audio sync | Reimplemented in Rust for `video/mp4/best_remux`: `ffprobe -select_streams v` gate, duration-scaled timeout `max(600, ceil(duration/2))` (or 1800 unknown), `ffmpeg -y -loglevel warning -i <f> -map 0 -dn -ignore_unknown -c copy -c:a aac -b:a 256k -movflags +faststart <tmp>` then atomic rename. Runs as a **hook, not a yt-dlp `Exec` postprocessor**, so the item sits in `postprocessing` with a real phase (`phase: "audio_sync"`) instead of appearing frozen, and a failure marks the item `error` with a real message rather than a cryptic postprocessor failure. The hard-coded `/app/app/audio_sync_fix.py` path disappears. |

All hooks run in their own task; hook failure never changes an item's terminal status except
`audio_sync`, which is part of producing the output.

---

## 14. `aulos-server`

### 14.1 Startup order

```
1. tracing init (LOGLEVEL, dampen httpx/httpcore/teloxide/notify/hyper to WARN)
2. Config::from_env()                       → exit(1) with a clear message on any error
3. ensure DOWNLOAD_DIR / AUDIO_DOWNLOAD_DIR / TEMP_DIR / STATE_DIR (recursive)
4. Store::spawn()  → migrate → maybe import legacy → SeqAllocator ready
5. load state → Hub::new(published, ring, bus) → spawn Hub
6. Registry::build (plugins → sc → ytdlp), provider health probes (non-fatal)
7. Scheduler + Resolver + HookDispatcher spawn; restart triage (§12.7)
8. PotSupervisor spawn (if AULOS_POT_ENABLED)
9. ConfigWatcher spawn (notify on YTDL_OPTIONS_FILE and AULOS_PLUGINS_DIR)
10. SubscriptionScheduler spawn
11. Telegram bot spawn (if TELEGRAM_BOT_ENABLED and token/chat-ids present)
12. axum serve (HOST:PORT, optional TLS from CERTFILE/KEYFILE, SO_REUSEPORT)
13. readyz flips to 200
```

Every step 4–11 is spawned into a `TaskTracker` (tokio-util) so shutdown can await them.

### 14.2 POT sidecar supervision (BRIEF §14)

```rust
struct PotSupervisor { backoff: ExponentialBackoff /* 1s → 60s, ×2, ±20 % jitter */,
                       state: Arc<ArcSwap<PotState>>, restarts: AtomicU32 }
```

Spawns `bgutil-pot <AULOS_POT_ARGS>` in its own process group, pipes stdout/stderr into `tracing`
(one span, level from a prefix heuristic), and on exit logs the code and restarts after backoff.
Backoff resets after 60 s of healthy uptime. A liveness probe hits `AULOS_POT_HEALTH_URL` every
15 s; three consecutive failures force a restart even if the process is alive (a wedged sidecar is
worse than a dead one, because yt-dlp then fails bot checks silently — spec §13.28). State is
published to `healthz`. On shutdown, `kill_group` with the same grace.

### 14.3 `healthz`

```json
{ "status": "ok", "version": "2026.09.04", "yt_dlp": "2026.8.30.232658.dev0",
  "url_prefix": "/", "boot_id": "01JBQ8YQ…", "uptime_s": 3620,
  "pot": { "enabled": true, "state": "running", "pid": 42, "restarts": 0, "probe_ms": 3 },
  "db": { "ok": true, "wal_bytes": 1048576, "queue_depth": 0, "flush_p99_ms": 1.4 },
  "queue": { "downloading": 3, "postprocessing": 0, "queued": 486, "resolving": 0,
             "groups": 1, "slots_free": 0, "resolver_free": 4 },
  "ws": { "clients": 3, "seq": 90261, "lagged_frames": 0, "slow_disconnects": 0 },
  "providers": [ { "id": "ytdlp", "ok": true },
                 { "id": "streamingcommunity", "ok": true, "impersonating": true },
                 { "id": "example", "ok": true, "kind": "command" } ],
  "hooks": { "jellyfin": { "enabled": true, "last_ok_at": 1767225900000, "pending": false } } }
```
`503` when the store is unreachable or `readyz` has not flipped. Docker `HEALTHCHECK` hits
`<URL_PREFIX>healthz` (legacy hit the SPA index and ignored `URL_PREFIX`, spec §12.1).

### 14.4 Shutdown

`SIGTERM`/`SIGINT` → `shutdown_token.cancel()` → in order: stop accepting HTTP/WS (axum graceful,
close sockets with 1001 `"server shutting down"` so clients reconnect rather than error) → stop
the subscription scheduler and the Telegram updater → `kill_group` every entry in
`CancelRegistry` in parallel (grace `AULOS_KILL_GRACE_MS`) → in-flight items persisted as
`downloading` (so the restart triage picks them up) → Hub final flush → store drain +
`PRAGMA optimize` → POT `kill_group` → `TaskTracker::wait()` with a 10 s ceiling, then hard exit.

`tini -g` remains PID 1 so anything that escapes is reaped.

---

## 15. Channel and backpressure inventory

| Channel | Type | Cap | Producers | Consumer | Full ⇒ |
|---|---|---|---|---|---|
| Hub inbox | `mpsc<Mutation>` | 4096 | API, resolver, job tasks, subs, config watcher | Hub | `try_send`, then `send().await` for structural; `Notice` dropped + counter |
| WS bus | `broadcast<Arc<WireFrame>>` | 256 | Hub | each session writer | oldest overwritten; reader gets `Lagged` → snapshot resync (§5.7) |
| Store inbox | `mpsc<Envelope>` | 1024 | everything | store thread | `send().await`; a full store queue means disk trouble and callers *should* slow down |
| Store read pool | `deadpool` | 2 conns | REST history, boot | — | queued; 5 s timeout → `503` |
| Scheduler inbox | `mpsc<SchedCmd>` | 512 | Hub, API | Scheduler | `send().await` |
| Resolver queue | `Semaphore(4)` + `mpsc<ResolveJob>` | 4096 | API, subs | resolver tasks | `try_send`; on full, the add still succeeds and the item stays `resolving` until a retry sweep picks it up (30 s) |
| Hook queue | `mpsc<Hook>` | 256 | Hub | HookDispatcher | drop + WARN (hooks are best-effort) |
| Telegram outbound | `mpsc<TgMsg>` | 256 | Hub, watchdogs | bot task (throttled) | drop oldest progress edit, keep terminal messages |
| Per-job stdout | pipe | 64 KiB (OS) | child | job task `LineReader` | child blocks — which is correct backpressure on a chatty child |
| Per-job stderr | pipe → `StderrRing` | 64 lines | child | drain task | oldest lines evicted; **must** be drained (§9.4) |

Rule applied throughout: **bounded everywhere; the only unbounded thing in the process is the
SQLite file.** Anything that can be dropped without user-visible loss (progress, notices, hook
retries) is dropped with a counter; anything that cannot (status transitions, terminal states,
store writes) applies backpressure to its producer.

---

## 16. Memory bounds (**I9**)

| Structure | Bound | Steady-state size |
|---|---|---|
| `Hub.items` | queued + active + `AULOS_MEM_DONE_ITEMS` (500) | ~350 B/item state + ~450 B view ⇒ ~0.8 KB/item; 1000 items ≈ 800 KB |
| `Hub.done` window | 500 ids, FIFO; older items only in SQLite, served by paged `history` | 4 KB |
| `entry_json` in memory | **not** held — only the DB column; loaded on demand for `?verbose=1` and NFO | 0 |
| `Published` | 2 generations alive at once (`ArcSwap` + one reader guard) | 2 × (8 B × n) + shared views |
| `ReplayRing` | `min(512 frames, 4 MiB)` | ≤ 4 MiB |
| `broadcast` buffer | 256 × `Arc` (frames shared with the ring) | ≤ 2 KB of pointers |
| Per session | 1 receiver + 1 write buffer (64 KiB) | ≤ 80 KB × ≤64 clients ⇒ ≤5 MB |
| Per running job | `HotCell` (64 B) + `StderrRing` (32 KiB) + `LineReader` buffer (≤64 KiB) | ≤100 KB × 4 = 400 KB |
| SQLite | 16 MiB page cache + 256 MiB mmap (virtual, not resident) | ~20 MB RSS |

Target RSS for the stock configuration with 1000 queued items and 3 downloads: **< 90 MB**
(legacy sits at 250–400 MB with a forked interpreter per download).

---

## 17. Sequence walkthroughs

Times are targets on the VPS class this runs on (2 vCPU, NVMe), excluding network to the
provider.

### 17.1 Add a single video

| t | Actor | Action | Wire |
|---|---|---|---|
| 0 | iOS | `POST api/v2/downloads {"url":"…watch?v=abc","format":"mp4","quality":"1080",…}` | |
| +0.3 ms | api | validate, `SafeRelPath` folder check, preset check, mint `id=01J…A`, `seq=90180`, `canonical_key` | |
| +0.6 ms | store | `InsertItems{Sync}` → one txn, WAL | |
| +0.8 ms | api | `Mutation::AddItems`; **respond `202 {"id":"01J…A","status":"resolving","seq":90180}`** | |
| +1 ms | Hub | apply; `urgent_at = now+25 ms` | |
| +25 ms | Hub | flush | `added` (1 full item, `status:"resolving"`) |
| +25 ms | resolver | permit acquired, shim `mode:"extract"` spawned | |
| +1.4 s | shim | `info` + `entries` (1 entry) | |
| +1.4 s | Hub | `Title` + `Status→Queued`; urgent | |
| +1.42 s | Hub | flush | `delta` `[{id, title, status:"queued", …}]` |
| +1.42 s | sched | global permit free ⇒ `JobTask` spawned; `Attach{hot}` | |
| +1.45 s | Hub | urgent flush | `delta` `[{id, status:"preparing"}]` |
| +1.9 s | job | first yt-dlp progress line → `HotCell::write` | — (no frame) |
| +2.15 s | Hub | tick samples the cell | `delta` `[{id,status:"downloading",percent:0.4,speed:…,eta:…,downloaded_bytes:…}]` |
| … | Hub | one ~110 B delta every 250 ms | |
| +180 s | shim | `pp{MoveFiles,finished}` → `Status→Postprocessing`, `phase:"remux"` | urgent `delta` |
| +186 s | job | `Outcome::Finished`; `SetOutput{Sync}`; `Status→Finished` | |
| +186 s | Hub | urgent flush | `completed` (full item incl. `filename`, `download_url`, `size`, `files`) |
| +186 s | hooks | Jellyfin debounce armed; NFO skipped (ytdlp, no retained entry) | |

Client-visible latency from tap to a row on screen: **~25 ms** (legacy: 1.4–8 s, which is why the
whole background-upload stack exists). Total bytes over WS for a 3-minute download: `added`
~700 B + 720 deltas × ~110 B + `completed` ~800 B ≈ **80 KB**. Legacy for the same download:
~3600 broadcasts × ~2 KB ≈ **7 MB**.

### 17.2 Add a 500-item playlist

| t | Action | Wire | Cost |
|---|---|---|---|
| 0 | `POST api/v2/downloads` with a playlist URL | | |
| +0.8 ms | `202 {"id":"01J…G"}`; 1 row inserted (`Sync`) | | 1 txn, 1 KB |
| +25 ms | `added` — one provisional item, `status:"resolving"` | 1 frame, 700 B | |
| +2.8 s | shim streams `entries` chunk 1 (100) | | |
| +2.8 s | resolver → `AddItems{ replace: G, group: NewGroup{id:G,seq:G.seq}, items:[100] }` | | |
| +2.83 s | Hub flush: `added` with inline `removed{ids:[G],reason:"expanded"}` + the group + the **first 50** children (`AULOS_SNAPSHOT_GROUP_INLINE`) | 1 frame, ~14 KB | |
| +2.9 s–3.6 s | chunks 2–5; each is one `AddItems`, one store txn | 4 frames, ~380 B each (group aggregate deltas only — children of a non-inlined group are suppressed unless `watch`ed) | 5 txns total, ~180 KB written |
| +3.6 s | scheduler: 3 `Interactive`-priority slots taken by children 1–3; 497 wait | | |
| steady | per tick: 1 `GroupDelta` + 3 `ItemDelta` | ~1 frame, ~450 B / 250 ms ⇒ **1.8 KB/s** | |
| on expand | client sends `{"t":"watch","groups":["01J…G"]}` → server replies `added` with children in pages of 250 | 2 frames, ~120 KB | |

Comparison table for the same operation:

| Metric | Legacy | This design |
|---|---|---|
| HTTP response latency | 30 s – 4 min (synchronous extraction of 500 entries) | **0.8 ms** |
| Disk writes | ~500 full-file rewrites, ~63 MB, ~1000 fsyncs | **5 transactions, ~180 KB** |
| Socket frames during add | 500 `added` broadcasts of full objects (~1 MB/client) | **6 frames (~16 KB/client)** |
| Event-loop stalls | seconds at a time (sync `json.dump` + fsync) | none (store is a separate thread) |
| Downloads starved by extraction | yes (shared executor, spec §13.9) | no (separate resolver pool, BRIEF §10) |
| Steady-state WS rate | 60–90 full objects/s ≈ 180 KB/s | **1.8 KB/s** |

### 17.3 Cancel mid-download

| t | Actor | Action |
|---|---|---|
| 0 | iOS | `POST api/v2/items/01J…A/cancel` |
| +0.1 ms | api | look up `CancelHandle` in `DashMap`; `Mutation::Status{Canceled}` (urgent); `Remove` intent recorded |
| +0.2 ms | api | `204 No Content` |
| +0.2 ms | api | `handle.token.cancel()`; `kill_group(pgid, SIGTERM)` |
| +25 ms | Hub | flush | `removed {"ids":["01J…A"],"reason":"canceled"}` — the row disappears from the UI in one frame |
| +40 ms | shim | SIGTERM → `KeyboardInterrupt` → yt-dlp removes its `.part`/`.ytdl` files → exit 130 |
| +45 ms | job | `child.wait()` returns; token was set ⇒ `Outcome::Canceled`; delete every tracked tmpfile; **no hooks**, **no Jellyfin refresh** |
| +45 ms | job | `Detach{id}` (Hub stops sampling); slot permit dropped |
| +46 ms | sched | next `Ready` item spawned |
| +50 ms | store | `SetStatus{Canceled, Sync}` + `Remove` in one txn |
| +3.045 s | (only if the child ignored SIGTERM) | `SIGKILL` to the group |

Legacy for comparison: `proc.kill()` (SIGKILL) leaves ffmpeg/N_m3u8DL-RE grandchildren running
and writing into the download dir, and cleans only the single (often stale) `tmpfilename`.

Cancelling a `Queued` item: removed from the ready heap, one `removed` frame, no signals.
Cancelling a group: one `Mutation::Remove` with every non-terminal child id, one txn, one frame.

### 17.4 Reconnect with `?since=`

| t | Action |
|---|---|
| 0 | iOS backgrounds; socket closed by the OS. Client remembers `seq=90261`, `boot_id=01JBQ8YQ…` |
| +45 s | iOS foregrounds; `GET ws?since=90261&boot=01JBQ8YQ…` |
| +45 s | session: `ring.resume(90261, boot)` → `Resume::Merged{from:90261, to:90441}` (180 frames in the window) |
| +45 s | server sends `resume` (~120 B), then merged `added`(1) / `completed`(2) / `removed`(1) / `delta`(4 items, 1 group) — **~2.4 KB total** |
| +45 s | client applies and is exactly in sync; no `/history` fetch, no sort, no flicker |

If the client was away for 20 minutes, or the server restarted (`boot_id` mismatch, or `since`
below `ring.floor`), it gets a `snapshot` instead — the same code path as a first connect. This
deletes `SocketService.fetchInitialState`, the `HistoryResponse` decode on connect, and the
"Socket.IO `all` event is unreliable" workaround (client ref §6.1).

### 17.5 Server restart with in-flight downloads

| t | Action |
|---|---|
| 0 | `SIGTERM` (docker restart). `shutdown_token.cancel()` |
| +5 ms | axum stops accepting; open WS sockets closed with `1001 "server shutting down"` (clients schedule a reconnect instead of showing an error) |
| +10 ms | `kill_group(SIGTERM)` to all 3 running jobs, in parallel |
| +60 ms | jobs exit; yt-dlp leaves `.part` files; items are **left at `downloading`** in the DB (deliberately: the triage needs to know they were running) |
| +70 ms | Hub final flush; store drains its queue in one txn; `PRAGMA optimize`; POT killed |
| +120 ms | process exits |
| — | container restarts |
| +0 | config, store open, `user_version` check, no import (marker present) |
| +40 ms | state load: `SELECT … WHERE status != 'finished'` + the 500 most recent terminal items — **one query each**, ~6 ms for 1000 rows |
| +50 ms | triage: 3 × `downloading → queued{auto_start:true}`, `attempt+1`, `msg="resumed after restart"`; 0 × `resolving`; 486 × `queued` unchanged |
| +55 ms | `readyz` 200; scheduler starts exactly `MAX_CONCURRENT_DOWNLOADS` jobs (not all 489) |
| +60 ms | yt-dlp resumes each from its `.part` (HTTP range) — the user loses seconds, not the file |
| +1.5 s | first client reconnects with `since=90441`; `boot_id` differs ⇒ `snapshot` |

Legacy: `__import_queue` re-`add`ed **every** queued item at once with `auto_start=True`, before
any client connected (spec §13.12), so a 40-item queue meant 40 concurrent forks and 40
extractions competing on the same executor.

### 17.6 Subscription tick

| t | Action |
|---|---|
| 0 | Scheduler wakes at `next_check_at` for sub `S` (interval 60 min, jitter −4 min) |
| +0 | `Semaphore(4)` permit acquired; `spawn_check(S)` |
| +5 ms | `subs` frame: `{"t":"subs","seq":…,"upsert":[{…,"checking":true}]}` (the UI can show a spinner — legacy had no such signal) |
| +5 ms | `provider.resolve(url, ResolveRequest{flat:true, playlistend: SUBSCRIPTION_SCAN_PLAYLIST_END})` |
| +2.1 s | 50 entries; filter with `_is_media_entry`; `SELECT entry_id FROM subscription_seen WHERE sub_id=?` (indexed, ~0.2 ms) |
| +2.1 s | new = unseen ∪ already-seen-but-`is_live`; say 2 new |
| +2.2 s | 2 items created in **one** `InsertItems{Sync}` txn with `Source::Subscription{S}`, `Priority::Subscription`; `MarkSeen{Sync}` in the same batch; trim to `SUBSCRIPTION_MAX_SEEN_IDS` |
| +2.2 s | `last_checked = now`, `consecutive_failures = 0`, `error = null`, `next_check_at = now + 60 min ± 6 min` |
| +2.23 s | one urgent flush: `added` (2 items) **+** `subs` (1 upsert, `checking:false`) |
| on failure | `error = msg`, `consecutive_failures += 1`, `next_check_at = now + min(60 min × 2^n, 6 h) ± jitter`; `subs` frame carries the error |

Legacy did all of this sequentially on a 60 s global tick, rewrote a 50 000-element JSON array on
every check, and on failure left `last_checked` untouched — re-extracting a permanently broken
feed every 60 seconds forever (spec §13.17).

### 17.7 Telegram message with 3 URLs

| t | Action |
|---|---|
| 0 | Update arrives; `chat_id` checked against `TELEGRAM_ALLOWED_CHAT_IDS` |
| +1 ms | `URL_RE` extraction, `rstrip` of `.,;:!?)]}>'"`, dedupe preserving order → 3 URLs |
| +1 ms | count ≤ `TELEGRAM_MAX_URLS_PER_MESSAGE`; SSRF validation → 3 valid (a rejected one would be reported as `Ignored invalid links:`) |
| +2 ms | chat config loaded from `telegram_chat`, normalised to the v2 4-tuple |
| +3 ms | **one** `POST`-equivalent call into `QueueHandle::add_batch` → 3 ULIDs, `Source::Telegram{chat_id}`, `Priority::Interactive`, **one** store txn |
| +3 ms | bot replies `Queued 3 link(s) with current chat config.` |
| +25 ms | Hub flush: **one** `added` frame with 3 items (so a watching iOS client sees all three appear together) |
| +25 ms–3 s | 3 resolves (pool of 4 ⇒ all in parallel); 3 `delta`s coalesced into ~2 frames |
| +3 s | `ProgressEditor` sends the chat's live message; edits it at most every 3 s while any of the 3 is active, and only when the rendered text changed |
| terminal | final edit becomes the summary (✅/❌ per item, filenames, sizes) — no message spam |

Attribution survives a restart because `source_json` is persisted (legacy's contextvar did not).

### 17.8 `YTDL_OPTIONS_FILE` edit

| t | Action |
|---|---|
| 0 | Operator saves `/config/ytdl.json` (editors typically write-to-temp + rename ⇒ a `Remove`+`Create` pair) |
| +0 | `notify` (inotify) event; `notify-debouncer-full` coalesces the pair over a 200 ms window and re-resolves the path (handles the rename-over case that a naive string compare misses) |
| +200 ms | read + `serde_json` parse; must be a JSON object |
| +201 ms | on success: `merged = YTDL_OPTIONS ∪ file ∪ runtime_overrides`; `Live.ytdl_options.store(Arc::new(merged))`; `Live.ytdl_state.store({ok:true, msg:"", mtime})` |
| +201 ms | `Mutation::ConfigChanged` (urgent) |
| +225 ms | `config` frame: `{"t":"config","seq":…,"ytdl_options":{"ok":true,"msg":"","mtime":1767226001,"source":"file"}}` |
| — | on failure: the previous `Arc` is **kept** (the server never runs with a broken option set) and the frame carries `{"ok":false,"msg":"YTDL_OPTIONS_FILE contents is invalid"}` — same message strings as legacy |
| — | in-flight jobs are unaffected (their options were merged at spawn); **queued** items pick up the new options when they start |

The last row is a deliberate change from legacy, which froze options at *add* time so an edit
never reached an already-queued item (§19.6).

Presets files are also watched now (legacy's README claimed this but the code did not — spec
§1.3), and so is `AULOS_PLUGINS_DIR` for hot plugin add/remove.

---

## 18. Legacy behaviour map (spec §1–§12 → this design)

Legend: **=** identical behaviour · **+** preserved and extended · **Δ** deliberately changed (see §19).

### §1 Config

| Legacy behaviour | Lives in | ? |
|---|---|---|
| `_DEFAULTS` table, all 60 keys, string-first | `aulos-core::config` (`env_table!`) | = |
| `%%KEY` indirection | `config::resolve_indirection` | = |
| `_BOOLEAN` token set, exit(1) on a bad token | `config::BoolToken` | = |
| `URL_PREFIX` trailing slash; `PUBLIC_HOST_*` only when non-empty | `config::normalise` | = |
| `YTDL_OPTIONS` / presets JSON validation, exit(1) | `config::YtdlOptions::parse` | = |
| `YTDL_OPTIONS_FILE` merged **over** env | `provider-ytdlp::opts::layer` | = |
| `watchfiles` hot reload of the options file, `samefile` check | `server::ConfigWatcher` (`notify` + debouncer) | + (also presets, also plugins) |
| `set_runtime_override('cookiefile')` incl. boot autodetect of `STATE_DIR/cookies.txt` | `store::runtime_override` + `Live.ytdl_options` | = |
| `frontend_safe()` 8-key `configuration` payload | `snapshot.config` (§5.2), typed not string-typed | Δ (types) |
| `dampenThirdPartyLoggers` | `server::log` | = |
| `DEBUG` flips yt-dlp `quiet/verbose` | `Job.debug` → shim `opts` | = |
| `JELLYFIN_LIBRARY_ID` / refresh modes silently ignored | `hooks::jellyfin` — now honoured | Δ |
| `SC_THREAD_COUNT`/`SC_USE_FFMPEG` re-read from `os.environ` in the child | read from `Config` once | Δ (bugfix) |

### §2 REST

| Legacy | Lives in | ? |
|---|---|---|
| `POST add` incl. `_migrate_legacy_request` and all validation | `api::v1::add` → `api::v2::downloads` | = |
| `GET history` `{done,queue,pending}` flat arrays | `api::v1::history` (projection of the published snapshot) | = |
| `POST delete` (`where=queue\|done`), `POST start` | `api::v1` → `items/actions` | = (ids matched by id **or** url) |
| `GET presets`, `POST cancel-add`, cookie routes | `api::v1` + `api::v2` | = / Δ (`cancel-add` semantics) |
| `GET version` | `api::v1::version` | + (`url_prefix`, `boot_id`) |
| static `download/`, `audio_download/`, `show_index` | `api::static_files` (`ServeDir`) | + (ranges) |
| `robots.txt`, default body | `api::static_files` | = |
| `text/plain` JSON bodies | `application/json` everywhere | Δ |
| HTTP 200 + `{"status":"error"}` for business errors | v1 keeps it; v2 uses 4xx + envelope | = / Δ |
| CORS `on_response_prepare` | `mw::cors` (`CorsLayer`) | + (adds `Allow-Methods`) |
| `SO_REUSEPORT` probe, TLS from `CERTFILE`/`KEYFILE` | `server::serve` | = |
| aiohttp startup/cleanup ordering | `server::startup` (§14.1) / `shutdown` (§14.4) | + |

### §3 Socket.IO

| Legacy event | v2 equivalent | ? |
|---|---|---|
| `all` (`[[key,info]…]` pairs, double-encoded) | `snapshot` frame, flat objects, single-encoded | Δ |
| `added` | `added` frame (batched at 25 ms) | + |
| `updated` (one per progress hook, full object) | `delta` frame (250 ms, changed fields only) | Δ |
| `completed` | `completed` frame | = |
| `canceled` / `cleared` (bare url string) | `removed {ids, reason}` | Δ |
| `configuration`, `custom_dirs`, `ytdl_options_changed` | `snapshot.config` / `api/v2/dirs` / `config` frame | + |
| `subscriptions_all`, `subscription_{added,updated,removed}` | `snapshot` + `subs` frames | + |
| `formats` (consumed by iOS, **never emitted** by the server) | `snapshot.capabilities.formats` | Δ (now real) |
| no inbound events | `ping`/`ack`/`watch`/`unwatch` | + |

### §4 Download model

Every `DownloadInfo` field maps to `ItemView` (§3.3): `id`(→`media_id`, with a new ULID `id`),
`title`, `url`, `quality`, `download_type`, `codec`, `format`, `folder`, `custom_name_prefix`
(→`request_json`), `msg`, `percent`, `speed`, `eta`, `downloaded_bytes`, `total_bytes`,
`total_bytes_estimate`, `fragment_index`, `fragment_count`, `status`, `size`, `timestamp`(→`seq`
+ `created_at`), `error`, `entry`(→`entry_json`), `playlist_item_limit`, `split_by_chapters`,
`chapter_template`, `subtitle_language`, `subtitle_mode`, `ytdl_options_presets`,
`ytdl_options_overrides`, `subtitle_files`+`chapter_files`(→ unified `files[]`), `filename`.
Δ: `filename`/`files` are **always present** (`null`/`[]`) rather than lazily created (spec
§13.19); `subtitle_files` is now persisted (spec §13.20); status gains `canceled`, `resolving`,
`postprocessing`, and `pending` splits into `queued` + `auto_start`.

### §5 Queue mechanics

| Legacy | Lives in | ? |
|---|---|---|
| three `PersistentQueue`s (`queue`/`pending`/`completed`) | one `item` table + `status`/`auto_start` | Δ |
| `AtomicJsonStore`, schema_version 2, quarantine | `store::import` (read path only) | = |
| whole-file rewrite per put/delete | batched transactions (§8.4) | Δ |
| transient progress not persisted | **I5**, schema-enforced | = |
| `_compact_persisted_entry`, SC entries kept whole | `store::entry compaction` (§8.5) | = |
| global `Semaphore(MAX_CONCURRENT_DOWNLOADS)` | `Scheduler.global` | = |
| SC semaphore acquired *outside* the global one | `ProviderLimits{holds_global_slot:false}` | = |
| `__extract_info` incl. flat-extract ordering and the strict-retry heuristic | `provider-ytdlp::extract` | = |
| playlist/channel expansion, injected `playlist_index` (zero-padded), `playlist_count`, `playlist_autonumber`, `n_entries`, `__last_playlist_index`, copied parent props | `resolver` + `MediaEntry.tmpl_fields` | = |
| `_resolve_outtmpl_fields` via yt-dlp `evaluate_outtmpl`, `_sanitize_path_component` | shim helper `mode:"outtmpl"` (yt-dlp is the only correct evaluator) | = |
| `playlist_item_limit` applied twice (slice + `playlistend`) | `resolver` slice + `opts.playlistend` | = |
| `__calc_download_path` containment + `CREATE_CUSTOM_DIRS` | `core::paths::SafeRelPath` | + (component-wise, fixes §13.21) |
| `_post_download_cleanup` (delete tmp, force error, move to done, Jellyfin, `CLEAR_COMPLETED_AFTER`) | `JobTask::finish` + `HookDispatcher` + a `clear_after` timer | + (outside the slot) |
| `cancel_add` generation counter | `POST v1 cancel-add` = cancel recent `resolving` | Δ |
| `_canceled_urls` skip set | `CancelRegistry` + `canonical_key` | = |
| `get_custom_dirs()` 5 s memo, recursive glob **on the event loop** | `Live.custom_dirs` refreshed in `spawn_blocking` | Δ |
| `CLEAR_COMPLETED_AFTER` auto-clear | `Hub` timer → `removed{reason:"auto_cleared"}` | = |
| `DELETE_FILE_ON_TRASHCAN` | `?delete_files=` / v1 `where=done` | + (also deletes chapter/subtitle files, fixes §13.20) |

### §6 `dl_formats`

`get_format` (all 8 branches incl. `custom:`, `ios`, `best_remux`, codec filters, `vres/vfmt/afmt`)
and `get_opts` (audio/thumbnail/best_remux/captions branches, prepended vs late postprocessors,
`writethumbnail` guard, caption language lists, normalisation fallbacks) are a **line-by-line
port** in `aulos-core::formats`, with the legacy unit tests translated. **=**
One Δ: `preferredquality` is emitted as a number, not the string `"192"`.

### §7 Subscriptions

Data model, public projection, backfill suppression, `is_live` re-queue, `_is_media_entry`,
one-level recursion, `VIDEO_ONLY_MSG`, duplicate-URL rejection incl. the in-flight `_pending_urls`
guard, `SUBSCRIPTION_MAX_SEEN_IDS` trim, `folder:""→None`: all **=** in `aulos-subscriptions`.
Δ: per-sub scheduling with jitter + exponential backoff, bounded parallel checks, non-blocking
`check`, `next_check_at` persisted, `seen_ids` as a table, `update` returns 400 not 500.

### §8 Telegram bot

Commands, callback-data grammar, keyboards, allowed-chat gate, per-chat defaults and their
defaults, URL regex + rstrip + dedupe, `TELEGRAM_MAX_URLS_PER_MESSAGE`, SSRF guard, selection
normalisation, "Queued N link(s)" / "Some links failed" replies, stall and hard-timeout warnings
(once per chat): **=** in `aulos-telegram`.
Δ: live edited progress message (BRIEF §11), attribution via persisted `source`, watchdogs are
timers not a 15 s poll, and web/subscription downloads can also be watched.

### §9 StreamingCommunity

`can_extract` substring match, Inertia version, `_inertia_get` headers, embed→iframe→
`window.streams`/`masterPlaylist`/`token`/`expires`/`canPlayFHD`, watch/season/title dispatch,
entry shape incl. `_sc_needs_m3u8_extraction`/`_sc_base_url`, just-in-time `get_fresh_m3u8` with
Referer/Origin/UA/Cookie, N_m3u8DL-RE argv, ffmpeg retry, ANSI last-match-wins progress with all
four patterns, **gapless natural-order segment mux**, `_cleanup_streamingcommunity_partial`,
`<title>.mp4` + `.info.json` naming: **=** in `aulos-provider-sc`.
Δ: no diagnostic `GET` of the m3u8 (§13.27); parallel episode fetches; version cached with a TTL.

### §10 Jellyfin / NFO / audio sync

`refresh_jellyfin_library` semantics, header, error mapping, timeout parsing: **=**.
Δ: debounced (BRIEF §13), targeted when `JELLYFIN_LIBRARY_ID` is set.
NFO generator: **=** on output shape, Δ on being *wired in* and not deleting `.info.json`.
`audio_sync_fix`: **=** on the ffmpeg/ffprobe commands and the duration-scaled timeout, Δ on
running as a first-class hook with a visible `postprocessing` phase instead of an `Exec`
postprocessor at a hard-coded path.

### §11 BgUtils POT

Sidecar binary + yt-dlp plugin in site-packages: **=** (Dockerfile). Δ: supervised with
exponential backoff and a liveness probe, surfaced in `healthz` (§14.2), fixing spec §13.28.
`extractor_args` still flow through `YTDL_OPTIONS` untouched.

### §12 Process / deploy

Multi-stage Dockerfile with cargo-chef; `debian:bookworm-slim` runtime carrying python3 +
pip-pinned nightly yt-dlp + the plugins dir + deno + ffmpeg + N_m3u8DL-RE + bgutil-pot + tini +
gosu; multi-arch amd64/arm64; `PUID/PGID/UMASK/CHOWN_DIRS` entrypoint semantics; the four CI
workflows including the yt-dlp nightly bump PR automation: **=** (BRIEF §16).
Δ: no Node/Angular build stage; `HEALTHCHECK` hits `<URL_PREFIX>healthz`; the recursive `chown`
is skipped when the target dir already has the right owner (a `stat` on the root instead of a
multi-TB walk).

---

## 19. Intentional divergences (and why they are better for the user)

| # | Change | User-visible benefit |
|---|---|---|
| 1 | Progress is batched at 250 ms with changed fields only, instead of one full-object broadcast per hook | the queue list stops stuttering; ~120–500× less data; the client's throttle and its row-reordering bug disappear |
| 2 | `POST` returns `202` before extraction | share-sheet tap → row on screen in ~25 ms instead of 1.4–8 s; deletes ~900 lines of iOS background-upload machinery |
| 3 | Server-assigned ULID `id` is the only key; `url` is data | fixes the `id → url → UUID` fallback chain, the dual-key delete, and `clearCompleted` silently skipping items with no url |
| 4 | New `postprocessing` status + `phase` | a 4-minute remux shows "Post-processing · remux" instead of a bar frozen at 99.9 % |
| 5 | New `canceled` status and an explicit `removed{reason}` | a cancelled item vanishes deliberately instead of being dropped with a bare url string |
| 6 | yt-dlp options are merged at **spawn**, not at add | editing `YTDL_OPTIONS_FILE` now affects the 486 items still queued, which is what everyone expects |
| 7 | `Priority::Interactive` beats `Bulk` | pasting a link while a 500-item playlist is running starts *your* download next, not in 40 hours |
| 8 | Cancel = `SIGTERM` to the process group, then `SIGKILL` | no orphaned ffmpeg writing garbage into the library; partials actually cleaned |
| 9 | Per-subscription scheduling with backoff | a broken feed stops hammering the provider every 60 s; a healthy feed is checked on time instead of up to 60 s late |
| 10 | `POST subscriptions/check` returns immediately | the button stops hanging for minutes |
| 11 | Dedupe enforced by a partial unique index across all states | re-adding a pending URL no longer silently destroys the first entry |
| 12 | JSON everywhere with 4xx/401 and an error envelope | the client can trust status codes; "2xx with an HTML body means the session expired" heuristic goes away |
| 13 | `?since=` resume over WS and HTTP with `ETag` | foregrounding the app costs ~2 KB, not a socket teardown plus a full `/history` |
| 14 | Groups with server-computed aggregates | one row with real progress for a 500-item playlist, instead of 500 rows and no total |
| 15 | Completed items are paged from SQLite, memory holds a 500-item window | RSS stops growing with library size; `history` stays fast after 50 000 downloads |
| 16 | Jellyfin refresh debounced (and targeted) | a 20-episode season triggers 1 scan, not 20 |
| 17 | NFO generation wired in; `.info.json` retained | Jellyfin gets correct metadata for SC items without the user configuring an `Exec` postprocessor |
| 18 | `JELLYFIN_LIBRARY_ID` and the refresh-mode vars honoured | the documented compose file finally does what it says |
| 19 | POT sidecar supervised with a liveness probe | YouTube downloads stop silently failing with bot checks after the sidecar dies |
| 20 | Restart resumes through the scheduler, honouring `MAX_CONCURRENT_DOWNLOADS` | a restart no longer saturates the box with 40 simultaneous downloads |
| 21 | `subscriptions.extract_flat_playlist` option ordering aligned with `__extract_info` | a `YTDL_OPTIONS` entry can no longer break subscription scanning while leaving normal adds fine |
| 22 | Subscription `update` returns 400 on bad input | a typo returns a message instead of a 500 |
| 23 | `formats`/capability catalogue actually delivered | the share sheet can grow options without an app release |
| 24 | `download_url` + ranged file serving | open / stream / share a finished file — impossible today |

Behaviours deliberately **kept** even though they look like bugs, because changing them would
surprise existing users: `quality: "worst"` degenerating to a `best…` selector (spec §6.1);
`playlist_item_limit` applied both as a slice and as `playlistend`; SC ignoring `OUTPUT_TEMPLATE`;
the v1 shim's HTTP-200-with-`{"status":"error"}` convention; `CLEAR_COMPLETED_AFTER` semantics;
`CUSTOM_DIRS_EXCLUDE_REGEX` default.

---

## 20. Dependencies

Majors I am confident exist as of 2026-09. Where a minor is uncertain it is marked "pin latest at
implementation time"; CI runs `cargo deny check` (advisories + licences + duplicate versions).

### 20.1 Workspace-wide

| Crate | Ver | Why |
|---|---|---|
| `tokio` | 1 | the runtime; features `rt-multi-thread, macros, process, signal, fs, sync, time, io-util` |
| `tokio-util` | 0.7 | `CancellationToken`, `TaskTracker`, `codec` for the line readers |
| `futures-util` | 0.3 | `FuturesUnordered`, stream combinators for parallel SC episode fetches |
| `serde` | 1 | derive everywhere |
| `serde_json` | 1 | the wire format and yt-dlp option dicts; `RawValue` for pass-through option blobs |
| `thiserror` | 2 | library error types (BRIEF §18) |
| `anyhow` | 1 | binary-level errors only (BRIEF §18) |
| `tracing` | 0.1 | structured logs with request ids |
| `tracing-subscriber` | 0.3 | `env-filter`, `fmt`, optional `json`; third-party dampening |
| `bitflags` | 2 | `FieldMask` — the delta mask must be a real bitset, not a `HashSet` |
| `ulid` | 1 | `ItemId`/`GroupId`; time-sortable, 26-char, no coordination to mint |
| `arc-swap` | 1 | wait-free reads of `Published`, `YtdlOptions`, `Presets`, `CustomDirs` |
| `dashmap` | 6 | `CancelRegistry` and Telegram chat state — genuinely disjoint-key, read-mostly |
| `indexmap` | 2 | `Hub.items` (insertion order == `seq` order) and the dirty sets (`IndexSet` gives O(1) insert + stable iteration for the fairness cursor) |
| `smallvec` | 1 | `Hub.running` — no allocation for the common ≤32 running jobs |
| `bytes` | 1 | shared, refcounted frame buffers (`Utf8Bytes` under axum) |
| `parking_lot` | 0.12 | the few genuinely-sync mutexes (`StderrRing`); smaller and faster uncontended than `std` |
| `url` | 2 | URL parsing, host extraction, the SSRF guard, canonicalisation |
| `regex` | 1 | plugin match/progress patterns, ANSI stripping, Telegram URL extraction |
| `rand` | 0.9 | scheduler jitter, backoff jitter |
| `time` | 0.3 | `formatting`, `parsing`, `macros`; NFO `dateadded`, `upload_date` parsing. UTC only, so no tz database is needed (chosen over `chrono` for the smaller surface, over `jiff` for maturity) |
| `async-trait` | 0.1 | `dyn Provider` and `dyn Notifier` need dyn-compatible async methods |
| `metrics` | 0.24 | counters/histograms with no exporter coupling |
| `metrics-exporter-prometheus` | 0.16 | `<prefix>metrics`, gated by `AULOS_METRICS_ENABLED` |

### 20.2 Per crate

| Crate | Dep | Ver | Why |
|---|---|---|---|
| `aulos-store` | `rusqlite` | 0.3x, feature `bundled` | BRIEF §2; `bundled` removes the libsqlite3 system dependency and pins the SQLite version with the image |
| | `deadpool-sqlite` | 0.9 | 2-connection read pool so paged `history` never queues behind writes (hand-rolling this is ~80 lines if the version drifts) |
| | `tempfile` | 3 | atomic writes for the quarantine/import rename dance |
| `aulos-api` | `axum` | 0.8 | BRIEF §1; native `ws`, `Utf8Bytes` frames, typed extractors |
| | `tower` | 0.5 | `ConcurrencyLimitLayer`, `TimeoutLayer` |
| | `tower-http` | 0.6 | `TraceLayer`, `CorsLayer`, `SetRequestIdLayer`, `ServeDir` with ranged requests |
| | `axum-extra` | 0.10 | typed query/header extractors, `multipart` for the cookie upload |
| | `http` / `mime` | 1 / 0.3 | header and content-type constants |
| | `percent-encoding` | 2 | building `download_url` correctly |
| `aulos-provider` | `nix` | 0.29+ | `killpg`, `Pid` — process-**group** signalling is the whole point of §10.4 |
| | `toml` | 0.8 | `plugin.toml` |
| | `notify` + `notify-debouncer-full` | 8 / 0.5 | BRIEF §15 hot reload; the debouncer handles editor write-to-temp+rename, which a raw watcher gets wrong |
| `aulos-provider-ytdlp` | *(no extra)* | | it is process + NDJSON; `serde_json` covers it |
| `aulos-provider-sc` | `wreq` | 6.x (default feature `sc-impersonate`) | Chrome TLS/HTTP2 fingerprint impersonation — SC is behind Cloudflare and legacy relies on `curl_cffi impersonate="chrome"` |
| | `reqwest` | 0.12, `rustls-tls` | the compiled fallback when `sc-impersonate` is off, and the client used by Jellyfin/POT probes |
| | `scraper` | 0.23 | two CSS selectors on the Inertia page and the embed iframe (alt: `tl`, lighter, if build weight bites) |
| `aulos-telegram` | `teloxide` | 0.13+, features `macros`, `throttle` | BRIEF §11; `Throttle` handles Telegram's rate limits and `RetryAfter` |
| `aulos-hooks` | `quick-xml` | 0.37 | NFO writing with correct escaping (hand-rolled XML is how you get a corrupt library) |
| `aulos-server` | `clap` | 4 | `--import-only`, `--dry-run-import`, `--print-config`, `--check` |
| | `mimalloc` | 0.1, optional | `AULOS_ALLOC=mimalloc` build; measured, not assumed (§21.4) |

### 20.3 Dev-dependencies

| Crate | Ver | Why |
|---|---|---|
| `tokio-test` | 0.4 | deterministic time for the tick/urgent/backoff logic (`time::pause`) |
| `rstest` | 0.23 | table-driven ports of the legacy `dl_formats` and progress tests |
| `insta` | 1 | snapshot tests for **every** frame and endpoint JSON — the protocol becomes a reviewed artefact |
| `proptest` | 1 | **I4** (percent monotonicity) and the delta-merge fold (`merge(a,b,c) == apply(apply(apply(s,a),b),c)`) |
| `criterion` | 0.5 | benches for `Hub::apply`, `flush`, `sample_hot`, `merge_after`, store batch commit |
| `tempfile` | 3 | throwaway DBs and download roots |
| `wiremock` | 0.6 | Jellyfin, POT probe and SC HTML fixtures without network |
| `tungstenite` | 0.24 | a blocking WS client for the integration suite |

Deliberately **not** used: `figment`/`config` (BRIEF §15 requires byte-exact legacy env
semantics, which a generic loader fights); `sqlx` (compile-time-checked queries need a live DB in
CI and the async layer buys nothing over one dedicated thread); `socketioxide` (BRIEF §3);
`simd-json`/`sonic-rs` (frames are ~200–900 B; `serde_json` is not the bottleneck and the unsafe
surface is not worth it); `once_cell` (edition 2024 has `std::sync::LazyLock`); `im`/`rpds`
(persistent maps are slower here than copy-on-write of a `Vec<Arc<_>>`); `moka` (the two caches
are a 5 s memo and a 10 min TTL — `ArcSwap` + an instant is enough).

---

## 21. Performance budgets, benchmarks, observability

### 21.1 Budgets (CI-enforced where marked ✅)

| Path | Budget | Test |
|---|---|---|
| `POST api/v2/downloads` (single) p99 | < 3 ms | ✅ integration, `fake` provider |
| `POST api/v2/downloads` (batch of 50) p99 | < 15 ms | ✅ |
| `Hub::apply(Mutation::Status)` | < 200 ns | ✅ criterion |
| `Hub::sample_hot` with 32 running | < 3 µs | ✅ criterion |
| `Hub::flush` with 200 dirty items | < 400 µs | ✅ criterion |
| WS bytes/s per client, 3 downloads, steady | < 2 KB/s | ✅ integration, byte-counting client |
| WS frames/s per client, steady | ≤ 5 | ✅ |
| 500-child insert (one txn) | < 5 ms | ✅ criterion |
| `GET api/v2/items` with 1000 items | < 1 ms | ✅ |
| `GET api/v2/state?since=` merged, 512-frame window | < 2 ms | ✅ |
| Connect → snapshot delivered, 1000 items | < 20 ms | ✅ |
| Cancel → `removed` frame | < 30 ms | ✅ |
| Cancel → process group reaped | < 100 ms (SIGTERM path) | ✅ |
| RSS, 1000 queued + 3 downloading | < 90 MB | manual, tracked in the release notes |
| Boot → `readyz` with 5000 rows | < 250 ms | ✅ |

The integration suite uses the `fake` provider from BRIEF §17: a scripted timeline that can emit
50 items updating at 50–200 ms with a 15 % failure rate — deliberately the same shape as the iOS
app's own `StressTestService` (client ref §5), so the two sides are stressed identically.

### 21.2 Load scenarios in CI

| Scenario | Asserts |
|---|---|
| `stress_500_playlist` | frame count ≤ 25 during add; store txns ≤ 8; no download starvation (a job starts within 100 ms of a free slot) |
| `stress_50_concurrent` | `MAX_CONCURRENT_DOWNLOADS=50`; `sample_hot` p99 < 20 µs; frames/s ≤ 5; splitting kicks in and every item is emitted within 4 ticks (fairness) |
| `stress_slow_client` | a client reading at 1 frame/s gets `Lagged` → snapshot resync; the fast client's frame timing is unaffected (p99 jitter < 20 ms) |
| `stress_reconnect_storm` | 64 clients connect/disconnect for 60 s; no seq regression; snapshot p99 < 40 ms |
| `stress_kill_storm` | 200 add/cancel cycles; zero leaked processes (`/proc` scan), zero leftover `.part` files |
| `stress_restart` | 10 restarts with in-flight jobs; no duplicate items, no lost terminal states, `seq` strictly increasing across boots |

### 21.3 Metrics

`aulos_ws_clients`, `aulos_ws_frames_total{kind}`, `aulos_ws_bytes_total`,
`aulos_ws_lagged_frames_total`, `aulos_ws_slow_disconnects_total`, `aulos_ws_tick_backoff_ms`,
`aulos_hub_mutations_total{kind}`, `aulos_hub_flush_seconds` (histogram),
`aulos_hub_dirty_items` (gauge), `aulos_store_batch_size`, `aulos_store_commit_seconds`,
`aulos_store_queue_depth`, `aulos_jobs_active{provider}`, `aulos_jobs_total{provider,outcome}`,
`aulos_job_duration_seconds`, `aulos_resolve_seconds{provider}`, `aulos_resolve_queue_depth`,
`aulos_pot_restarts_total`, `aulos_pot_up`, `aulos_hook_total{hook,outcome}`,
`aulos_subscription_checks_total{outcome}`, `aulos_http_requests_total{route,status}`.

Every log line carries `request_id` (HTTP) or `item_id` (jobs); a job's span covers spawn →
terminal so one `grep item_id=01J…` yields the whole story.

### 21.4 Allocator

The steady-state allocation profile is small and uniform (`Arc<ItemView>` ~450 B, `Arc<str>`
titles, one frame buffer that is reused). Default glibc malloc is fine; the fragmentation risk is
the 500-item burst. `mimalloc` behind `AULOS_ALLOC` is available, and the decision is deferred to
a measurement against `stress_500_playlist` RSS-after-drain rather than taken on faith.

---

## 22. Risk register

| # | Risk | Likelihood | Impact | Mitigation | Trigger to re-plan |
|---|---|---|---|---|---|
| R1 | The Python shim's option dict semantics drift from `yt_dlp.YoutubeDL(params)` (e.g. a hook signature change in a nightly) | Med | High — downloads stop | The shim is thin and *only* forwards a dict; a golden test runs `mode:"extract"` against a fixture URL in CI on every yt-dlp bump PR; the bump workflow gates merge on it | Two consecutive bump PRs failing the golden test |
| R2 | `wreq`/BoringSSL breaks the arm64 Docker build | Med | Med — SC stops working on the VPS arch | `ScHttp` trait with a shipped `reqwest` fallback (§11.1); `healthz` reports `impersonating`; CI builds both feature combinations | Build time > 15 min or a failed arm64 build |
| R3 | SC's Inertia/embed scraping breaks (site redesign) | High | Med — SC only | The provider is isolated; failures are per-item `error`, never fatal; fixtures in `tests/fixtures/sc/` make the fix a 30-line diff; `Provider::health` probes `GET {base}/it` and `healthz` shows it red | Two breakages in a quarter ⇒ consider a `command` plugin so users can patch without a release |
| R4 | POT provider protocol change or repo disappearance | Low | High — YouTube stops | The sidecar is supervised and probed; the version is pinned in the Dockerfile; a `AULOS_POT_ENABLED=false` escape hatch exists; `extractor_args` remain user-overridable | Upstream archive notice |
| R5 | Delta protocol bug (a field changes but its mask bit is not set ⇒ a permanently stale value on the client) | Med | High — silently wrong UI | The mask is set in **one** place per field (a `set_field!` macro that writes the value *and* ORs the bit; direct field assignment is denied by a clippy `disallowed_methods` lint); `insta` snapshots of every frame; a `stress_consistency` test that reconstructs client state from the frame stream and asserts equality with the final snapshot every 5 s | any mismatch |
| R6 | `seq` monotonicity broken by a DB restore/rollback | Low | High — clients silently drop frames | `boot_id` in every snapshot; `?since=` beyond `current` ⇒ full snapshot; the hi/lo hwm only ever increases; a boot check refuses to start if `seq_hwm` < `max(item.seq)` and offers `--repair-seq` | any occurrence |
| R7 | A slow/hostile WS client degrades everyone | Low | Med | `broadcast` never blocks the Hub; per-client lag budget → close 1013; `AULOS_WS_MAX_CLIENTS`; **I6** covered by `stress_slow_client` | p99 frame jitter > 50 ms |
| R8 | Hub inbox saturation from an unexpected producer (e.g. a provider calling `status()` per progress line) | Med | Med — added latency | `status()` de-duplicates identical consecutive values; `aulos_hub_mutations_total{kind}` alerts; a debug-build assertion fires if any item exceeds 20 status mutations/s | metric breach |
| R9 | SQLite corruption or WAL growth on a full disk | Low | High | `PRAGMA wal_autocheckpoint`; `healthz` reports `wal_bytes` and flips to 503 above 256 MB; a startup `PRAGMA quick_check`; the DB is *derivable* — a corrupt DB can be deleted and the legacy importer or an empty state is a valid recovery | any `SQLITE_CORRUPT` |
| R10 | Losing up to `AULOS_DB_FLUSH_MS` of non-critical writes on host power loss | Med | Low | `Sync` durability on adds, terminals, `MarkSeen` and removals; documented; `AULOS_DB_SYNCHRONOUS=FULL` available | user complaint |
| R11 | The v1 shim diverges subtly and breaks the current iOS build mid-cutover | Med | High | The shim is generated from a table of legacy shapes and covered by `insta` snapshots captured from the **Python** server (a `tests/compat/` corpus recorded before cutover); the shim is behind `AULOS_V1_ENABLED` so it can be turned off deliberately, not accidentally | any snapshot mismatch |
| R12 | Process-group kill misses a grandchild that re-parents (e.g. a daemonising helper) | Low | Med — orphan writing to the library | `killpg` + `kill_on_drop` + `tini -g`; `stress_kill_storm` scans `/proc` for leaked children; a boot-time sweep kills any `bgutil-pot`/`N_m3u8DL-RE` older than the process | leak detected |
| R13 | `notify` misses a config-file change on a bind-mounted volume (some Docker/NFS setups deliver no inotify events) | Med | Low | `notify-debouncer-full` plus a 60 s mtime poll fallback; the `config` frame reports the effective `mtime` so the operator can see whether it took | operator report |
| R14 | Group aggregate drift (incremental sums diverge from reality after edge-case transitions) | Med | Low — cosmetic | Aggregates are recomputed from scratch every 60 s and compared in debug builds (`debug_assert_eq!`); a release-mode periodic recompute every 5 min corrects silently and increments `aulos_group_drift_total` | metric non-zero in production |
| R15 | Cross-compilation of `rusqlite bundled` + BoringSSL for arm64 is slow, making CI painful | High | Low | `cargo-chef` dependency layer caching; native arm64 runners if available, QEMU otherwise; the `sc-impersonate` feature can be dropped from the arm64 image if needed | CI > 30 min |
| R16 | The legacy import mangles a large `completed.json` | Med | Med — history loss | Import is one transaction, dry-runnable (`--dry-run-import`), never deletes the source (renames to `.imported.<ts>`), and quarantines bad files; a rollback to the Python image is a rename away | dry run mismatch |
| R17 | 25 ms urgent coalescing still produces too many frames on a pathological add (e.g. 5000 URLs pasted) | Low | Low | batch add is capped at `AULOS_MAX_BATCH_URLS` (default 500, 413 above); the resolver chunks at 100; frames are additionally capped by `AULOS_WS_MAX_DELTAS_PER_FRAME` | >30 frames in a single add |
| R18 | `teloxide` major bump churn (the bot is the least stable dependency surface) | Med | Low | The bot is one crate behind a `Notifier` trait; nothing else depends on it; `TELEGRAM_BOT_ENABLED=false` disables it entirely | breaking bump |
| R19 | Single-owner Hub becomes a bottleneck at an unforeseen scale | Low | Med | Budgets in §21.1 with criterion gates; the documented escape hatch is the dirty-bit variant (§4.4) and, beyond that, sharding the Hub by `group_id` with a merging publisher | `aulos_hub_flush_seconds` p99 > 2 ms |
| R20 | Scope: eleven crates is a lot of surface for a solo maintainer | High | Med | Milestones below deliver a *working* server at M3; `aulos-provider-sc`, `aulos-telegram`, `aulos-subscriptions` and the command plugins are independently disableable and independently testable; every crate has a single owner concept | M3 slipping by > 2 weeks |

### 22.1 Milestones

| M | Deliverable | Exit criterion |
|---|---|---|
| M0 | Workspace, CI (fmt/clippy/test), `aulos-core` types + config + formats port | legacy config and `dl_formats` tests translated and green |
| M1 | `aulos-store` (DDL, batching, seq, importer) | `--dry-run-import` on a copy of the real `STATE_DIR` reports zero errors |
| M2 | `aulos-queue` Hub + scheduler + `fake` provider; `aulos-api` v2 REST + WS | `stress_500_playlist`, `stress_slow_client`, budget suite green; a hand-written WS client renders progress |
| M3 | `aulos-provider-ytdlp` + shim + POT supervision + Dockerfile | real YouTube download end-to-end in the image; `healthz` green |
| M4 | v1 shim + `tests/compat` corpus | the **current** iOS build works against the Rust server for add/history/delete |
| M5 | `aulos-provider-sc` + `aulos-hooks` | SC season download + NFO + debounced Jellyfin refresh |
| M6 | `aulos-subscriptions` + `aulos-telegram` | subscription tick and the live Telegram progress message |
| M7 | Command plugins + example + docs (`PROTOCOL.md`) | a third-party plugin downloads a URL with no recompile |
| M8 | VPS cutover | side-by-side run on a second port, then swap; v1 shim retired after the iOS v2 build ships |

---

## 23. Open questions

1. **Auth for `ws`.** Authelia's session cookie is sent on the upgrade, so this works today — but
   does the VPS Authelia config allow the `Upgrade` header through for `<prefix>ws`? If not, the
   client needs a short-lived ticket (`POST api/v2/ws-ticket` → one-time token in the query
   string). Needs a check against the live proxy config before M2.
2. **Done-window size.** `AULOS_MEM_DONE_ITEMS=500` shapes the snapshot (~200 KB for 500 items).
   Should the snapshot default to `done=false` and let the client page history on demand, making a
   connect ~15 KB? I lean yes for the v2 client, `true` for the shim. Product call.
3. **Delta of `files[]`.** Chapter/subtitle arrays currently only ever grow, so a delta could send
   just the appended entries. Worth the protocol complexity, or always send the whole (usually
   ≤3-element) array? I have assumed whole-array.
4. **`percent` on `postprocessing`.** Should it stay at 100.0, drop to a separate
   `phase_percent`, or become a second progress track? ffmpeg can report real remux progress
   (`-progress pipe:1`), which would let the bar keep moving. Nice-to-have; not in M-scope.
5. **Retry policy.** `POST items/{id}/retry` is manual. Should transient errors (HTTP 5xx,
   `socket_timeout`) auto-retry with backoff up to N attempts? Legacy never did. I would add
   `AULOS_AUTO_RETRY=2` for classified-transient failures only.
6. **`wreq` vs a headless-browser fallback for SC.** If Cloudflare escalates to a JS challenge,
   TLS impersonation is not enough and the only answers are a `command` plugin shelling out to
   something heavier, or dropping SC. Which does the user prefer?
7. **APNs.** BRIEF leaves a `Notifier` hook. Does the app have (or will it get) a push
   entitlement and a token-registration endpoint? That decides whether a `device_token` table
   lands in the initial schema or in a migration.
8. **`CLEAR_COMPLETED_AFTER` vs the done window.** If the window evicts an item before its
   auto-clear timer fires, the timer must still delete the row (and optionally the file). Confirm
   that "auto-clear also applies to items no longer in memory" is the intended behaviour.
9. **Multi-user.** Everything above is single-tenant. If per-user queues are ever wanted, `Source`
   is the natural discriminator, but the Hub, snapshot and dedupe index would all need a tenant
   dimension. Confirm this is out of scope so the shortcuts stay taken deliberately.
