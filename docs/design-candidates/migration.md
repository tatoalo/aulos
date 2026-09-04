# Aulos Server — Architecture Proposal (migration / compatibility / operations lens)

Status: design candidate. Author lens: **migration, compatibility & operations first**.
Scope: the whole system, all crates in the BRIEF layout, deepest on the v1 shim, the legacy
importer, env/config semantics, Telegram parity, subscriptions, hooks, packaging, CI, cutover
and testing.

Reading order for implementers: §2 (topology) → §3 (crates) → §4 (domain) → §5 (store) →
§9 (protocol) → §10 (v1 shim) → §16 (sequences) → §17 (legacy behaviour map) → §18–§22 (ops).

---

## 1. Design theses

| # | Thesis | Consequence |
|---|---|---|
| T1 | The VPS `docker-compose.yml` must not need editing to cut over. | Every legacy env var name/default/semantic is preserved verbatim (§8). New knobs are `AULOS_*` and all have safe defaults. |
| T2 | Rollback must be free. | The importer **reads** the legacy JSON files and never mutates or deletes them. Downgrading to the Python image resumes from the untouched JSON (§20). |
| T3 | Progress is the hot path and must never touch disk, locks or the scheduler. | Progress flows on a dedicated mpsc into a single aggregator task, is coalesced at a fixed cadence, serialized **once** into `Bytes`, and broadcast (§7, §9). |
| T4 | One immutable ULID per item, everywhere. | `url` is data. The v1 shim is the only place that still resolves URL→id, and it does so by lookup (§10). |
| T5 | Everything that legacy did with a fork+pickle-queue, we do with one child process per job speaking a documented JSON-lines protocol. | `YTDL_OPTIONS`/presets/overrides stay Python option dicts; the POT plugin and nightly pin keep working unchanged (§6.2). |
| T6 | Operability is a feature. | `healthz` reports every component including the POT sidecar; the POT sidecar is supervised; logs are structured; the cutover has a written runbook and a rehearsal step (§14, §20). |
| T7 | Any legacy behaviour we change is listed, with a user-visible reason. | §17.13. Nothing is changed silently. |

---

## 2. Runtime topology

### 2.1 Process tree inside the container

```
PID 1  tini -g --
  └── /usr/local/bin/aulos-entrypoint            (sh: PUID/PGID/UMASK/CHOWN_DIRS, then exec)
        └── aulos-server                          (Rust, tokio multi-thread)
              ├── [supervised child] bgutil-pot server        (restart w/ backoff, §13.2)
              ├── [job child] python3 /app/python/ytdlp_runner.py   (one per yt-dlp job, own pgid)
              ├── [job child] N_m3u8DL-RE / ffmpeg                  (one per SC job, own pgid)
              ├── [job child] <command plugin>                      (one per plugin job, own pgid)
              └── [hook child] ffmpeg / ffprobe                     (audio-sync, short-lived)
```

`aulos-server` is PID-1-adjacent only; `tini -g` keeps process-group signal forwarding so a
`docker stop` reaps grandchildren even if we crash. Every job child is spawned with
`process_group(0)` so we can `kill(-pgid, …)` (§7.5).

### 2.2 Task / actor topology (inside `aulos-server`)

```
                        ┌──────────────────────────────────────────────┐
   HTTP/WS  ────────────►  aulos-api (axum)                            │
   (axum handlers)      │   • v2 REST   • v2 WS   • v1 shim  • healthz │
                        └───┬───────────────▲──────────────▲───────────┘
                            │ EngineCmd     │ ArcSwap read │ broadcast::Receiver<Arc<WsFrame>>
                            │ (mpsc 1024)   │ (snapshot)   │
                        ┌───▼───────────────┴──────────────┴───────────┐
                        │  QueueEngine  (single task, owns all queue   │
                        │  state: ready deque, slots, cancel tokens,   │
                        │  group counters, retry policy)               │
                        └─┬────────┬─────────────┬─────────────┬───────┘
        StoreCmd (mpsc)   │        │ spawn       │ spawn       │ DomainEvent (mpsc 4096)
                          │        │ resolve job │ download job│
              ┌───────────▼──┐  ┌──▼──────────┐ ┌▼───────────┐ ┌──────▼──────────────┐
              │ Store actor  │  │ ResolvePool │ │ RunPool    │ │ EventHub            │
              │ 1 writer thr │  │ Semaphore(4)│ │ Semaphore  │ │ • assigns frame seq │
              │ + N readers  │  │             │ │ global+per │ │ • serializes once   │
              └──────────────┘  └─────┬───────┘ └──┬─────────┘ │ • broadcast(1024)   │
                                      │            │           │ • 60 s replay ring  │
                                      │  ProgressMsg (mpsc 8192, try_send)           │
                                      │            │           └──────▲──────────────┘
                                      │       ┌────▼────────────────┐ │ FlushBatch
                                      └──────►│ ProgressAggregator  ├─┘
                                              │ • HashMap<Id,Cell>  │
                                              │ • interval(250 ms)  │
                                              │ • ArcSwap snapshot  │
                                              └─────────────────────┘

   Side actors, all fed from DomainEvent fan-out (a small `Router` task):
     HookDispatcher (jellyfin debounce, nfo, audio-sync)   §12
     TelegramActor  (live progress edit, rate limiter)     §11
     SubscriptionScheduler (per-sub timers, backoff)       §13
     ClearScheduler (CLEAR_COMPLETED_AFTER, persisted)     §7.7
     PotSupervisor  (bgutil-pot child)                     §14.2
     ConfigWatcher  (YTDL_OPTIONS_FILE / presets file)     §8.4
```

Design rules for this topology:

| Rule | Why |
|---|---|
| The `QueueEngine` is a **single task with owned state**, no `Mutex`. All mutation is a message. | Deterministic, unit-testable with a fake clock; no lock ordering bugs; cancel/start/complete races become message ordering. |
| Progress **never** enters the `QueueEngine`. | A 500-item playlist at 20 msg/s/item cannot starve `POST /downloads`. |
| Stage transitions (`preparing→downloading→postprocessing→finished`) **do** enter the engine (they are persisted). | Restart consistency; they are ≤ 6 messages per job. |
| `DomainEvent` is the single fan-out point for anything non-queue (hooks, telegram, subscriptions, WS). | Adding a `notifier` (APNs) later = one more subscriber, zero engine changes. |
| The store is an actor with **one** writer thread and a read pool. | SQLite WAL: one writer, many readers. No `Send`-across-await of `Connection`. |

### 2.3 Backpressure and drop policy

| Channel | Capacity | Full behaviour |
|---|---|---|
| `EngineCmd` | 1024 | `send().await` — API handler awaits (bounded by axum's own concurrency); never dropped. |
| `ProgressMsg::Progress` | 8192 | `try_send`, **drop on full** and increment `aulos_progress_dropped_total`. Losing a percent tick is invisible; blocking a downloader is not. |
| `ProgressMsg::Stage` | same channel | `send().await` — stage changes are never dropped. |
| `DomainEvent` | 4096 | `send().await`. Subscribers must be non-blocking (each has its own inbox). |
| `broadcast::<Arc<WsFrame>>` | 1024 | Lagged receivers get `RecvError::Lagged(n)`; that connection is sent a fresh `snapshot` and continues. This is the WS self-heal. |

---

## 3. Crates

Workspace `resolver = "3"`, edition 2024, `rust-version = "1.95"`. `[workspace.lints]` sets
`unwrap_used = "deny"`, `expect_used = "warn"`, `clippy::pedantic` selectively.

| Crate | Depends on | Modules |
|---|---|---|
| `aulos-core` | serde, thiserror, ulid, url, time | `id` (`ItemId`, `GroupId`, `SubId`, `Seq`), `status`, `item` (`Item`, `ItemView`, `Group`), `request` (`DownloadRequest`, `Selection`), `source` (attribution), `event` (`DomainEvent`), `config` (`Config`, `RawEnv`, parse/validate), `ytdl_options` (layering + `ArcSwap` holder), `paths` (containment, sanitisation), `error`, `clock` (`Clock` trait + `FakeClock`), `progress` (`ProgressCell`, `percent`) |
| `aulos-store` | aulos-core, rusqlite(bundled), rusqlite_migration, serde_json, tokio | `lib` (`Store` handle), `actor` (writer thread), `readers` (read pool), `schema` (DDL + migrations), `items`, `subscriptions`, `telegram`, `kv`, `import` (**legacy JSON importer**), `import/legacy_model` (serde types for schema_version 1 & 2) |
| `aulos-provider` | aulos-core, tokio, async-trait, serde | `provider` (trait), `entry` (`MediaEntry`), `sink` (`ProgressSink`), `registry`, `outcome`, `proc` (spawn helpers, pgid kill, line reader), `command` (`command` plugin: `plugin.toml` model, discovery, regex/json_lines progress), `fake` (feature `fake`: scripted timeline provider) |
| `aulos-provider-ytdlp` | aulos-provider, serde_json | `lib`, `formats` (port of `dl_formats.get_format`), `opts` (port of `get_opts`), `runner` (child protocol client), `progress` (port of `_calculate_progress_percent`), `outtmpl` (playlist/channel pre-resolution), `python/ytdlp_runner.py` (shipped asset) |
| `aulos-provider-sc` | aulos-provider, wreq\|reqwest, scraper, serde_json | `lib`, `http` (`ScHttp` trait: impersonating + plain impls), `inertia`, `watch`, `season`, `embed` (`window.streams`, token/expires), `jit` (just-in-time m3u8), `nm3u8dl`, `ffmpeg`, `mux` (natural-order gapless concat), `progress` (ANSI frame parsing) |
| `aulos-queue` | aulos-core, aulos-store, aulos-provider, tokio | `engine`, `cmd`, `slots`, `resolve`, `run`, `cancel`, `recovery` (boot reconciliation), `groups`, `clear`, `progress_agg`, `hub` (EventHub + replay ring) |
| `aulos-api` | aulos-queue, axum, tower-http | `lib` (router), `v2/state`, `v2/downloads`, `v2/items`, `v2/subscriptions`, `v2/capabilities`, `v2/cookies`, `v2/dirs`, `v2/ytdl_options`, `ws` (frames, connection task), `v1` (**shim**: `add`, `history`, `delete`, `start`, `version`, `subscribe`, `subscriptions*`, `presets`, `cancel_add`, cookies), `files` (download/audio_download + optional index), `health`, `error` (envelope), `cors`, `trace` |
| `aulos-telegram` | aulos-core, teloxide, governor | `bot`, `commands`, `config_ui` (inline keyboards), `urls` (extraction + SSRF guard), `watch` (per-job live message), `render` (progress text), `limiter` (token buckets), `store` (per-chat config via `aulos-store`) |
| `aulos-subscriptions` | aulos-core, aulos-store, aulos-provider | `manager`, `scheduler` (per-sub timer + jitter + backoff), `check`, `detect` (`is_media_entry`, ids), `model`, `public` (wire projection) |
| `aulos-hooks` | aulos-core, reqwest, quick-xml | `dispatcher`, `jellyfin` (debounce + targeted refresh), `nfo`, `audio_sync`, `ffprobe` |
| `aulos-server` | everything | `main`, `wiring`, `pot` (supervisor), `signals`, `bootstrap` (import, recovery), `cli` (`serve`, `import`, `check-config`, `doctor`, `print-schema`) |

Dependency direction is strictly downward; `aulos-api` never sees `rusqlite`, `aulos-queue`
never sees `axum`. `aulos-core` has no async runtime dependency except `tokio::sync` types.

### 3.1 CLI surface of the binary (ops matters)

| Command | Purpose |
|---|---|
| `aulos-server serve` (default) | Normal run. |
| `aulos-server check-config` | Parse env + `YTDL_OPTIONS*`, print the effective config table, exit 0/1. Used in CI and in the runbook pre-flight. |
| `aulos-server import --state-dir DIR --db PATH [--dry-run] [--force]` | Run the legacy importer standalone. `--dry-run` prints a report and writes nothing. |
| `aulos-server doctor` | Probe ffmpeg/ffprobe/N_m3u8DL-RE/deno/python/yt-dlp/bgutil-pot, print versions, exit non-zero if a required tool is missing. |
| `aulos-server print-schema` | Dump the SQLite DDL and the JSON Schema of the v2 wire types (used to generate docs and iOS models). |

---

## 4. Domain model (`aulos-core`)

```rust
// ---------- identity ----------
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(Ulid);           // Display/FromStr = 26-char Crockford base32
pub type GroupId = ItemId;         // a group IS an item row with kind = Group
#[serde(transparent)] pub struct SubId(Ulid);
#[serde(transparent)] pub struct Seq(pub u64);   // WS frame sequence, process-global
pub type Ord0 = i64;               // item creation order (DB `ord`), the client sort key

// ---------- status ----------
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued, Resolving, Preparing, Downloading, Postprocessing, Finished, Error, Canceled,
}
impl Status {
    pub fn is_terminal(self) -> bool;      // Finished | Error | Canceled
    pub fn is_active(self) -> bool;        // Preparing | Downloading | Postprocessing
    pub fn v1(self) -> &'static str;       // §10.4 legacy projection
}

// ---------- request ----------
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum DownloadType { Video, Audio, Captions, Thumbnail }
pub enum Codec { Auto, H264, H265, Av1, Vp9 }

#[derive(Clone, Serialize, Deserialize)]
pub struct Selection {                  // validated; exactly the legacy matrix (§17.2)
    pub download_type: DownloadType,
    pub codec: Codec,
    pub format: FormatId,               // newtype over String, validated per type
    pub quality: QualityId,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: Url,
    pub selection: Selection,
    pub folder: Option<RelDir>,             // validated, containment-checked at add time
    pub custom_name_prefix: String,         // no "..", no leading / or \
    pub playlist_item_limit: u32,           // 0 = unlimited
    pub auto_start: bool,
    pub split_by_chapters: bool,
    pub chapter_template: String,
    pub subtitle_language: SubtitleLang,    // ^[A-Za-z0-9][A-Za-z0-9-]{0,34}$
    pub subtitle_mode: SubtitleMode,
    pub ytdl_options_presets: Vec<String>,
    pub ytdl_options_overrides: serde_json::Map<String, Value>,
}

// ---------- attribution (replaces the legacy contextvar) ----------
#[derive(Clone, Serialize, Deserialize)]
pub enum Source {
    ApiV2 { request_id: Option<String> },
    ApiV1 { request_id: Option<String> },
    Telegram { chat_id: i64, message_id: i32 },
    Subscription { id: SubId, name: String },
    Restart,                                // re-queued by boot recovery
}

// ---------- the item ----------
pub enum Kind { Item, Group }

pub struct Item {                          // the persisted row, minus transient progress
    pub id: ItemId,
    pub kind: Kind,
    pub group: Option<GroupRef>,           // { id, index, count }
    pub ord: Ord0,
    pub url: Url,
    pub provider: Option<ProviderId>,
    pub media_id: Option<String>,          // provider's own id ("legacy id")
    pub title: String,
    pub status: Status,
    pub msg: Option<String>,               // human stage text / last provider message
    pub error: Option<String>,             // terminal error, already cleaned
    pub request: DownloadRequest,
    pub entry: Option<EntryBlob>,          // compacted provider entry (see §5.4)
    pub filename: Option<RelPath>,         // relative to the item's download root
    pub size: Option<u64>,
    pub chapter_files: Vec<FileRef>,
    pub subtitle_files: Vec<FileRef>,
    pub created_at: UnixMs, pub started_at: Option<UnixMs>, pub finished_at: Option<UnixMs>,
    pub attempt: u16,
    pub source: Source,
    pub clear_after: Option<UnixMs>,
}

// ---------- what goes on the wire (identical for REST + WS snapshot) ----------
#[derive(Serialize)]
pub struct ItemView {
    pub id: ItemId, pub kind: Kind, pub ord: i64,
    pub group_id: Option<ItemId>, pub group_index: Option<u32>,
    pub url: String, pub title: String, pub status: Status,
    pub provider: Option<String>,
    pub percent: f64,                      // always a number, 0..=100
    pub speed: Option<f64>,                // bytes/s
    pub eta: Option<i64>,                  // whole seconds
    pub downloaded_bytes: Option<u64>, pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>, pub fragment_count: Option<u32>,
    pub msg: Option<String>, pub error: Option<String>,
    pub filename: Option<String>, pub size: Option<u64>,
    pub download_url: Option<String>,      // PUBLIC_HOST_URL + filename, ready to open
    pub chapter_files: Vec<FileRef>, pub subtitle_files: Vec<FileRef>,
    pub selection: SelectionView,          // { download_type, codec, format, quality }
    pub folder: Option<String>,
    pub created_at: i64, pub started_at: Option<i64>, pub finished_at: Option<i64>,
    pub attempt: u16, pub source: SourceView,
    // groups only:
    pub children_total: Option<u32>, pub children_done: Option<u32>,
    pub children_error: Option<u32>, pub children_active: Option<u32>,
}
```

Invariants the whole system upholds (they are the answers to ios-client-reference §7.7/§7.8):

| Invariant | Enforcement |
|---|---|
| `percent` is always present and always a JSON number in `[0,100]`. | `ItemView::percent` is `f64`, not `Option`. `0.0` before any progress. Terminal `Finished` ⇒ `100.0`; `Error`/`Canceled` keep the last value. |
| `eta` is always integer seconds or `null`; `speed` always bytes/s `f64` or `null`. | Newtypes at the provider boundary; serialised with `serde_json::Number`. |
| `filename`, `chapter_files`, `subtitle_files` keys **always exist** (`null` / `[]`). | Fixed struct, no lazy attributes. Kills ios pain point #19. |
| `status` is exactly the 8-value closed set. | `#[serde(rename_all="lowercase")]` enum, no fallthrough. |
| Sort key is `ord` (monotonic, server-assigned, stable across restarts). | DB column; every list endpoint documents `ORDER BY ord`. Kills ios pain points #6/#10/#11. |
| An item's `id` never changes, from `POST` acknowledgement to deletion. | ULID minted before validation completes; playlists keep the parent id as the group id (§16.2). |

### 4.1 Progress cell (memory only, never persisted)

```rust
#[derive(Clone, Copy, Default)]
pub struct ProgressCell {
    pub percent: f64, pub speed: Option<f64>, pub eta: Option<i64>,
    pub downloaded_bytes: Option<u64>, pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>, pub fragment_count: Option<u32>,
    pub progress_source: SourceHash,   // hash of filename|tmpfilename, for the monotonic reset
    pub last_update: Instant,          // stall detection (Telegram, watchdog)
}
```

`percent` is computed by the exact port of `_calculate_progress_percent` (§17.5.4) including the
`[0.0, 99.9]` clamp, the monotonic-per-source rule, the fragment floor/ceiling bounding and the
"ignore the bogus 1 KiB/1 KiB HLS estimate" rule. Its unit tests are ported 1:1 from the Python
test suite so a divergence is caught mechanically.

---

## 5. Storage (`aulos-store`)

### 5.1 Handle and actor

```rust
pub struct Store { w: mpsc::Sender<WriteJob>, r: Arc<ReadPool>, metrics: Arc<StoreMetrics> }

enum WriteJob {
    Batch { ops: Vec<WriteOp>, ack: oneshot::Sender<Result<(), StoreError>> },
    Raw   { f: Box<dyn FnOnce(&mut Transaction<'_>) -> Result<(), StoreError> + Send>,
            ack: oneshot::Sender<Result<(), StoreError>> },
}

pub enum WriteOp {                      // typed so the actor can coalesce & audit
    InsertItem(Box<Item>),
    SetStatus { id: ItemId, status: Status, msg: Option<String>, error: Option<String>, at: UnixMs },
    SetResolved { id: ItemId, provider: ProviderId, media_id: Option<String>,
                  title: String, entry: Option<EntryBlob> },
    ConvertToGroup { id: ItemId, children_total: u32 },
    SetOutput { id: ItemId, filename: Option<RelPath>, size: Option<u64> },
    PushFile { id: ItemId, slot: FileSlot, file: FileRef },   // chapter | subtitle
    SetClearAfter { id: ItemId, at: Option<UnixMs> },
    DeleteItems(Vec<ItemId>),
    UpsertSubscription(Box<SubscriptionRecord>),
    MarkSeen { sub: SubId, ids: Vec<String>, at: UnixMs },
    PruneSeen { sub: SubId, keep: u32 },
    DeleteSubscriptions(Vec<SubId>),
    UpsertTelegramChat { chat_id: i64, config: ChatConfig },
    SetKv { key: String, value: Value },
}

impl Store {
    pub async fn write(&self, ops: Vec<WriteOp>) -> Result<(), StoreError>;   // one txn
    pub async fn read<T: Send + 'static>(
        &self, f: impl FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static
    ) -> Result<T, StoreError>;

    // typed reads used by the rest of the system
    pub async fn snapshot_items(&self, f: ItemFilter) -> Result<Vec<Item>, StoreError>;
    pub async fn item(&self, id: ItemId) -> Result<Option<Item>, StoreError>;
    pub async fn active_urls(&self) -> Result<HashSet<String>, StoreError>;   // dedupe (§17.5.4)
    pub async fn ids_for_url(&self, url: &str, scope: Scope) -> Result<Vec<ItemId>, StoreError>;
    pub async fn subscriptions(&self) -> Result<Vec<SubscriptionRecord>, StoreError>;
    pub async fn seen(&self, sub: SubId) -> Result<HashSet<String>, StoreError>;
}
```

- Writer: **one** dedicated OS thread (`std::thread::spawn`, not a tokio worker) holding a
  `rusqlite::Connection`. It drains up to 64 `WriteJob`s per loop iteration into a single
  transaction — this is what turns "500 playlist inserts" into ~8 transactions instead of 500
  fsyncs (legacy did 500 whole-file rewrites, §17.13-P5).
- Readers: `ReadPool` = `Semaphore(AULOS_DB_READERS, default 4)` + 4 threads each with a
  read-only `Connection` (`SQLITE_OPEN_READ_ONLY`). Reads never block writes (WAL).
- `busy_timeout = 5000`, `synchronous = NORMAL`, `journal_mode = WAL`, `wal_autocheckpoint = 512`,
  `foreign_keys = ON`, `temp_store = MEMORY`, `mmap_size = 64 MiB`.
- A `PRAGMA wal_checkpoint(TRUNCATE)` runs on graceful shutdown and every 6 h.
- `StoreError` is a `thiserror` enum; the API maps `Busy`/`Locked` to `503` with `Retry-After: 1`.

### 5.2 DDL

```sql
-- migration 0001_init.sql
PRAGMA journal_mode = WAL;

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
-- rows: schema_version, instance_id, created_at, imported_from, imported_at, ord_high_water

CREATE TABLE items (
  id                  TEXT    PRIMARY KEY,          -- ULID
  kind                TEXT    NOT NULL CHECK (kind IN ('item','group')),
  group_id            TEXT    REFERENCES items(id) ON DELETE CASCADE,
  group_index         INTEGER,
  ord                 INTEGER NOT NULL UNIQUE,      -- client sort key, monotonic
  url                 TEXT    NOT NULL,
  provider            TEXT,
  media_id            TEXT,
  title               TEXT    NOT NULL,
  status              TEXT    NOT NULL CHECK (status IN
                        ('queued','resolving','preparing','downloading',
                         'postprocessing','finished','error','canceled')),
  msg                 TEXT,
  error               TEXT,
  request_json        TEXT    NOT NULL,             -- serde(DownloadRequest)
  entry_json          TEXT,                         -- compacted provider entry
  filename            TEXT,
  size                INTEGER,
  chapter_files_json  TEXT    NOT NULL DEFAULT '[]',
  subtitle_files_json TEXT    NOT NULL DEFAULT '[]',
  source_json         TEXT    NOT NULL,
  attempt             INTEGER NOT NULL DEFAULT 0,
  children_total      INTEGER,                      -- groups only
  created_at          INTEGER NOT NULL,             -- unix ms
  started_at          INTEGER,
  finished_at         INTEGER,
  updated_at          INTEGER NOT NULL,
  clear_after         INTEGER                        -- unix ms or NULL
) STRICT;

CREATE INDEX items_status_ord   ON items(status, ord);
CREATE INDEX items_group        ON items(group_id, group_index);
CREATE INDEX items_url          ON items(url);
CREATE INDEX items_clear_after  ON items(clear_after) WHERE clear_after IS NOT NULL;
CREATE INDEX items_finished_at  ON items(finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE subscriptions (
  id                    TEXT PRIMARY KEY,
  name                  TEXT NOT NULL,
  url                   TEXT NOT NULL UNIQUE,
  enabled               INTEGER NOT NULL DEFAULT 1,
  check_interval_minutes INTEGER NOT NULL DEFAULT 60,
  request_json          TEXT NOT NULL,      -- the DownloadRequest template (sans url)
  last_checked          INTEGER,            -- unix ms
  last_success          INTEGER,
  next_due              INTEGER,            -- unix ms; scheduler's authority, persisted
  consecutive_failures  INTEGER NOT NULL DEFAULT 0,
  error                 TEXT,
  created_at            INTEGER NOT NULL,
  updated_at            INTEGER NOT NULL
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

Notes:

| Choice | Rationale |
|---|---|
| Groups live in `items` with `kind='group'` and `ON DELETE CASCADE` on children. | Deleting a playlist group deletes its children in one statement; a group is queryable and sortable with the same code path as an item. |
| `ord` is `NOT NULL UNIQUE`, sourced from an in-memory `AtomicI64` seeded from `meta.ord_high_water` (bumped in the same txn every 256 allocations). | Stable client sort key that survives restart, cheaper than `MAX(ord)+1` per insert. |
| `subscription_seen` is a table, not a JSON list. | Legacy rewrote a 50 000-element JSON array on every check (§17.13-P5). Here a check writes only the new ids, and `PruneSeen` is one `DELETE … WHERE seen_at < (…LIMIT 1 OFFSET :keep)`. |
| No `progress` columns at all. | Preserves the one good legacy property: progress never hits disk (§17.13-P6). |
| `STRICT` tables. | Catches type drift at write time; the importer's tests rely on it. |
| `next_due` persisted. | A restart does not reset the subscription schedule, and the backoff survives (§13.2). |

### 5.3 Migrations

`rusqlite_migration` with an embedded `Vec<M>`; `meta.schema_version` is also written for
human inspection. Rules: forward-only, additive; every migration has an up-only SQL file under
`crates/aulos-store/migrations/`; `aulos-server print-schema` renders the current DDL; a test
asserts that applying all migrations from scratch equals the checked-in `schema.sql` snapshot
(`insta` snapshot test).

### 5.4 Entry compaction

The provider entry blob is the one place with unbounded size. Rules ported from
`_compact_persisted_entry` (§17.5.2) and then tightened:

| Provider | What is kept | Where |
|---|---|---|
| `ytdlp`, non-playlist child | nothing (`entry_json = NULL`) | — |
| `ytdlp`, playlist/channel child | keys matching `^(playlist|channel)`, plus `n_entries`, `__last_playlist_index` | needed for `outtmpl` resolution on restart |
| `streamingcommunity` | the **whole** entry | needed for JIT m3u8 (`_sc_base_url`, `_sc_needs_m3u8_extraction`) and NFO metadata |
| `command` plugin | the plugin's declared `persist_fields` (default: whole entry, capped at 64 KiB) | plugin-defined |
| Any | hard cap `AULOS_ENTRY_MAX_BYTES` (default 256 KiB); over cap ⇒ store `{"__truncated":true}` + a warning | protects the DB from a pathological playlist |

Terminal items (`finished`) drop `entry_json` on the transition, matching legacy
(`_should_persist_entry() == identifier != "completed"`), **except** SC entries, which are kept
until the NFO hook has run and then dropped.

### 5.5 Legacy JSON importer (`aulos-store::import`)

Runs from `aulos-server bootstrap` **only when the SQLite DB file does not exist**, or on demand
via `aulos-server import`. It is the single most cutover-critical component, so it is specified
exhaustively.

#### 5.5.1 Inputs and detection

| File in `STATE_DIR` | Legacy `kind` | Target |
|---|---|---|
| `queue.json` | `persistent_queue:queue` | `items` with status derived (§5.5.3), `auto_start = true` |
| `pending.json` | `persistent_queue:pending` | `items` with `status='queued'`, `auto_start = false` |
| `completed.json` | `persistent_queue:completed` | `items` with terminal status |
| `subscriptions.json` | `subscriptions` | `subscriptions` + `subscription_seen` |
| `telegram_bot_config.json` | *(bare object, no envelope)* | `telegram_chats` |
| `cookies.txt` | — | recorded as `kv['cookiefile']` runtime override |
| `queue` / `pending` / `completed` / `subscriptions` (extensionless, `shelve`) | pickle | **not supported** (BRIEF out-of-scope). Detected and reported: "legacy shelf found at …; start the Python image once to have it migrate to JSON, then re-run the import." |

Envelope validation: `schema_version ∈ {1,2}` and `kind` must match the expected string.
A mismatch or malformed JSON ⇒ the file is **skipped with a loud error**, the import continues
with the remaining files, and the final report is non-empty. The importer **never** quarantines
or renames legacy files (unlike legacy's own loader) — T2/rollback.

`schema_version: 1` records go through `LegacyDownloadInfoV1 → LegacyDownloadInfoV2` which
re-implements `DownloadInfo.__setstate__`'s migrations:

| Legacy field state | Migration |
|---|---|
| `format ∈ {m4a,mp3,opus,wav,flac}` | `download_type=audio`, `codec=auto`, `format` unchanged |
| `format == "thumbnail"` | `download_type=thumbnail`, `format=jpg`, `quality=best` |
| `format == "captions"` | `download_type=captions`, `format = subtitle_format or "srt"`, `quality=best` |
| `quality == "best_ios"` | `download_type=video`, `format=ios`, `quality=best` |
| `quality == "audio"` | `download_type=audio`, `format=m4a`, `quality=best` |
| `ytdl_options_preset: str` | → `ytdl_options_presets: [str]` |
| missing `status` | `pending` |
| missing any post-v1 field | the documented default (§17.4) |

`{"__metube_bytes__": "<b64>"}` and `{"__metube_datetime__": "<iso>"}` wrappers are decoded to
`Vec<u8>` (base64) / RFC3339 strings when they appear inside `entry`.

#### 5.5.2 Identity assignment

Legacy keys items by `url`. The importer mints ULIDs, and the mapping is what makes the v1 shim
work for pre-existing items:

1. Records are ordered by legacy `timestamp` (nanoseconds; missing ⇒ file order), globally across
   `completed` → `pending` → `queue` so that `ord` reflects real chronology.
2. `ord` is assigned `0,1,2,…`; `meta.ord_high_water` is set past the end.
3. `id = Ulid::from_datetime(timestamp_as_systemtime)` so ULID lexicographic order matches `ord`
   (nice property for debugging; not relied on).
4. `media_id` ← legacy `id` field (the yt-dlp video id, possibly `"<prefix>.<id>"`).
   `url` ← legacy `url`. **Both** are indexed, because the v1 shim accepts either as a delete key.
5. Duplicate legacy `url` across files (possible: legacy dedupes only within `queue`) ⇒ keep the
   most advanced record (terminal beats active beats pending), log a `duplicate_url` warning, and
   record the discarded one in the report.

#### 5.5.3 Status mapping

| Legacy `status` | Source file | Imported status | Notes |
|---|---|---|---|
| `pending` | `pending.json` | `queued` (`auto_start=false`) | user must press start |
| `pending` | `queue.json` | `queued` (`auto_start=true`) | picked up by the scheduler, **not** all at once (§17.13-C1) |
| `preparing`, `downloading` | `queue.json` | `queued` (`auto_start=true`, `attempt+=1`, `msg="Restarted after upgrade"`) | the Python process is gone; the partial file is left for yt-dlp's own `.part` resume |
| `finished` | `completed.json` | `finished` | `filename`, `size`, `chapter_files` preserved |
| `error` | `completed.json` | `error` | `error`/`msg` preserved |
| anything else / absent | any | `queued` if in queue/pending, else `error` with `msg="Imported with unknown legacy status: <x>"` | never guesses `finished` |

`CLEAR_COMPLETED_AFTER` is applied at import: `clear_after = finished_at + N` where
`finished_at` is unknown for legacy rows, so `finished_at = timestamp` is used. If that is
already in the past the item is cleared by the first `ClearScheduler` tick — same net effect as
legacy (which lost the timer on restart entirely, so this is strictly better).

#### 5.5.4 Subscriptions import

```
subscriptions.json.items[*] →
  id                     ← record.id (a UUIDv4 string; kept verbatim as the SubId's string form*)
  name, url, enabled     ← as-is (url .trim()'d; duplicate url ⇒ keep first, warn)
  check_interval_minutes ← max(1, value)
  request_json           ← DownloadRequest built from the flat legacy fields; folder "" → None;
                           chapter_template "" → config default; ytdl_options_preset → presets
  last_checked           ← round(value * 1000) (epoch s → ms) or NULL
  next_due               ← last_checked + interval, or now + jitter(0..30 s) if NULL
  consecutive_failures   ← 0        (a fresh start; error text preserved for display)
  error                  ← as-is
  seen_ids[*]            → subscription_seen(media_id, seen_at = last_checked or now)
                            inserted newest-first, capped at SUBSCRIPTION_MAX_SEEN_IDS
```

\* `SubId` is `Ulid`-typed internally; legacy UUID ids do not parse as ULIDs, so `SubId` is
defined as `enum SubIdRepr { Ulid(Ulid), Legacy(String) }` behind an opaque newtype whose
`Display`/`FromStr` round-trip both forms. **This is deliberate**: existing subscription ids are
referenced by nothing but the server itself, but keeping them stable means the v1
`POST /subscriptions/update` calls that any script or bookmark holds keep working. Rejected
alternative: remint ULIDs and keep a `legacy_id` column — more code, same result, extra join.

#### 5.5.5 Telegram chat config import

`telegram_bot_config.json` is a bare `{"<chat_id>": { …13 keys… }}` object (no envelope).
Each value is parsed into `ChatConfig` with legacy `format`/`quality` normalised through the
same `_normalize_download_selection` port used by the bot (§11.3), so a stored
`{"format":"m4a"}` becomes `download_type=audio, format=m4a`. Unknown keys are dropped with a
debug log. A chat id that is not currently in `TELEGRAM_ALLOWED_CHAT_IDS` is still imported
(the allow-list is checked at message time, not at load time — matches legacy).

#### 5.5.6 Transaction, idempotence, report

- The entire import is **one SQLite transaction**. Any hard error ⇒ rollback, the DB file is
  deleted, and the process exits non-zero with the report. There is never a half-imported DB.
- After success, in the same txn: `meta.imported_from = <STATE_DIR>`, `meta.imported_at = now`,
  `meta.import_report = <json>`, and a marker file `<STATE_DIR>/.aulos-imported` is written
  (containing the report) so a second start with a deleted DB does not silently re-import stale
  JSON without a warning; `--force` overrides.
- The report is logged at INFO as a table and also served at `GET <prefix>api/v2/import-report`:

```json
{
  "imported_at": 1757000000000,
  "state_dir": "/downloads/.metube",
  "files": [
    {"file":"queue.json","schema_version":2,"records":3,"imported":3,"skipped":0},
    {"file":"pending.json","schema_version":2,"records":0,"imported":0,"skipped":0},
    {"file":"completed.json","schema_version":1,"records":412,"imported":411,"skipped":1},
    {"file":"subscriptions.json","schema_version":2,"records":7,"imported":7,"skipped":0},
    {"file":"telegram_bot_config.json","schema_version":null,"records":2,"imported":2,"skipped":0}
  ],
  "warnings": [
    {"code":"duplicate_url","detail":"https://youtu.be/x present in queue.json and completed.json; kept finished"},
    {"code":"unknown_status","detail":"completed.json[87] status=\"cancelled\" → error"}
  ],
  "errors": [],
  "seen_ids_imported": 3121,
  "items": {"queued": 3, "finished": 401, "error": 10, "canceled": 0}
}
```

- `--dry-run` runs the whole thing against an in-memory DB (`:memory:`) and prints the same
  report without touching disk. **This is the runbook's rehearsal step** (§20.2).

---

## 6. Provider system (`aulos-provider` + implementations)

### 6.1 Trait

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)] pub struct ProviderId(&'static str);
                                            // or Arc<str> for `command:<name>`

pub enum Match { No, Weak(u8), Strong(u8) }     // score 0..=255; ytdlp = Weak(1)

#[derive(Clone, Serialize, Deserialize)]
pub struct MediaEntry {
    pub media_id: String,
    pub title: String,
    pub url: Url,                    // canonical page url ("webpage_url or url")
    pub kind: EntryKind,             // Video | Playlist { entries: Vec<MediaEntry> } | Redirect { url }
    pub pre_error: Option<String>,   // e.g. "Live stream is scheduled to start at …"
    pub live: LiveStatus,            // NotLive | IsUpcoming { at: Option<i64> } | IsLive | WasLive
    pub raw: serde_json::Value,      // provider-native entry, compacted by §5.4 before storage
    pub hints: EntryHints,           // { playlist_index, playlist_count, channel_index, ext, duration, … }
}

pub struct ResolveCtx<'a> {
    pub request: &'a DownloadRequest,
    pub ytdl_options: Arc<YtdlOptions>,   // already layered: env → file → presets → overrides
    pub paths: &'a Paths,                 // download_dir, audio_dir, temp_dir, state_dir
    pub cancel: CancellationToken,
    pub deadline: Instant,
}

pub struct DownloadCtx<'a> {
    pub item_id: ItemId,
    pub entry: &'a MediaEntry,
    pub request: &'a DownloadRequest,
    pub ytdl_options: Arc<YtdlOptions>,
    pub out_dir: PathBuf,          // already resolved & containment-checked
    pub tmp_dir: PathBuf,
    pub outtmpl: OutTmpl,          // default + chapter, playlist/channel fields pre-resolved
    pub cancel: CancellationToken,
}

pub struct Outcome {
    pub filename: Option<RelPath>, pub size: Option<u64>,
    pub chapter_files: Vec<FileRef>, pub subtitle_files: Vec<FileRef>,
    pub entry_final: Option<serde_json::Value>,   // for NFO / hooks
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn matches(&self, url: &Url) -> Match;
    /// Metadata only. Must respect `ctx.cancel` and `ctx.deadline`.
    async fn resolve(&self, url: &Url, ctx: ResolveCtx<'_>) -> Result<Vec<MediaEntry>, ProviderError>;
    /// Performs the download. Must emit progress through `sink` and honour `ctx.cancel`.
    async fn download(&self, ctx: DownloadCtx<'_>, sink: ProgressSink)
        -> Result<Outcome, ProviderError>;
    /// Optional: a provider-specific concurrency cap acquired *instead of* the global slot.
    fn own_slots(&self) -> Option<usize> { None }
    /// Optional: readiness for `healthz`.
    async fn probe(&self) -> ProviderHealth { ProviderHealth::Ok }
}

pub struct ProgressSink { tx: mpsc::Sender<ProgressMsg>, id: ItemId }
impl ProgressSink {
    pub fn progress(&self, p: RawProgress);          // try_send, droppable
    pub async fn stage(&self, s: Stage, msg: Option<String>);   // awaited, never dropped
    pub async fn file(&self, slot: FileSlot, f: FileRef);
}
pub enum Stage { Preparing, Downloading, Postprocessing }
```

`Registry::pick(&Url) -> &dyn Provider`: highest `Match` score wins; ties broken by registration
order; `ytdlp` registers last with `Weak(1)` and is therefore the fallback. `command` plugins
register with `Strong(priority)` where `priority` defaults to `100`; `streamingcommunity`
registers `Strong(200)` so a plugin can deliberately outrank it with `priority = 201`.

`ProviderError` variants: `NotFound`, `Auth`, `GeoBlocked`, `Upstream(String)`,
`Timeout`, `Canceled`, `ToolMissing(&'static str)`, `Io(std::io::Error)`, `Protocol(String)`,
`Fatal(String)`. The engine's retry policy reads this enum (§7.6).

### 6.2 `aulos-provider-ytdlp` and the Python shim

Rust owns: option construction, process lifecycle, pgid kill, timeouts, progress normalisation,
`outtmpl` pre-resolution. Python owns: nothing but calling `yt_dlp`.

`python/ytdlp_runner.py` (~180 lines, no third-party imports beyond `yt_dlp`):

**stdin** — exactly one JSON object then EOF:

```json
{
  "protocol": 1,
  "job_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
  "mode": "download",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "options": {
    "quiet": true, "verbose": false, "no_color": true,
    "paths": {"home": "/downloads", "temp": "/downloads"},
    "outtmpl": {"default": "%(title)s.%(ext)s",
                "chapter": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s"},
    "format": "bestvideo[height<=1080][ext=mp4]+bestaudio[ext=m4a]/best[height<=1080][ext=mp4]",
    "socket_timeout": 30,
    "ignore_no_formats_error": true,
    "merge_output_format": "mp4",
    "postprocessors": [{"key": "FFmpegVideoConvertor", "preferedformat": "mp4"}],
    "impersonate": "chrome"
  },
  "coerce": {"impersonate": "ImpersonateTarget"},
  "emit": {"progress": true, "postprocessor": true, "info": false}
}
```

**stdout** — newline-delimited JSON, one object per line, flushed per line:

| `t` | Fields | Emitted when |
|---|---|---|
| `hello` | `yt_dlp`, `python`, `pid`, `plugins` (list of discovered `yt_dlp_plugins` names) | first line, always |
| `progress` | `status`, `downloaded_bytes`, `total_bytes`, `total_bytes_estimate`, `fragment_index`, `fragment_count`, `speed`, `eta`, `filename`, `tmpfilename`, `msg` | every `progress_hooks` call (exactly the legacy key allow-list, §17.5.3) |
| `pp` | `postprocessor`, `status`, `filepath`, `finaldir`, `subtitle_filepaths[]`, `chapter_filepaths[]` | every `postprocessor_hooks` call; `MoveFiles`/`SplitChapters` shapes match legacy semantics |
| `info` | `entry` (the full info dict, `sanitize_info`'d) | `mode == "extract"` |
| `log` | `level` (`debug|info|warning|error`), `msg` | yt-dlp logger bridge (only `warning`/`error` unless `LOGLEVEL=DEBUG`) |
| `result` | `ok` (bool), `exit` (int), `error_kind`, `error_msg` | last line, always |

Rules:
- The shim installs a `logger` object so yt-dlp never writes to stdout directly; anything a
  plugin prints to stdout would corrupt the stream, so stdout is **duplicated to a devnull-backed
  fd** for the duration and only the shim's own `os.write(3, …)` on **fd 3** carries the protocol.
  Rust passes fd 3 as a pipe. Rationale: the BgUtils POT plugin and `yt-dlp-ejs`/deno subprocesses
  are known to print. Legacy did not have this problem because it used a pickled queue.
  Stderr is forwarded verbatim into `tracing` at DEBUG with `target = "ytdlp.child"`.
- `coerce` names the option keys whose string values the shim must convert to yt-dlp Python
  objects. Today: `impersonate → ImpersonateTarget.from_str`. Adding one later is a shim-only
  change. Unknown coercion name ⇒ `result.ok=false, error_kind="ProtocolError"`.
- `mode: "extract"` runs `YoutubeDL(opts).extract_info(url, download=False)`, then applies the
  **strict-retry rule** (`_type == "video"`, `formats == []`, has an id/url ⇒ retry with
  `extract_flat=False, ignore_no_formats_error=False`) so geo-blocks surface (§17.5.4).
  Rust could do the retry itself, but the retry must reuse the same `YoutubeDL` construction, so
  it stays in the shim.
- The shim exits 0 iff it emitted `result.ok = true`.
- Line length cap: Rust's reader uses `AsyncBufReadExt::read_until(b'\n')` with an 8 MiB cap
  (`info` frames for big playlists are large). Over cap ⇒ kill and report `Protocol`.

Rust side (`runner.rs`):

```rust
pub struct RunnerHandle { child: Child, pgid: Pid, proto: FramedRead<PipeFd3, JsonLines> }
pub async fn run_job(job: Job, sink: &ProgressSink, cancel: &CancellationToken)
    -> Result<Outcome, ProviderError>;
```
- Spawn: `Command::new(python3).arg(RUNNER_PATH)`, `.process_group(0)`, `stdin(piped)`,
  `stdout(null)`, `stderr(piped)`, fd 3 = pipe (via `CommandExt::pre_exec` + `dup2`, or
  `command_fds`), env inherited plus `PYTHONUNBUFFERED=1`, `PYTHONDONTWRITEBYTECODE=1`.
- `select!` over: fd-3 lines, stderr lines, `child.wait()`, `cancel.cancelled()`, and two timers
  (`AULOS_JOB_STALL_SECS` default 900 without any frame, `AULOS_JOB_TIMEOUT_SECS` default 0 = off).
- Cancel/timeout: `kill(-pgid, SIGTERM)`; if still alive after `AULOS_KILL_GRACE_MS` (default
  5000) `kill(-pgid, SIGKILL)`. This is the fix for orphaned ffmpeg grandchildren (§17.13-P10).

Format/opts construction is a **literal port** of `dl_formats.py`, table-driven, with the §17.6
tables encoded as `const` arrays and a property test asserting every
(`download_type`,`codec`,`format`,`quality`) tuple in the legacy allow-list produces the exact
legacy selector string. The Python selectors are checked into
`crates/aulos-provider-ytdlp/tests/golden/formats.json`, generated once from the legacy code.

### 6.3 `aulos-provider-sc` (StreamingCommunity)

HTTP client decision:

| Option | Verdict |
|---|---|
| `wreq` (the maintained rename of `rquest`; BoringSSL + Chrome JA3/JA4/HTTP2 fingerprint) | **Primary.** Legacy relies on `curl_cffi impersonate="chrome"`; vixcloud/CF fronting is fingerprint-sensitive, and losing it silently breaks all SC downloads. |
| plain `reqwest` (rustls) with hand-set Chrome headers | **Compiled-in fallback**, selected by `AULOS_SC_HTTP=plain`. Also the only client used in tests (no BoringSSL in the test matrix). |

Both sit behind `trait ScHttp { async fn get(&self, req: ScReq) -> Result<ScRes, ScError>; }`, so
the scraping logic is client-agnostic and unit-testable against `wiremock`. The default is
`impersonate`; if the `wreq` build is unavailable for a target (arm64 BoringSSL trouble), the
cargo feature `sc-impersonate` is off and the provider logs one WARN at boot naming the
degradation — it does not fail to start. `AULOS_SC_HTTP=auto|impersonate|plain` (default `auto`
= impersonate when compiled in).

Flow (ported 1:1 from §17.9, module per step):

| Step | Module | Notes |
|---|---|---|
| `matches`: hostname lower-cased **contains** `streamingcommunity` | `lib` | keeps mirror-domain behaviour; returns `Strong(200)` |
| Inertia version: `GET {base}/it`, parse `div#app[data-page]` JSON → `version` | `inertia` | cached in a `Mutex<Option<(String, Instant)>>` with a 30 min TTL (legacy cached per-instance forever) |
| `_inertia_get(path)` with `x-inertia: true`, `x-inertia-version` | `inertia` | 403/409 (version drift) ⇒ invalidate cache and retry once |
| `/watch/(\d+)(?:\?e=(\d+))?` → video entry | `watch` | entry shape identical to legacy incl. `_sc_needs_m3u8_extraction`, `_sc_base_url` |
| `/titles/(\d+)-([^/]+)/season-(\d+)` → playlist | `season` | **changed**: episodes are fetched with `buffer_unordered(AULOS_SC_META_CONCURRENCY, default 4)` instead of sequentially — a 20-episode season goes from ~60 serial round trips to ~15 parallel ones |
| `/titles/(\d+)-([^/]+)$` → movie delegate or all-seasons playlist | `lib` | same |
| JIT m3u8 at download time: embed page → first `<iframe>` → `window.streams`/`masterPlaylist` + `token`/`expires` (+`h=1` iff `canPlayFHD`) | `jit` | **changed**: the pure-debug `GET` of the m3u8 is removed (§17.13-P27) |
| N_m3u8DL-RE with the legacy argv | `nm3u8dl` | identical flags; ANSI-frame progress parser, last-match-wins |
| ffmpeg fallback on non-zero rc, after partial cleanup | `ffmpeg` | identical, incl. `-bsf:a aac_adtstoasc`, `-progress pipe:1` parsing |
| gapless natural-order segment concat + single-input ffmpeg remux | `mux` | identical; `natord` for the numeric-aware sort; **never** `-f concat` |
| output naming `<out_dir>/<sanitised title>.mp4` + `<…>.info.json` | `lib` | kept (see §17.13-K1) |

`own_slots() = Some(SC_MAX_CONCURRENT_DOWNLOADS)`.

### 6.4 `command` plugins (`aulos-provider::command`)

Discovery: at boot and on `SIGHUP`/`POST api/v2/plugins/reload`, scan `AULOS_PLUGINS_DIR`
(default `PLUGINS_DIR`, default `/config/plugins`) for `*/plugin.toml`. Invalid manifests are
logged and skipped; a plugin never prevents startup.

```toml
# /config/plugins/bandcamp/plugin.toml
name      = "bandcamp"
priority  = 120                      # Strong(120)
match     = ['(^|\.)bandcamp\.com$'] # regexes matched against the *host*
url_match = []                       # optional: regexes matched against the full URL

[resolve]                            # optional; omitted ⇒ a single synthetic entry from the URL
command      = ["./resolve.sh", "{url}"]
timeout_secs = 60
# stdout must be JSON: a MediaEntry-lite object or an array of them:
#   {"media_id":"…","title":"…","url":"…","kind":"video"}  |  {"kind":"playlist","entries":[…]}

[download]
command      = ["./download.sh", "{url}", "{out_dir}", "{out_name}", "{tmp_dir}"]
timeout_secs = 0                     # 0 = no hard timeout
# Placeholders: {url} {out_dir} {out_name} {tmp_dir} {media_id} {title}
#               {referer} {user_agent} {cookiefile} {quality} {format} {download_type}

[download.env]
BC_TOKEN = "${BC_TOKEN}"             # ${…} interpolates the server's own environment

[download.headers]
Referer = "https://bandcamp.com/"

[progress]
kind = "regex"                       # or "json_lines"
stream = "both"                      # stdout | stderr | both
# named groups, all optional: percent downloaded total speed eta status
pattern = '(?P<percent>\d+(?:\.\d+)?)%\s+(?P<downloaded>\d+)/(?P<total>\d+).*?(?P<speed>\d+(?:\.\d+)?)(?P<speed_unit>[KMG]?i?B)/s'
ansi_strip = true
last_match_wins = true               # for Spectre.Console-style repaints
min_interval_ms = 250

[output]
# how to find the produced file when the command doesn't say
glob = "{out_name}.*"
persist_fields = ["media_id", "title", "url"]
```

`json_lines` progress expects `{"percent":…,"downloaded":…,"total":…,"speed":…,"eta":…,"status":"downloading|finished|error","msg":"…"}`.
Speed/size units are parsed with a shared `humansize` parser (`KB/KiB/MB/MiB/GB/GiB`, 1024-based
to match N_m3u8DL-RE). `status: "error"` with `msg` becomes `ProviderError::Upstream(msg)`.

Security posture: plugin commands run as the server user, cwd = the plugin dir, `PATH` inherited,
`argv` built from a fixed template with **no shell** (`Command::new(argv[0]).args(&argv[1..])`);
placeholders are substituted as single argv elements, so a malicious title cannot inject
arguments. `{out_name}` is sanitised (`[<>:"/\\|?*]` → `_`, trimmed of `. `). The plugin dir is
expected to be operator-controlled; this is documented, and `AULOS_PLUGINS_ENABLED=false`
disables discovery entirely.

### 6.5 `fake` provider (test-only, feature `fake`)

```rust
pub struct FakeProvider { scripts: HashMap<String, Timeline> }
pub struct Timeline { pub steps: Vec<Step> }
pub enum Step {
    Wait(Duration),
    Stage(Stage),
    Progress { percent: f64, speed: Option<f64>, eta: Option<i64> },
    File { slot: FileSlot, name: String, size: u64 },
    Finish { filename: String, size: u64 },
    Fail(String),
    Hang,                    // to test stall/timeout/cancel
    ExpandPlaylist(usize),   // resolve() returns N children
}
```
Timelines are declared in TOML under `tests/fixtures/timelines/`; the URL host selects the
timeline (`fake://playlist-500`, `fake://slow`, `fake://fail-at-40`). It uses a `FakeClock` so a
"10-minute download" runs in microseconds. This is what makes the integration suite fast and
deterministic (§19).

---

## 7. Queue engine (`aulos-queue`)

### 7.1 Commands and events

```rust
pub enum EngineCmd {
    Add { requests: Vec<DownloadRequest>, source: Source,
          ack: oneshot::Sender<Result<Vec<ItemId>, AddError>> },
    Start   { ids: Vec<ItemId>, ack: Ack },      // pending → queued(auto_start)
    Cancel  { ids: Vec<ItemId>, ack: Ack },      // active or queued → canceled
    Retry   { ids: Vec<ItemId>, ack: Ack },      // error|canceled → queued, attempt += 1
    Delete  { ids: Vec<ItemId>, delete_files: Option<bool>, ack: Ack },
    CancelResolve { group_or_item: Vec<ItemId>, ack: Ack },   // v1 `cancel-add` (§10.3)
    // internal
    Resolved { id: ItemId, result: Result<Vec<MediaEntry>, ProviderError> },
    Stage    { id: ItemId, stage: Stage, msg: Option<String> },
    Finished { id: ItemId, outcome: Box<Outcome> },
    Failed   { id: ItemId, err: ProviderError },
    SlotFreed,
    Tick,                                        // 1 Hz: clear_after, stall watchdog
}

pub enum DomainEvent {
    Added(Vec<Arc<ItemView>>),
    StatusChanged { id: ItemId, from: Status, to: Status, view: Arc<ItemView> },
    Completed(Arc<ItemView>),          // terminal: finished | error | canceled
    Removed(Vec<ItemId>),
    GroupProgress { id: ItemId, counters: GroupCounters },
    SubscriptionChanged(Arc<SubscriptionView>),
    SubscriptionRemoved(SubId),
    YtdlOptionsReloaded { ok: bool, msg: String, update_time: Option<f64> },
    HealthChanged(Arc<HealthView>),
}
```

### 7.2 Engine state

```rust
struct Engine {
    store: Store,
    registry: Arc<Registry>,
    events: mpsc::Sender<DomainEvent>,
    progress_tx: mpsc::Sender<ProgressMsg>,
    cfg: Arc<Config>,
    ytdl: Arc<ArcSwap<YtdlOptions>>,

    ready: VecDeque<ItemId>,                       // FIFO by ord
    resolving: HashMap<ItemId, JoinHandle<()>>,
    running: HashMap<ItemId, RunSlot>,             // { handle, cancel, provider, started }
    cancel_tokens: HashMap<ItemId, CancellationToken>,
    groups: HashMap<GroupId, GroupCounters>,
    global: Arc<Semaphore>,                        // MAX_CONCURRENT_DOWNLOADS
    provider_slots: HashMap<ProviderId, Arc<Semaphore>>,
    resolve_slots: Arc<Semaphore>,                 // AULOS_RESOLVE_CONCURRENCY (4)
    dedupe: HashSet<String>,                       // active/queued urls
    add_generation: u64,                           // v1 cancel-add
    ord: AtomicI64,
}
```

### 7.3 Add path (async, per BRIEF §5)

`Add` does, **synchronously before acking**: per-request validation (§17.2 matrix), path
containment + optional `create_dir_all`, preset existence, overrides gating, dedupe against
`dedupe`, ULID mint, `ord` allocation, one batched `InsertItem` store write with
`status = resolving` (or `queued` if `auto_start = false`… see below), then `ack` with the ids and
publish `DomainEvent::Added`. Everything else is background.

`auto_start = false` items still resolve immediately (so the user sees a real title before
pressing start) but land in `queued` with `auto_start=false` and are not scheduled. Legacy
resolved before enqueueing too, so this is behaviour-compatible and strictly faster to ack.

`AddError` variants map to HTTP: `Validation{field,message}` → 400, `PresetUnknown` → 400,
`OverridesDisabled` → 400, `FolderOutsideBase` → 400, `FolderMissing` → 400,
`CustomDirsDisabled` → 400, `Duplicate{existing_id}` → **200/202 with the existing id** (see
§10.2 — legacy silently skipped duplicates, and the v1 shim must keep returning `status: ok`).

### 7.4 Resolution and playlist expansion

- Each `resolving` item spawns a task that acquires `resolve_slots`, calls
  `provider.resolve(...)`, and sends `EngineCmd::Resolved`.
- A resolution deadline of `AULOS_RESOLVE_TIMEOUT_SECS` (default 120) yields
  `ProviderError::Timeout` → item `error`.
- `Resolved(Ok(entries))`:
  - `entries.len() == 1 && kind == Video` ⇒ **same item id**: `SetResolved` (provider, media_id,
    title, entry), status → `queued`; if `auto_start` push to `ready` and try to schedule.
  - `kind == Playlist` ⇒ `ConvertToGroup { id, children_total }` and insert children in
    batches of 100 (`InsertItem` × 100 per transaction), each with `group_id = parent`,
    `group_index`, its own ULID and `ord`. Children are emitted as `DomainEvent::Added`
    **in batches** so a 500-item playlist produces ~5 `added` frames, not 500.
  - `kind == Redirect` ⇒ re-enter resolution once per item (recursion depth capped at
    `AULOS_RESOLVE_MAX_DEPTH`, default 3; legacy used an unbounded `already` set — the cap is a
    hardening, the `already` URL set is kept too).
  - `pre_error` on a child ⇒ child is inserted with `status = error, error = <text>`, matching
    legacy's "upcoming livestream" behaviour.
- `Resolved(Err(e))` ⇒ status `error`, `error = e.to_string()` (already prefix-cleaned, ios §7.12).
- **Group semantics**: a group row never downloads. Its `status` is derived and stored on each
  child transition: `resolving` while children are being created; `downloading` if any child is
  active; `queued` if any child is queued and none active; `finished` if all children finished;
  `error` if all terminal and ≥1 error; `canceled` if all terminal and ≥1 canceled and 0 errors.
  `GroupCounters { total, queued, active, finished, error, canceled }` and the derived
  `percent = mean(child percent)` are recomputed in memory and emitted at the delta cadence via
  `GroupProgress` — that is the "show progress for the whole group cheaply" requirement.

### 7.5 Scheduling, slots, cancel

```
schedule():
  while let Some(id) = peek(ready):
     item  = load(id)
     prov  = registry.by_id(item.provider)
     match prov.own_slots():
        Some(_) => permit = provider_slots[prov].try_acquire()          // NOT the global slot
        None    => permit = global.try_acquire()
     if permit.is_none() { break }                                       // head-of-line, FIFO
     pop(ready); spawn run_job(id, permit, cancel_token)
```
`own_slots` providers bypass the global semaphore exactly like legacy's `sc_semaphore`
(§17.5.3). Head-of-line blocking is intentional and matches legacy FIFO; an SC item at the head
does **not** block yt-dlp items, because the loop `break`s only when the *needed* pool is empty —
the implementation therefore scans up to `AULOS_SCHED_LOOKAHEAD` (default 32) ready items and
picks the first that can get a permit, so a saturated SC pool cannot starve YouTube downloads.

Cancel:

| Item state | Action |
|---|---|
| `resolving` | `cancel_token.cancel()`; the resolve task aborts; status → `canceled`. |
| `queued` | remove from `ready`; status → `canceled`. |
| `preparing`/`downloading`/`postprocessing` | `cancel_token.cancel()` → provider kills its pgid (SIGTERM, grace, SIGKILL) → the run task returns `ProviderError::Canceled` → status → `canceled`, partial files removed (`.part`, `.ytdl`, tmp dir, SC seg dir). |
| terminal | no-op, `ack` ok (idempotent). |
| group | recursively cancels non-terminal children; the group becomes `canceled`. |

Cancel is **idempotent and immediate at the API layer**: the status write and the `Completed`
event are emitted from the engine as soon as the token is cancelled and the child is confirmed
dead; the client sees `canceled` in the next delta (≤250 ms) rather than waiting for cleanup.

### 7.6 Retry policy

`Retry` is explicit (user or v1 `/start` on a failed item). Automatic retry is **narrow on
purpose**: only `ProviderError::Timeout` and `Upstream` with an HTTP 5xx/429 marker are
auto-retried, up to `AULOS_AUTO_RETRY_MAX` (default 2) with delay `30 s × 2^attempt` and ±20%
jitter. `Auth`, `GeoBlocked`, `NotFound`, `Fatal`, `ToolMissing`, `Canceled` are never retried.
This is new behaviour (legacy had none) and is why `attempt` is on the item and in `ItemView`.

### 7.7 Clear / delete / `CLEAR_COMPLETED_AFTER`

- On terminal `finished`/`error`, if `CLEAR_COMPLETED_AFTER > 0`, `clear_after = now + N` is
  **persisted**. The `ClearScheduler` (1 Hz `Tick`, plus a `sleep_until(min(clear_after))`
  fast path) deletes due items. Legacy lost the timer on restart; ours survives.
- `Delete { delete_files }`: `None` ⇒ use `DELETE_FILE_ON_TRASHCAN`. When deleting files we
  remove `filename`, **and** every `chapter_files`/`subtitle_files` entry, and the SC
  `.info.json` sibling — legacy orphaned all of those (§17.13-P20). Each unlink is best-effort
  with a WARN; the DB row is deleted regardless.
- Deleting a group deletes its children (`ON DELETE CASCADE`) and cancels any active child first.

### 7.8 Boot recovery (`recovery.rs`)

Runs after migrations/import, before the HTTP listener binds (so the first client sees a
consistent snapshot):

| Found status | Action | Reason |
|---|---|---|
| `resolving` | → `queued` (re-resolve on schedule), `msg = "Re-queued after restart"` | The resolve task is gone. |
| `preparing`, `downloading`, `postprocessing` | → `queued`, `attempt += 1`, `source = Restart` | Legacy restarted these *all at once*; we let the scheduler admit them `MAX_CONCURRENT_DOWNLOADS` at a time (§17.13-C1). |
| `queued` with `auto_start = true` | left as-is, pushed to `ready` ordered by `ord` | |
| `queued` with `auto_start = false` | left as-is, not scheduled | legacy `pending` |
| terminal | untouched; `clear_after` re-armed | |
| groups | counters recomputed from children in one `GROUP BY` query | |

Stale temp files: on boot, files in `TEMP_DIR` matching `*.part`/`*.ytdl` whose owning item no
longer exists are logged (not deleted) unless `AULOS_CLEAN_ORPHAN_TEMP=true`. Deleting user data
on boot by default is not acceptable.

### 7.9 Progress aggregator and the delta batcher

```rust
enum ProgressMsg {
    Progress { id: ItemId, raw: RawProgress },
    Stage    { id: ItemId, stage: Stage, msg: Option<String> },
    Forget(ItemId),
}

struct Aggregator {
    cells: HashMap<ItemId, ProgressCell>,
    dirty: HashSet<ItemId>,
    snapshot: Arc<ArcSwap<ProgressSnapshot>>,   // read lock-free by REST/WS connect
    hub: EventHub,
    interval: Interval,                          // AULOS_WS_BATCH_MS (default 250)
}
```
Loop: `select!` on `rx.recv()` and `interval.tick()`.
- `Progress` updates the cell via the ported percent algorithm; marks dirty.
- `Stage` forwards to the engine (persisted) **and** requests a *priority flush* — the batch is
  emitted immediately instead of waiting for the tick, so status changes feel instant while
  percent ticks stay at 4 Hz.
- On tick: build one `delta` frame from `dirty`, publish, clear `dirty`, and `store()` a new
  `ProgressSnapshot` into the `ArcSwap`.
- Items with no change produce no entry; an empty batch produces **no frame at all** (an idle
  server sends nothing — important for iOS battery).

### 7.10 EventHub, frame `seq`, and the replay ring

```rust
pub struct WsFrame { pub seq: Seq, pub kind: FrameKind, pub bytes: Bytes }  // pre-serialized
pub struct EventHub {
    seq: AtomicU64,
    tx: broadcast::Sender<Arc<WsFrame>>,          // capacity 1024
    ring: Mutex<VecDeque<Arc<WsFrame>>>,          // last AULOS_REPLAY_FRAMES (2048) or 120 s
}
impl EventHub {
    pub fn publish(&self, kind: FrameKind, body: impl Serialize) -> Seq;   // serializes once
    pub fn since(&self, since: Seq) -> Replay;   // Frames(Vec<Arc<WsFrame>>) | TooOld
}
```
Serialising once into `Bytes` and broadcasting `Arc<WsFrame>` means N connected clients cost N
`send`s of a refcount, not N JSON encodings — the direct fix for §17.13-P1/P2.
`GET api/v2/state?since=N` calls `since()`; `TooOld` ⇒ a full snapshot with `"full": true`.

---

## 8. Configuration (`aulos-core::config`)

### 8.1 Loading algorithm (byte-compatible with legacy §17.1)

```rust
pub struct RawEnv(BTreeMap<String, String>);          // every key defaulted, all values strings
pub struct Config { /* typed fields */ }
pub fn load(env: &RawEnv) -> Result<Config, Vec<ConfigError>>;
```

1. Start from the `DEFAULTS` table (§8.2) and overlay `std::env::vars()`. **All values are
   strings at this stage**, exactly like legacy.
2. `%%INDIRECTION`: a value starting with `%%` is replaced by the value of the named key
   (`AUDIO_DOWNLOAD_DIR=%%DOWNLOAD_DIR`, `TEMP_DIR=%%DOWNLOAD_DIR`). Resolution is iterative with
   a cycle check; a cycle or unknown target is a fatal config error (legacy would panic with
   `AttributeError` — we report it).
3. Booleans: the accepted token set is **exactly** `true|false|True|False|on|off|1|0`; truthy set
   is `{true, True, on, 1}`. Anything else ⇒ error `INVALID_BOOLEAN`. Same key list as legacy
   `_BOOLEAN`, plus the new `AULOS_*` booleans.
4. `URL_PREFIX`: append `/` if missing (so `""` → `"/"`); additionally **prepend** `/` if missing
   (legacy did not, and `URL_PREFIX=metube` produced routes like `metubeadd`; we normalise and
   log a WARN — an intentional, safe correction).
5. `PUBLIC_HOST_URL`, `PUBLIC_HOST_AUDIO_URL`: append `/` only when non-empty.
6. `YTDL_OPTIONS_FILE`, `YTDL_OPTIONS_PRESETS_FILE`: values starting with `.` are canonicalised
   to absolute paths (relative to cwd, as legacy did with `Path().resolve()`).
7. Integers/floats are parsed with the legacy leniency documented per row in §8.2
   (`CLEAR_COMPLETED_AFTER` invalid ⇒ log + 0; `JELLYFIN_SYNC_TIMEOUT_SECONDS` invalid ⇒ warn + 20;
   `PORT`/`MAX_CONCURRENT_DOWNLOADS` invalid ⇒ **fatal**, because legacy crashed on them anyway).
8. `YTDL_OPTIONS` + `YTDL_OPTIONS_FILE` and the presets pair are loaded (§8.3). Failure of either
   ⇒ **exit non-zero** with the exact legacy message strings.
9. All errors are collected and printed as a table, then `exit(2)` — legacy exited on the first.
   `aulos-server check-config` prints the effective config and exits 0/1 without binding a port.

### 8.2 Env var table (complete)

Legacy names, defaults and meanings are preserved. "Type" is the parse rule; "Δ" flags a
behaviour note.

| Env var | Default | Type | Used by | Δ |
|---|---|---|---|---|
| `DOWNLOAD_DIR` | `.` (image: `/downloads`) | path | `Paths`, `files` route | |
| `AUDIO_DOWNLOAD_DIR` | `%%DOWNLOAD_DIR` | path | audio items | |
| `TEMP_DIR` | `%%DOWNLOAD_DIR` | path | provider tmp | |
| `DOWNLOAD_DIRS_INDEXABLE` | `false` | bool | `files` route index | |
| `CUSTOM_DIRS` | `true` | bool | add validation, `custom-dirs` | |
| `CREATE_CUSTOM_DIRS` | `true` | bool | add validation | |
| `CUSTOM_DIRS_EXCLUDE_REGEX` | `(^\|/)[.@].*$` | regex (empty ⇒ none) | `custom-dirs` | invalid regex now fatal at boot instead of at first request |
| `DELETE_FILE_ON_TRASHCAN` | `false` | bool | delete | also deletes chapter/subtitle files (Δ) |
| `STATE_DIR` | `.` (image: `/downloads/.metube`) | path | importer input, `cookies.txt`, default DB dir | |
| `URL_PREFIX` | `''`→`/` | str | every route | leading `/` normalised (Δ) |
| `PUBLIC_HOST_URL` | `download/` | str | `download_url` field | |
| `PUBLIC_HOST_AUDIO_URL` | `audio_download/` | str | `download_url` field | |
| `OUTPUT_TEMPLATE` | `%(title)s.%(ext)s` | str | ytdlp outtmpl | |
| `OUTPUT_TEMPLATE_CHAPTER` | `%(title)s - %(section_number)02d - %(section_title)s.%(ext)s` | str | outtmpl chapter; default `chapter_template` | |
| `OUTPUT_TEMPLATE_PLAYLIST` | `%(playlist_title)s/%(title)s.%(ext)s` | str (empty ⇒ keep default) | playlist children | |
| `OUTPUT_TEMPLATE_CHANNEL` | `%(channel)s/%(title)s.%(ext)s` | str (empty ⇒ keep) | channel children | |
| `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` | `0` | int | add default; capabilities | emitted as a **number** in v2, as a **string** in the v1 shim (Δ, §10.5) |
| `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` | `60` | int (min) | subscribe default | same string/number split |
| `SUBSCRIPTION_SCAN_PLAYLIST_END` | `50` | int | sub extraction `playlistend` | |
| `SUBSCRIPTION_MAX_SEEN_IDS` | `50000` | int | `PruneSeen` | |
| `CLEAR_COMPLETED_AFTER` | `0` | int s (invalid ⇒ 0 + error log) | ClearScheduler | now survives restart (Δ) |
| `YTDL_OPTIONS` | `{}` | JSON object (else fatal) | option layering | |
| `YTDL_OPTIONS_FILE` | `''` | path | option layering, hot reload | |
| `YTDL_OPTIONS_PRESETS` | `{}` | JSON object of objects | presets | |
| `YTDL_OPTIONS_PRESETS_FILE` | `''` | path | presets | **now watched** (Δ, legacy README claimed it) |
| `ALLOW_YTDL_OPTIONS_OVERRIDES` | `false` | bool | add validation | |
| `CORS_ALLOWED_ORIGINS` | `''` | comma list, `*` = all | CORS layer | |
| `ROBOTS_TXT` | `''` | path | `robots.txt` | |
| `HOST` | `0.0.0.0` | str | bind | |
| `PORT` | `8081` | int (fatal) | bind | |
| `HTTPS` | `false` | bool | TLS | |
| `CERTFILE` / `KEYFILE` | `''` | path | TLS | PEM only; loaded via `rustls-pemfile` |
| `BASE_DIR` | `''` | path | `ROBOTS_TXT` resolution | UI serving dropped (Δ) |
| `DEFAULT_THEME` | `auto` | `light\|dark\|auto` | accepted + echoed in capabilities | no cookie is set (Δ) |
| `MAX_CONCURRENT_DOWNLOADS` | `3` | int ≥1 (fatal) | global slots | |
| `LOGLEVEL` | `INFO` | str (unknown ⇒ INFO + warn) | tracing filter | |
| `ENABLE_ACCESSLOG` | `false` | bool | request-log layer | |
| `SC_THREAD_COUNT` | `16` | int | `N_m3u8DL-RE --thread-count` | now read from `Config`, not re-read from env in a child (Δ) |
| `SC_USE_FFMPEG` | `false` | bool | SC path choice | single source of truth (Δ) |
| `SC_MAX_CONCURRENT_DOWNLOADS` | `1` | int ≥1 | SC provider slots | |
| `JELLYFIN_SYNC_ENABLED` | `false` | bool | hook | |
| `JELLYFIN_URL` | `''` | str (trailing `/` stripped) | hook | |
| `JELLYFIN_API_KEY` | `''` | secret str | hook | redacted in all logs and in `check-config` |
| `JELLYFIN_SYNC_TIMEOUT_SECONDS` | `20` | float (invalid ⇒ warn+20) | hook | |
| `JELLYFIN_LIBRARY_ID` | `''` | str | hook | **now implemented** (Δ) |
| `JELLYFIN_METADATA_REFRESH_MODE` | `Default` | `None\|ValidationOnly\|Default\|FullRefresh` | hook | **now implemented** (Δ) |
| `JELLYFIN_IMAGE_REFRESH_MODE` | `Default` | same set | hook | **now implemented** (Δ) |
| `TELEGRAM_BOT_ENABLED` | `false` | bool | bot | |
| `TELEGRAM_BOT_TOKEN` | `''` | secret str | bot | redacted |
| `TELEGRAM_ALLOWED_CHAT_IDS` | `''` | comma list of i64 | bot | empty ⇒ bot refuses to start (kept) |
| `TELEGRAM_STALL_TIMEOUT_SECONDS` | `180` | int | bot watchdog | |
| `TELEGRAM_HARD_TIMEOUT_SECONDS` | `7200` | int | bot watchdog | |
| `TELEGRAM_MAX_URLS_PER_MESSAGE` | `10` | int | bot | |
| `METUBE_VERSION` | `dev` | str | `/version`, `healthz` | kept; `AULOS_VERSION` is an alias |
| `PUID` / `PGID` / `UID` / `GID` / `UMASK` / `CHOWN_DIRS` | `1000/1000/-/-/022/true` | entrypoint | §18.2 | |
| `DOTNET_SYSTEM_GLOBALIZATION_INVARIANT` | `1` | image ENV | N_m3u8DL-RE | |
| **New** `AULOS_DB_PATH` | `<STATE_DIR>/aulos.db` | path | store | |
| `AULOS_DB_READERS` | `4` | int | read pool | |
| `AULOS_WS_BATCH_MS` | `250` | int 50..5000 | delta cadence | |
| `AULOS_REPLAY_FRAMES` | `2048` | int | `?since=` ring | |
| `AULOS_RESOLVE_CONCURRENCY` | `4` | int | resolve pool | |
| `AULOS_RESOLVE_TIMEOUT_SECS` | `120` | int | resolve deadline | |
| `AULOS_RESOLVE_MAX_DEPTH` | `3` | int | redirect recursion | |
| `AULOS_SCHED_LOOKAHEAD` | `32` | int | anti-head-of-line | |
| `AULOS_JOB_STALL_SECS` | `900` | int (0=off) | no-frame watchdog | |
| `AULOS_JOB_TIMEOUT_SECS` | `0` | int (0=off) | hard job timeout | |
| `AULOS_KILL_GRACE_MS` | `5000` | int | SIGTERM→SIGKILL | |
| `AULOS_AUTO_RETRY_MAX` | `2` | int | retry policy | |
| `AULOS_PLUGINS_DIR` | `${PLUGINS_DIR:-/config/plugins}` | path | command plugins | |
| `AULOS_PLUGINS_ENABLED` | `true` | bool | plugin discovery | |
| `AULOS_SC_HTTP` | `auto` | `auto\|impersonate\|plain` | SC client | |
| `AULOS_SC_META_CONCURRENCY` | `4` | int | SC season fetch | |
| `AULOS_POT_ENABLED` | `true` | bool | POT supervisor | |
| `AULOS_POT_CMD` | `bgutil-pot server` | argv str | POT supervisor | |
| `AULOS_POT_URL` | `http://127.0.0.1:4416` | str | POT health probe | |
| `AULOS_JELLYFIN_DEBOUNCE_SECS` | `30` | int | hook debounce | |
| `AULOS_JELLYFIN_MAX_WAIT_SECS` | `300` | int | debounce cap | |
| `AULOS_NFO_ENABLED` | `true` | bool | NFO hook | |
| `AULOS_TELEGRAM_EDIT_INTERVAL_MS` | `3000` | int | live progress edits | |
| `AULOS_SUB_CHECK_CONCURRENCY` | `2` | int | subscription checks | |
| `AULOS_SUB_BACKOFF_MAX_SECS` | `21600` | int | subscription backoff cap | |
| `AULOS_SUB_FIRST_CHECK_DELAY_SECS` | `10` | int | first check after boot | |
| `AULOS_LOG_FORMAT` | `text` | `text\|json` | tracing | |
| `AULOS_CLEAN_ORPHAN_TEMP` | `false` | bool | boot cleanup | |
| `AULOS_V1_ENABLED` | `true` | bool | v1 shim mount | set `false` post-cutover |
| `AULOS_ENTRY_MAX_BYTES` | `262144` | int | entry compaction | |

Unknown `AULOS_*` variables are **rejected at boot** (typo protection); unknown non-`AULOS_`
variables are ignored (the container inherits a lot).

### 8.3 `YTDL_OPTIONS` layering

```rust
pub struct YtdlOptions {
    pub base: Map<String, Value>,            // env YTDL_OPTIONS, then FILE merged over it
    pub presets: BTreeMap<String, Map<String, Value>>,
    pub overrides: Map<String, Value>,       // runtime overrides (cookiefile)
    pub file_mtime: Option<f64>,
    pub loaded_at: Instant,
}
impl YtdlOptions {
    /// env/file → presets in request order → per-request overrides. `null` values are kept
    /// (so a preset can clear a global `download_archive`). Exactly legacy `_build_ytdl_options`.
    pub fn layer(&self, presets: &[String], overrides: &Map<String, Value>) -> Map<String, Value>;
}
```
Held in `Arc<ArcSwap<YtdlOptions>>`. A job snapshots it at spawn time, so a reload never mutates
an in-flight job's options. Error strings are preserved verbatim for the v1/v2 wire:
`Environment variable YTDL_OPTIONS is invalid`, `File "<path>" not found`,
`YTDL_OPTIONS_FILE contents is invalid`, and the presets analogues.

**Δ (better)**: on a reload failure we keep the last-good `YtdlOptions` and report the error.
Legacy re-read `YTDL_OPTIONS` from env first and then failed, silently discarding the file's
contribution until the next successful reload — a config edit typo would quietly change
behaviour. Ours changes nothing until the file parses.

`_apply_runtime_overrides` is preserved: `set_runtime_override("cookiefile", path)` /
`remove_runtime_override` write into `kv` and are re-applied after every reload. On boot, if
`<STATE_DIR>/cookies.txt` exists, the override is set (legacy did this only under `__main__`).

### 8.4 `YTDL_OPTIONS_FILE` hot reload (`ConfigWatcher`)

Legacy used `watchfiles.awatch(<file>)` with a `samefile` filter. That has a real failure mode:
editors and `docker cp`/Ansible **replace** the file (`rename(tmp, target)`), which invalidates
an inode-level watch, and `awatch` on a *file* path stops delivering events after the first
replace on some backends. Our design watches the **directory**:

```rust
pub struct ConfigWatcher {
    watcher: notify::RecommendedWatcher,      // non-recursive watch on parent dirs
    targets: Vec<PathBuf>,                    // YTDL_OPTIONS_FILE, YTDL_OPTIONS_PRESETS_FILE
    debounce: Duration,                       // AULOS_CONFIG_DEBOUNCE_MS, default 250
}
```

Algorithm:
1. For each non-empty target, canonicalise it and `watch(parent, NonRecursive)`. If two targets
   share a parent, one watch covers both.
2. Accept an event iff `event.paths` contains a path whose **file name** equals the target's file
   name, **and** the event kind ∈ {`Create`, `Modify(Data|Any|Name)`, `Remove`}. (Name/rename
   events are what an atomic replace produces; legacy's `{modified, added, deleted}` set maps to
   the same three.)
3. Coalesce: reset a `250 ms` debounce timer; reload once per quiet period. A `for` loop of
   `sed -i` edits therefore triggers one reload, not five.
4. Reload → `load_ytdl_options()` (env re-read + file merge + runtime overrides) into a new
   `YtdlOptions`; on success `ArcSwap::store`, on failure keep the old one.
5. Publish `DomainEvent::YtdlOptionsReloaded { ok, msg, update_time }` where `update_time` is
   `mtime` as fractional epoch seconds or `null` — the exact legacy payload — which becomes the
   WS frame `{"t":"ytdl_options", …}` (§9.4). No v1 equivalent exists (legacy used Socket.IO,
   which we do not provide); the v1 shim is unaffected.
6. If the file is **deleted**, the reload fails with `File "<path>" not found`, the last-good
   options are kept, and `healthz.components.ytdl_options.status = "degraded"`. Re-creating the
   file heals it, because the directory watch is still live. Legacy would have been left with
   env-only options and no way back short of a restart.
7. `POST <prefix>api/v2/ytdl-options/reload` forces the same path (useful when the file lives on
   a network mount where inotify does not fire — a real VPS scenario with NFS/SMB `/config`).
   `GET <prefix>api/v2/ytdl-options` returns `{ok, msg, update_time, keys:[…], presets:[…]}`
   (values redacted: they can contain cookies/proxy credentials).
8. A 30 s poll fallback (`AULOS_CONFIG_POLL_SECS`, default 30, 0 = off) compares `(mtime, size)`
   and reloads on change. `notify`'s `PollWatcher` is used automatically when the inotify backend
   is unavailable (containers with `fs.inotify` limits exhausted).

---

## 9. Protocol v2 (`aulos-api`)

`<p>` = `URL_PREFIX` (always starts and ends with `/`). Every response is
`Content-Type: application/json; charset=utf-8` except the file routes and `robots.txt`.
Every response carries `X-Request-Id` and `X-Aulos-Seq` (the hub seq at response time, so a REST
mutation can be correlated with the WS frame it caused).

### 9.1 Error envelope

```json
{ "error": { "code": "validation_failed", "message": "quality must be one of [best, worst, 2160, 1440, 1080, 720, 480, 360, 240, best_remux]", "field": "quality", "request_id": "01JBQ7…" } }
```

| Code | HTTP | Meaning |
|---|---|---|
| `validation_failed` | 400 | body/field invalid; `field` set |
| `unsupported_url` | 400 | no provider matched and yt-dlp rejected the scheme |
| `overrides_disabled` | 400 | `ALLOW_YTDL_OPTIONS_OVERRIDES=false` |
| `preset_unknown` | 400 | named preset not in the catalogue |
| `folder_invalid` | 400 | containment / missing / custom dirs disabled |
| `unauthorized` | 401 | reverse-proxy auth failed (never a redirect — we set `WWW-Authenticate` only if configured) |
| `not_found` | 404 | unknown item / subscription id |
| `conflict` | 409 | e.g. subscribing to an already-subscribed URL |
| `payload_too_large` | 413 | cookie upload > 1 MB |
| `state_unavailable` | 503 | SQLite busy/locked; `Retry-After: 1` |
| `internal` | 500 | bug; the message is a request id, details only in logs |

### 9.2 REST v2 endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `<p>healthz` | liveness/readiness (§14.3) |
| GET | `<p>api/v2/capabilities` | static catalogue + config flags |
| GET | `<p>api/v2/state?since=<seq>` | delta list or full snapshot |
| GET | `<p>api/v2/items?status=&group_id=&limit=&cursor=&order=ord` | paged list |
| GET | `<p>api/v2/items/{id}` | one item |
| POST | `<p>api/v2/downloads` | add (single or batch), 202 |
| POST | `<p>api/v2/items/actions` | `start\|cancel\|retry\|delete` on ids |
| DELETE | `<p>api/v2/items/{id}?delete_file=` | delete one |
| GET | `<p>api/v2/items/{id}/file` | 302 to the file route (or 404) |
| GET/POST | `<p>api/v2/subscriptions` | list / create |
| PATCH/DELETE | `<p>api/v2/subscriptions/{id}` | update / delete |
| POST | `<p>api/v2/subscriptions/{id}/check`, `<p>api/v2/subscriptions/check` | 202 + job id |
| GET | `<p>api/v2/ytdl-options`, POST `…/reload` | inspect / force reload |
| GET | `<p>api/v2/presets` | preset names |
| GET/POST/DELETE | `<p>api/v2/cookies` | status / upload (multipart `cookies`) / delete |
| GET | `<p>api/v2/custom-dirs` | dir listing |
| GET | `<p>api/v2/import-report` | the importer's report (§5.5.6) |
| POST | `<p>api/v2/plugins/reload` | re-scan `PLUGINS_DIR` |
| GET | `<p>ws` | WebSocket upgrade |
| GET | `<p>download/*`, `<p>audio_download/*` | files, Range-capable |
| GET | `<p>robots.txt` | as legacy |
| GET | `<p>` | 60-line static status page (no Angular) |

#### `POST <p>api/v2/downloads`

Request (single):
```json
{ "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "download_type": "video", "codec": "auto", "format": "mp4", "quality": "1080",
  "folder": "Music/Live", "custom_name_prefix": "", "playlist_item_limit": 0,
  "auto_start": true, "split_by_chapters": false,
  "chapter_template": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
  "subtitle_language": "en", "subtitle_mode": "prefer_manual",
  "ytdl_options_presets": ["sponsorblock"], "ytdl_options_overrides": {} }
```
Request (batch): `{ "items": [ {…}, {…} ], "defaults": {…} }` — `defaults` is merged under each
item, so a share-sheet can send 3 URLs with one selection.

Response `202`:
```json
{ "ids": ["01JBQ7Z5T9K3M2R8V4XW6Y0AAA"], "seq": 10241,
  "duplicates": [ { "url": "https://youtu.be/x", "existing_id": "01JBQ6…" } ] }
```
Single-URL requests also get `"id"` (the first element) for convenience. The item is already in
the snapshot with `status: "resolving"` when this returns.

#### `POST <p>api/v2/items/actions`
```json
{ "action": "cancel", "ids": ["01JBQ7…", "01JBQ8…"] }
```
→ `200 { "applied": ["01JBQ7…"], "skipped": [ { "id": "01JBQ8…", "reason": "already_terminal" } ], "seq": 10250 }`
`action: "delete"` accepts `"delete_file": true|false` (default = `DELETE_FILE_ON_TRASHCAN`).

#### `GET <p>api/v2/capabilities`
```json
{ "version": "2026.09.04", "yt_dlp": "2026.8.30.232658.dev0", "url_prefix": "/",
  "protocol": { "v2": true, "v1_shim": true, "socketio": false, "ws_batch_ms": 250 },
  "formats": [
    { "id": "any", "text": "Any", "download_type": "video",
      "qualities": [ {"id":"best","text":"Best"}, {"id":"2160","text":"2160p"}, {"id":"1440","text":"1440p"},
                     {"id":"1080","text":"1080p"}, {"id":"720","text":"720p"}, {"id":"480","text":"480p"},
                     {"id":"360","text":"360p"}, {"id":"240","text":"240p"}, {"id":"worst","text":"Worst"} ] },
    { "id": "mp4", "text": "MP4", "download_type": "video",
      "qualities": [ {"id":"best"},{"id":"best_remux"},{"id":"2160"},{"id":"1440"},{"id":"1080"},
                     {"id":"720"},{"id":"480"},{"id":"360"},{"id":"240"},{"id":"worst"} ] },
    { "id": "ios", "text": "iOS", "download_type": "video", "qualities": [ {"id":"best"} ] },
    { "id": "m4a", "text": "M4A", "download_type": "audio", "qualities": [ {"id":"best"},{"id":"192"},{"id":"128"} ] },
    { "id": "mp3", "text": "MP3", "download_type": "audio", "qualities": [ {"id":"best"},{"id":"320"},{"id":"192"},{"id":"128"} ] },
    { "id": "opus","text": "Opus","download_type": "audio", "qualities": [ {"id":"best"} ] },
    { "id": "wav", "text": "WAV", "download_type": "audio", "qualities": [ {"id":"best"} ] },
    { "id": "flac","text": "FLAC","download_type": "audio", "qualities": [ {"id":"best"} ] },
    { "id": "srt", "text": "SRT", "download_type": "captions", "qualities": [ {"id":"best"} ] },
    { "id": "jpg", "text": "Thumbnail", "download_type": "thumbnail", "qualities": [ {"id":"best"} ] }
  ],
  "presets": ["sponsorblock", "archive"],
  "providers": [ {"id":"ytdlp","fallback":true}, {"id":"streamingcommunity","slots":1},
                 {"id":"command:bandcamp","priority":120} ],
  "config": { "custom_dirs": true, "create_custom_dirs": true,
              "allow_ytdl_options_overrides": false,
              "default_option_playlist_item_limit": 0,
              "subscription_default_check_interval": 60,
              "output_template_chapter": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
              "public_host_url": "download/", "public_host_audio_url": "audio_download/",
              "default_theme": "auto", "max_concurrent_downloads": 3,
              "delete_file_on_trashcan": false, "clear_completed_after": 0 },
  "actions": ["start","cancel","retry","delete"] }
```
This is the endpoint that lets the iOS app drop its `/version`-on-every-reconnect and its
`ServerFormat.defaultFormats` fallback (ios §7.10, §7.15, §7.16). It is cheap (all in memory) and
carries `ETag` (hash of the payload) so a reconnect is a `304`.

#### `GET <p>api/v2/state?since=<seq>`
```json
{ "mode": "delta", "from": 10240, "seq": 10251,
  "frames": [ {"t":"delta","seq":10241,"items":[…]}, {"t":"added","seq":10248,"items":[…]} ] }
```
or, when `since` is too old / absent:
```json
{ "mode": "snapshot", "seq": 10251, "items": [ … ], "subscriptions": [ … ], "counts": {…} }
```
Supports `If-None-Match`/`ETag` (ETag = `"seq-10251"`), so pull-to-refresh with no changes is a
`304` with an empty body (ios §7.9).

### 9.3 WebSocket

`GET <p>ws` (upgrade). Subprotocol `aulos.v2`. Auth is whatever the reverse proxy does (cookies
are forwarded by the browser/URLSession); the server additionally accepts
`Sec-WebSocket-Protocol: aulos.v2, bearer.<token>` when `AULOS_WS_TOKEN` is set (optional,
default off).

Server→client envelope: `{"t": <type>, "seq": <u64>, …}`. Types: `snapshot`, `delta`, `added`,
`completed`, `removed`, `group`, `subscription`, `subscription_removed`, `ytdl_options`,
`health`, `pong`, `error`.

Client→server: `{"t":"ping"}`, `{"t":"resume","since":<seq>}`, `{"t":"hello","client":"aulos-ios/1.2","topics":["items","subscriptions","health"]}`.

Connection task algorithm:
1. Read an optional `hello` (100 ms grace). Default topics = all.
2. Subscribe to the broadcast **first**, then read the state snapshot, then send `snapshot` with
   the seq recorded at subscribe time, then forward buffered frames with `seq > snapshot.seq`.
   (Subscribe-then-snapshot ordering is what makes "no lost updates" true.)
3. Loop `select!` on: broadcast recv, socket recv, 30 s keepalive `Ping` (WS-level).
4. `RecvError::Lagged(n)` ⇒ log, send a fresh `snapshot`, continue.
5. Backpressure: if the socket's send buffer is full for > `AULOS_WS_SEND_TIMEOUT_MS` (5000), the
   connection is closed with code 1013 (`Try Again Later`). A stalled client can never hold
   memory in the hub.

### 9.4 Every frame, with an example

**`snapshot`** — sent on connect and after a lag; same item shape as REST.
```json
{ "t": "snapshot", "seq": 10251, "server_time": 1757000000123,
  "counts": { "queued": 2, "resolving": 0, "active": 1, "finished": 401, "error": 10, "canceled": 3 },
  "items": [
    { "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA", "kind": "item", "ord": 981,
      "group_id": null, "group_index": null,
      "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
      "title": "Rick Astley - Never Gonna Give You Up",
      "status": "downloading", "provider": "ytdlp",
      "percent": 42.7, "speed": 3145728.0, "eta": 63,
      "downloaded_bytes": 44040192, "total_bytes": null, "total_bytes_estimate": 103809024,
      "fragment_index": null, "fragment_count": null,
      "msg": null, "error": null,
      "filename": null, "size": null, "download_url": null,
      "chapter_files": [], "subtitle_files": [],
      "selection": { "download_type": "video", "codec": "auto", "format": "mp4", "quality": "1080" },
      "folder": null,
      "created_at": 1756999900000, "started_at": 1756999901000, "finished_at": null,
      "attempt": 0, "source": { "kind": "telegram", "chat_id": 12345 },
      "children_total": null, "children_done": null, "children_error": null, "children_active": null },
    { "id": "01JBQ8AA0000000000000000GG", "kind": "group", "ord": 982,
      "group_id": null, "group_index": null,
      "url": "https://www.youtube.com/playlist?list=PL123",
      "title": "Lo-fi beats (500)", "status": "downloading", "provider": "ytdlp",
      "percent": 12.4, "speed": null, "eta": null,
      "downloaded_bytes": null, "total_bytes": null, "total_bytes_estimate": null,
      "fragment_index": null, "fragment_count": null,
      "msg": null, "error": null, "filename": null, "size": null, "download_url": null,
      "chapter_files": [], "subtitle_files": [],
      "selection": { "download_type": "video", "codec": "auto", "format": "mp4", "quality": "best" },
      "folder": null, "created_at": 1756999950000, "started_at": 1756999951000, "finished_at": null,
      "attempt": 0, "source": { "kind": "api_v2" },
      "children_total": 500, "children_done": 62, "children_error": 1, "children_active": 3 }
  ],
  "subscriptions": [ { "id": "…", "name": "…", "url": "…", "enabled": true,
                       "check_interval_minutes": 60, "download_type": "video", "codec": "auto",
                       "format": "any", "quality": "best", "folder": "",
                       "last_checked": 1756999000000, "next_due": 1757002600000,
                       "seen_count": 312, "consecutive_failures": 0, "error": null } ] }
```

**`delta`** — batched at `AULOS_WS_BATCH_MS`; only changed fields, plus the id. Absent field =
unchanged. Never contains `added`/`removed` items.
```json
{ "t": "delta", "seq": 10252, "items": [
    { "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA", "percent": 43.9, "speed": 3210000.0, "eta": 61,
      "downloaded_bytes": 45613056 },
    { "id": "01JBQ9BB…", "status": "postprocessing", "msg": "Merging formats", "percent": 100.0 }
  ] }
```

**`added`** — prompt (not batched with progress), but coalesced within one expansion batch.
```json
{ "t": "added", "seq": 10253, "items": [ { …full ItemView… }, { …full ItemView… } ] }
```

**`completed`** — prompt, terminal, full item (so the client never needs a follow-up fetch).
```json
{ "t": "completed", "seq": 10260,
  "item": { "id": "01JBQ7…", "status": "finished", "percent": 100.0,
            "filename": "Rick Astley - Never Gonna Give You Up.mp4", "size": 103809024,
            "download_url": "download/Rick%20Astley%20-%20Never%20Gonna%20Give%20You%20Up.mp4",
            "finished_at": 1757000060000, "…": "…" } }
```

**`removed`** — prompt; ids only.
```json
{ "t": "removed", "seq": 10261, "ids": ["01JBQ7Z5T9K3M2R8V4XW6Y0AAA"], "reason": "deleted" }
```
`reason` ∈ `deleted | cleared | auto_cleared | group_cascade`.

**`group`** — group aggregate at the delta cadence (avoids sending 500 child deltas to update one
progress bar).
```json
{ "t": "group", "seq": 10262, "groups": [
    { "id": "01JBQ8AA…", "status": "downloading", "percent": 12.6,
      "children_total": 500, "children_done": 63, "children_error": 1, "children_active": 3 } ] }
```

**`subscription`** / **`subscription_removed`**
```json
{ "t": "subscription", "seq": 10270, "subscription": { "id": "9c1f…", "name": "Veritasium",
    "url": "https://www.youtube.com/@veritasium", "enabled": true, "check_interval_minutes": 60,
    "download_type": "video", "codec": "auto", "format": "any", "quality": "best", "folder": "",
    "last_checked": 1757000100000, "next_due": 1757003700000, "seen_count": 314,
    "consecutive_failures": 0, "error": null, "checking": false } }
{ "t": "subscription_removed", "seq": 10271, "id": "9c1f…" }
```

**`ytdl_options`** — the hot-reload notification (legacy `ytdl_options_changed`).
```json
{ "t": "ytdl_options", "seq": 10280, "ok": true, "msg": "", "update_time": 1757000200.412 }
```

**`health`** — emitted only on a component status transition (not periodically).
```json
{ "t": "health", "seq": 10290, "status": "degraded",
  "changed": [ { "component": "pot", "from": "ok", "to": "down", "detail": "child exited: signal 9" } ] }
```

**`pong`** / **`error`**
```json
{ "t": "pong", "seq": 10291, "server_time": 1757000300000 }
{ "t": "error", "seq": 10292, "code": "bad_frame", "message": "unknown frame type \"subscribe\"" }
```

### 9.5 Static file routes

`<p>download/*` and `<p>audio_download/*` are served by a hand-rolled handler over
`tower-http::services::ServeFile` semantics: canonicalise, verify the resolved path is inside the
base **by path components** (not `starts_with` on strings — legacy bug §17.13-P21), support
`Range`/`If-Range`/`Last-Modified`/`ETag` (mtime+size), `Content-Disposition: inline`, and
`Content-Type` from `mime_guess`. When `DOWNLOAD_DIRS_INDEXABLE=true`, a directory request
renders a minimal HTML index (`tower-http` has no listing, so ~60 lines of our own). Symlinks
that escape the base are rejected.

---

## 10. The v1 compatibility shim (`aulos-api::v1`) — deep dive

Purpose: the **existing** iOS build (`metube_ios` @ `8622a2f`), the README bookmarklet and the iOS
Shortcut must keep working unchanged through cutover. The shim is a pure translation layer: it
constructs `DownloadRequest`s / `EngineCmd`s and projects `ItemView` down to the legacy shape.
It owns **no state**. Mounted iff `AULOS_V1_ENABLED=true` (default), so it can be switched off
after the client ships a v2 build, and its removal is a one-line change.

### 10.1 Route parity table

| Legacy route | Provided | Implementation | Deviations |
|---|---|---|---|
| `POST <p>add` | ✅ | parse legacy body (§10.2) → `EngineCmd::Add` → `{"status":"ok"}` | HTTP 200 kept; `Content-Type` is now `application/json` (was `text/plain`) |
| `GET <p>presets` | ✅ | `{"presets":[sorted names]}` | none |
| `POST <p>cancel-add` | ✅ | `EngineCmd::CancelResolve` on all in-flight resolutions of this "generation" → `{"status":"ok"}` | now actually cancels in-flight resolution and marks remaining children canceled (legacy only checked between entries) |
| `POST <p>subscribe` | ✅ | legacy body + `check_interval_minutes` → subscriptions manager | `{"status":"ok","subscription":{…13 keys…}}` unchanged |
| `GET <p>subscriptions` | ✅ | array of legacy public dicts | adds nothing, removes nothing |
| `POST <p>subscriptions/update` | ✅ | `{"id",…}`; only `enabled`/`check_interval_minutes`/`name` | a bad `enabled` is now **400**, not 500 (§17.13-C25) |
| `POST <p>subscriptions/delete` | ✅ | `{"ids":[…]}` | `[]` still 400 |
| `POST <p>subscriptions/check` | ✅ | 200 `{"status":"ok"}` **immediately** | no longer blocks for minutes (§17.13-C9); a `job_id` is added to the body |
| `POST <p>delete` | ✅ | `{"ids":[…],"where":"queue"\|"done"}` → cancel/delete (§10.3) | ids may be URLs, legacy media ids **or** ULIDs |
| `POST <p>start` | ✅ | `{"ids":[…]}` → `Start`, and `Retry` for terminal items | `ids: null` is 400, not 500 |
| `POST <p>upload-cookies` | ✅ | multipart field `cookies`, 1 MB cap | same messages |
| `POST <p>delete-cookies` | ✅ | same messages | |
| `GET <p>cookie-status` | ✅ | `{"status":"ok","has_cookies":bool}` | |
| `GET <p>history` | ✅ | `{"done":[…],"queue":[…],"pending":[…]}` (§10.4) | all three keys always present (the iOS decoder requires it) |
| `GET <p>version` | ✅ | `{"yt-dlp":"…","version":"…"}` + `"url_prefix"`, `"protocol":"v2"` | additive only |
| `GET <p>robots.txt` | ✅ | identical | |
| `GET <p>` | ⚠️ | 60-line status page instead of the Angular SPA | `metube_theme` cookie is not set |
| `GET <p>download/*`, `<p>audio_download/*` | ✅ | §9.5 | Range support added |
| `OPTIONS` on all of the above | ✅ | `{"status":"ok"}` + CORS headers | also emits `Access-Control-Allow-Methods` |
| `GET <p>socket.io/*` | ❌ | returns `501` with `{"error":{"code":"socketio_removed","message":"Socket.IO is not supported; use <prefix>ws (protocol v2) or GET <prefix>api/v2/state"}}` | Deliberate and explicit, so a stale client fails loudly rather than hanging on a handshake. |
| `GET /` (when `URL_PREFIX != "/"`) | ✅ | `302` to `URL_PREFIX` | |

### 10.2 `POST <p>add` request translation

The shim runs **both** legacy migration and current-schema parsing, in the legacy order:

1. Body must be a JSON object (else 400 `Invalid JSON request body` /
   `JSON request body must be an object`, kept as the `message` in the JSON envelope).
2. If `download_type` is absent, apply `_migrate_legacy_request` — the exact table:

| legacy `format` | legacy `quality` | → `download_type` | `codec` | `format` | `quality` |
|---|---|---|---|---|---|
| `m4a\|mp3\|opus\|wav\|flac` | any | `audio` | `auto` | same | unchanged |
| `thumbnail` | any | `thumbnail` | `auto` | `jpg` | `best` |
| `captions` | any | `captions` | `auto` | `subtitle_format` or `srt` | `best` |
| other | `best_ios` | `video` | `video_codec` | `ios` | `best` |
| other | `audio` | `audio` | `auto` | `m4a` | `best` |
| other | else | `video` | `video_codec` | legacy `format` | legacy `quality` |

3. Validate with the same matrix as v2 (§17.6). Validation failures return **400** with the
   legacy `reason` string as `error.message` — the iOS `AddResultClassifier` treats any non-2xx as
   a failure and shows the message, so this is an improvement it already handles.
4. `auto_start`: legacy compared `is True`, so the JSON string `"true"` silently routed to
   *pending*. The shim accepts real booleans and the strings `true/false/1/0/on/off`
   (case-insensitive) — a fix, because the iOS app sends a real boolean and a Shortcut sends a
   string, and both intended "start it".
5. Duplicate URL (already queued/active) ⇒ `200 {"status":"ok"}` with no new item, matching
   legacy's silent skip. The v2 endpoint reports it in `duplicates`.
6. Response is always `200`:
   - success: `{"status":"ok"}` (plus additive `"ids":[…]`, which old clients ignore)
   - business error: `{"status":"error","msg":"<text>"}` — same as legacy so the iOS
     classifier's "body JSON `status != ok` ⇒ failure(msg)" path is preserved.

The bookmarklet (`POST <p>add` with `{"url":…,"quality":…,"format":…}` form/JSON) and the iOS
Shortcut hit exactly this path.

### 10.3 Id resolution for `delete` / `start`

Legacy keyed everything by `url`; the iOS app sends `item.url ?? item.id`, and `clearCompleted`
sends only urls. The shim therefore resolves **each token** in `ids` through this ladder:

```
resolve(token) -> Vec<ItemId>
  1. if token parses as a ULID and that item exists            -> [that id]
  2. else exact match on items.url                             -> all matching ids
  3. else exact match on items.media_id                        -> all matching ids
  4. else empty (recorded in the response's `skipped`, logged at DEBUG)
```
Ties (the same URL added twice) resolve to **all** matches, which is what a legacy user expects
from a URL-keyed API. The lookup is a single `SELECT id FROM items WHERE url IN (…) OR media_id IN (…) OR id IN (…)`.

`where` semantics:

| `where` | v2 action |
|---|---|
| `"queue"` | `Cancel` on non-terminal, then `Delete` (legacy dropped the row entirely) |
| `"done"` | `Delete` with `delete_file = DELETE_FILE_ON_TRASHCAN` |
| other / missing | `400` (legacy had a reasonless 400) |

`POST <p>start` maps `queued(auto_start=false)` → `Start`, and `error`/`canceled` → `Retry`.
Legacy only handled the pending case; retry-on-failed is additive and closes ios pain point #24
without any client change (the client just has no button yet — a Shortcut can call it).

### 10.4 `GET <p>history` projection

```json
{ "queue":  [ /* status ∈ {resolving, preparing, downloading, postprocessing} and queued(auto_start=true) */ ],
  "pending":[ /* status == queued && auto_start == false */ ],
  "done":   [ /* status ∈ {finished, error} */ ] }
```

Rules:

| Rule | Reason |
|---|---|
| Groups (`kind == "group"`) are **omitted**; only children appear. | Legacy had no group concept; a group row would render as a bogus "In Progress" item with no progress in the old client. |
| `canceled` items are **omitted** from all three arrays. | The iOS `DownloadStatus` has no `canceled` case; unknown → `.pending` → the row would be stuck in "In Progress" forever. v2 clients see them. |
| Order within each array is by `ord` ascending. | Deterministic; the old client re-sorts by title anyway. |
| All three keys are always present, even when empty. | `HistoryResponse` requires all three (ios §4). |

Per-item legacy field projection:

| Legacy field | Value in the shim |
|---|---|
| `id` | `media_id` if present, else the ULID (legacy `id` was the yt-dlp video id, optionally `"<prefix>.<id>"`; the prefix behaviour is reproduced) |
| `title` | `title` (same prefixing) |
| `url` | `url` |
| `status` | §10.5 mapping |
| `percent` | `percent` (always a number; legacy sometimes `null` — harmless, the client already clamps) |
| `speed`, `eta` | `speed` f64/null, `eta` int seconds/null |
| `downloaded_bytes`, `total_bytes`, `total_bytes_estimate`, `fragment_index`, `fragment_count` | as-is |
| `msg` | `msg` for active items, `error` for terminal errors (legacy overloaded `msg`) |
| `error` | `error` |
| `filename`, `size` | as-is (`null` when unknown — the key is always present now) |
| `quality`, `format`, `codec`, `download_type` | from `selection` |
| `folder`, `custom_name_prefix` | from `request` |
| `playlist_item_limit`, `split_by_chapters`, `chapter_template`, `subtitle_language`, `subtitle_mode`, `ytdl_options_presets`, `ytdl_options_overrides` | from `request` |
| `timestamp` | `created_at * 1_000_000` (legacy was `time.time_ns()`) |
| `subtitle_files`, `chapter_files` | as-is |
| `entry` | **omitted.** It was the full yt-dlp info dict; the iOS client never reads it and it is the single biggest payload contributor (ios §3 "the full yt-dlp metadata tree"). Dropping it is a pure win and no known consumer breaks. |

### 10.5 Status mapping v2 → v1

| v2 status | v1 `status` | Rationale |
|---|---|---|
| `queued` (`auto_start=false`) | `pending` | appears in `pending[]` |
| `queued` (`auto_start=true`) | `pending` | appears in `queue[]`, exactly like legacy's queued-but-not-started |
| `resolving` | `pending` | legacy had no such state; the item existed only after extraction, so `pending` is the closest truth |
| `preparing` | `preparing` | 1:1 |
| `downloading` | `downloading` | 1:1 |
| `postprocessing` | `downloading` | legacy showed a frozen `downloading` during ffmpeg; identical UX, and `msg` carries the phase |
| `finished` | `finished` | 1:1 |
| `error` | `error` | 1:1 |
| `canceled` | *(item omitted)* | §10.4 |

### 10.6 What the shim does **not** provide, and the mitigation

| Missing | Mitigation |
|---|---|
| Socket.IO (`all`, `updated`, `added`, `completed`, `canceled`, `cleared`, `formats`, `configuration`, `custom_dirs`, `ytdl_options_changed`, `subscriptions_all`, `subscription_*`) | `<p>socket.io` returns `501` with a pointer. The existing iOS app degrades to: no live updates, but `GET /history` on pull-to-refresh still works (it already distrusts the socket and fetches `/history` on connect — ios §6.1). **This is the one user-visible regression during the overlap window**, and it is why the cutover runbook (§20) ships the v2 iOS build in the same session. |
| The Angular UI | Out of scope per BRIEF. `<p>` serves a status page. |
| `formats` socket event | Never actually emitted by legacy either (verified in `main.py`: `get_available_formats()` is only passed to the Telegram bot). The iOS client already falls back to its built-in list, so nothing regresses. `GET api/v2/capabilities` is the v2 replacement. |

### 10.7 CORS parity

Legacy: on every response, if `Origin` is present and (`*` in `CORS_ALLOWED_ORIGINS` or the origin
is listed), set `Access-Control-Allow-Origin: <Origin>` and
`Access-Control-Allow-Headers: Content-Type`. No methods header, no credentials.

The shim layer reproduces that exactly for v1 routes; v2 routes additionally send
`Access-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS`, `Vary: Origin` and
`Access-Control-Max-Age: 600`. `Access-Control-Allow-Credentials` is **not** sent (legacy did not,
and the iOS client sets cookies manually).

---

## 11. Telegram (`aulos-telegram`) — deep dive

### 11.1 Actor shape

```rust
pub struct TelegramActor {
    bot: teloxide::Bot,
    cfg: Arc<TelegramConfig>,          // token, allowed_chat_ids, timeouts, max_urls
    store: Store,                      // per-chat config in `telegram_chats`
    engine: mpsc::Sender<EngineCmd>,
    events: mpsc::Receiver<DomainEvent>,
    watches: HashMap<ItemId, Watch>,   // job → the message that reports it
    chat_msgs: HashMap<i64, ChatBoard>,// chat → its live board message
    limiter: Limiter,
}
struct ChatBoard {
    message_id: MessageId,
    jobs: IndexMap<ItemId, JobLine>,   // insertion-ordered, capped at 12 visible + "+N more"
    last_edit: Instant,
    last_rendered: String,             // to skip no-op edits
    dirty: bool,
    finished_at: Option<Instant>,      // board is retired 60 s after the last job ends
}
struct JobLine { title: String, status: Status, percent: f64, speed: Option<f64>,
                 eta: Option<i64>, started: Instant, last_progress: Instant,
                 stall_notified: bool, timeout_notified: bool }
```

`teloxide` runs its own update dispatcher (long polling with
`drop_pending_updates = true`, matching legacy). Handlers send `EngineCmd`s and never block.
The actor's own `select!` loop handles: dispatcher messages, `DomainEvent`s, and a
`interval(1 s)` tick that drives edits + watchdogs.

### 11.2 Command / config parity

| Trigger | Behaviour (byte-identical text to legacy) |
|---|---|
| `/start` | `Hi! Send one or more links and I will queue them for download.\nUse /config to set default format/quality for this chat.` |
| `/config` | The config text + main inline keyboard |
| config text | `Current download config:\n- Format: {format}\n- Quality: {quality}\n- Split by chapters: {on|off}\n- Playlist item limit: {n}` |
| main keyboard | rows `Format: {f}` → `cfg:menu:format`; `Quality: {q}` → `cfg:menu:quality`; `Split Chapters: {on|off}` → `cfg:toggle:split`; `Playlist Limit: {n}` → `cfg:menu:limit` |
| `cfg:menu:format` | text `Select format`, one button per catalogue format id + `Back` → `cfg:menu:main` |
| `cfg:menu:quality` | text `Select quality`, one button per quality of the current format + `Back` |
| `cfg:menu:limit` | text `Select playlist limit`, buttons `0 1 5 10 20` + `Back` |
| `cfg:set:format:{f}` | set format; if the current quality is not in the new format's list, reset to the first |
| `cfg:set:quality:{q}` | set only if present in the current format's list, else ignore |
| `cfg:set:limit:{n}` | parse int, ignore on failure |
| `cfg:toggle:split` | flip `split_by_chapters` |
| any callback | `answer_callback_query` then `edit_message_text` |
| unauthorised chat | silently ignored + one WARN log line (`chat_id` included) |

The format catalogue is the same list legacy passed from `get_available_formats()`
(`any, mp4, ios, m4a, mp3, opus, wav, flac, thumbnail` with their quality lists) — served from
`api/v2/capabilities`'s internal model so there is exactly one catalogue in the codebase.
Per-chat config is persisted in `telegram_chats` (SQLite) instead of
`telegram_bot_config.json`; the legacy file is imported once (§5.5.5).

### 11.3 Message → jobs

1. Extract URLs with `URL_RE = https?://[^\s<>()\[\]{}"']+`, `rstrip` each of `.,;:!?)]}>'"`,
   dedupe preserving order.
2. If `count > TELEGRAM_MAX_URLS_PER_MESSAGE`: reply
   `Too many links in one message (N). Maximum allowed: M.` and truncate.
3. SSRF guard (`urls::validate`): scheme ∈ {http, https}; host required; reject `localhost` and
   `*.local`; if the host parses as an IP, reject loopback/private/link-local/multicast/reserved/
   unspecified/unique-local. **Added**: also reject `0.0.0.0/8`, IPv4-mapped IPv6
   (`::ffff:10.0.0.1`), and `[::1]`. Rejected URLs are reported as
   `Ignored invalid links:\n- <url> (<reason>)`.
4. `_normalize_download_selection` port: the stored legacy `format`/`quality` pair maps to the
   4-tuple — audio formats ⇒ `audio`; `thumbnail` ⇒ `thumbnail/jpg/best`; `captions` ⇒
   `captions/srt/best`; `quality == "audio"` ⇒ `audio/m4a/best`; `quality == "best_ios"` ⇒
   `video/ios/best`; else pass through.
5. One `EngineCmd::Add` with **all** the URLs in a single batch and
   `source = Source::Telegram { chat_id, message_id }` — this is the replacement for the
   `contextvars` hack, and it means *every* job the bot creates is attributable, including
   playlist children (they inherit the parent's `source`).
6. Reply `Queued N link(s) with current chat config.` and, if any failed,
   `Some links failed:\n- <url>: <msg>`.

### 11.4 Live progress message (new)

One **board** message per chat, created on the first job of a burst and edited in place.

Rendering (`render.rs`), Markdown-free plain text to avoid entity-escaping bugs:

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
- Bar = 10 blocks; group lines show `[done/total]` instead of a byte rate.
- Max 12 lines; overflow becomes `… +N more`.
- Terminal lines stay for 60 s with ✅ / ❌ / 🚫 then drop off.
- The board is deleted (or edited to a one-line summary
  `✅ 4 downloads finished · ❌ 1 failed`) 60 s after the last job ends. `AULOS_TELEGRAM_BOARD=board|per_job`
  selects board mode (default) or one message per job.

Rate-limit design (this is the part that breaks naive implementations):

| Limit | Reality | Our budget |
|---|---|---|
| Per-chat message/edit rate | Telegram enforces roughly 1 msg/s sustained per chat, ~20/min for groups; `editMessageText` counts against it | one `governor` `RateLimiter` per chat: quota = 1 edit per `AULOS_TELEGRAM_EDIT_INTERVAL_MS` (default **3000 ms**), burst 1 |
| Global bot rate | ~30 msg/s | one global `RateLimiter`, quota 20/s, burst 5 |
| `message is not modified` (400) | Telegram rejects an edit whose text is unchanged | we compare against `last_rendered` and skip the API call entirely |
| `429 Too Many Requests` with `retry_after` | must be respected or the bot gets throttled harder | on `RequestError::RetryAfter(d)` we (a) sleep the chat's limiter by `d + 250 ms` via `jitter`, (b) **double** that chat's effective edit interval up to 30 s, (c) halve it back after 3 consecutive successful edits |
| Network / 5xx | teloxide's default `throttle` + our retry | 3 attempts with exponential backoff, then drop this edit (the next tick will carry newer data anyway — never queue stale edits) |

The 1 Hz tick walks `chat_msgs`, and for each `dirty` board whose chat limiter has a permit and
whose rendered text differs, issues one `editMessageText`. If a chat is over budget, the board
simply stays stale until the next permit — progress is idempotent, so nothing is lost.
`teloxide::adaptors::Throttle` is **also** enabled as a second line of defence.

### 11.5 Discrete notifications (parity, kept)

| Event | Message | Notes |
|---|---|---|
| item finished | `✅ Download complete: {title}` + `\nFile: {filename}` when known | identical to legacy |
| item error | `❌ Download failed: {title}\n{msg or error or "Download failed"}` | identical |
| item canceled | silent (watch dropped) | identical |
| stall | `⚠️ Download seems stalled for {secs}s:\n{url}` once per chat per job | trigger: `now - last_progress > TELEGRAM_STALL_TIMEOUT_SECONDS` (default 180); watchdog runs on the 1 Hz tick (legacy: 15 s poll) |
| hard timeout | `⏱️ Download is taking longer than expected ({secs}s):\n{url}` once per chat per job | `now - started > TELEGRAM_HARD_TIMEOUT_SECONDS` (default 7200) |

Neither timeout cancels the download (parity). In board mode the two warnings are sent as
**separate** messages (they are alerts, not state) and the board line gets a `⚠️`/`⏱️` marker.

### 11.6 The `Notifier` seam (for APNs later)

```rust
#[async_trait]
pub trait Notifier: Send + Sync {
    fn id(&self) -> &'static str;
    async fn on_event(&self, ev: &DomainEvent);
    /// Which items this notifier cares about; lets the dispatcher skip work.
    fn interested(&self, item: &ItemView) -> bool;
}
```
`aulos-telegram` provides `TelegramNotifier` (interested iff `item.source` is
`Telegram{chat_id}` **or** `AULOS_TELEGRAM_WATCH_ALL=true`, which is a new opt-in that fixes the
legacy blind spot where web/subscription downloads were invisible to the bot). An APNs notifier
later implements the same trait with `interested = |_| true` and a device-token table — zero
changes anywhere else.

---

## 12. Post-completion hooks (`aulos-hooks`)

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &'static str;
    fn applies(&self, item: &Item) -> bool;
    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError>;
    fn ordering(&self) -> i16;              // lower runs first
}
pub struct HookDispatcher { hooks: Vec<Arc<dyn Hook>>, /* per-hook inbox + concurrency 2 */ }
```

The dispatcher subscribes to `DomainEvent::Completed`. Hooks run **outside** the download slot
(legacy ran cleanup inside the semaphore, blocking the next download — §17.13-P11). Hook failure
never changes the item's status; it is logged, counted, and surfaced in `healthz`.

| Hook | `ordering` | `applies` | Behaviour |
|---|---|---|---|
| `audio_sync` | 10 | `status==finished && selection == (video, mp4, best_remux)` | §12.3 |
| `nfo` | 20 | `status==finished && provider=="streamingcommunity" && AULOS_NFO_ENABLED` | §12.2 |
| `jellyfin` | 90 | `status==finished && JELLYFIN_SYNC_ENABLED` | §12.1 |

### 12.1 Jellyfin refresh with debounce

```rust
struct Jellyfin { client: reqwest::Client, pending: Mutex<Option<PendingRefresh>> }
struct PendingRefresh { first_at: Instant, timer: JoinHandle<()>, count: u32 }
```
- Each completion arms/extends a `AULOS_JELLYFIN_DEBOUNCE_SECS` (30) trailing timer, but the fire
  time is capped at `first_at + AULOS_JELLYFIN_MAX_WAIT_SECS` (300) so a 500-item playlist still
  refreshes every 5 minutes rather than only at the very end.
- Request:

| Condition | Request |
|---|---|
| `JELLYFIN_LIBRARY_ID` empty | `POST {base}/Library/Refresh` (legacy behaviour: refreshes all libraries) |
| `JELLYFIN_LIBRARY_ID` set | `POST {base}/Items/{id}/Refresh?metadataRefreshMode={JELLYFIN_METADATA_REFRESH_MODE}&imageRefreshMode={JELLYFIN_IMAGE_REFRESH_MODE}&replaceAllMetadata=false&replaceAllImages=false` — the **targeted** refresh, per BRIEF §13 |

  Headers: `Accept: application/json`, `Authorization: MediaBrowser Token="<key>"`, no body.
  Timeout `JELLYFIN_SYNC_TIMEOUT_SECONDS`. A targeted refresh that returns 400/404 falls back
  once to the global `/Library/Refresh` and logs a WARN naming the bad library id (a mistyped
  `JELLYFIN_LIBRARY_ID` must not silently disable sync).
- Error mapping keeps the legacy messages: `Jellyfin refresh failed with HTTP {code}: {details}`
  where `details` prefers the JSON `message`/`Message` field; transport errors become
  `Jellyfin refresh request failed: {err}`.
- Retries: 3 attempts, 2 s/8 s backoff, then give up until the next completion.
- `healthz.components.jellyfin` reports `{status, last_success_at, last_error, pending}`.

### 12.2 NFO generation (now wired)

Port of `jellyfin_nfo_generator.py`, in-process with `quick-xml`, applied to SC items:

- Input: the item's `entry_json` (SC entries are stored whole) — **not** the on-disk
  `.info.json`, so there is nothing to delete and no race with the user.
- Output: `<base>.nfo` next to the produced file. Root element `episodedetails` when any of
  `series`/`season_number`/`episode_number` is set, else `movie`.
- Elements, in legacy order: `title`, `originaltitle`, (`showtitle`, `season`, `episode`,
  `subtitle`), `plot`, `year`, `premiered` (from `upload_date` `YYYYMMDD`), `dateadded`
  (`%Y-%m-%d %H:%M:%S` UTC now), `studio`, `director` (from `uploader` or `channel`),
  `uniqueid type="streamingcommunity"|"youtube"`, `website` (`original_url` or `webpage_url`),
  up to 20 `tag`, `runtime` (whole minutes). Pretty-printed, no blank lines.
- The SC downloader still writes `<safe_title>.info.json` (parity — some users' own `Exec`
  postprocessors consume it). After a successful NFO write we delete it **only when**
  `AULOS_NFO_DELETE_INFO_JSON=true` (default `false`). Legacy's CLI always deleted it; making
  that opt-in avoids surprising anyone who relied on the file. Once the NFO exists, the item's
  `entry_json` is dropped from the DB.

### 12.3 `best_remux` audio-sync fix (in-process)

Port of `audio_sync_fix.py`, no `Exec` postprocessor, no `/app/app/...` hard-coded path:

1. Skip unless the produced file exists and ends in `.mp4`.
2. `ffprobe -v error -select_streams v -show_entries stream=codec_type -of json` — skip when
   there is no video stream.
3. Duration: `ffprobe -v error -show_entries format=duration -of json` with a 30 s timeout;
   `timeout = max(600, ceil(duration / 2))` s, or `1800` when unknown (matches commit `0b350d6`).
4. `ffmpeg -y -loglevel warning -i <file> -map 0 -dn -ignore_unknown -c copy -c:a aac -b:a 256k -movflags +faststart <tmp>`
   into a sibling temp file, then `rename(tmp, file)` and update `size` in the DB.
5. While this runs, the item is in status `postprocessing` with `msg = "Re-encoding audio"` —
   which is exactly the state legacy could not express (§17.13-C2). On failure: WARN, temp file
   removed, the item stays `finished` with the original file (legacy's `Exec` failure made yt-dlp
   report a postprocessor error, i.e. the whole download looked broken — a worse outcome).
6. Emits progress by parsing `-progress pipe:1` when the input duration is known, so a 40-minute
   re-encode shows a moving bar instead of a frozen UI.

---

## 13. Subscriptions (`aulos-subscriptions`)

### 13.1 Model and public projection

`SubscriptionRecord` mirrors the legacy dataclass; the wire projection is the legacy
`to_public_dict()` **plus three additive keys** (v2 only; the v1 shim emits exactly the legacy 13):

```json
{ "id":"9c1f…", "name":"Veritasium", "url":"https://www.youtube.com/@veritasium",
  "enabled":true, "check_interval_minutes":60, "download_type":"video", "codec":"auto",
  "format":"any", "quality":"best", "folder":"", "last_checked":1757000100000,
  "seen_count":314, "error":null,
  "next_due":1757003700000, "consecutive_failures":0, "checking":false }
```
`last_checked` is **milliseconds** in v2; the v1 shim divides by 1000 and emits a float, matching
legacy's `time.time()`. Secrets/knobs (`custom_name_prefix`, `ytdl_options_*`, the seen set) stay
unexposed, as legacy.

### 13.2 Scheduler

One `tokio::task` per subscription:

```
loop {
    sleep_until(next_due)                    // persisted, so a restart keeps the schedule
    if !enabled { park until notified }
    permit = check_slots.acquire().await     // AULOS_SUB_CHECK_CONCURRENCY (2)
    result = check_one(sub).await            // has its own AULOS_SUB_CHECK_TIMEOUT_SECS (180)
    match result {
      Ok(_)  => { failures = 0; next_due = now + interval + jitter(±10%) }
      Err(e) => { failures += 1;
                  backoff = min(interval * 2^min(failures,8), AULOS_SUB_BACKOFF_MAX_SECS /*6 h*/);
                  next_due = now + backoff + jitter(±10%);
                  error = e.to_string() }
    }
    persist(last_checked = now, next_due, failures, error)   // one row + new seen ids only
    publish(SubscriptionChanged)
}
```

| Legacy problem | Fix |
|---|---|
| First check at boot **+60 s** | First check at `now + AULOS_SUB_FIRST_CHECK_DELAY_SECS` (10) `+ jitter(0..30 s)`, or at `next_due` if that is later. Jitter spreads N subscriptions so 40 channels don't hit YouTube in the same second. |
| 60 s tick granularity | Per-subscription timers: a 5-minute subscription fires at 5 minutes, not at the next 60 s multiple. |
| Sequential checks; one slow feed blocks all | `AULOS_SUB_CHECK_CONCURRENCY` permits + a per-check timeout. |
| Failure did not update `last_checked` ⇒ hot retry every 60 s forever | `last_checked` **always** updated; exponential backoff to 6 h. |
| `POST /subscriptions/check` awaited every check | 202 + `job_id`; progress observable via the `subscription` frames (`checking: true/false`). |
| A restart reset the schedule | `next_due` and `consecutive_failures` are persisted. |

`enabled = false` parks the task (a `Notify` wakes it on update). Adding/deleting a subscription
spawns/aborts its task. All tasks live in a `JoinSet` so shutdown is one `abort_all()`.

### 13.3 Check algorithm (parity where it matters)

1. Flat extraction through the **provider** (`resolve` with `flat: true, playlistend:
   SUBSCRIPTION_SCAN_PLAYLIST_END`), so subscriptions work for any provider, not just yt-dlp.
   For `ytdlp` the option layering is the **same order as everywhere else**
   (MeTube keys after user options) — fixing the legacy asymmetry where `YTDL_OPTIONS` could
   break subscription extraction but not normal adds (§17.13-C13).
2. `_is_media_entry` port: not `playlist|multi_video|channel`, no `entries`, has `webpage_url|url`,
   and — when `ie_key`/`extractor_key` contains `playlist|channel|tab` — at least one of
   `duration, timestamp, release_timestamp, upload_date, view_count, live_status, availability`
   is non-null.
3. Tab-page recursion: if no media entries and depth < 1, try the first up to 5 child URLs
   (handles YouTube "channel of tabs"). Kept verbatim.
4. `_type == "video"` or zero entries ⇒ `error = "This URL points to a single video, not a channel
   or playlist. Use Download instead."`, and this counts as a **failure** for backoff purposes
   (legacy hot-retried it forever).
5. New items = entries whose `media_id` is not in `subscription_seen`, **plus** already-seen
   entries with `live_status == "is_live"` (parity: a live stream is re-queued when it goes live).
6. Queue each as one `EngineCmd::Add` batch with `source = Source::Subscription { id, name }`.
   Entries that fail validation are **not** marked seen (parity — they retry) and their messages
   are collected into `error` (first 3, `"; "`-joined).
7. `MarkSeen` writes only the new ids; `PruneSeen` trims to `SUBSCRIPTION_MAX_SEEN_IDS` by
   `seen_at DESC`.
8. Backfill suppression on **subscribe** is preserved exactly: every currently visible media id is
   marked seen without queueing, **except** entries with `live_status == "is_upcoming"`.

---

## 14. The binary (`aulos-server`)

### 14.1 Startup order

```
1  parse env → Config (fatal on error, exit 2)                       §8.1
2  init tracing (LOGLEVEL, AULOS_LOG_FORMAT, third-party dampening)  §14.4
3  log the effective config table (secrets redacted)
4  mkdir -p DOWNLOAD_DIR, AUDIO_DOWNLOAD_DIR, TEMP_DIR, STATE_DIR, dirname(AULOS_DB_PATH)
5  open SQLite, run migrations
6  if the DB was just created → run the legacy importer (one txn)     §5.5
7  load YTDL_OPTIONS + presets (fatal on error); adopt STATE_DIR/cookies.txt if present
8  discover command plugins; build the provider registry
9  `doctor` probes (ffmpeg, ffprobe, N_m3u8DL-RE, python3, yt-dlp, deno) — WARN, not fatal,
   except python3+yt-dlp which are fatal (the ytdlp provider is the fallback for everything)
10 spawn the POT supervisor (if AULOS_POT_ENABLED)                    §14.2
11 spawn Store actor, EventHub, ProgressAggregator, QueueEngine
12 boot recovery (re-queue in-flight items, recompute groups)         §7.8
13 spawn HookDispatcher, SubscriptionScheduler, ClearScheduler, ConfigWatcher
14 spawn the Telegram actor (if enabled and configured)
15 bind HOST:PORT (TLS if HTTPS), start axum with graceful shutdown
16 log "aulos-server <version> listening on <addr><prefix> (v1 shim: on/off)"
```

Steps 5–12 complete **before** the listener binds, so the first client request already sees a
consistent snapshot. `SO_REUSEPORT` is set when available (parity with legacy's
`supports_reuse_port()`), which also makes a blue/green port swap on the VPS possible.

### 14.2 POT sidecar supervisor

```rust
struct PotSupervisor { cmd: Vec<String>, url: Url, state: Arc<ArcSwap<PotState>> }
struct PotState { status: Status, pid: Option<u32>, restarts: u32,
                  last_exit: Option<String>, last_probe: Option<ProbeResult>,
                  since: SystemTime }
```
- Spawns `bgutil-pot server` with `process_group(0)`, stdout/stderr piped into `tracing`
  (`target = "bgutil_pot"`, INFO for stdout, WARN for stderr).
- On exit: log the status/signal, then restart with backoff `1s, 2s, 4s, …, 60s` (cap), resetting
  the backoff after 60 s of healthy uptime. `restarts` is monotonic and exposed.
- Health probe every 15 s: `GET {AULOS_POT_URL}/ping` (falling back to a TCP connect on the
  host:port if the endpoint 404s, because the provider's route set may change across versions).
  Three consecutive failures ⇒ `status = "down"` and a `health` frame.
- After `AULOS_POT_MAX_RESTARTS` (default 10) in 10 minutes, the supervisor enters
  `status = "failed"`, stops restarting, and logs an ERROR with remediation text. The server keeps
  serving — YouTube downloads may start hitting bot checks, which is exactly what the operator
  needs to see in `healthz` rather than guess at.
- On shutdown: `SIGTERM` to the pgid, 5 s grace, `SIGKILL`.

This closes §17.13-P28 (an unsupervised shell child that the healthcheck could not see).

### 14.3 `healthz`

`GET <p>healthz` → `200` when everything required works, `200` with `"status":"degraded"` when an
optional component is down, `503` only when the store is unusable (that is the one thing that
makes the service useless). The Docker `HEALTHCHECK` uses it and therefore only restarts the
container for real failures.

```json
{ "status": "degraded",
  "version": "2026.09.04", "yt_dlp": "2026.8.30.232658.dev0",
  "uptime_s": 43201, "url_prefix": "/", "v1_shim": true,
  "components": {
    "store":         { "status":"ok", "latency_ms":0.42, "wal_bytes":1048576, "db_bytes":41943040 },
    "queue":         { "status":"ok", "downloading":2, "postprocessing":0, "queued":5, "resolving":1,
                       "slots":{"global":{"total":3,"used":2},"streamingcommunity":{"total":1,"used":0}},
                       "progress_dropped_total": 0 },
    "pot":           { "status":"down", "pid":null, "restarts":3,
                       "last_exit":"exited with code 1", "endpoint":"http://127.0.0.1:4416",
                       "detail":"3 consecutive probe failures" },
    "ytdlp_runner":  { "status":"ok", "python":"3.13.5", "yt_dlp":"2026.8.30.232658.dev0",
                       "plugins":["bgutil_ytdlp_pot_provider"] },
    "ffmpeg":        { "status":"ok", "version":"6.1.1" },
    "nm3u8dl":       { "status":"ok", "version":"v0.5.1-beta" },
    "deno":          { "status":"ok", "version":"2.x" },
    "ytdl_options":  { "status":"ok", "update_time":1757000200.412, "presets":2 },
    "telegram":      { "status":"ok", "chats":2, "edits_throttled_total":11 },
    "jellyfin":      { "status":"ok", "last_success_at":1757000300000, "pending":false },
    "subscriptions": { "status":"ok", "total":7, "failing":1, "next_due_in_s":412 },
    "importer":      { "status":"ok", "imported_at":1757000000000, "warnings":2 }
  },
  "ws": { "clients": 2, "frames_total": 10293, "lagged_total": 0 } }
```
`GET <p>healthz?probe=deep` additionally runs the tool probes live (used by `doctor` and by the
runbook), and is rate-limited to one per 10 s.

A separate `GET <p>livez` returns `200 {"ok":true}` with no work at all — for load balancers.

### 14.4 Logging and tracing

- `tracing-subscriber` with `EnvFilter`. Base filter from `LOGLEVEL`; the legacy
  `dampenThirdPartyLoggers()` becomes default directives:
  `hyper=warn,h2=warn,rustls=warn,reqwest=warn,teloxide=warn,notify=warn,sqlx=warn,html5ever=warn,tungstenite=warn`.
  `RUST_LOG` overrides everything (documented escape hatch).
- `AULOS_LOG_FORMAT=json` emits one JSON object per line (`tracing-subscriber`'s `json()` layer)
  for log shippers; `text` is the compact human format with ANSI only when stderr is a TTY.
- Request ids: `tower-http::request_id` (`SetRequestIdLayer` accepting an inbound
  `X-Request-Id`, else a ULID) + `TraceLayer` producing one span per request with
  `method, path, status, latency_ms, request_id`. `ENABLE_ACCESSLOG=false` sets that span to
  DEBUG, `true` to INFO — same on/off knob as legacy.
- Every job gets a span `job{item_id, provider, url_host}`; provider child stderr is logged
  inside it, so grepping one ULID gives the whole story of a download.
- **No eager formatting on hot paths.** Progress logging is `tracing::trace!` with structured
  fields (evaluated lazily), fixing §17.13-P4.
- Secrets (`TELEGRAM_BOT_TOKEN`, `JELLYFIN_API_KEY`, anything in `YTDL_OPTIONS` under keys
  matching `(?i)(cookie|password|passwd|token|key|secret|proxy)`) are replaced with `«redacted»`
  by a `Redact` newtype used in every `Debug`/`Display` impl and in `check-config`.

### 14.5 Signals and graceful shutdown

| Signal | Behaviour |
|---|---|
| `SIGTERM` / `SIGINT` | 1. stop accepting new HTTP connections (axum `with_graceful_shutdown`); 2. send `Close` to WS clients with code 1001; 3. stop the subscription scheduler and the Telegram poller; 4. **let in-flight downloads finish** for up to `AULOS_SHUTDOWN_GRACE_SECS` (default 20); 5. after the grace, SIGTERM each job pgid, 5 s, SIGKILL; 6. mark still-active items `queued` (`msg = "Interrupted by shutdown"`) so the next boot resumes them; 7. drain the store actor, `wal_checkpoint(TRUNCATE)`, close; 8. SIGTERM the POT child; 9. exit 0. |
| `SIGHUP` | reload `YTDL_OPTIONS*` and re-scan `PLUGINS_DIR` (a nice ops affordance; `docker kill -s HUP`). |
| `SIGQUIT` | dump all task backtraces (`tokio-console`-style via `tokio::runtime::Handle::dump()` when `tokio_unstable` is on) at ERROR and continue. Debug aid for a wedged container. |
| Panic in a task | caught by the spawning wrapper (`JoinHandle` result inspected), logged with the span, the owning item is failed with `error = "internal error <request_id>"`. A panic never takes down the process except in the engine/store actors, where it is fatal by design (`abort()` after logging) — a corrupted queue is worse than a restart. |

---

## 15. Auth, security and reverse-proxy posture

The VPS runs Authelia in front of the service; the server itself has no user model, and that does
not change. Concretely:

| Concern | Design |
|---|---|
| Authentication | Delegated to the proxy. The server never redirects on auth failure; if a
`AULOS_TRUSTED_PROXY_AUTH_HEADER` (e.g. `Remote-User`) is configured, its absence on a v2 route
yields `401 {"error":{"code":"unauthorized",…}}` — never a `303` (ios §7.2). |
| WebSocket auth | Cookies flow with the upgrade request; nothing extra needed. Optional
`AULOS_WS_TOKEN` for setups where the proxy cannot pass cookies to `/ws`. |
| CSRF | All mutating v2 routes require `Content-Type: application/json` (so a form POST from
another origin cannot reach them) and are exempt from CORS credentials. |
| Path traversal | `folder`, `custom_name_prefix`, `chapter_template` reject `..` and leading
separators; the resolved download path is compared **component-wise** against the base
(`Path::components()` prefix match after `canonicalize`), fixing the `/downloads-evil` bypass
(§17.13-P21). Symlink escape is rejected. |
| SSRF | The Telegram URL guard (§11.3) is the only place we accept URLs from an untrusted-ish
channel. v2/v1 API adds are trusted (they are behind auth) but the same validator runs with
`allow_private = AULOS_ALLOW_PRIVATE_TARGETS` (default `true` for the API, `false` for Telegram)
so a locked-down deployment can turn it on everywhere. |
| Cookie upload | 1 MB cap, written atomically to `<STATE_DIR>/cookies.txt` with mode `0600`,
then registered as the `cookiefile` runtime override. |
| Secrets in logs | §14.4 redaction. `vps_setup.md` in the legacy tree contains live credentials
(spec §13.29): the runbook's first step is **rotate the Telegram token, the Jellyfin API key and
the WireGuard key**, and `.gitignore`/`git-secrets` are set up in the new repo. |
| Container | Runs as `PUID:PGID` (non-root) after the entrypoint drops privileges; no
`CAP_*` needed; the image sets `USER` only if `PUID` handling is bypassed. `--read-only` root fs
is supported (`TMPDIR` and `/tmp` are the only writable non-volume paths). |

---

## 16. Sequences

### 16.1 Add a single video

```
iOS/share-sheet ──POST <p>api/v2/downloads {url, mp4/1080} ─────────────► api
api      validate (matrix, folder containment, presets, overrides)         ~40 µs
api      mint ULID 01JBQ…AAA, allocate ord=981
api  ──► EngineCmd::Add ───────────────────────────────────────────────► engine
engine   dedupe miss → store.write([InsertItem{status:resolving}])         (batched txn)
engine   publish DomainEvent::Added([view])
engine   ack(ids=[01JBQ…AAA])
api  ◄── 202 {"ids":["01JBQ…AAA"],"seq":10241}                              total ≈ 3–8 ms
hub      frame {"t":"added","seq":10241,"items":[{status:"resolving",percent:0}]}  → all WS clients
engine   spawn resolve task (permit from resolve_slots)
resolve  ytdlp provider → spawn python3 ytdlp_runner.py mode=extract
runner   fd3: hello, info(entry), result(ok)                               ~0.6–4 s (POT)
resolve  EngineCmd::Resolved(Ok([one Video entry]))
engine   store.write([SetResolved{title,media_id,entry:null}, SetStatus{queued}])
engine   push ready → try_acquire(global) OK → spawn run task
run      provider.download(): spawn runner mode=download (own pgid)
run      sink.stage(Preparing) ──► aggregator ──► engine (persist) + priority flush
hub      {"t":"delta","seq":10243,"items":[{"id":"01JBQ…AAA","status":"preparing","title":"Rick…"}]}
runner   progress frames ~10–40/s ──► sink.progress (try_send) ──► aggregator (no locks)
agg      every 250 ms: {"t":"delta","seq":…,"items":[{id,percent,speed,eta,downloaded_bytes}]}
runner   pp MoveFiles finished{filepath} → sink.file/Outcome
run      EngineCmd::Finished{outcome}
engine   store.write([SetOutput, SetStatus{finished}, SetClearAfter]) ; release permit
hub      {"t":"completed","seq":…,"item":{…filename,size,download_url,percent:100}}
hooks    audio_sync? no · nfo? no · jellyfin: arm 30 s debounce
telegram (source=api) not watched unless AULOS_TELEGRAM_WATCH_ALL
```

### 16.2 Add a 500-item playlist

| t | Actor | Action | Client sees |
|---|---|---|---|
| 0 ms | api | validate, mint `01JBQ…GG`, `InsertItem{kind:item,status:resolving}` | `202 {"ids":["01JBQ…GG"]}` |
| 5 ms | hub | `added` (1 item, `resolving`) | one row, spinner |
| 5 ms–6 s | resolve | `ytdlp_runner` `mode=extract` with `extract_flat=true, noplaylist=true` (MeTube keys applied **after** user opts, so a preset cannot break it); returns a `Playlist` with 500 entries | still one row |
| 6 s | engine | `ConvertToGroup{id:01JBQ…GG, children_total:500}` — the id the client already holds becomes the **group** id | row's `kind` flips to `group` in the next delta |
| 6.0–6.4 s | engine | insert children in 5 transactions of 100: ULID + `ord` + `group_id` + `group_index` (1-based, zero-padded index/`playlist_count`/`playlist_autonumber`/`n_entries`/`__last_playlist_index` injected into each entry, `playlist*` outtmpl fields pre-resolved with the sanitiser) | 5 `added` frames of 100 items each (≈70 KB each) |
| 6.4 s | engine | `playlist_item_limit > 0` ⇒ children beyond the limit are not created **and** `playlistend` is set on each child's options (both legacy applications preserved) | |
| 6.4 s→ | engine | schedule 3 (`MAX_CONCURRENT_DOWNLOADS`) children; the rest stay `queued` | 3 rows progressing |
| every 250 ms | agg | one `delta` with ≤3 changed children + one `group` frame with `children_done/total` | one group progress bar + 3 rows |
| … | engine | as each child finishes: `completed` (child) + `group` counter update | |
| end | engine | group status → `finished`/`error`; hooks run per child; Jellyfin fires at most every 300 s during the run and once at the end | |

Cost profile vs legacy: **5 `added` frames instead of 500 socket broadcasts**, **~10 SQLite
transactions instead of 500 whole-file JSON rewrites with 1000 fsyncs**, and the resolution runs
in one child process instead of 500 sequential executor extractions.

### 16.3 Cancel mid-download

```
client ──POST <p>api/v2/items/actions {"action":"cancel","ids":["01JBQ…AAA"]} ──► api
api  ──► EngineCmd::Cancel ─────────────────────────────────────────────────► engine
engine   running.get(id) → cancel_token.cancel()                          t+0
run      provider select! sees cancel → kill(-pgid, SIGTERM)              t+0.2 ms
runner   yt-dlp gets SIGTERM; ffmpeg child in the same pgid also does
run      wait_timeout(AULOS_KILL_GRACE_MS=5000)
           ├─ exited in 40 ms  → ok
           └─ still alive      → kill(-pgid, SIGKILL)                     t+5 s worst case
run      cleanup: remove <tmp>/*.part, *.ytdl, SC seg dir, partial .mp4
run  ──► EngineCmd::Failed{ProviderError::Canceled}
engine   store.write([SetStatus{canceled, msg:"Canceled by user"}]) ; release permit
engine   publish Completed(view{status:canceled})
hub      {"t":"completed","seq":…,"item":{"status":"canceled",…}}          t+1–6 ms
api  ◄── 200 {"applied":["01JBQ…AAA"],"skipped":[],"seq":…}
engine   SlotFreed → schedule() admits the next queued item
```
The HTTP response returns as soon as the token is cancelled and the status is persisted — it does
not wait for SIGKILL. v1 clients calling `POST <p>delete {ids:[url],where:"queue"}` get the same
path plus a `Delete`, and the item disappears (legacy semantics), so the iOS app's optimistic
local removal stays correct.

### 16.4 Server restart with in-flight downloads

```
docker stop  → tini forwards SIGTERM → aulos-server
  t+0     stop accepting HTTP; WS clients get Close(1001)
  t+0     subscription scheduler + telegram poller stopped
  t+0..20 in-flight downloads keep running (AULOS_SHUTDOWN_GRACE_SECS)
  t+20    still-running jobs: SIGTERM pgid, 5 s, SIGKILL
  t+25    store.write([SetStatus{queued, msg:"Interrupted by shutdown", attempt+1} × n])
  t+25    wal_checkpoint(TRUNCATE); POT child SIGTERM; exit 0
--- container replaced ---
  boot    config → tracing → open DB (exists ⇒ no import) → migrations (no-op)
  boot    recovery: resolving→queued, preparing|downloading|postprocessing→queued(attempt+1)
          groups: counters recomputed with one GROUP BY
          clear_after re-armed for terminal items
  boot    listener binds
  t+0     first WS client: {"t":"snapshot", … items with status "queued", percent 0}
  t+0     scheduler admits MAX_CONCURRENT_DOWNLOADS items; yt-dlp resumes from .part where possible
```
Contrast with legacy: it re-`add`ed **every** queued item at once (`__import_queue`), each
re-running metadata extraction on the shared executor before the UI could even connect.
Here the DB already holds the resolved title/entry, so recovery is a single UPDATE and the client
sees real titles instantly.

Hard-kill (`SIGKILL`/OOM/host reboot) case: nothing is written at shutdown, so items remain
`downloading` in the DB; boot recovery's table (§7.8) converts exactly those to `queued`. WAL
guarantees no torn writes. This is the scenario the `recovery` unit tests seed directly.

### 16.5 Subscription tick

```
scheduler(sub 9c1f…)  sleep_until(next_due = 1757003700000)
  wake → enabled? yes → acquire check permit (2 total)
  provider.resolve(url, flat, playlistend=SUBSCRIPTION_SCAN_PLAYLIST_END=50)
     ytdlp_runner mode=extract, lazy_playlist=true          ~1.5 s
  filter to media entries (_is_media_entry port)  → 50 entries
  store.seen(9c1f…) → HashSet of 314 ids (one indexed query, not a 50 000-element JSON parse)
  new = entries not seen (3) + already-seen entries with live_status=="is_live" (0)
  EngineCmd::Add(batch of 3, source=Subscription{9c1f…,"Veritasium"})
     → 3 ULIDs, 3 `added` frames coalesced into 1
  store.write([MarkSeen{3 ids}, PruneSeen{keep:50000},
               UpsertSubscription{last_checked, next_due=now+60min±10%, failures:0, error:null}])
  publish SubscriptionChanged → {"t":"subscription","seq":…,"subscription":{…,"seen_count":317}}
  release permit; loop
```
Failure branch (`ProviderError::Upstream("HTTP 403")`):
`failures 0→1`, `next_due = now + min(60min × 2, 6h) ± 10%`, `error = "HTTP 403"`,
`last_checked = now`. Three more failures ⇒ next check in 6 h, not in 60 s.

### 16.6 Telegram message with 3 URLs

```
user → "check these https://youtu.be/a https://youtu.be/b https://192.168.1.5/x"
bot   authorised chat? yes
bot   extract 3 URLs, dedupe, rstrip punctuation
bot   validate: a ok, b ok, 192.168.1.5 → rejected (private IP)
bot   reply #1: "Ignored invalid links:\n- https://192.168.1.5/x (private address)"
bot   chat config → normalize_download_selection → (video, auto, mp4, best)
bot ──► EngineCmd::Add([a, b], source=Telegram{chat_id, message_id})
engine  2 ULIDs, 2 InsertItem in one txn, Added event
bot   reply #2: "Queued 2 link(s) with current chat config."
bot   creates the board message: "⬇️ Aulos — 2 active, 0 done  …"
--- as progress arrives ---
bot   DomainEvent deltas update JobLines; the 1 Hz tick edits the board when
        (a) the chat limiter has a permit (1 per 3000 ms), and
        (b) the rendered text differs from last_rendered
bot   on 429 RetryAfter(7): sleep the chat limiter 7.25 s, double its interval to 6 s
bot   a finished at 41 s → board line becomes "✅ …", plus the parity message
        "✅ Download complete: Rick Astley…\nFile: Rick Astley….mp4"
bot   b stalls: no progress for 180 s → separate message
        "⚠️ Download seems stalled for 180s:\nhttps://youtu.be/b"  (once per chat)
bot   b finishes at 2 h 15 m → the hard-timeout message already fired at 7200 s
bot   60 s after the last job: board edited to "✅ 2 downloads finished"
```
Edit budget for this burst: 2 h of activity at 1 edit/3 s worst case = 2400 edits, but the board
is only edited when the **text changes**, so in practice a slow download produces ~1 edit per
percent-point change of any visible line (~a few hundred). `edits_throttled_total` in `healthz`
makes over-budget behaviour observable.

### 16.7 `YTDL_OPTIONS_FILE` edit

```
operator: vi /config/ytdl-options.json   (vim writes 4913, then renames tmp → target)
notify (watch on /config, non-recursive) delivers:
   Create("/config/4913"), Remove(…), Modify(Name(To))("/config/ytdl-options.json"), …
ConfigWatcher: keep only events whose file name == "ytdl-options.json"  → 1 accepted
ConfigWatcher: reset the 250 ms debounce; more events arrive → still one reload
t+250 ms  reload: YTDL_OPTIONS (env) → merge file over it → re-apply runtime overrides
   success → ArcSwap::store(new)  ; update_time = mtime = 1757000200.412
   failure → keep the previous YtdlOptions; msg = "YTDL_OPTIONS_FILE contents is invalid"
publish YtdlOptionsReloaded{ok, msg, update_time}
hub  {"t":"ytdl_options","seq":10280,"ok":false,
      "msg":"YTDL_OPTIONS_FILE contents is invalid","update_time":1757000200.412}
healthz.components.ytdl_options.status = "degraded" (until a good reload)
in-flight jobs: unaffected (each snapshotted Arc<YtdlOptions> at spawn)
next job: picks up the new options
```
Deleting the file: reload fails with `File "<path>" not found`, last-good options are kept, the
directory watch survives, and re-creating the file heals it without a restart.
`POST <p>api/v2/ytdl-options/reload` does the same synchronously and returns the outcome — the
supported path for NFS/SMB mounts where inotify never fires. The presets file is watched by the
same machinery (legacy did not watch it despite the README claim).

---

## 17. Legacy behaviour map (spec §1–§12 → this design)

Numbering: **§17.N mirrors legacy-spec §N**, so a reference like §17.5.3 elsewhere in this
document means "legacy spec §5.3". `Pn` refers to the numbered pain points in legacy spec §13;
`Cn` / `Kn` refer to the rows of §17.13 below.

Legend: **K** kept identical · **K\*** kept, implementation differs · **Δ** intentional change
(reason in §17.13) · **✗** dropped (BRIEF out-of-scope).

### 17.1 Config (spec §1)

| Legacy behaviour | Lives in | |
|---|---|---|
| `_DEFAULTS` table, all values start as strings | `core::config::RawEnv` + §8.2 | K |
| `%%KEY` indirection | `config::resolve_indirections` (cycle-checked) | K\* |
| Boolean token set + truthy set, exit 1 on bad token | `config::parse_bool` | K |
| `URL_PREFIX` trailing `/` | `config::normalize_prefix` | Δ (also adds a leading `/`) |
| `PUBLIC_HOST_*` trailing `/` only if non-empty | same | K |
| `.`-prefixed option-file paths resolved absolute | same | K |
| `load_ytdl_options()` order (env, then file over env), exact error strings | `core::ytdl_options` §8.3 | K |
| Presets `dict[str, dict]` invariant + error strings | same | K |
| Runtime overrides (`cookiefile`) re-applied after reload | `kv` table + `YtdlOptions::overrides` | K\* |
| `watchfiles` hot reload of `YTDL_OPTIONS_FILE`, `samefile` filter, `{modified,added,deleted}` | `ConfigWatcher` §8.4 (directory watch + debounce + poll fallback) | K\* |
| Presets file **not** watched | §8.4 | Δ (now watched) |
| `frontend_safe()` 8 keys (two emitted as strings) | `api/v2/capabilities.config` (numbers) + v1 shim (strings) | K\* |
| Env vars outside `_DEFAULTS` (`TELEGRAM_BOT_TOKEN`, `TELEGRAM_ALLOWED_CHAT_IDS`, `METUBE_VERSION`, `PUID`…) | §8.2 | K |
| `JELLYFIN_LIBRARY_ID` / `*_REFRESH_MODE` silently ignored | `hooks::jellyfin` §12.1 | Δ (implemented) |
| Pre-config `basicConfig`, third-party dampening, DEBUG→yt-dlp verbose | §14.4; DEBUG sets `verbose:true` in the runner job | K\* |

### 17.2 REST (spec §2)

| Legacy behaviour | Lives in | |
|---|---|---|
| Every route under `URL_PREFIX` | `api::router` | K |
| `text/plain` JSON bodies | `api::error`/handlers | Δ (`application/json` everywhere) |
| Route table (`add`, `presets`, `cancel-add`, `subscribe`, `subscriptions*`, `delete`, `start`, cookies, `history`, `version`, `robots.txt`, static, OPTIONS) | `api::v1` §10.1 | K |
| `GET <p>` = Angular index + `metube_theme` cookie | status page | Δ/✗ |
| `GET /` → 302 `URL_PREFIX` | `api::v1` | K |
| CORS `on_response_prepare` reflection | `api::cors` §10.7 | K (+ methods on v2) |
| `parse_download_options` validation matrix + messages | `core::request::validate` | K |
| `_migrate_legacy_request` table | `api::v1::legacy_request` §10.2 | K |
| Positional `dqueue.add(...)` argument order | irrelevant (typed struct); the order is asserted in a test that mirrors the legacy test | K\* |
| Business errors as HTTP 200 `{"status":"error"}` | v1 shim only | K (v2 uses 4xx) |
| Validation errors as bare-reason 400 | v1 shim returns the same text in a JSON envelope | K\* |
| `supports_reuse_port()` | `SO_REUSEPORT` when available | K |
| Startup/cleanup hook order | §14.1 / §14.5 | K\* |

### 17.3 Socket.IO (spec §3)

| Legacy | Replacement | |
|---|---|---|
| `socketio.AsyncServer` at `<prefix>socket.io`, default namespace, double-encoded JSON strings | `<prefix>ws`, one JSON object per frame | ✗ / Δ |
| `all` = `[[ [key,info]… ], [ [key,info]… ]]` | `snapshot` with flat items in the **same shape as REST** | Δ (ios §7.4) |
| `added` / `updated` / `completed` (full object, unthrottled broadcast) | `added` / `delta` (changed fields, 250 ms) / `completed` | Δ (ios §7.5) |
| `canceled` / `cleared` = bare url string | `completed{status:canceled}` / `removed{ids}` | Δ (ios §7.3) |
| `configuration`, `custom_dirs` on connect | `api/v2/capabilities`, `api/v2/custom-dirs` | Δ |
| `ytdl_options_changed` | `ytdl_options` frame (identical payload) | K\* |
| `subscriptions_all`, `subscription_added/updated/removed` | `snapshot.subscriptions`, `subscription`, `subscription_removed` | K\* |
| `formats` event (documented by the client, never emitted by the server) | `api/v2/capabilities.formats` | Δ |

### 17.4 Download model (spec §4)

Every `DownloadInfo` field maps to `Item`/`ItemView` as in §4 / §10.4. Specifics:

| Legacy | Here | |
|---|---|---|
| `url` is the primary key | `id` (ULID) is; `url` is indexed data | Δ (ios §7.3) |
| `id` = `entry['id']`, prefixed `"<prefix>.<id>"` | `media_id`, same prefixing; v1 `id` projects from it | K |
| `timestamp` = `time.time_ns()` | `created_at` (ms); v1 multiplies by 1e6 | K\* |
| `entry` full sanitised info dict on the wire | not on the wire at all | Δ |
| `filename`/`chapter_files` created lazily (keys sometimes absent) | always present | Δ (ios §19) |
| `percent` clamp `[0,99.9]`, monotonic per progress source, `100.0` on finish | `progress::percent` (ported + golden tests) | K |
| Status enum `pending/preparing/downloading/finished/error` | 8-value v2 vocabulary; v1 mapping §10.5 | Δ |
| No `postprocessing` status (frozen UI during ffmpeg) | `postprocessing` + `msg` phase text | Δ |
| Transition table incl. cancel/clear paths | §7.5 / §7.7 | K\* |

### 17.5 Queue mechanics (spec §5)

| Legacy | Here | |
|---|---|---|
| 3 `PersistentQueue`s (`queue`, `pending`, `completed`) | one `items` table; `pending` ≡ `queued && !auto_start`, `done` ≡ terminal | K\* |
| `AtomicJsonStore` schema_version 2, tempfile+fsync+rename, `.invalid.<ts>` quarantine | SQLite WAL; the importer reads v1/v2 JSON and never quarantines | Δ |
| Whole-file rewrite per put/delete | batched transactions | Δ (P5) |
| Transient progress fields not persisted | still not persisted | K |
| `entry` compaction rules incl. the whole-SC-entry exception | §5.4 | K |
| Legacy shelf (pickle) import | ✗ (BRIEF); detected and reported | ✗ |
| `initialize()` re-adds all of `queue.json` at once | boot recovery re-queues, scheduler admits `MAX_CONCURRENT_DOWNLOADS` | Δ (C1) |
| `get()` 2-tuple of `[key, info]` pairs | `snapshot` / `history` | Δ |
| Global semaphore + SC semaphore acquired **outside** it | `global` + `provider_slots`, `own_slots()` bypasses global | K |
| Both entry points re-check `canceled` before start | engine checks the token before spawning (same regression test) | K |
| `multiprocessing.Process` + Manager queue + 2 threads/download | one child process + fd-3 JSON lines + async reader | Δ (P8) |
| Child `ytdl_params` construction incl. user-opts-last | `ytdlp::opts` (same precedence) | K |
| `put_status` key allow-list | runner `progress` frame fields | K |
| `put_status_postprocessor` MoveFiles/SplitChapters semantics | runner `pp` frame + `run` handling | K |
| Captions extension filtering + `.srt`→`.txt` conversion | `ytdlp::outcome` (same allow-list, same stripping) | K |
| Thumbnail `.webm`→`.jpg` rewrite | same | K |
| `progress_source` change resets the monotonic clamp | `ProgressCell.progress_source` | K |
| `cancel()` = `proc.kill()` (SIGKILL, orphans) | SIGTERM pgid → grace → SIGKILL | Δ (P10) |
| `_post_download_cleanup` inside the semaphore | permit released before hooks | Δ (P11) |
| Delete `tmpfilename` on non-finished | full partial cleanup (`.part`, `.ytdl`, tmp dir, seg dir) | Δ (P23) |
| `CLEAR_COMPLETED_AFTER` timer (lost on restart) | persisted `clear_after` | Δ |
| Add recursion guard (`already` URL set), `_canceled_urls`, `_add_generation` | `dedupe`, cancel registry, `add_generation` (+ depth cap) | K\* |
| `__extract_info` MeTube-keys-after-user-opts + strict retry rule | `ytdlp` resolve + runner strict retry | K |
| Playlist/channel field injection (`{etype}_index` zero-padded, `_count`, `_autonumber`, `n_entries`, `__last_playlist_index`, parent props) | `ytdlp::outtmpl` + engine expansion | K |
| `playlist_item_limit` applied twice (slice + `playlistend`) | same | K |
| Dedupe checks only `queue` | checks queued + active (`pending`/`done` also considered) | Δ (P18) |
| `__calc_download_path` messages + `startswith` containment | same messages, component-wise containment | Δ (P21) |
| `OUTPUT_TEMPLATE_PLAYLIST`/`_CHANNEL` swap + `_resolve_outtmpl_fields` + `_sanitize_path_component` | `ytdlp::outtmpl` (pre-resolution done by a short `mode=outtmpl` runner call, so yt-dlp's own `evaluate_outtmpl` and full template syntax are preserved) | K\* |
| Chapter template: global at construction, per-download only when `split_by_chapters` | same | K |
| `_build_ytdl_options` layering, `null` preserved | `YtdlOptions::layer` | K |
| `impersonate` string → `ImpersonateTarget` | runner `coerce` | K\* |
| `auto_start is True` comparison | accepts bools + boolean strings | Δ (P22) |
| `start_pending` / `cancel` / `clear` semantics | `Start` / `Cancel` / `Delete` (§7.5, §7.7) | K\* |
| `DELETE_FILE_ON_TRASHCAN` deletes only `filename` | also chapter/subtitle/info.json | Δ (P20) |
| `get_custom_dirs()` 5 s memo + recursive glob on the event loop | `api/v2/custom-dirs`: same exclusion regex, walk on `spawn_blocking`, 30 s cache, `max_depth = AULOS_CUSTOM_DIRS_MAX_DEPTH` (8) | Δ (P14) |
| Jellyfin hook per finished download | debounced hook §12.1 | Δ |
| ffmpeg PP timeout `max(600, ceil(dur/2))`, 1800 unknown | `hooks::audio_sync` | K |

### 17.6 `dl_formats` (spec §6)

`aulos-provider-ytdlp::formats` / `::opts` are a literal port: `AUDIO_FORMATS`, `CAPTION_MODES`,
`CODEC_FILTER_MAP`, the `get_format` decision table (including `custom:` first, the `ios`
selector chain, `best_remux` → `bestvideo+bestaudio/best`, and the quirk that `quality=="worst"`
produces no `worst*` selector), and the `get_opts` branch table (audio PP chain with the
`writethumbnail` guard and the **string** `preferredquality`, thumbnail, `best_remux`
`opts.pop("format")` + `merge_output_format` + `FFmpegVideoConvertor` + the late audio-sync step,
captions per-mode `subtitleslangs` ordering). **K**, with two deltas:
- the late `Exec` postprocessor is replaced by the in-process `audio_sync` hook (§12.3);
- `preferredquality` stays a string for byte-compatibility with legacy output, with a comment.

### 17.7 Subscriptions (spec §7)

| Legacy | Here | |
|---|---|---|
| Data model + `to_public_dict()` 13 keys | §13.1 (v1 shim = exactly 13) | K |
| `subscriptions.json` whole-file rewrite; legacy shelf import | `subscriptions` + `subscription_seen` tables; JSON importer | Δ |
| `timestamp` in-memory only, not persisted | same (not persisted) | K |
| 60 s tick, first check at +60 s, sequential checks | per-sub timers, first check ~+10 s, bounded concurrency | Δ (C9) |
| `extract_flat_playlist` params with user options **last** | same order as normal adds (MeTube keys last) | Δ (C13) |
| `_is_media_entry`, `_entry_id`, `_entry_video_url`, tab recursion depth 1 / first 5 children | ported verbatim | K |
| `add_subscription`: duplicate url + in-flight `_pending_urls` guard, `Missing URL`, `Could not resolve URL`, `VIDEO_ONLY_MSG` | ported verbatim (unique index + in-flight set) | K |
| Backfill suppression except `is_upcoming` | ported verbatim | K |
| New = unseen + already-seen `is_live` | ported verbatim | K |
| Failed entries not marked seen; first 3 errors joined | ported verbatim | K |
| `seen_ids` dedupe + truncate newest-first | `subscription_seen` + `PruneSeen` | K\* |
| Extraction failure leaves `last_checked` untouched (hot retry) | always updates + exponential backoff | Δ (P17) |
| `update_subscription`: only `enabled`/`interval`/`name`, `ValueError` → 500 | same fields, 400 on bad input | Δ (C25) |
| `delete_subscriptions` always ok, emits per id | same | K |
| `folder == ""` → `None` | same | K |

### 17.8 Telegram (spec §8)

All of §8 is **K** except: attribution via `Source` instead of `contextvars` (**Δ**, and it now
covers playlist children *and* optionally web/subscription jobs); the 15 s monitor loop becomes a
1 Hz tick (**K\***); per-chat config moves from `telegram_bot_config.json` to SQLite (**K\***,
imported once); and the **new** live progress board (§11.4) which legacy did not have at all.
Exact texts, the `cfg:` callback grammar, the `[0,1,5,10,20]` limit keyboard, the URL regex and
trailing-punctuation set, the max-URLs message, the SSRF guard, `_normalize_download_selection`,
the ✅/❌ completion messages and both watchdog messages are preserved verbatim.

### 17.9 StreamingCommunity (spec §9)

All of §9 is **K** (§6.3 lists the module per step) except: episode metadata fetching is
concurrent (**Δ**, faster, same output); the debug-only `GET` of the m3u8 is removed (**Δ**, P27);
`SC_THREAD_COUNT`/`SC_USE_FFMPEG` come from `Config` rather than being re-read from the
environment inside a child (**Δ**, single source of truth); and the HTTP client is
`wreq`/`reqwest` behind a trait instead of `curl_cffi` (**K\***). Output naming
(`<download_dir>/<sanitised title>.mp4` + `.info.json`, ignoring `OUTPUT_TEMPLATE*`) is
deliberately **kept** — users' Jellyfin libraries already depend on those paths (§17.13-K1).

### 17.10 Jellyfin / NFO / audio-sync (spec §10)

| Legacy | Here | |
|---|---|---|
| `POST {base}/Library/Refresh`, `MediaBrowser Token`, no body, error message shapes | `hooks::jellyfin` | K |
| Refreshes all libraries; `JELLYFIN_LIBRARY_ID` inert | targeted `Items/{id}/Refresh` when set, with fallback | Δ |
| One refresh per finished download | debounce 30 s / cap 300 s | Δ |
| `jellyfin_nfo_generator.py` unwired CLI that deletes `info.json` | in-process hook for SC items; deletion opt-in | Δ |
| `audio_sync_fix.py` as an `Exec` PP at a hard-coded path | in-process hook, `postprocessing` status, progress | Δ |

### 17.11 BgUtils POT (spec §11)

| Legacy | Here | |
|---|---|---|
| Sidecar binary at `/usr/local/bin/bgutil-pot`, arch-matched, from the latest release tag | same, but the tag is **pinned** via a build arg (§18.1) | Δ |
| yt-dlp plugin zip unpacked into site-packages | same; the runner logs the discovered plugin list in its `hello` frame, and `healthz` shows it | K\* |
| Started by the entrypoint as an unsupervised `&` child | supervised by the server with backoff + health probe | Δ (P28) |
| deno installed for `yt-dlp[deno]` / `yt-dlp-ejs` | same | K |
| `extractor_args` via `YTDL_OPTIONS` | same (option dicts are passed through untouched) | K |

### 17.12 Process / deploy (spec §12)

Dockerfile, entrypoint and CI are re-derived in §18/§21. Preserved: PUID/PGID/UID/GID precedence,
`umask`, `mkdir -p` of the three dirs, `CHOWN_DIRS` gating, the root warning, `gosu` privilege
drop, `tini -g` as PID 1, the `/downloads` volume, `EXPOSE 8081`, all the `ENV` defaults, the
`VERSION`→`METUBE_VERSION` build arg, and multi-arch amd64/arm64. Changed: the healthcheck hits
`healthz` (and honours `URL_PREFIX`), the Node/Angular build stage is gone, and Python exists only
for the runner shim + yt-dlp.

### 17.13 Intentional changes (the complete list)

| # | Change | Why it is better for the user |
|---|---|---|
| C1 | Boot does not restart everything at once; items are re-queued and admitted `MAX_CONCURRENT_DOWNLOADS` at a time. | A restart with 80 queued items no longer saturates the box, the network and the executor before the app can even connect. |
| C2 | New statuses `resolving`, `postprocessing`, `canceled`. | The two states the old UI lied about: "nothing is happening yet" and "ffmpeg is remuxing a 4 GB file". Progress bars stop looking frozen. |
| C3 | `canceled` items are retained (v2) instead of vanishing. | A cancel is now visible and auditable; v1 clients still see them disappear (§10.4), so nothing regresses. |
| C4 | Progress is batched (250 ms), delta-encoded, serialised once. | The single biggest snappiness fix (spec §13.1–2, ios §7.5): the app can delete its own throttle and stops burning battery parsing full metadata trees. |
| C5 | One immutable server-assigned ULID used by every endpoint and event. | Deletes the client's `id → url → UUID` fallback chain and the "skip items with no url" bug in *Clear completed*. |
| C6 | `POST add` returns before metadata extraction. | Deletes the whole background-upload/app-group/staging/dedup/sweeper machinery in the share extension (ios §7.1) and makes the share sheet honest. |
| C7 | Honest HTTP: 4xx/401/202, `application/json`, error envelope. | The client no longer has to treat "2xx with an HTML body" as an expired session. |
| C8 | `?since=<seq>` + `ETag` on state. | Pull-to-refresh becomes a 304 instead of a full history payload, and no longer needs to tear down the socket. |
| C9 | `POST subscriptions/check` returns immediately; checks are concurrent and bounded. | A slow feed can no longer hang an HTTP request for minutes or block every other subscription. |
| C10 | Subscription exponential backoff, `last_checked` always updated, first check ~10 s after boot with jitter. | A dead feed stops hammering YouTube every 60 s forever; a fresh boot picks up new videos immediately instead of after a minute. |
| C11 | Jellyfin debounce + targeted library refresh; `JELLYFIN_LIBRARY_ID`/refresh-mode vars implemented. | A 500-item playlist triggers a handful of scans instead of 500, and only rescans the library that changed — minutes instead of hours of Jellyfin CPU. |
| C12 | NFO generation and the audio-sync re-encode run in-process. | NFOs actually get written (the legacy script was never wired), and a failed audio-sync no longer makes a perfectly good download report as failed. |
| C13 | Subscription extraction uses the same option precedence as normal adds. | A `YTDL_OPTIONS` that works for downloads can no longer silently break every subscription. |
| C14 | Cancel = SIGTERM to the process group, then SIGKILL. | No more orphaned ffmpeg/N_m3u8DL-RE processes eating CPU after a cancel, and partial files are cleaned up. |
| C15 | Dedupe considers pending and active items. | Re-adding a URL that is already pending no longer silently replaces the first entry. |
| C16 | Path containment is component-wise; symlink escape rejected. | `/downloads-evil` no longer passes as inside `/downloads`. |
| C17 | `auto_start` accepts boolean strings. | An iOS Shortcut sending `"true"` no longer has its download silently parked in *pending*. |
| C18 | `CLEAR_COMPLETED_AFTER` survives restarts. | The setting now actually means what it says. |
| C19 | `DELETE_FILE_ON_TRASHCAN` deletes chapter/subtitle/`.info.json` siblings; `subtitle_files` is persisted. | No orphaned files after a delete; caption downloads keep their file list after a restart. |
| C20 | `YTDL_OPTIONS_FILE` reload keeps the last-good config on failure, watches the directory, debounces, has a poll fallback and a manual reload endpoint; the presets file is watched too. | A typo in the options file no longer silently changes behaviour, and atomic-replace edits (vim, Ansible, `docker cp`) are actually detected. |
| C21 | POT sidecar supervised, health surfaced in `healthz`; the Docker healthcheck hits `healthz`. | "YouTube suddenly wants a login" becomes a visible red component instead of a mystery. |
| C22 | `entry` (the full yt-dlp info dict) is no longer sent to clients. | Payloads shrink by 10–100×; the client's own comments say logging them stalls the UI. |
| C23 | Automatic retry for timeouts/5xx/429 (max 2), plus a real `retry` action. | Transient failures self-heal; a failed item is one tap from retrying instead of delete-and-re-share. |
| C24 | Bounded resolution pool separate from download slots. | A 500-item playlist can never starve downloads or the API. |
| C25 | `subscriptions/update` returns 400 instead of leaking a 500. | Actionable errors. |
| C26 | Telegram: live progress board with rate-limit awareness; optional watching of non-Telegram jobs. | The bot finally answers "how far along is it?" without spamming, and can report web/subscription downloads too. |
| C27 | Socket.IO, the Angular UI, the `metube_theme` cookie and pickle/shelve import are dropped. | BRIEF scope. The one real consequence — no live updates for the *current* iOS build — is handled by shipping the v2 client in the same cutover (§20). |
| K1 | **Not changed on purpose:** SC output naming, the `[0,99.9]` percent clamp, `preferredquality` as a string, the exact SC N_m3u8DL-RE argv, the gapless natural-order concat, the legacy validation matrix and all legacy error strings. | Existing Jellyfin libraries, user scripts and muscle memory depend on them. |

---

## 18. Packaging (`docker/`)

### 18.1 Dockerfile (multi-stage, multi-arch)

```dockerfile
# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.95
ARG DEBIAN=bookworm

# ---------- planner / builder (cargo-chef for a cached dependency layer) ----------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-${DEBIAN} AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETPLATFORM
# cross-compile: linux/amd64 -> x86_64-unknown-linux-gnu, linux/arm64 -> aarch64-…
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    set -eux; case "$TARGETPLATFORM" in \
      linux/amd64) T=x86_64-unknown-linux-gnu;  P=""              ;; \
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
ARG BGUTIL_TAG=v1.2.3                 # PINNED (legacy resolved "latest" at build time)
ARG NM3U8DL_VERSION=v0.5.1-beta
ARG NM3U8DL_BUILD=20251029
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl unzip file tini gosu \
      ffmpeg aria2 coreutils \
      python3 python3-pip \
      libssl3 libstdc++6 \
 && rm -rf /var/lib/apt/lists/* && mkdir -p /.cache && chmod 777 /.cache

# yt-dlp: the nightly pin, exactly as the legacy image did
RUN pip3 install --break-system-packages --no-cache-dir --no-deps "yt-dlp==${YTDLP_VERSION}"

# deno (yt-dlp[deno] / yt-dlp-ejs JS challenge solver)
RUN curl -fsSL https://deno.land/install.sh | DENO_INSTALL=/usr/local sh -s -- -y

# BgUtils POT: sidecar binary + yt-dlp plugin into site-packages
RUN set -eux; case "$TARGETARCH" in amd64) A=x86_64 ;; arm64) A=aarch64 ;; \
      *) echo "unsupported $TARGETARCH" >&2; exit 1 ;; esac; \
    B=https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs/releases/download/${BGUTIL_TAG}; \
    curl -fL -o /usr/local/bin/bgutil-pot "$B/bgutil-pot-linux-${A}"; chmod +x /usr/local/bin/bgutil-pot; \
    PD="$(python3 -c 'import site; print(site.getsitepackages()[0])')"; \
    curl -fL -o /tmp/p.zip "$B/bgutil-ytdlp-pot-provider-rs.zip"; unzip -oq /tmp/p.zip -d "$PD"; rm /tmp/p.zip

# N_m3u8DL-RE (StreamingCommunity HLS)
RUN set -eux; A=$([ "$TARGETARCH" = "arm64" ] && echo arm64 || echo x64); \
    curl -fL "https://github.com/nilaoda/N_m3u8DL-RE/releases/download/${NM3U8DL_VERSION}/N_m3u8DL-RE_${NM3U8DL_VERSION}_linux-${A}_${NM3U8DL_BUILD}.tar.gz" \
      -o /tmp/n.tgz && tar -xzf /tmp/n.tgz -C /usr/local/bin && chmod +x /usr/local/bin/N_m3u8DL-RE && rm /tmp/n.tgz

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
    PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 \
    RUST_BACKTRACE=1
VOLUME /downloads
EXPOSE 8081
HEALTHCHECK --interval=30s --timeout=5s --start-period=25s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT}${URL_PREFIX:-/}healthz" >/dev/null || exit 1
ARG VERSION=dev
ENV METUBE_VERSION=$VERSION AULOS_VERSION=$VERSION
ENTRYPOINT ["/usr/bin/tini","-g","--","/usr/local/bin/aulos-entrypoint"]
```

Notes: no Node stage (≈300 MB and one whole toolchain gone); Rust cross-compiles on the build
host so the arm64 image does not need QEMU emulation for the compile (only for the few `RUN`s in
the runtime stage, which are downloads); `cargo-chef` keeps dependency compilation cached across
commits (the yt-dlp bump PRs then rebuild in ~2 minutes because only the runtime stage changes).
`BGUTIL_TAG` is pinned: resolving `latest` at build time made the legacy image non-reproducible
and could break a build with no repo change.

### 18.2 `docker/entrypoint.sh`

```sh
#!/bin/sh
set -eu
PUID="${UID:-$PUID}"          # legacy UID/GID win, exactly as before
PGID="${GID:-$PGID}"
echo "Setting umask to ${UMASK}"
umask "${UMASK}"
echo "Creating download (${DOWNLOAD_DIR}), state (${STATE_DIR}), temp (${TEMP_DIR}) directories"
mkdir -p "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}" "${AUDIO_DOWNLOAD_DIR:-$DOWNLOAD_DIR}"

if [ "$(id -u)" -eq 0 ] && [ "$(id -g)" -eq 0 ]; then
  [ "${PUID}" -eq 0 ] && echo "Warning: running as root is not recommended; check PUID/PGID (or legacy UID/GID)"
  if [ "${CHOWN_DIRS:-true}" != "false" ]; then
    echo "Changing ownership of state and download directories to ${PUID}:${PGID}"
    # Δ: /app is no longer chowned (nothing there is written at runtime), and the download
    # volume is chowned NON-recursively by default — a multi-TB library made startup take
    # minutes. CHOWN_DIRS=recursive restores the legacy `chown -R` behaviour verbatim.
    case "${CHOWN_DIRS:-true}" in
      recursive) chown -R "${PUID}:${PGID}" "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}" ;;
      *)         chown    "${PUID}:${PGID}" "${DOWNLOAD_DIR}" "${STATE_DIR}" "${TEMP_DIR}"
                 chown -R "${PUID}:${PGID}" "${STATE_DIR}" ;;
    esac
  fi
  echo "Running aulos-server as ${PUID}:${PGID}"
  exec gosu "${PUID}:${PGID}" /usr/local/bin/aulos-server "$@"
else
  echo "User set by docker; running aulos-server as $(id -u):$(id -g)"
  exec /usr/local/bin/aulos-server "$@"
fi
```
`bgutil-pot` is **not** started here — the server supervises it, so it inherits the right uid via
the already-dropped privileges and gets restarted when it dies. `CHOWN_DIRS` gains a `recursive`
value for exact legacy behaviour; `true` now means "the dirs themselves plus the state dir",
which is the useful part and is O(1) instead of O(library size).

### 18.3 Compose example (`docker/compose.example.yml`)

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
      AULOS_DB_PATH: /downloads/.metube/aulos.db
    volumes:
      - /srv/media:/downloads
      - /srv/aulos/config:/config
```

---

## 19. Test strategy

| Layer | Tooling | What it proves |
|---|---|---|
| Unit — `aulos-core` | `rstest` cases | config parsing (every row of §8.2, `%%` indirection, cycles, bad booleans), path containment (incl. `/downloads-evil`), the validation matrix, `percent` (golden vectors ported from the Python tests), status projections. |
| Unit — `aulos-provider-ytdlp` | golden JSON | `get_format`/`get_opts` for **every** legal tuple, compared against `tests/golden/formats.json` generated from the legacy Python. A diff fails CI. |
| Unit — `aulos-provider-sc` | `wiremock` + recorded HTML fixtures | Inertia version extraction, watch/season/title parsing, `window.streams` + token/expires + `h=1`, N_m3u8DL-RE ANSI progress (last-match-wins on real captured frames), natural-order segment sort. |
| Unit — `aulos-store` | in-memory SQLite | migrations from scratch match the snapshot; every `WriteOp`; `PruneSeen` boundary. |
| Unit — **importer** | fixture corpus | `tests/fixtures/state/{v1,v2,mixed,corrupt,shelf-present}/` with real-shaped `queue.json`/`pending.json`/`completed.json`/`subscriptions.json`/`telegram_bot_config.json`. Asserts: the report, statuses, id/ord assignment, dedupe, seen-id import, atomicity on a deliberately corrupt fifth file, **and that the input files are byte-identical afterwards** (T2). |
| Unit — `aulos-telegram` | fake bot transport | callback grammar, exact message texts, SSRF guard table, limiter behaviour under a `FakeClock` incl. a synthetic 429. |
| Integration — queue | `fake` provider + `FakeClock`, no network | add/cancel/retry/delete, 500-item playlist expansion, slot accounting (global vs SC), head-of-line avoidance, boot recovery from every seeded status, `clear_after`, group counters, dropped-progress accounting. |
| Integration — API | `axum::Router` in-process + `reqwest` + `tokio-tungstenite` | every v2 endpoint's request/response shape (snapshot-tested with `insta`), the full WS frame sequence for §16.1–16.3, `?since=` delta vs snapshot, ETag/304, error envelope for every code. |
| **Contract — v1 shim** | recorded legacy responses | The highest-value tests: a table of (legacy request → legacy response) captured from the Python server, replayed against the shim, asserting field-by-field equality modulo the documented deltas. Plus a decode test that runs the **actual Swift models' JSON expectations** (a small JSON-Schema check generated by `aulos-server print-schema`) against `/history`, `/version`, `/add`. |
| Integration — hooks | `wiremock` Jellyfin, real ffmpeg on a 2-second generated clip | debounce coalescing, targeted vs global refresh, fallback on 404, NFO XML snapshot, audio-sync round trip. |
| Integration — subscriptions | fake provider + `FakeClock` | backfill suppression, `is_live` re-queue, backoff curve, concurrency cap, persistence of `next_due` across a simulated restart. |
| Property | `proptest` | `percent` monotonicity, `ItemView` serialisation never omits a required key, `resolve(token)` never returns an id twice. |
| e2e — docker | `tests/e2e/run.sh`, gated on `AULOS_E2E=1` | Builds the image, runs it (OrbStack locally, `docker` in CI), then: `healthz` green incl. POT; `POST api/v2/downloads` a small public CC video; WS receives `added`→`delta`→`completed`; the file exists in the volume; `GET download/<name>` returns 200 with a `Range` response; `POST <p>add` (v1) works; a container restart mid-download resumes; `docker logs` contains no ERROR. A second profile seeds a legacy `STATE_DIR` and asserts the import report. |
| Load | `tests/load/` (a small Rust bin) | 50 fake items updating every 50–200 ms (the iOS `StressTestService`'s own model) with 5 WS clients; asserts frames/s ≤ `1000/AULOS_WS_BATCH_MS` per client, p99 frame size, and zero `Lagged`. |

CI gates: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test --workspace`, `cargo deny check` (licences + advisories), `cargo about` for the
notice file. `cargo llvm-cov` reports coverage; the importer and the v1 shim have a **90 % line
coverage floor** enforced in CI (they are the modules where a bug is silent and expensive).

---

## 20. Cutover runbook (the VPS)

Assumptions: one VPS, docker-compose, service `metube` on port 8081 behind Authelia + a reverse
proxy, volume `/srv/media:/downloads`, state in `/srv/media/.metube`.

### 20.1 Pre-flight (T-7 days)

| # | Action |
|---|---|
| 1 | **Rotate the credentials leaked in `vps_setup.md`** (Telegram bot token, Jellyfin API key, WireGuard private key) and move them into a `.env` file that is `chmod 600` and git-ignored. |
| 2 | `docker compose exec metube ls -la /downloads/.metube` — confirm `queue.json`, `pending.json`, `completed.json`, `subscriptions.json`, `telegram_bot_config.json` exist and are `schema_version: 2`. If any legacy extensionless shelf is present, let the Python image run once so it migrates. |
| 3 | Back up state: `tar czf /root/metube-state-$(date +%F).tgz -C /srv/media .metube` (a few MB). |
| 4 | Snapshot the compose file and the running image digest: `docker compose config > /root/compose.pre-aulos.yml; docker inspect --format '{{index .RepoDigests 0}}' metube-pot > /root/image.pre-aulos`. |
| 5 | Pull the new image: `docker pull ghcr.io/tatoalo/aulos-server:<tag>`. |

### 20.2 Rehearsal (T-1 day, zero downtime, read-only)

```bash
# 1. Dry-run the importer against the LIVE state dir. Reads only; writes nothing.
docker run --rm -v /srv/media:/downloads ghcr.io/tatoalo/aulos-server:<tag> \
  aulos-server import --state-dir /downloads/.metube --db /tmp/probe.db --dry-run
#    -> prints the import report (§5.5.6). Expect 0 errors. Investigate every warning.

# 2. Validate the config the compose file will actually produce.
docker run --rm --env-file /srv/aulos/.env -v /srv/media:/downloads \
  ghcr.io/tatoalo/aulos-server:<tag> aulos-server check-config
#    -> effective config table, exit 0.

# 3. Tool probe inside the image.
docker run --rm ghcr.io/tatoalo/aulos-server:<tag> aulos-server doctor

# 4. Shadow run on a spare port, with a COPY of the state dir. No writes to the real one.
cp -a /srv/media/.metube /srv/media/.aulos-shadow
docker run -d --name aulos-shadow -p 8082:8081 --env-file /srv/aulos/.env \
  -e STATE_DIR=/downloads/.aulos-shadow -e AULOS_DB_PATH=/downloads/.aulos-shadow/aulos.db \
  -e TELEGRAM_BOT_ENABLED=false -e JELLYFIN_SYNC_ENABLED=false \
  -v /srv/media:/downloads ghcr.io/tatoalo/aulos-server:<tag>
curl -s localhost:8082/healthz | jq          # all components ok, pot ok
curl -s localhost:8082/history | jq 'keys'   # ["done","pending","queue"]
curl -s localhost:8082/api/v2/import-report | jq
curl -s localhost:8082/version | jq
# add one small public video, watch the WS:
websocat ws://localhost:8082/ws | head -20
```
Telegram and Jellyfin are disabled in the shadow so it cannot double-post or double-scan.
Tear down: `docker rm -f aulos-shadow && rm -rf /srv/media/.aulos-shadow`.

### 20.3 Cutover (T-0, ~2 minutes of downtime)

```bash
# 0. Announce in the Telegram chat. Ship the v2 iOS build to the device FIRST
#    (TestFlight/Xcode) so the phone is not left without live updates.
# 1. Quiesce: let in-flight downloads finish, or accept the re-queue.
curl -s localhost:8081/history | jq '[.queue[]|select(.status=="downloading")]|length'
# 2. Stop the old service (state files are left untouched).
docker compose stop metube
# 3. Edit compose: swap the image, add the two new lines. Keep the service NAME and the
#    volume mounts identical so the proxy and Authelia config need no change.
#      image: ghcr.io/tatoalo/aulos-server:<tag>
#      environment:  AULOS_DB_PATH: /downloads/.metube/aulos.db
#                    AULOS_V1_ENABLED: "true"
# 4. Start.
docker compose up -d metube
# 5. Verify, in this order:
curl -fsS localhost:8081/healthz | jq '.status, .components.pot.status, .components.importer'
curl -fsS localhost:8081/api/v2/import-report | jq '.errors, .warnings, .items'
curl -fsS localhost:8081/history | jq '{q:(.queue|length),p:(.pending|length),d:(.done|length)}'
#    ^ compare against the numbers recorded in step 20.1.2
docker compose logs --since 2m metube | grep -iE 'error|warn' | head
# 6. Functional smoke, in this order (each one exercises a different surface):
#    a) iOS app (v2 build): snapshot arrives, a live download shows moving progress
#    b) iOS share sheet: add returns instantly, the item appears as `resolving`
#    c) Telegram: send one link -> "Queued 1 link(s)", the board appears and updates
#    d) Subscriptions: curl -fsS localhost:8081/subscriptions | jq length  (== step 2 count)
#       then POST /subscriptions/check -> 200 immediately; watch one check complete in the logs
#    e) Jellyfin: after the smoke download finishes, confirm one scan (not N) in Jellyfin's log
#    f) A file URL: curl -sI "localhost:8081/download/<name>" -> 200 + Accept-Ranges
# 7. Watch for 30 minutes: healthz every minute, `docker stats` for RSS/CPU, and one
#    subscription tick.
```

### 20.4 Rollback (any time, < 2 minutes)

```bash
docker compose stop metube
# restore the previous compose file (image digest recorded in 20.1.4)
cp /root/compose.pre-aulos.yml /srv/aulos/docker-compose.yml   # or edit the image line back
docker compose up -d metube
curl -fsS localhost:8081/history | jq 'keys'
```
Why this is safe:

| Question | Answer |
|---|---|
| Did Aulos modify the legacy JSON? | No. The importer opens them read-only; the only new file in `STATE_DIR` is `aulos.db*` and `.aulos-imported`. |
| What is lost by rolling back? | Everything that happened **after** the cutover: items added, items completed, subscription `last_checked`/`seen_ids` advances, Telegram chat-config changes. The Python server resumes from its own JSON, so it may re-download items a subscription already fetched during the Aulos window (the files are on disk, so yt-dlp's `download_archive`—if configured—or the dedupe check will usually skip them). |
| Can I roll forward again later? | Yes, but the DB now exists, so the importer will **not** re-run. To re-import the (now newer) legacy JSON: `mv /downloads/.metube/aulos.db{,.bak}` and start, or `aulos-server import --force`. |
| What if the import report has errors? | The DB is not created (atomic), the process exits non-zero, the container restarts in a loop, and `docker compose logs` shows the report. Roll back and file the fixture. |
| What if only one subsystem is broken? | Kill switches, no rebuild: `TELEGRAM_BOT_ENABLED=false`, `JELLYFIN_SYNC_ENABLED=false`, `AULOS_POT_ENABLED=false`, `AULOS_PLUGINS_ENABLED=false`, `AULOS_SC_HTTP=plain`, `AULOS_V1_ENABLED=false`, `MAX_CONCURRENT_DOWNLOADS=1`. |

### 20.5 Post-cutover

| When | Action |
|---|---|
| +1 day | Confirm one full subscription cycle ran for every subscription (`healthz.components.subscriptions.failing == 0`). |
| +7 days | If the v2 iOS build is the only client, set `AULOS_V1_ENABLED=false` and re-smoke. Keep the bookmarklet working by leaving it `true` if it is still in use. |
| +30 days | Archive the legacy JSON (`tar` + move out of `STATE_DIR`). Only then is the rollback path gone; note it in the changelog. |

---

## 21. CI / GitHub Actions (`.github/workflows/`)

| Workflow | Trigger | Jobs |
|---|---|---|
| `ci.yml` | PR + push to `master` (paths-ignore `**.md`) | `fmt` (`cargo fmt --check`) · `clippy` (`--all-targets --all-features -D warnings`) · `test` (`cargo test --workspace --locked`, `Swatinem/rust-cache`) · `deny` (`cargo deny check advisories bans licenses sources`) · `python` (`ruff` + `python -m py_compile` on `ytdlp_runner.py`, and a shim contract test that pipes a canned job to it with a stubbed `yt_dlp`) · `coverage` (`cargo llvm-cov`, floors: workspace 70 %, `aulos-store::import` 90 %, `aulos-api::v1` 90 %) · `msrv` (build with 1.95) |
| `docker.yml` | push to `master`, tags `v*`, manual `workflow_dispatch{publish}` | QEMU + Buildx, `linux/amd64,linux/arm64`, GHA build cache, `VERSION=$(date +%Y.%m.%d)`, tags `ghcr.io/<repo>:latest`, `:<date>`, `:sha-<short>` (+ `:<tag>` for tags). Then `e2e` on the amd64 image with `AULOS_E2E=1`, then `trivy image` (CRITICAL/HIGH, non-blocking) and `syft` SBOM attached to the release. Push only on `master`/tags or explicit dispatch. |
| `dev-build.yml` | PR labelled/synchronised/closed | If the PR carries `dev`: amd64-only build, `VERSION=dev-pr<N>`, push `ghcr.io/<repo>:dev`, comment on the PR. On close: delete the `dev` package version and comment. (Ported unchanged.) |
| `update-yt-dlp.yml` | cron `0 0 */3 * *` + manual | **Ported and hardened** (§21.1). |
| `update-sidecars.yml` | cron `0 2 * * 1` + manual | Same pattern for the two newly-pinned versions: `BGUTIL_TAG` (latest release of `jim60105/bgutil-ytdlp-pot-provider-rs`) and `NM3U8DL_VERSION`/`NM3U8DL_BUILD`. Separate PRs so a POT-provider regression is bisectable. |
| `upstream-sync-check.yml` / `upstream-sync-label.yml` | cron `0 3 * * 6` / issue closed | Ported as-is (they track `alexta69/metube` releases and store the last-synced version in a `synced:<ver>` label). Now also useful for tracking the Python fork we are replacing. |
| `release.yml` | tag `v*` | `cargo dist`-style: build the two Linux binaries, attach them plus the SBOM and the image digests, generate the body from `git log <prev tag>..HEAD`. |

### 21.1 The yt-dlp nightly bump PR automation

Ported from `update-yt-dlp.yml` with the same shape (grep the pin, `pip install --dry-run --pre`,
`sed`, reusable branch `auto/update-yt-dlp-nightly-<ver>`, reuse an open PR, `automated` label if
it exists, `gh pr merge --auto --squash`, step summary) and four changes that matter for a Rust
image:

| Change | Why |
|---|---|
| The pin lives in **one** place — `docker/Dockerfile`'s `ARG YTDLP_VERSION=` — and the grep is `-oE 'YTDLP_VERSION=[^ ]+'`. A second grep asserts the string appears exactly once, failing loudly if someone adds a duplicate. | Legacy grepped `yt-dlp==` out of the Dockerfile; with a build arg the automation stays a one-line `sed` and the version is also visible to `docker build --build-arg`. |
| After the `sed`, the workflow **builds the amd64 image and runs a smoke job**: `docker run --rm <img> aulos-server doctor` plus a real `mode=extract` against a public CC video through the runner shim. Only then is the PR opened/auto-merged. | This is the whole point of pinning a nightly: a bad nightly must fail *in CI*, not on the VPS. Legacy auto-merged on a version-string diff alone. |
| The commit message is `upgrade yt-dlp nightly to <ver>` (byte-identical to legacy, so the existing release-notes tooling and muscle memory keep working), and the PR body lists the yt-dlp release notes range. | Continuity. |
| `secrets.TATOALO_REPO_PAT` is replaced by a fine-grained token with `contents:write` + `pull_requests:write` on this repo only; if absent, the workflow opens the PR with `GITHUB_TOKEN` and skips auto-merge with a warning. | The legacy PAT was broad. |

Every dependency-bump workflow uses `concurrency: group: bump-<name>, cancel-in-progress: false`
so two crons cannot race on the same branch.

---

## 22. Dependencies (crates.io)

Versions are the major/minor lines I am confident exist in 2026; `cargo update` at
implementation time is expected, and `cargo deny` pins the set in `Cargo.lock`.

| Crate | Ver | Why this one |
|---|---|---|
| `tokio` | 1 | Mandated runtime; `full` in the binary, narrow features in libs. |
| `tokio-util` | 0.7 | `CancellationToken` (the cancel primitive in the BRIEF), `codec` for the JSON-lines reader. |
| `axum` | 0.8 | Mandated; native `ws`, typed extractors, `tower` ecosystem. |
| `tower` | 0.5 | Middleware composition, `ConcurrencyLimit`, `timeout`. |
| `tower-http` | 0.6 | `TraceLayer`, `SetRequestIdLayer`, `CompressionLayer`, `ServeFile`/`ServeDir`, `CorsLayer` (v2 routes). |
| `hyper` / `hyper-util` | 1 / 0.1 | axum's server plumbing; needed directly for the graceful-shutdown handle. |
| `serde` | 1 | Mandated. |
| `serde_json` | 1 | The wire format and the yt-dlp option dicts (`preserve_order` off; `arbitrary_precision` off to keep numbers plain). |
| `serde_with` | 3 | `DisplayFromStr` for ULIDs, `skip_serializing_none` for the delta frames. |
| `rusqlite` | 0.37 | Mandated, `bundled` so no libsqlite3 in the image; `serde_json` + `functions` features. |
| `rusqlite_migration` | 2 | Small, embedded, forward-only migrations; no build-time codegen. |
| `ulid` | 1 | Mandated id scheme; `Ulid::from_datetime` for the importer's ordering trick. |
| `thiserror` | 2 | Mandated for library errors. |
| `anyhow` | 1 | Mandated for the binary. |
| `tracing` | 0.1 | Mandated. |
| `tracing-subscriber` | 0.3 | `EnvFilter` + `json` + `fmt`; the whole logging story (§14.4). |
| `arc-swap` | 1 | Lock-free `Arc` swap for `YtdlOptions`, the progress snapshot and the POT state — read on every job spawn and every WS connect. |
| `bytes` | 1 | Pre-serialised WS frames shared across clients without copies (the T3 optimisation). |
| `futures` | 0.3 | `StreamExt::buffer_unordered` for the SC season fetch and batch inserts. |
| `async-trait` | 0.1 | The `Provider`/`Hook`/`Notifier` traits are object-safe and async. |
| `url` | 2 | Parsing/normalising URLs; the SSRF guard needs its host/IP classification. |
| `regex` | 1 | Plugin `match`/progress patterns, custom-dirs exclusion, Telegram URL extraction. |
| `notify` | 8 | Mandated for the `YTDL_OPTIONS_FILE` watch; has a `PollWatcher` fallback for NFS. |
| `teloxide` | 0.17 | Mandated; the maintained Rust Telegram framework, has a `Throttle` adaptor we need. |
| `governor` | 0.8 | Per-chat + global token buckets for the Telegram edit budget; GCRA, no background task. |
| `reqwest` | 0.12 | Jellyfin, POT probe, SC fallback client. `rustls-tls`, `json`, no default TLS features. |
| `wreq` | 6 | Chrome TLS/HTTP2 impersonation for StreamingCommunity (the maintained successor to `rquest`, replacing `curl_cffi`). Behind the `sc-impersonate` feature with a plain-`reqwest` fallback. |
| `scraper` | 0.23 | html5ever-backed CSS selection for the SC pages (`div#app[data-page]`, `iframe`, `script`). |
| `quick-xml` | 0.38 | NFO writing with correct escaping; `serialize` feature not needed. |
| `nix` | 0.30 | `killpg`, `setsid`, `waitpid` — the process-group kill that legacy got wrong. |
| `command-fds` | 0.3 | Passing fd 3 to the Python shim without a hand-rolled `pre_exec`. |
| `natord` | 1 | The natural (numeric-aware) filename sort the gapless SC mux depends on. |
| `strip-ansi-escapes` | 0.2 | N_m3u8DL-RE / Spectre.Console frame cleaning before progress parsing. |
| `mime_guess` | 2 | `Content-Type` for the file routes. |
| `percent-encoding` | 2 | Building `download_url` from a filename correctly. |
| `humantime` | 2 | Human durations in logs and in the Telegram board. |
| `time` | 0.3 | Timestamps (`OffsetDateTime`, RFC3339, the NFO `dateadded` format); `chrono` avoided to keep one date crate. |
| `rand` | 0.9 | Jitter for backoff and subscription spread. |
| `indexmap` | 2 | Insertion-ordered job lines in the Telegram board and ordered option merging. |
| `smallvec` | 1 | Delta frames are mostly 1–3 items. |
| `base64` | 0.22 | Decoding the importer's `__metube_bytes__` wrappers. |
| `rustls` / `rustls-pemfile` | 0.23 / 2 | `HTTPS=true` with `CERTFILE`/`KEYFILE`, no OpenSSL in the image. |
| `axum-server` | 0.7 | TLS acceptor for the same, with graceful shutdown. |
| `clap` | 4 | The subcommands in §3.1; `derive` + `env` features. |
| `dashmap` | 6 | Only for the WS client registry (metrics/inspection); the hot paths use owned state. |
| **dev** `insta` | 1 | Snapshot tests for every JSON response and every WS frame — the cheapest way to keep the v1 shim honest. |
| **dev** `wiremock` | 0.6 | Jellyfin, POT and SC HTTP fixtures. |
| **dev** `rstest` | 0.26 | Table-driven cases for the config/validation matrices. |
| **dev** `proptest` | 1 | Percent monotonicity and serialisation invariants. |
| **dev** `tempfile` | 3 | Importer and file-route fixtures. |
| **dev** `tokio-test` | 0.4 | `FakeClock`-driven time advance. |
| **dev** `tokio-tungstenite` | 0.27 | A real WS client for the integration tests. |
| **dev** `assert_cmd` + `predicates` | 2 / 3 | `aulos-server import --dry-run` / `check-config` / `doctor` CLI tests. |
| **dev** `criterion` | 0.7 | Benchmarks for `percent`, delta serialisation and the store's batch path. |
| **build** `cargo-chef`, `cargo-deny`, `cargo-llvm-cov`, `cargo-about` | — | Image layer caching, licence/advisory gate, coverage floors, notice file. |

Deliberately **not** used: `sqlx` (compile-time DB access needs a live DB in CI; `rusqlite` is
mandated anyway), `socketioxide` (Socket.IO is out of scope), `chrono` (one date crate is enough),
`lazy_static` (`std::sync::LazyLock`), `once_cell` (same), `async-std`/`smol`, `openssl`
(rustls everywhere keeps the image slim), `pyo3` (embedding CPython would tie the binary to one
interpreter ABI and break the "swap the yt-dlp pin without rebuilding Rust" property).

---

## 23. Risk register

Likelihood/Impact: L/M/H. Ordered by L×I.

| # | Risk | L | I | Mitigation | Detection |
|---|---|---|---|---|---|
| R1 | **The v1 shim is subtly wrong and the current iOS build silently shows a broken queue during cutover.** | M | H | The contract tests in §19 replay captured legacy responses field-by-field; a JSON-Schema check generated from the Swift models runs in CI; the shadow run in §20.2 exercises `/history` and `/add` against real state before any downtime. | `curl /history \| jq` counts compared against the pre-cutover numbers (step 20.3.5). |
| R2 | **Losing Socket.IO leaves the old client without live updates.** | H | M | Accepted and scheduled: the v2 iOS build ships in the same session (§20.3 step 0). The old client already fetches `/history` on connect and on pull-to-refresh, so it degrades to manual refresh rather than to a blank screen. `<p>socket.io` returns a 501 with a pointer instead of hanging. | The 501 shows up in access logs; the app's `.error` handler surfaces it. |
| R3 | **yt-dlp option-dict compatibility drift** — a user's `YTDL_OPTIONS` contains something the shim's JSON round-trip cannot express (a Python callable, a tuple, a `datetime`). | M | H | The shim receives a JSON *object* and hands it straight to `YoutubeDL(**opts)`; anything JSON can hold works unchanged. Non-JSON values were already impossible in `YTDL_OPTIONS`/`YTDL_OPTIONS_FILE` (both are JSON files in legacy). `coerce` handles the one known object type (`ImpersonateTarget`); an unknown key type produces a loud `ProtocolError` naming the key rather than a silent behaviour change. | `healthz.components.ytdl_options`, the `ytdl_options` frame, and a `ProtocolError` in the item's `error`. |
| R4 | **A nightly yt-dlp bump breaks extraction on the VPS.** | H | M | The bump PR now builds the image and runs a real extract before auto-merging (§21.1). The pin is a build arg, so rolling back is `--build-arg YTDLP_VERSION=<old>` or the previous image tag. Because Python is only in the runtime stage, the rollback image builds in ~2 min. | The bump PR's smoke job; `healthz.components.ytdlp_runner`; items failing with the same `error` en masse. |
| R5 | **`wreq`/BoringSSL does not build for arm64**, or its API churns. | M | M | The provider is behind `trait ScHttp` with a compiled-in plain-`reqwest` implementation and the runtime switch `AULOS_SC_HTTP`. The cargo feature is per-target, so an arm64 image can ship without it and only StreamingCommunity degrades (and only if the site fingerprint-checks). | Build failure in CI (blocking); at runtime a boot WARN naming the degradation, plus SC items failing with a 403. |
| R6 | **StreamingCommunity changes its page structure** (Inertia version, `window.streams`, embed host). | H | M | All scraping is fixture-driven and isolated in five small modules; the fixtures are captured HTML so a fix is a fixture + a selector. The `command` plugin system means a user can bridge the gap in Python/Node without waiting for a release. Failures are per-item, never process-wide. | SC items failing with `Upstream("Could not get site version")`; an SC-specific error-rate counter in `healthz`. |
| R7 | **Importer edge case corrupts or loses history** (unknown legacy status, duplicate URL, a 400 MB `completed.json`). | M | H | One transaction, atomic all-or-nothing, DB deleted on failure; legacy files never mutated (T2); a fixture corpus incl. corrupt and mixed-version inputs; `--dry-run` rehearsal is a mandatory runbook step; the report is served over HTTP and checked at cutover. | `api/v2/import-report` errors/warnings; the container restart loop with the report in the logs. |
| R8 | **Telegram rate limits get the bot throttled** by the new live-edit behaviour. | M | M | Per-chat GCRA at 1 edit/3 s + a global 20/s cap, no-op edits skipped by text comparison, `RetryAfter` doubles the chat's interval, `teloxide::Throttle` as a second layer, and `AULOS_TELEGRAM_BOARD=per_job` / a large `AULOS_TELEGRAM_EDIT_INTERVAL_MS` as escape hatches. | `healthz.components.telegram.edits_throttled_total`; 429s logged at WARN with the chat id. |
| R9 | **Process-group kill takes down something it should not** (e.g. the POT sidecar shares a pgid). | L | H | Every child is spawned with `process_group(0)`, so each has its **own** group; the supervisor's child is a separate group again. An integration test asserts that cancelling a download does not touch the POT pid, and that `docker stop` reaps every grandchild. | `healthz.components.pot.restarts` jumping on a cancel would show it immediately. |
| R10 | **SQLite contention / `database is locked` under a 500-item burst.** | L | H | WAL, one writer thread, batched transactions (≤64 ops), `busy_timeout=5000`, read-only reader connections, and `503 + Retry-After` rather than a 500 if it ever happens. The load test in §19 covers the burst. | `state_unavailable` responses; `healthz.components.store.latency_ms`. |
| R11 | **Progress-drop policy hides a stuck download** (we drop frames on a full channel). | L | M | Stage messages are never dropped; the per-job stall watchdog (`AULOS_JOB_STALL_SECS`) works off `ProgressCell.last_update`, which is set by frames we *did* process; the drop counter is in `healthz`. | `progress_dropped_total > 0` is a WARN-level signal. |
| R12 | **`URL_PREFIX` handling diverges** somewhere (a route, the WS path, `download_url`, the healthcheck). | M | M | One `Prefix` newtype builds every path; a parametrised integration test runs the whole API suite twice, with `URL_PREFIX=/` and `URL_PREFIX=/metube/`; the healthcheck interpolates `${URL_PREFIX}`; `capabilities.url_prefix` lets the client verify (ios §7.15). | The prefixed test run; a 404 on the healthcheck. |
| R13 | **Groups confuse a client** (a `kind:"group"` row rendered as a download). | M | M | The v1 shim omits groups entirely; v2 documents `kind` and the `children_*` counters, and the snapshot test pins the shape. Children carry `group_id` so a client can collapse or ignore them freely. | Contract tests; manual smoke on the v2 build. |
| R14 | **Legacy `shelve` state still present** on the VPS (BRIEF excludes pickle import). | L | H | Detected at import time with an explicit, actionable message ("run the Python image once, then re-run the import"); the runbook's step 20.1.2 checks for it a week early. | The import report's `errors`. |
| R15 | **Debian `bookworm-slim` ffmpeg is too old** for something the SC/audio-sync paths need. | L | M | `doctor` prints and asserts an ffmpeg ≥ 6 at boot (WARN, not fatal); the e2e test does a real remux and a real audio-sync re-encode; `trixie` is a one-line base bump if needed. | `healthz.components.ffmpeg.version`; the e2e remux test. |
| R16 | **`CHOWN_DIRS` semantics change surprises an operator** (non-recursive by default). | M | L | Documented in the compose example and the changelog; `CHOWN_DIRS=recursive` restores the exact legacy behaviour; the entrypoint logs which mode it used. | Permission-denied errors on write, visible in the first minute of logs. |
| R17 | **Scope creep**: the group model, plugins, retry policy and the Telegram board are all new surface. | M | M | Every one of them has a kill switch (`AULOS_PLUGINS_ENABLED`, `AULOS_AUTO_RETRY_MAX=0`, `AULOS_TELEGRAM_BOARD=per_job`) and none is on the critical path of "add a URL, get a file". Implementation order in the plan puts core+shim+importer first and these last. | — |
| R18 | **Rust rewrite of `_calculate_progress_percent` diverges**, making bars jump backwards. | M | L | Golden vectors ported 1:1 from the Python unit tests, plus a `proptest` monotonicity invariant. | The golden test; visually obvious in the app. |
| R19 | **Secrets leak** (the legacy `vps_setup.md` already contains live ones). | H | H | Runbook step 1 is rotation; `.env` with `chmod 600`; `git-secrets`/`gitleaks` in CI; the `Redact` newtype in every log path; `check-config` prints `«redacted»`. | `gitleaks` in CI; a manual grep of the new repo before the first push. |
| R20 | **A `command` plugin is malicious or broken** and hangs a slot forever. | L | M | No shell, fixed argv template, sanitised placeholders, per-plugin `timeout_secs`, its own pgid, and `AULOS_PLUGINS_ENABLED=false`. The plugin dir is operator-controlled and documented as trusted. | A slot stuck in `preparing`; the job stall watchdog. |

---

## 24. Open questions

1. **Overlap window for the iOS client.** This design assumes the v2 iOS build ships in the same
   session as the server cutover (R2). If that is not possible, the alternative is a small
   Socket.IO shim (`socketioxide`) emitting `all`/`added`/`updated`/`completed`/`canceled`/`cleared`
   in the legacy double-encoded form — roughly 400 lines and one more dependency. **Decide before
   implementation starts**, because it changes the crate list.
2. **Group representation on the v1 wire.** §10.4 omits groups. Should the shim instead synthesise
   a legacy-looking parent row (as a `pending` item with the playlist title) so the old app shows
   *something* for a 500-item add? My recommendation is no (it would be a row that never
   progresses), but it is a UX call.
3. **`SubId` for imported subscriptions.** Keeping legacy UUID strings (§5.5.4) means the id type
   is not uniformly a ULID. The alternative is reminting with a `legacy_id` column. Which matters
   more: type purity, or any external script that stored a subscription id?
4. **`.info.json` deletion after NFO generation** defaults to *off* here (§12.2), diverging from
   the legacy CLI. Is anyone's `Exec` postprocessor consuming those files, or should the default
   flip to match the old script?
5. **SC output naming.** Kept as `<sanitised title>.mp4`, ignoring `OUTPUT_TEMPLATE*` (K1). Series
   would organise far better as `<series>/Season 01/<title> S01E02.mp4`. Should there be an
   opt-in `AULOS_SC_USE_OUTPUT_TEMPLATE=true`, and does anything (Jellyfin library paths) break?
6. **`percent` clamp at 99.9.** Faithful to legacy, but with a real `postprocessing` status the
   clamp is no longer needed to avoid a premature "100%". Keep it for continuity, or let
   `downloading` reach 100.0 and let the status carry the meaning?
7. **Auth.** Is Authelia the only front, or should the server grow optional bearer-token auth for
   the Shortcut/bookmarklet paths (which currently ride on cookies)? This affects whether
   `AULOS_WS_TOKEN` needs to become a real credential store.
8. **APNs.** The `Notifier` seam exists. Does the push story need a device-token table and an
   APNs key in this repo, or will it live in a separate service that consumes a webhook? A
   webhook notifier is ~80 lines and worth adding now if the answer is "separate service".
9. **`MAX_CONCURRENT_DOWNLOADS` semantics for groups.** Should a single 500-item playlist be
   allowed to occupy all global slots, or should there be a per-group cap (e.g. 2) so a
   concurrently added single video is not stuck behind 498 playlist items? Legacy had no notion of
   this; FIFO means the playlist wins.
10. **Metrics.** `healthz` carries counters, but is a Prometheus `/metrics` endpoint wanted
    (`metrics` + `metrics-exporter-prometheus`, ~60 lines)? It would make the load-test numbers
    observable in production.
11. **`bgutil-pot` version pinning** (§18.1) changes the legacy "always latest" behaviour. Is a
    weekly auto-bump PR (`update-sidecars.yml`) acceptable, or should POT stay floating so a
    YouTube-side change is picked up without a merge?
12. **`STATE_DIR` for the DB.** `AULOS_DB_PATH` defaults inside `STATE_DIR` (`/downloads/.metube`),
    which is on the media volume — possibly a slow or network filesystem. Should the default move
    to a dedicated `/config` volume, accepting that it needs a compose change at cutover?
