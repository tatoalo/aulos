# Aulos Server — Architecture Proposal: "Extensibility First"

Design candidate · lens: **provider/plugin architecture**
Author: design agent · Target: `aulos_server` @ workspace root · Rust edition 2024, toolchain 1.95

> This document is a complete architecture proposal for the whole system as laid out in
> `docs/BRIEF.md`. It goes deepest on the provider/plugin subsystem (§4–§9), because that is the
> part that decides whether this codebase is still pleasant in two years, and it is the part the
> legacy Python design got most structurally wrong (StreamingCommunity is welded into
> `Download._download()` with an `if extractor contains 'streamingcommunity'` gate). Everything
> else — store, queue, API, Telegram, subscriptions, hooks, binary, packaging — is covered to
> implementable depth (§10–§17), with the legacy mapping in §19 and the risk register in §21.

**Contents**

| § | Topic |
|---|---|
| 1 | Design theses and non-goals |
| 2 | Crate graph and module lists |
| 3 | `aulos-core`: domain types, status, ids, config, errors |
| 4 | `aulos-provider`: the Provider contract (**lens**) |
| 5 | Progress: `ProgressSink`, normalisation, coalescing (**lens**) |
| 6 | Registry and match scoring (**lens**) |
| 7 | `aulos-provider-ytdlp` and the Python shim protocol (**lens**) |
| 8 | `aulos-provider-sc`: StreamingCommunity native port (**lens**) |
| 9 | The `command` community plugin format (**lens**) |
| 10 | The format/quality catalog exposed to clients (**lens**) |
| 11 | `aulos-store`: SQLite schema, store actor, legacy importer |
| 12 | `aulos-queue`: scheduler, slots, resolution, cancellation, groups |
| 13 | `aulos-api`: v2 REST, WebSocket v2, v1 shim, healthz, files |
| 14 | `aulos-telegram` |
| 15 | `aulos-subscriptions` |
| 16 | `aulos-hooks` and the `Notifier` trait |
| 17 | `aulos-server`: wiring, supervisors, signals, hot reload |
| 18 | Sequences (8 required scenarios) |
| 19 | Legacy behaviour map (spec §1–§12) + intentional changes |
| 20 | Dependencies |
| 21 | Risk register |
| 22 | Testing strategy |
| 23 | Open questions |

---

## 1. Design theses and non-goals

### 1.1 Theses

| # | Thesis | Consequence |
|---|---|---|
| T1 | A provider is a *complete* media pipeline, not a URL matcher | Legacy treats "which site is this" as a boolean inside the downloader, which is why StreamingCommunity leaks into `PersistentQueue._compact_persisted_entry`, `__start_download`'s second semaphore, `Download._download()` and the NFO generator. Here a provider owns resolution, download, its own concurrency limit, its own format catalog, its own naming and its own health. The queue knows nothing about yt-dlp, m3u8 or Python. |
| T2 | The provider boundary must be crossable by a shell script | If adding a site means writing Rust and rebuilding a multi-arch image, the plugin system is decorative. The `command` provider (§9) makes the boundary a TOML manifest plus a process that prints JSON or matches a regex. `ytdlp` is deliberately the *hardest* client of the trait (streaming resolve, dual-mode subprocess, postprocessor artifacts, process-group kill) so the trait cannot grow yt-dlp-shaped holes. |
| T3 | Progress is lossy; lifecycle is lossless | Byte counters/speed/ETA are latest-wins and droppable; status transitions and artifact discoveries never are. Two channels, two backpressure policies (§5). Conflating them is what makes legacy emit dozens of full-object broadcasts per second. |
| T4 | Options are a layered, auditable pipeline | `YTDL_OPTIONS` is the most compatibility-sensitive surface in the system (users have JSON in production). The merge is a pure function with a fixed layer order, golden-file tested against legacy, and observable via `GET api/v2/debug/options`. |
| T5 | Identity is server-assigned and immutable | One ULID per item and per group; `url` is data. Every event, mutation and row keys on the ULID. This is the single largest deletion of client code (§19.3). |

### 1.2 Non-goals

Web UI, Socket.IO, pickle/shelve import, APNs (hook only), in-process yt-dlp (Python is a
subprocess by decision — see §7.1), multi-tenant auth, provider sandboxing beyond OS-level
uid/rlimits (see risk R7).

---

## 2. Crate graph and module lists

```
                       ┌───────────────┐
                       │  aulos-core   │  types, status, ids, config, errors, events
                       └───────┬───────┘
             ┌─────────────────┼──────────────────┬────────────────┐
             ▼                 ▼                  ▼                ▼
      ┌────────────┐   ┌───────────────┐   ┌──────────────┐  ┌───────────┐
      │aulos-store │   │aulos-provider │   │  aulos-hooks │  │ (leafs)   │
      └─────┬──────┘   └───┬───────┬───┘   └──────┬───────┘  └───────────┘
            │              │       │              │
            │     ┌────────┘       └────────┐     │
            │     ▼                         ▼     │
            │ ┌──────────────────┐  ┌─────────────────────┐
            │ │aulos-provider-   │  │aulos-provider-sc    │
            │ │ytdlp (+python/)  │  │                     │
            │ └────────┬─────────┘  └──────────┬──────────┘
            │          └───────────┬───────────┘
            ▼                      ▼
        ┌─────────────────────────────────────┐
        │            aulos-queue              │  scheduler, slots, resolution pool, hooks dispatch
        └───────┬───────────────┬─────────────┘
                ▼               ▼
     ┌────────────────┐  ┌────────────────────┐  ┌──────────────────┐
     │   aulos-api    │  │ aulos-subscriptions│  │  aulos-telegram  │
     └───────┬────────┘  └─────────┬──────────┘  └────────┬─────────┘
             └────────────┬────────┴──────────────────────┘
                          ▼
                   ┌──────────────┐
                   │ aulos-server │  bin: wiring, bgutil-pot supervisor, signals
                   └──────────────┘
```

Dependency rule enforced in CI (`cargo deny`-style check plus a `tests/arch.rs` that greps
`Cargo.toml` files): **no crate depends on `aulos-api`, `aulos-telegram` or
`aulos-subscriptions`** except `aulos-server`; **no provider crate depends on `aulos-queue` or
`aulos-store`**. Providers cannot see the database. That is what keeps them replaceable.

### 2.1 Module lists

| Crate | Modules |
|---|---|
| `aulos-core` | `id` (ULID newtypes), `status`, `item` (`Item`, `ItemView`, `Group`), `selection` (`DownloadType`/`Format`/`Quality`/`Codec`/`SubtitleMode`), `request` (`AddRequest`, normalisation, legacy migration), `progress` (`Metrics`, `ProgressNormalizer`), `event` (`StateEvent`, `Delta`, `Seq`), `config` (`Config`, env parsing, `BoolToken`, indirection, validation), `error` (`CoreError`, `ErrorCode`), `paths` (safe join / containment), `outtmpl` (template field pre-resolution contract), `catalog` (`FormatCatalog` types), `notifier` (`Notifier` trait), `time` (monotonic + wall clock helpers) |
| `aulos-store` | `lib` (`Store` handle), `actor` (writer thread), `read` (reader pool), `schema` (DDL + migrations), `items`, `groups`, `artifacts`, `subscriptions`, `telegram`, `eventlog`, `importer` (legacy JSON), `sql` (statement cache) |
| `aulos-provider` | `provider` (trait), `types` (`MediaEntry`, `DownloadJob`, `Outcome`, `ResolveOpts`), `matching` (`Match`, scores), `sink` (`ProgressSink`, `ProviderEvent`), `registry`, `catalog` (catalog merge), `error` (`ProviderError`), `proc` (shared child-process helpers: process group, SIGTERM→SIGKILL, line reader, timeouts), `command` (the community plugin provider: `manifest`, `discovery`, `template`, `progress_parser`, `exec`), `fake` (test provider, `cfg(any(test, feature="testkit"))`) |
| `aulos-provider-ytdlp` | `provider`, `options` (layering + `get_format`/`get_opts` port), `shim` (spawn, JSON-lines codec, frame types), `frames` (serde types for every message), `errmap` (yt-dlp error taxonomy), `catalog`, `python/ytdlp_runner.py` (shipped asset) |
| `aulos-provider-sc` | `provider`, `http` (impersonating client, feature-gated), `inertia` (version + `x-inertia` GET), `scrape` (watch/title/season pages), `embed` (iframe → `window.streams` → token/expires), `entry` (id/title/series shaping), `engine_nm3u8` , `engine_ffmpeg`, `mux` (natural-order gapless concat), `progress` (ANSI frame parser), `catalog` |
| `aulos-queue` | `queue` (`QueueHandle` façade), `actor` (single scheduler task), `slots` (global + per-provider semaphores), `resolve_pool`, `job` (per-download task), `progress_bus` (coalescer → `StateEvent`), `groups` (aggregates), `cancel`, `hooks` (post-completion dispatch), `restart` (in-flight recovery), `autoclear` |
| `aulos-api` | `router`, `v2::{state, downloads, items, groups, catalog, config, subscriptions, cookies, presets, debug}`, `v2::ws` (frames, seq, batching), `v1` (compat shim), `healthz`, `files` (ServeDir + range), `auth` (pass-through/none), `error` (JSON envelope), `middleware` (request-id, CORS, tracing) |
| `aulos-telegram` | `bot`, `handlers`, `config_store`, `keyboards`, `urls` (extraction + SSRF guard), `live_message` (per-job editing, rate-limit aware), `notifier` (impl `Notifier`) |
| `aulos-subscriptions` | `manager`, `scheduler` (per-sub timers + jitter + backoff), `check`, `model`, `projection` (public dict) |
| `aulos-hooks` | `dispatch`, `jellyfin`, `nfo`, `audio_sync` (ffprobe/ffmpeg), `debounce` |
| `aulos-server` | `main`, `wire` (dependency injection), `supervisor` (bgutil-pot), `signals`, `reload` (`notify` watcher), `telemetry` |

---

## 3. `aulos-core`

### 3.1 Identity and status

```rust
// id.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(Ulid);
pub struct GroupId(Ulid);
pub struct SubscriptionId(Ulid);
pub struct JobId(Ulid);           // resolution / subscription-check handles

impl ItemId { pub fn new() -> Self; pub fn as_str(&self) -> String; }
// Display / FromStr use Crockford base32, 26 chars. Serialized as a JSON string, always.
```

ULID is chosen over UUIDv4 because it sorts lexicographically by creation time — the store gets a
free stable sort key (iOS ask #6), and `ORDER BY id` needs no extra index.

```rust
// status.rs — closed vocabulary (BRIEF §6)
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued, Resolving, Preparing, Downloading, Postprocessing,
    Finished, Error, Canceled,
}

impl Status {
    pub fn is_terminal(self) -> bool;         // Finished | Error | Canceled
    pub fn is_active(self) -> bool;           // Resolving | Preparing | Downloading | Postprocessing
    pub fn v1_name(self) -> &'static str;     // Queued|Resolving => "pending", Postprocessing => "downloading",
                                              // Canceled => "error" (see §13.4)
    pub fn can_transition_to(self, next: Status) -> bool;
}
```

The transition table is a `const` 8×8 matrix asserted in a unit test; illegal transitions are a
`tracing::error!` plus a drop (never a panic), so a misbehaving plugin cannot corrupt state.

| from ↓ / to → | Queued | Resolving | Preparing | Downloading | Postprocessing | Finished | Error | Canceled |
|---|---|---|---|---|---|---|---|---|
| Queued | – | ✓ | ✓ | – | – | – | ✓ | ✓ |
| Resolving | ✓ | – | ✓ | – | – | – | ✓ | ✓ |
| Preparing | – | – | – | ✓ | ✓ | ✓ | ✓ | ✓ |
| Downloading | – | – | – | ✓ | ✓ | ✓ | ✓ | ✓ |
| Postprocessing | – | – | – | ✓ | ✓ | ✓ | ✓ | ✓ |
| Finished / Error / Canceled | – | – | ✓¹ | – | – | – | – | – |

¹ only via an explicit retry (`POST api/v2/items/{id}/start`), which is a new lifecycle, not a
transition — the item is reset to `Queued` first.

### 3.2 The item shape (one shape for REST, WS snapshot and WS added/completed)

```rust
pub struct ItemView {
    pub id: ItemId,
    pub group_id: Option<GroupId>,
    pub seq: Seq,                     // last seq that touched this item
    pub url: String,                  // source page url
    pub title: String,
    pub provider: ProviderId,         // "ytdlp" | "streamingcommunity" | "command:<name>"
    pub status: Status,
    pub percent: f64,                 // ALWAYS a number, 0..=100
    pub speed: Option<f64>,           // bytes/s
    pub eta: Option<u32>,             // whole seconds
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>,
    pub fragment_count: Option<u32>,
    pub size: Option<u64>,
    pub msg: Option<String>,          // human phase text
    pub error: Option<ItemError>,     // { code, message } — cleaned, no "ERROR: " prefix
    pub selection: Selection,         // download_type/format/quality/codec/...
    pub folder: Option<String>,
    pub filename: Option<String>,     // relative to the serving root; ALWAYS present as a key (null)
    pub download_url: Option<String>, // PUBLIC_HOST_URL + filename (iOS ask #14)
    pub artifacts: Vec<Artifact>,     // subtitle/chapter/thumbnail/nfo/infojson
    pub created_at: i64,              // epoch ms
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub source: Source,               // { kind: "api"|"v1"|"telegram"|"subscription"|"plugin", ref: Option<String> }
    pub playlist: Option<PlaylistPos>,// { index, count, title }
}
```

Every optional field is **always serialised** (`null`, never absent) — killing the legacy
"lazily created `filename` key" class of client bug. `serde(default)` on the way in.

### 3.3 Config

`Config` is a plain struct built by `Config::from_env(&HashMap<String,String>) -> Result<Config, ConfigError>`
so it is unit-testable without touching the process environment. Rules preserved verbatim from
legacy §1: `%%KEY` indirection (single pass, cycle-detected), the exact boolean token set
(`true|false|True|False|on|off|1|0`, truthy = `true|True|on|1`), `URL_PREFIX` trailing-slash
normalisation, `PUBLIC_HOST_URL`/`PUBLIC_HOST_AUDIO_URL` trailing slash only when non-empty,
`.`-relative resolution of `YTDL_OPTIONS_FILE`/`YTDL_OPTIONS_PRESETS_FILE`.

Legacy left ints as strings and re-parsed them at use sites (and *silently* tolerated garbage in
some places, e.g. `CLEAR_COMPLETED_AFTER`). We parse everything once at boot and **exit non-zero
with one aggregated error report** listing every bad key, e.g.:

```
error: invalid configuration (3 problems)
  MAX_CONCURRENT_DOWNLOADS: expected a positive integer, got "three"
  JELLYFIN_SYNC_ENABLED: expected one of true|false|True|False|on|off|1|0, got "yes"
  YTDL_OPTIONS: invalid JSON at line 1 column 14: expected `,` or `}`
```

New vars, all `AULOS_`-prefixed:

| Var | Default | Meaning |
|---|---|---|
| `AULOS_DB_PATH` | `<STATE_DIR>/aulos.db` | SQLite file (WAL + `-wal`/`-shm` siblings) |
| `AULOS_WS_BATCH_MS` | `250` | delta batching cadence |
| `AULOS_PLUGINS_DIR` | `/config/plugins` | `command` plugin discovery root |
| `AULOS_RESOLVE_CONCURRENCY` | `4` | resolution pool size |
| `AULOS_EVENT_LOG_KEEP` | `20000` | `?since=` replay ring size |
| `AULOS_YTDLP_PYTHON` | `python3` | interpreter for the shim |
| `AULOS_YTDLP_SHIM` | `/app/python/ytdlp_runner.py` | shim path |
| `AULOS_YTDLP_EXTRACT_TIMEOUT` | `180` | seconds; per resolve job |
| `AULOS_YTDLP_STALL_TIMEOUT` | `900` | seconds without any frame during download ⇒ kill + error |
| `AULOS_KILL_GRACE_MS` | `5000` | SIGTERM → SIGKILL grace for a process group |
| `AULOS_SC_HTTP` | `impersonate` | `impersonate` \| `plain` \| `curl` (§8.1) |
| `AULOS_JELLYFIN_DEBOUNCE_SECONDS` | `30` | refresh debounce window |
| `AULOS_POT_SUPERVISE` | `true` | supervise `bgutil-pot server` |
| `AULOS_POT_URL` | `http://127.0.0.1:4416` | health probe target |
| `AULOS_NFO_ENABLED` | `true` | write `.nfo` for providers that declare `nfo_capable` |
| `AULOS_PLUGIN_TIMEOUT_RESOLVE` | `60` | `command` plugin resolve timeout |
| `AULOS_TELEGRAM_PROGRESS_EDIT_MS` | `3000` | live message edit floor |

### 3.4 Errors

`thiserror` per crate, one shared wire taxonomy in `aulos-core::error::ErrorCode` (a
`#[non_exhaustive]` enum with `snake_case` serde). Every HTTP error and every terminal item error
carries a code from this list, so clients can branch without regex-matching prose.

| Code | HTTP | Meaning |
|---|---|---|
| `bad_request` | 400 | malformed body / unparseable field |
| `validation_failed` | 400 | field-level validation, with `details: [{field, message}]` |
| `unsupported_url` | 400 | no provider matched (impossible while `ytdlp` is the fallback, but plugins can be forced) |
| `overrides_disabled` | 400 | `ALLOW_YTDL_OPTIONS_OVERRIDES=false` |
| `unknown_preset` | 400 | preset name not in the catalog |
| `path_escape` | 400 | `folder`/prefix containment violation |
| `not_found` | 404 | unknown item/group/subscription id |
| `conflict` | 409 | duplicate subscription URL; item already terminal |
| `unauthorized` | 401 | auth failure — **never a redirect** |
| `payload_too_large` | 413 | cookie upload > 1 MiB |
| `provider_error` | 502 | provider/plugin failed (with `provider` + `provider_code`) |
| `internal` | 500 | bug; request id logged |

Wire envelope, identical everywhere:

```json
{ "error": { "code": "validation_failed", "message": "quality \"1081\" is not valid for format \"mp4\"",
             "details": [{"field": "quality", "message": "expected one of best,best_remux,2160,…"}],
             "request_id": "01JC3Q7ZK8V0Q4E7P2W6R5T9XN" } }
```

---

## 4. `aulos-provider` — the Provider contract

This is the crate that decides how much of the system a new site touches. Target: **a new provider
adds exactly one file plus one registry line, and nothing else in the workspace changes.**

### 4.1 Core types

```rust
// ---------- identity & description ----------
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(Cow<'static, str>);      // "ytdlp", "streamingcommunity", "command:kinotek"

pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: String,
    pub kind: ProviderKind,                     // Builtin | Command
    pub version: Option<String>,                // plugin manifest version / yt-dlp version
    pub capabilities: Capabilities,
    pub limits: ProviderLimits,
}

#[derive(Clone, Copy, Default, Serialize)]
pub struct Capabilities {
    pub resolve: bool,              // can enumerate; false ⇒ resolution is synthetic (§4.6)
    pub playlists: bool,            // can return >1 entry
    pub streaming_resolve: bool,    // emits entries incrementally
    pub subtitles: bool,
    pub chapters: bool,
    pub thumbnails: bool,
    pub audio_extract: bool,
    pub cancel: CancelKind,         // ProcessGroup | Cooperative | None
    pub quality_advisory: bool,     // true ⇒ requested quality is a hint (SC: always source quality)
    pub nfo_capable: bool,          // entry metadata is rich enough for an .nfo
    pub needs_network_at_download: bool, // true ⇒ just-in-time re-extraction (SC tokens)
    pub resume: bool,               // partial-file resume across restarts
}

pub struct ProviderLimits {
    pub max_concurrent: Option<u32>,         // None ⇒ only the global slot applies
    pub uses_global_slot: bool,              // false ⇒ own limit only (SC, legacy behaviour)
    pub max_concurrent_resolves: Option<u32>,
    pub min_request_interval: Option<Duration>, // politeness throttle per provider
}
```

`uses_global_slot: false` is exactly the legacy `sc_semaphore`-outside-`semaphore` behaviour,
expressed as data rather than an `if` in the scheduler (legacy `__start_download`).

```rust
// ---------- what resolution produces ----------
pub struct MediaEntry {
    /// Stable, provider-scoped identity. Used for subscription "seen" tracking and for dedupe.
    pub key: EntryKey,                    // (ProviderId, String) — e.g. ("ytdlp", "dQw4w9WgXcQ")
    pub url: String,                      // canonical page url to hand back to download()
    pub title: String,
    pub kind: EntryKind,                  // Video | Audio | Image | Container
    pub duration_secs: Option<f64>,
    pub thumbnail: Option<String>,
    pub uploader: Option<String>,
    pub upload_date: Option<i32>,         // YYYYMMDD, as yt-dlp reports it
    pub live: LiveStatus,                 // NotLive | Upcoming{ release_ts } | Live | WasLive
    pub playlist: Option<PlaylistPos>,    // { id, title, index, count, uploader }
    pub series: Option<SeriesInfo>,       // { series, season, episode, episode_title }
    /// Pre-download problem the client should see immediately (upcoming livestream, geo note).
    pub note: Option<String>,
    /// Opaque, provider-owned resume/handoff state. Persisted verbatim (JSON) and handed back to
    /// download(). This replaces the legacy "persist the whole yt-dlp entry" hack.
    pub state: ProviderState,
    /// Metadata for NFO/Jellyfin and for outtmpl pre-resolution. Flat, typed, small.
    pub meta: EntryMeta,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderState(pub serde_json::Value);   // <= 64 KiB enforced by the registry

pub struct EntryKey { pub provider: ProviderId, pub id: String }
```

`ProviderState` is the single most important type here. Legacy persisted *the entire yt-dlp info
dict* for StreamingCommunity items (spec §5.2) because the download path needed `_sc_base_url`
and `_sc_needs_m3u8_extraction`. Here each provider declares its own tiny handoff blob:

| Provider | `ProviderState` contents |
|---|---|
| `ytdlp` | `{}` for plain videos; `{"outtmpl_fields": {...}, "playlist": {...}}` for playlist children (the pre-resolved `playlist*`/`channel*` template fields, port of `_resolve_outtmpl_fields`) |
| `streamingcommunity` | `{"base_url": "https://x.tld", "title_id": 1234, "episode_id": 5678, "series": "...", "season": 1, "episode": 2}` |
| `command:<name>` | whatever the plugin's `resolve` command printed in its `state` field (opaque to us) |

```rust
// ---------- what download consumes ----------
pub struct DownloadJob<'a> {
    pub item_id: ItemId,                  // for logging/correlation only
    pub entry: &'a MediaEntry,
    pub request: &'a DownloadRequest,
}

pub struct DownloadRequest {
    pub selection: Selection,             // download_type, format, quality, codec, subtitle_*
    pub out_dir: PathBuf,                 // already resolved + containment-checked by the queue
    pub tmp_dir: PathBuf,
    pub out_template: OutTemplate,        // { default, chapter, pre_resolved: BTreeMap<String,String> }
    pub name_prefix: Option<String>,
    pub split_by_chapters: bool,
    pub playlist_item_limit: u32,
    /// Provider-agnostic knobs a provider may ignore. `ytdlp` maps these to option layers.
    pub tuning: Tuning,                   // { presets: Vec<String>, overrides: JsonMap, cookies: Option<PathBuf> }
    pub deadline: Option<Instant>,
}

pub struct Outcome {
    pub primary: Artifact,                // the produced media file
    pub extra: Vec<Artifact>,             // subtitles, chapters, thumbnail, info.json
    pub bytes: Option<u64>,
    pub duration: Duration,               // wall time
    pub provider_meta: serde_json::Value, // free-form, persisted for hooks (NFO) and debugging
}

pub struct Artifact {
    pub role: ArtifactRole,               // Media | Subtitle | Chapter | Thumbnail | InfoJson | Nfo | Other
    pub path: PathBuf,                    // absolute
    pub size: Option<u64>,
    pub language: Option<String>,         // subtitles
    pub label: Option<String>,            // chapter titles
}
```

### 4.2 The trait

```rust
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn descriptor(&self) -> &ProviderDescriptor;

    /// Pure, synchronous, no I/O, no allocation-heavy work. Called for every add and every
    /// catalog query, possibly thousands of times when a 500-item playlist resolves.
    fn matches(&self, url: &Url) -> Match;

    /// The client-facing format/quality catalog for this provider (§10).
    fn catalog(&self) -> &FormatCatalog;

    /// Enumerate what `url` contains. May stream entries through `sink` *and* return them;
    /// see §4.3 for the streaming contract.
    async fn resolve(
        &self,
        url: &Url,
        opts: &ResolveOpts,
        sink: &dyn ResolveSink,
        cancel: CancellationToken,
    ) -> Result<ResolveSummary, ProviderError>;

    /// Fetch one entry. Must honour `cancel` within `AULOS_KILL_GRACE_MS`.
    async fn download(
        &self,
        job: DownloadJob<'_>,
        sink: &dyn ProgressSink,
        cancel: CancellationToken,
    ) -> Result<Outcome, ProviderError>;

    /// Cheap liveness for `GET healthz`. Default: `Ok(Healthy)`.
    async fn health(&self) -> ProviderHealth { ProviderHealth::healthy() }

    /// Called once at boot after config load; a provider that cannot work (missing binary,
    /// bad manifest) returns Err and is **registered as degraded**, not dropped, so
    /// `healthz` and the catalog can explain why.
    async fn preflight(&self) -> Result<(), ProviderError> { Ok(()) }
}
```

**Deviation from BRIEF §9, declared:** the brief writes `resolve(url, opts) -> Vec<MediaEntry>`.
We keep `Vec<MediaEntry>` semantics (`ResolveSummary` contains `entries: Vec<MediaEntry>` when the
provider is not streaming) but add the `sink` parameter so a 500-item YouTube playlist becomes
visible to the client after ~200 ms instead of after the whole flat extraction. A provider that
does not care ignores `sink` and returns the vector; the queue handles both. Rationale: BRIEF §5
requires the client to see playlist children promptly, and that is impossible with a
collect-then-return signature.

```rust
pub struct ResolveSummary {
    pub entries: Vec<MediaEntry>,   // empty if everything was streamed via `sink`
    pub streamed: usize,
    pub group: Option<GroupMeta>,   // Some ⇒ create/attach a group row
    pub truncated: bool,            // playlist_item_limit or provider cap hit
}

pub struct GroupMeta {
    pub provider_key: String,       // stable per source (playlist id) for idempotent re-adds
    pub title: String,
    pub kind: GroupKind,            // Playlist | Channel | Season | Series
    pub expected: Option<u32>,
}

pub trait ResolveSink: Send + Sync {
    /// Back-pressured: returns Err(Canceled) if the add was canceled.
    fn entry(&self, entry: MediaEntry) -> Result<(), SinkClosed>;
    fn group(&self, meta: GroupMeta) -> Result<(), SinkClosed>;
    fn note(&self, message: String);
}
```

`async_trait` (boxed futures) rather than AFIT/RPITIT because the registry stores
`Arc<dyn Provider>`; per-job boxing is measured in nanoseconds against multi-second network work.

### 4.3 Resolution contract (normative)

| Rule | Requirement |
|---|---|
| R-1 | A provider MUST emit `group()` before the first `entry()` when `GroupMeta` is known. |
| R-2 | `entry.key` MUST be stable for the same media across runs (used by subscriptions and dedupe). |
| R-3 | `entry.url` MUST be a URL that this same provider will `matches()` with score ≥ the one that selected it. This makes retry-after-restart provider-stable. |
| R-4 | `resolve` MUST return within `ResolveOpts::deadline` or `Err(Timeout)`. |
| R-5 | A single-video URL MUST resolve to exactly one entry with `group: None`. |
| R-6 | Streaming providers MUST return `entries: vec![]` and set `streamed`. Mixing is a contract violation (debug-asserted, warned in release). |
| R-7 | `ProviderState` MUST be ≤ 64 KiB serialised; the registry wrapper truncates + errors otherwise. |
| R-8 | `resolve` MUST NOT write to `out_dir` (no side effects before a download slot is granted). |

### 4.4 Download contract (normative)

| Rule | Requirement |
|---|---|
| D-1 | First event MUST be `Status(Preparing)`; last event before returning MUST be a terminal status or nothing (the queue derives terminal state from the `Result`). |
| D-2 | `Outcome::primary.path` MUST exist and be inside `out_dir` (checked by the wrapper; violation ⇒ `ProviderError::Contract`). |
| D-3 | On `cancel`, the provider MUST stop within `AULOS_KILL_GRACE_MS` and return `Err(ProviderError::Canceled)`. Partial files MUST be left in `tmp_dir` or removed, never left half-written in `out_dir`. |
| D-4 | A provider MUST NOT emit progress after returning. |
| D-5 | `Metrics` may be emitted at any rate; the sink is lossy. Discrete events (status, artifact) MUST be ≤ ~100 per job (asserted at 10 000 with a warning). |
| D-6 | A provider MUST tolerate being called twice for the same entry (retry) — idempotent overwrite or a `.partN` suffix, its choice, documented in the catalog. |

### 4.5 Error type

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("canceled")]                                    Canceled,
    #[error("timed out after {0:?}")]                        Timeout(Duration),
    #[error("unsupported url: {0}")]                         Unsupported(String),
    #[error("authentication required: {0}")]                 AuthRequired(String),
    #[error("geo restricted: {0}")]                          GeoRestricted(String),
    #[error("unavailable: {0}")]                             Unavailable(String),
    #[error("not yet live (starts {at})")]                    NotYetLive { at: Option<i64> },
    #[error("no matching format: {0}")]                      NoFormat(String),
    #[error("bot check / token provider failed: {0}")]       BotCheck(String),
    #[error("network: {0}")]                                 Network(String),
    #[error("postprocessing failed: {0}")]                   Postprocessing(String),
    #[error("external tool {tool} exited {code}: {tail}")]   Tool { tool: String, code: i32, tail: String },
    #[error("disk: {0}")]                                    Disk(String),
    #[error("contract violation: {0}")]                      Contract(String),
    #[error(transparent)]                                    Other(#[from] anyhow::Error),
}

impl ProviderError {
    pub fn code(&self) -> &'static str;     // stable snake_case string for the wire
    pub fn retryable(&self) -> Retryable;   // No | After(Duration) | Immediate
    pub fn user_message(&self) -> String;   // cleaned; no "ERROR: " prefix, no tracebacks
}
```

`retryable()` drives the queue's automatic retry policy (§12.6) and the subscription backoff.

### 4.6 Non-resolving providers

A `command` plugin may declare `resolve = false` (no `resolve` command in its manifest). The
registry then wraps it in `SyntheticResolve`, which produces exactly one `MediaEntry`:

```
key   = (provider_id, sha256(url)[..16])
url   = the input url
title = manifest `title_template` rendered ({host}, {path}, {basename}), default = url basename
state = {}
```

This is what makes "I have a shell one-liner that downloads from site X" a five-line plugin.

---

## 5. Progress: `ProgressSink`, normalisation, coalescing

### 5.1 Two channels, two policies

```rust
pub trait ProgressSink: Send + Sync {
    /// LOSSY, non-blocking, latest-wins. Called as often as the provider likes.
    fn metrics(&self, m: Metrics);

    /// LOSSLESS, non-blocking, bounded (capacity 256). If the buffer is full the caller is
    /// *slowed* by an internal `Notify` handshake rather than losing the event.
    fn event(&self, e: ProviderEvent) -> Result<(), SinkClosed>;

    /// Structured passthrough to `tracing`, span-attached to the item. Never reaches clients.
    fn log(&self, level: LogLevel, msg: &str);
}

#[derive(Clone, Copy, Default)]
pub struct Metrics {
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub total_bytes_estimate: Option<u64>,
    pub fragment_index: Option<u32>,
    pub fragment_count: Option<u32>,
    pub speed: Option<f64>,
    pub eta: Option<u32>,
    /// A provider that knows better than the byte math (SC ffmpeg: out_time/duration) sets this.
    pub percent_hint: Option<f64>,
    /// Identifies which stream the numbers belong to; a change resets the monotonic clamp
    /// (port of legacy `progress_source`).
    pub source_tag: Option<u64>,          // hash of filename/tmpfilename
}

pub enum ProviderEvent {
    Status { status: Status, msg: Option<String> },
    Artifact(Artifact),
    Phase(String),                        // free text for `msg` ("N_m3u8DL-RE failed, retrying with ffmpeg…")
    TotalKnown { total_bytes: u64 },
    Warning { code: &'static str, message: String },
}
```

Implementation: `QueueSink { item: ItemId, metrics: Arc<AtomicCell<Metrics>>, events: mpsc::Sender<(ItemId, ProviderEvent)>, ticker_wake: Arc<Notify> }`. `metrics()` is a single
`store` on a `crossbeam`-style seqlock cell (a `Mutex<Metrics>` is fine too — contention is one
writer, one reader). No allocation, no await, no syscall: a provider can call it 1000×/s at zero
cost, which is what makes it safe for the shim to forward every yt-dlp progress hook.

### 5.2 Normalisation (port of `_calculate_progress_percent`)

Lives in `aulos-core::progress::ProgressNormalizer`, owned by the queue's per-item state, **not**
by providers — so all three providers get identical semantics and the unit tests transfer.

```rust
pub struct ProgressNormalizer { prev: Option<f64>, source_tag: Option<u64> }

impl ProgressNormalizer {
    pub fn apply(&mut self, m: &Metrics, status: Status) -> f64 {
        if self.source_tag != m.source_tag { self.prev = None; self.source_tag = m.source_tag; }
        if status == Status::Finished { self.prev = Some(100.0); return 100.0; }
        let candidate = m.percent_hint
            .or_else(|| exact(m))            // downloaded/total*100 when total_bytes is exact
            .or_else(|| estimated(m));       // estimate bounded by fragment floor/ceiling
        let v = match candidate { Some(v) => v, None => return self.prev.unwrap_or(0.0) };
        let v = v.clamp(0.0, 99.9).max(self.prev.unwrap_or(0.0));
        self.prev = Some(v);
        v
    }
}
```

`estimated()` keeps every legacy subtlety, each with a named unit test:

| Case | Behaviour |
|---|---|
| `total_bytes` exact and > 0 | `downloaded / total * 100` |
| fragments known | `floor = idx/count*100`, `ceil = min((idx+1)/count*100, 99.9)`, result = `estimate.clamp(floor, ceil)`; with no estimate, `floor` |
| no fragments, `total_bytes_estimate <= downloaded_bytes` | estimate ignored (the bogus HLS 1 KiB/1 KiB frame) |
| nothing usable | keep previous |
| output stream changed (`source_tag`) | reset monotonic floor (video→audio leg of a merge) |

### 5.3 Coalescing to the wire

```
provider ──metrics()──► AtomicCell<Metrics>  ──┐
         ──event()───► mpsc(256) ─────────────┐│
                                              ▼▼
                                   ProgressBus (one task)
                                   ├── every AULOS_WS_BATCH_MS: read all dirty cells,
                                   │   normalise, diff vs last-sent ItemView, emit Delta
                                   ├── on event: apply immediately, emit added/completed/removed
                                   │   promptly (out of band, BRIEF §3)
                                   └── writes to store: only on status change / artifact /
                                       terminal (never on metrics)
```

Per BRIEF §3 the batched frame is `delta`; `added`, `completed`, `removed` bypass the ticker. The
bus keeps `last_sent: HashMap<ItemId, ItemView>` and emits **changed fields only**, so a stalled
download at 43.2 % produces *zero* bytes on the wire, versus the legacy full-object flood.

---

## 6. Registry and match scoring

```rust
pub struct Registry {
    providers: Vec<Entry>,                       // declaration order == tie-break order
    by_id: HashMap<ProviderId, usize>,
    fallback: usize,                             // index of `ytdlp`
}
struct Entry { provider: Arc<dyn Provider>, order: u16, state: RegState /* Ready | Degraded(String) */ }

impl Registry {
    pub fn builder() -> RegistryBuilder;
    pub fn select(&self, url: &Url, hint: Option<&ProviderId>) -> Selected;
    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>>;
    pub fn descriptors(&self) -> Vec<&ProviderDescriptor>;
    pub fn merged_catalog(&self) -> &MergedCatalog;                   // §10
    pub async fn reload_plugins(&self, dir: &Path) -> ReloadReport;   // hot reload of `command` plugins
}

pub struct Selected { pub provider: Arc<dyn Provider>, pub score: u16, pub reason: MatchReason,
                      pub runner_up: Option<(ProviderId, u16)> }
```

### 6.1 `Match`

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Match { pub score: u16, pub reason: MatchReason }

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MatchReason { None, Fallback, HostSuffix, HostRegex, HostAndPath, Exact, Forced }

impl Match {
    pub const NONE: Match          = Match { score: 0,    reason: MatchReason::None };
    pub const FALLBACK: Match      = Match { score: 1,    reason: MatchReason::Fallback };
    pub fn host_suffix()  -> Match { Match { score: 100,  reason: MatchReason::HostSuffix } }
    pub fn host_regex()   -> Match { Match { score: 150,  reason: MatchReason::HostRegex } }
    pub fn host_path()    -> Match { Match { score: 250,  reason: MatchReason::HostAndPath } }
    pub fn exact()        -> Match { Match { score: 400,  reason: MatchReason::Exact } }
    pub const FORCED: Match        = Match { score: 1000, reason: MatchReason::Forced };
}
```

| Score band | Who uses it | Example |
|---|---|---|
| 0 | not mine | `ytdlp` never returns 0 |
| 1 | catch-all fallback | `ytdlp` for every `http(s)` URL |
| 100 | community plugin host match | `plugin.toml` `match.hosts = ["kinotek.example"]` |
| 150 | community plugin host regex | `match.host_regex = "^(www\\.)?kinotek\\.(example|test)$"` |
| 250 | built-in host + path shape | `streamingcommunity` on `/watch/`, `/titles/`, `/season-` |
| 400 | exact provider-owned scheme/host | reserved (e.g. a future `aulos://` internal scheme) |
| 1000 | `provider` field in the add request | operator override / debugging |

Selection: `max(score)`; ties broken by lowest `order` (declaration order — built-ins are declared
`streamingcommunity`, then plugins in directory-sorted order, then `ytdlp` last). A `Degraded`
provider still matches but the queue fails the item immediately with
`provider_error/provider_degraded` and the reason string, rather than silently falling through to
yt-dlp and downloading a login page. `runner_up` is surfaced in `GET api/v2/resolve-preview?url=`
for debugging.

`matches()` is called with a **pre-parsed, normalised `Url`** (lowercased host, IDNA-encoded,
default port stripped) so plugin regexes are written once, correctly.

### 6.2 Provider-scoped concurrency

The scheduler reads `ProviderLimits` and maintains:

```rust
struct Slots {
    global: Arc<Semaphore>,                              // MAX_CONCURRENT_DOWNLOADS
    per_provider: HashMap<ProviderId, Arc<Semaphore>>,   // from ProviderLimits::max_concurrent
    resolve: Arc<Semaphore>,                             // AULOS_RESOLVE_CONCURRENCY
    per_provider_resolve: HashMap<ProviderId, Arc<Semaphore>>,
}
```

Acquisition order (deadlock-free because it is a fixed total order):
`per_provider` → `global` (skipped when `uses_global_slot == false`). `streamingcommunity` ships
`{ max_concurrent: SC_MAX_CONCURRENT_DOWNLOADS, uses_global_slot: false }`, reproducing legacy
behaviour exactly, as data.

---

## 7. `aulos-provider-ytdlp` and the Python shim

### 7.1 Why a Python shim (and not a `yt-dlp` CLI, and not PyO3)

| Option | Verdict |
|---|---|
| `yt-dlp` CLI with flags | **Rejected.** `YTDL_OPTIONS` is a *Python API option dict* (`postprocessors` lists, `extractor_args` nested maps, `paths`, `outtmpl` maps, `impersonate` objects). There is no lossless dict→CLI translation, and users have these dicts in production. `--progress-template` also cannot express postprocessor `info_dict` fields we need. |
| PyO3 embedding | **Rejected.** One GIL for the whole server; a hung extractor blocks every job; a segfault in a C extension takes down the queue; `abi3` + nightly yt-dlp + the BgUtils plugin + deno subprocesses inside our address space is a support nightmare. |
| Thin Python shim, one process per job, JSON-lines stdout | **Chosen (BRIEF §9).** Full option fidelity, hard isolation, process-group kill, and the BgUtils POT plugin + nightly pin keep working untouched. Rust owns option construction, lifecycle, timeouts and normalisation. |

The shim is ~350 lines of Python with **no dependencies beyond yt-dlp and the stdlib**, shipped at
`/app/python/ytdlp_runner.py`, and version-locked to the Rust side by a `protocol` integer.

### 7.2 The option pipeline

`options.rs` is a pure function tree with no I/O; it is the most heavily unit-tested module in the
workspace (golden JSON fixtures compared byte-for-byte against captured legacy dicts).

```rust
pub struct OptionLayers<'a> {
    pub env: &'a JsonMap,            // YTDL_OPTIONS (parsed at boot, revalidated on reload)
    pub file: Option<&'a JsonMap>,   // YTDL_OPTIONS_FILE (hot-reloaded; FILE WINS over env)
    pub runtime: &'a JsonMap,        // runtime overrides: currently only `cookiefile`
    pub presets: &'a [&'a JsonMap],  // request order
    pub overrides: &'a JsonMap,      // per-request
}

pub fn build_download_opts(sel: &Selection, req: &DownloadRequest, layers: OptionLayers<'_>)
    -> Result<JsonMap, OptionError>;
pub fn build_extract_opts(opts: &ResolveOpts, layers: OptionLayers<'_>) -> Result<JsonMap, OptionError>;
```

**Download-mode layer order** (later wins unless marked *pinned*):

| # | Layer | Notes |
|---|---|---|
| 1 | Aulos base | `quiet`/`verbose` from log level, `no_color: true`, `paths: {home, temp}`, `outtmpl: {default, chapter}`, `socket_timeout: 30`, `ignore_no_formats_error: true` |
| 2 | `format` from `get_format(...)` | port of `dl_formats.get_format` — identical selector strings |
| 3 | `get_opts(...)` derived postprocessors/flags | `postprocessors` = *derived-prepend* + user's own + *derived-late* (exact legacy ordering) |
| 4 | `env` (`YTDL_OPTIONS`) | |
| 5 | `file` (`YTDL_OPTIONS_FILE`) | file beats env (legacy §1.3) |
| 6 | `runtime` (`cookiefile`) | |
| 7 | presets, in request order | `null` values are preserved and *clear* a key (legacy) |
| 8 | per-request `overrides` | gated by `ALLOW_YTDL_OPTIONS_OVERRIDES` |
| 9 | **pinned** Aulos keys | `progress_hooks`/`postprocessor_hooks` (shim-injected, unset here), `logger` sentinel, and — when `quality == best_remux` — `format` is re-pinned and any user `format` removed (legacy `opts.pop("format")`) |
| 10 | `split_by_chapters` | `outtmpl.chapter = request.chapter_template` + append `FFmpegSplitChapters{force_keyframes:false}` |

**Extract-mode layer order**: `env` → `file` → `runtime` → presets → `overrides` → **pinned Aulos
keys last**: `extract_flat: "in_playlist"`, `noplaylist`, `ignore_no_formats_error: true`,
`lazy_playlist: true`, `paths`, `quiet/verbose`, `no_color`. Legacy did this for `__extract_info`
but the *opposite* for `subscriptions.extract_flat_playlist` (spec §7.4 — "that asymmetry looks
like a bug"). **We unify on pinned-last for both** (change C-7, §19.3).

Callable-valued options cannot survive JSON. The shim materialises a fixed sugar set and hard-fails
on anything else:

| Option key | JSON value | Shim materialises |
|---|---|---|
| `impersonate` | `"chrome"` / `"chrome-110:windows-10"` | `ImpersonateTarget.from_str(...)` |
| `match_filter` | `"duration > 60"` | `yt_dlp.utils.match_filter_func(...)` |
| `progress_template` | object | passthrough |
| `postprocessors[].when` | string | passthrough |
| `logger` | absent | shim's own logger (forwards to `log` frames) |
| `progress_hooks`, `postprocessor_hooks` | any | **rejected** with `bad_job/hook_override` — Aulos owns these |
| anything else needing a callable | — | rejected with `bad_job/uncoercible_option` naming the key |

Presets are loaded from `YTDL_OPTIONS_PRESETS` + `YTDL_OPTIONS_PRESETS_FILE` with the legacy
`dict[str, dict]` invariant, and — change C-8 — the presets file **is** watched too (the legacy
README claimed it was; the code did not).

### 7.3 Shim protocol, v1

Transport: one child process per job.
- **stdin**: exactly one JSON object terminated by `\n`, then EOF. The shim never reads again.
- **stdout**: JSON Lines, UTF-8, one object per line, `\n`-terminated, flushed per line
  (`sys.stdout.reconfigure(line_buffering=True)`). Max line 4 MiB (Rust reader cap; longer ⇒
  `Contract` error and kill).
- **stderr**: raw text, forwarded to `tracing` at `DEBUG` (or `WARN` for lines matching
  `^(ERROR|WARNING)`), last 8 KiB retained as `tail` for error reporting.
- **exit codes**: `0` clean, `2` bad job (malformed request), `3` internal shim error, `64` protocol
  version mismatch, `130` canceled via SIGTERM/SIGINT.

Every frame carries `{"v":1,"t":<type>,"n":<seq u64>,"ts":<epoch float>}`. `n` starts at 1 and
increments per frame; a gap means a lost line (⇒ `Contract` error, kill, item errors).

#### 7.3.1 Rust → shim: the job

```json
{
  "v": 1,
  "protocol": 1,
  "mode": "download",
  "job_id": "01JC3Q7ZK8V0Q4E7P2W6R5T9XN",
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "opts": { "...": "fully merged yt-dlp option dict" },
  "aulos": {
    "download_type": "video",
    "download_dir": "/downloads",
    "temp_dir": "/downloads",
    "caption_exts": [".vtt", ".srt", ".sbv", ".scc", ".ttml", ".dfxp"],
    "convert_srt_to_txt": false,
    "thumbnail_ext_rewrite": false,
    "emit_progress_every_ms": 100,
    "max_frames": 100000,
    "debug": false
  }
}
```

`mode` ∈ `extract` | `download`. `aulos` carries the small amount of *policy* the shim must apply
locally (which filenames count as captions, whether to rewrite `.webm`→`.jpg` for thumbnails)
because those decisions need the postprocessor `info_dict`, which never crosses the boundary.

Extract-mode job adds:

```json
{ "mode": "extract",
  "extract": { "flat": true, "noplaylist": true, "playlist_end": 50,
               "strict_retry": true, "stream_entries": true, "max_entries": 5000 } }
```

#### 7.3.2 shim → Rust: message types

| `t` | Mode | Fields | Rust action |
|---|---|---|---|
| `hello` | both | `protocol:int`, `yt_dlp:str`, `python:str`, `pid:int`, `plugins:[str]`, `pot:{available:bool, url:str\|null}` | verify `protocol == 1` else kill + `Contract`; record `yt_dlp` version for `GET version`/healthz; `pid` is the process-group leader for kill |
| `resolved` | extract | `root:{type, id, title, webpage_url, extractor, playlist_count, uploader, uploader_id}` | emit `ResolveSink::group(...)` when `type ∈ {playlist, channel}` |
| `entry` | extract | `index:int`, `entry:{…flat info dict subset…}`, `note:str\|null` | map to `MediaEntry`, `sink.entry(...)` |
| `progress` | download | see §7.4 | `sink.metrics(...)`, `sink.event(Status(Downloading))` on first |
| `pp` | download | see §7.5 | artifacts + `Status(Postprocessing)` |
| `artifact` | download | `role`, `path`, `size:int\|null`, `language`, `label` | `sink.event(Artifact(..))` |
| `phase` | download | `msg:str` | `sink.event(Phase(msg))` — sets `ItemView.msg` |
| `log` | both | `level:"debug"\|"info"\|"warning"\|"error"`, `message`, `extractor:str\|null` | `sink.log(...)`; `warning` also becomes `ProviderEvent::Warning` if it matches a curated allow-list (e.g. "Requested format is not available") |
| `result` | both | download: `ok:true`, `filename`, `size`, `artifacts:[…]`, `retcode:int`; extract: `ok:true`, `count:int`, `truncated:bool` | build `Outcome`/`ResolveSummary` |
| `error` | both | `code`, `message`, `retryable:bool`, `extractor`, `fatal:bool`, `traceback:str\|null` | map via `errmap` to `ProviderError` |
| `bye` | both | `elapsed_ms`, `frames`, `peak_rss_kb` | record metrics; expect EOF next |

Ordering guarantees: `hello` first; exactly one of `result` or `error` (the `error` frame may be
followed by `bye`); `bye` last. Violations ⇒ `ProviderError::Contract`.

#### 7.3.3 Example transcript (download, YouTube, 1080p mp4)

```jsonl
{"v":1,"t":"hello","n":1,"ts":1772668800.11,"protocol":1,"yt_dlp":"2026.8.30.232658.dev0","python":"3.13.2","pid":4821,"plugins":["bgutil_pot"],"pot":{"available":true,"url":"http://127.0.0.1:4416"}}
{"v":1,"t":"log","n":2,"ts":1772668800.42,"level":"debug","message":"[youtube] Extracting URL: …","extractor":"youtube"}
{"v":1,"t":"progress","n":3,"ts":1772668801.90,"status":"downloading","filename":null,"tmpfilename":"/downloads/Rick.f616.mp4.part","downloaded_bytes":262144,"total_bytes":null,"total_bytes_estimate":58720256,"fragment_index":null,"fragment_count":null,"speed":1310720.0,"eta":44,"elapsed":0.3,"stream":"video"}
{"v":1,"t":"progress","n":4,"ts":1772668802.15,"status":"downloading","tmpfilename":"/downloads/Rick.f616.mp4.part","downloaded_bytes":3145728,"total_bytes_estimate":58720256,"speed":11534336.0,"eta":5,"elapsed":0.55,"stream":"video"}
{"v":1,"t":"progress","n":5,"ts":1772668807.02,"status":"finished","filename":"/downloads/Rick.f616.mp4","downloaded_bytes":58720256,"total_bytes":58720256,"speed":null,"eta":null,"elapsed":5.4,"stream":"video"}
{"v":1,"t":"progress","n":6,"ts":1772668807.10,"status":"downloading","tmpfilename":"/downloads/Rick.f140.m4a.part","downloaded_bytes":65536,"total_bytes_estimate":3670016,"speed":655360.0,"eta":5,"elapsed":0.08,"stream":"audio"}
{"v":1,"t":"progress","n":7,"ts":1772668809.44,"status":"finished","filename":"/downloads/Rick.f140.m4a","downloaded_bytes":3670016,"total_bytes":3670016,"elapsed":2.4,"stream":"audio"}
{"v":1,"t":"pp","n":8,"ts":1772668809.50,"postprocessor":"Merger","status":"started","filepath":"/downloads/Rick.mp4"}
{"v":1,"t":"pp","n":9,"ts":1772668812.71,"postprocessor":"Merger","status":"finished","filepath":"/downloads/Rick.mp4"}
{"v":1,"t":"pp","n":10,"ts":1772668812.75,"postprocessor":"MoveFiles","status":"finished","filepath":"/downloads/Rick.mp4","finaldir":null,"subtitles":[],"chapters":[]}
{"v":1,"t":"artifact","n":11,"ts":1772668812.76,"role":"media","path":"/downloads/Rick.mp4","size":62390272,"language":null,"label":null}
{"v":1,"t":"result","n":12,"ts":1772668812.80,"ok":true,"retcode":0,"filename":"/downloads/Rick.mp4","size":62390272,"artifacts":[{"role":"media","path":"/downloads/Rick.mp4","size":62390272}]}
{"v":1,"t":"bye","n":13,"ts":1772668812.81,"elapsed_ms":12700,"frames":13,"peak_rss_kb":91240}
```

#### 7.3.4 Example transcript (extract, 500-item playlist, streaming)

```jsonl
{"v":1,"t":"hello","n":1,"ts":…,"protocol":1,"yt_dlp":"2026.8.30.232658.dev0",…}
{"v":1,"t":"resolved","n":2,"ts":…,"root":{"type":"playlist","id":"PL9tY0BWXOZFv","title":"Mix - lofi","webpage_url":"https://www.youtube.com/playlist?list=PL9tY0BWXOZFv","extractor":"youtube:tab","playlist_count":500,"uploader":"Chillhop","uploader_id":"@chillhop"}}
{"v":1,"t":"entry","n":3,"ts":…,"index":1,"entry":{"id":"aXbZ1","title":"Track 1","url":"https://www.youtube.com/watch?v=aXbZ1","webpage_url":"https://www.youtube.com/watch?v=aXbZ1","duration":183.0,"thumbnail":"https://…","live_status":"not_live","uploader":"Chillhop"},"note":null}
… 499 more `entry` frames, emitted as yt-dlp's lazy playlist yields them …
{"v":1,"t":"result","n":503,"ts":…,"ok":true,"count":500,"truncated":false}
{"v":1,"t":"bye","n":504,"ts":…,"elapsed_ms":4180,"frames":504}
```

Rust streams these into `ResolveSink`, so the client sees the group row and the first children
within a few hundred milliseconds (§18.2).

#### 7.3.5 Error frame examples

```json
{"v":1,"t":"error","n":7,"ts":…,"code":"login_required","message":"Sign in to confirm your age",
 "retryable":false,"extractor":"youtube","fatal":true,"traceback":null}

{"v":1,"t":"error","n":4,"ts":…,"code":"bot_check",
 "message":"Sign in to confirm you're not a bot. Use --cookies-from-browser or the POT provider",
 "retryable":true,"extractor":"youtube","fatal":true,"traceback":null}

{"v":1,"t":"error","n":2,"ts":…,"code":"bad_job","message":"opts.progress_hooks may not be set by the caller",
 "retryable":false,"extractor":null,"fatal":true,"traceback":null}
```

Taxonomy: the shim classifies by exception class first, then by a small ordered regex table over
the message. The table lives in the shim (so it ships with the yt-dlp pin) and Rust maps codes to
`ProviderError` mechanically.

| Shim `code` | Detected from | `ProviderError` |
|---|---|---|
| `canceled` | `KeyboardInterrupt` after SIGTERM | `Canceled` |
| `unsupported_url` | `UnsupportedError` | `Unsupported` |
| `login_required` | `ExtractorError` + `/sign in\|log in\|members-only\|private video/i` | `AuthRequired` |
| `geo_restricted` | `GeoRestrictedError` | `GeoRestricted` |
| `unavailable` | `/video unavailable\|removed by the uploader\|account.*terminated/i` | `Unavailable` |
| `live_not_started` | `entry.live_status == "is_upcoming"` / `/premieres in/i` | `NotYetLive` |
| `format_unavailable` | `/requested format is not available/i` | `NoFormat` |
| `bot_check` | `/confirm you'?re not a bot\|failed to extract any player response/i` | `BotCheck` |
| `network` | `DownloadError` wrapping `URLError`/`TimeoutError`, `/HTTP Error 5\d\d/` | `Network` |
| `throttled` | `/HTTP Error 429\|too many requests/i` | `Network` (retryable after 60 s) |
| `postprocessing_failed` | `PostProcessingError` | `Postprocessing` |
| `disk_full` | `OSError` errno 28 | `Disk` |
| `timeout` | shim watchdog | `Timeout` |
| `bad_job` | request validation | `Contract` |
| `internal` | anything else | `Other` |

### 7.4 `progress` frame ↔ yt-dlp `progress_hooks`

The shim forwards **only** the legacy key set plus two additions, so `YTDL_OPTIONS` cannot make
frames unboundedly large:

| Frame field | yt-dlp `d[...]` | Notes |
|---|---|---|
| `status` | `status` | `downloading` \| `finished` \| `error` |
| `filename` | `filename` | final path once known |
| `tmpfilename` | `tmpfilename` | **kept per-frame, not sticky** — Rust only overwrites its stored value when the key is present (fixes legacy pain point #23) |
| `downloaded_bytes` | `downloaded_bytes` | |
| `total_bytes` | `total_bytes` | |
| `total_bytes_estimate` | `total_bytes_estimate` | |
| `fragment_index` / `fragment_count` | same | |
| `speed` | `speed` | bytes/s float |
| `eta` | `eta` | seconds int |
| `elapsed` | `elapsed` | *new*: lets Rust detect stalls without a wall clock |
| `stream` | derived | *new*: `"video"`/`"audio"`/`"fragment"`/`"unknown"` from `info_dict.get("vcodec"/"acodec")`; hashed into `Metrics::source_tag` so the monotonic clamp resets per leg of a merge (legacy did this by hashing the filename; explicit is better) |
| `msg` | `msg` | rare; yt-dlp puts text here on some errors |

Rate limiting inside the shim: at most one `progress` frame per `emit_progress_every_ms` (default
100 ms) **per `stream`**, plus every `status != "downloading"` frame unconditionally. This caps the
worst case (thousands of tiny HLS fragments) at ~10 frames/s/job at the source, before any Rust
coalescing.

Rust mapping:

```
first `progress` for a job              -> event Status{Downloading}
status == "downloading"                 -> metrics(...)                      (lossy)
status == "finished"                    -> metrics(percent_hint=100 for that stream) ; no status change
status == "error"                       -> event Warning{code:"stream_error"} (the fatal `error` frame decides)
```

Note `progress.status == "finished"` is per-*stream*, not per-item: the legacy code treated it as
item-finished and then had to be corrected by later frames. Here only the `result` frame finishes
the item.

### 7.5 `pp` frame ↔ yt-dlp `postprocessor_hooks`

```json
{"v":1,"t":"pp","n":9,"ts":…,"postprocessor":"MoveFiles","status":"finished",
 "filepath":"/downloads/S01E02.mkv","finaldir":"/downloads/Show/Season 01",
 "subtitles":[{"path":"/downloads/Show/S01E02.en.srt","language":"en"}],
 "chapters":[{"path":"/downloads/Show/S01E02 - 01 - Intro.mkv","label":"Intro"}]}
```

| `postprocessor` | `status` | Shim behaviour | Rust behaviour |
|---|---|---|---|
| any | `started` | frame with `filepath` | `Status(Postprocessing)` + `msg = "<Postprocessor>…"` **(new: fixes legacy "no postprocessing status")** |
| any | `processing` | throttled to 1 s | keep `Postprocessing` |
| `MoveFiles` | `finished` | `filepath` = `join(info_dict['__finaldir'], basename(filepath))` when `__finaldir` present, else `filepath`; for `download_type == "captions"` also enumerate `info_dict['requested_subtitles'][*]['filepath']` into `subtitles[]` | primary artifact candidate |
| `SplitChapters` | `finished` | one `chapters[]` entry per `info_dict['chapters'][*]['filepath']`, de-duplicated | `Artifact{role: Chapter}` each |
| `FFmpegExtractAudio` / `FFmpegVideoConvertor` / `Merger` | `finished` | `filepath` | primary artifact candidate (last one wins) |
| `EmbedThumbnail`, `FFmpegMetadata` | `finished` | frame only (no artifact) | `msg` update |
| `Exec` | `finished`/`error` | `filepath`, and on error the exec's return code in `message` | error ⇒ `Postprocessing` failure |

Caption-specific policy the shim applies locally (from `aulos.caption_exts` /
`aulos.convert_srt_to_txt`), preserving legacy `update_status` behaviour that cannot be done in
Rust because it needs `info_dict`:
- filenames not ending in an allowed caption extension are **not** reported as artifacts for
  `download_type == "captions"`;
- when `format == "txt"`, the shim converts the `.srt` to `.txt` in place (strip cue numbers,
  timestamps, tags), deletes the `.srt`, and reports the `.txt`;
- for `download_type == "thumbnail"`, a `.webm` primary path is rewritten to `.jpg`.

### 7.6 Process lifecycle, cancellation, timeouts

`aulos-provider::proc::Child` is shared by `ytdlp`, `sc` (for N_m3u8DL-RE/ffmpeg) and `command`:

```rust
pub struct SpawnSpec {
    pub program: OsString, pub args: Vec<OsString>,
    pub env: EnvPolicy,                 // Inherit{allow: &[&str]} | Clear{set: BTreeMap} — plugins get Clear
    pub cwd: Option<PathBuf>,
    pub stdin: StdinPolicy,             // Null | Pipe(Bytes) — one-shot write then close
    pub kill_grace: Duration,
    pub stall_timeout: Option<Duration>,
    pub hard_timeout: Option<Duration>,
    pub rlimits: Rlimits,               // as_bytes, cpu_secs, nofile, fsize  (setrlimit in pre_exec)
    pub nice: Option<i32>,
}

pub struct Child { /* pid, pgid, stdout LinesStream, stderr tail ring, deadline state */ }
impl Child {
    pub fn spawn(spec: SpawnSpec) -> io::Result<Self>;   // process_group(0) => new pgid == pid
    pub async fn next_line(&mut self) -> Option<io::Result<String>>;
    pub async fn wait(&mut self) -> io::Result<ExitStatus>;
    pub async fn shutdown(&mut self, reason: KillReason);// SIGTERM(-pgid) → grace → SIGKILL(-pgid)
    pub fn stderr_tail(&self) -> &str;
}
```

- **Process group**: `std::os::unix::process::CommandExt::process_group(0)` makes the child a group
  leader, so `killpg` reaches `ffmpeg`, `deno`, `N_m3u8DL-RE` grandchildren. Legacy `proc.kill()`
  orphaned them (pain point #10).
- **Cancellation**: `tokio::select!` on `cancel.cancelled()`; then `shutdown(Canceled)`:
  `SIGTERM` to `-pgid`, wait `AULOS_KILL_GRACE_MS`, `SIGKILL` to `-pgid`, `waitpid` reap. The shim
  installs a `SIGTERM` handler that raises `KeyboardInterrupt` inside yt-dlp so `.part` files are
  flushed and an `error{code:"canceled"}` frame is emitted — graceful where legacy was `SIGKILL`.
- **Stall watchdog**: no frame for `AULOS_YTDLP_STALL_TIMEOUT` ⇒ `shutdown(Stalled)` ⇒
  `ProviderError::Timeout`. Independent of the Telegram stall *notification* (§14).
- **Hard timeout**: `AULOS_YTDLP_EXTRACT_TIMEOUT` for `extract`; downloads have no hard cap by
  default (a 10-hour 4K download is legitimate) — the stall watchdog is the safety net.
- **Zombie safety**: one reaper task per child; `Child::drop` sends `SIGKILL` to the group and spawns
  a detached reap task, so a panicking job never leaks a process.
- **Backpressure**: stdout is read with a 64 KiB `BufReader` and a 4 MiB line cap; if the Rust side
  cannot keep up, the pipe fills and the shim blocks in `write` — which is correct, since progress
  frames are already rate-limited at the source.

### 7.7 Shim implementation notes

`ytdlp_runner.py`, ~350 lines, stdlib + yt-dlp only. Structure, in order:

| Part | Behaviour |
|---|---|
| `emit(t, **kw)` | one-line JSON with `v/t/n/ts`, `ensure_ascii=False`, `separators=(",",":")`, line-buffered stdout |
| signal setup | `SIGTERM`/`SIGINT` → raise `KeyboardInterrupt` (so yt-dlp flushes `.part` and unwinds cleanly), then `error{code:"canceled"}` + `bye`, exit 130 |
| `coerce_opts(opts)` | reject `progress_hooks`/`postprocessor_hooks` (`bad_job/hook_override`); `impersonate: str` → `ImpersonateTarget.from_str`; `match_filter: str` → `match_filter_func`; anything else needing a callable ⇒ `bad_job/uncoercible_option` naming the key |
| `hello` | protocol, `yt_dlp.version.__version__`, `sys.version`, `os.getpid()`, discovered `yt_dlp_plugins` module names, POT probe |
| progress hook | key whitelist (§7.4) + `elapsed` + derived `stream`; throttled per `stream` to `emit_progress_every_ms`; non-`downloading` statuses always emitted |
| postprocessor hook | the mapping table of §7.5, including `__finaldir` joining, `requested_subtitles` and `chapters` enumeration |
| extract mode | `YoutubeDL(coerce_opts(opts))`; `extract_info(url, download=False)`; emit `resolved`; iterate the lazy `entries` generator emitting `entry` frames up to `max_entries`; the legacy strict-retry rule (`_type == "video"` and `formats == []` and an id/url present ⇒ retry with `extract_flat=False, ignore_no_formats_error=False`) |
| download mode | `YoutubeDL(...).download([url])`; caption `.srt`→`.txt` conversion and thumbnail extension rewrite per `aulos`; emit `artifact` frames; `result{ok, retcode, filename, size, artifacts}` |
| error classifier | the ordered exception-class → regex table of §7.3.5; `traceback` only when `aulos.debug` |
| frame budget | after `max_frames`, progress frames are dropped and one `warning` is emitted (a pathological HLS job cannot flood the pipe) |

### 7.8 `ytdlp` provider surface

| Aspect | Value |
|---|---|
| `matches` | `Match::FALLBACK` for `http`/`https`, `Match::NONE` otherwise |
| `capabilities` | resolve ✓, playlists ✓, streaming_resolve ✓, subtitles ✓, chapters ✓, thumbnails ✓, audio_extract ✓, cancel `ProcessGroup`, quality_advisory ✗, nfo_capable ✓, needs_network_at_download ✗, resume ✓ (`.part` files) |
| `limits` | `max_concurrent: None`, `uses_global_slot: true`, `max_concurrent_resolves: Some(AULOS_RESOLVE_CONCURRENCY)` |
| `preflight` | run the shim with `{"mode":"selftest"}`: verifies interpreter, yt-dlp import, protocol, plugin list; caches `yt_dlp` version for `GET version` |
| `health` | last selftest result + POT probe (`GET AULOS_POT_URL/ping`) + rolling error-rate over the last 20 jobs |
| `catalog` | the full legacy matrix (§10.2) |

---

## 8. `aulos-provider-sc` — StreamingCommunity, native

### 8.1 HTTP client decision

**Decision: `wreq` (the maintained successor of `rquest`) behind default-on feature
`sc-impersonate`, with two documented fallbacks selected by `AULOS_SC_HTTP`.**

| Mode | Client | When |
|---|---|---|
| `impersonate` (default) | `wreq` with a Chrome-1xx emulation profile (BoringSSL JA3/JA4 + HTTP/2 SETTINGS + header order) | normal operation; matches legacy `curl_cffi(impersonate="chrome")` |
| `plain` | `reqwest` + `rustls`, Chrome-ish header set/order | escape hatch if `wreq` breaks on a toolchain/arch bump; expected to be blocked by the site's WAF, but keeps the server building and the other providers alive |
| `curl` | spawn `curl-impersonate-chrome` via `proc::Child` | last resort; only needs a binary in the image |

Rationale and risk: TLS impersonation is the *only* reason legacy uses `curl_cffi`; without it the
Inertia endpoints return a challenge page. `wreq`/`rquest` vendor BoringSSL, which needs
`cmake` + `clang` in the builder stage and historically had arm64 build friction — hence the
feature gate and the two fallbacks (risk R3). The provider is written against a small internal
trait so the three modes are interchangeable:

```rust
#[async_trait]
trait ScHttp: Send + Sync {
    async fn get(&self, url: &Url, headers: &HeaderMap) -> Result<HttpResponse, ScError>;
    fn cookie_header(&self, url: &Url) -> Option<String>;   // serialised jar, for handoff to N_m3u8DL-RE
}
```

### 8.2 Matching

```rust
fn matches(&self, url: &Url) -> Match {
    let Some(host) = url.host_str() else { return Match::NONE };
    if !host.to_ascii_lowercase().contains("streamingcommunity") { return Match::NONE; }
    let p = url.path();
    if p.contains("/watch/") || p.contains("/season-") || p.contains("/titles/") {
        Match::host_path()      // 250
    } else {
        Match::host_suffix()    // 100 — still ours, but a plugin could outrank us on a mirror
    }
}
```

Legacy used a substring test on the hostname, matching any `*streamingcommunity*.tld` mirror. We
keep that (users depend on domain rotation) but additionally allow an operator to pin extra hosts
via `AULOS_SC_EXTRA_HOSTS` (comma list) — mirrors sometimes rebrand.

### 8.3 Scrape pipeline

| Step | Request | Extract | Cache |
|---|---|---|---|
| S1 site version | `GET {base}/it` | `div#app[data-page]` → JSON → `.version` | per `base`, TTL 30 min, single-flight |
| S2 Inertia page | `GET {base}{path}` with `x-inertia: true`, `x-inertia-version: <S1>`; `Accept: application/json` | JSON props | none (title pages: 60 s, so a 20-episode season is 1 fetch not 20) |
| S3 embed page | `GET props.embedUrl` | first `<iframe src=…>` (vixcloud) | none |
| S4 stream params | `GET iframe_src` | scan `<script>` bodies containing `masterPlaylist`: `'token': '<t>'`, `'expires': '<digits>'`, `window.streams = [...]` (JSON; pick `active == true`, else first, take `url`, unescape `\/`), fallback `url: '<u>'` inside `masterPlaylist`; `window.canPlayFHD = true` ⇒ add `h=1`; preserve existing query params; append `token`/`expires` | never cached (tokens expire in minutes) |

On a `409`/`404` from S2 the version is invalidated and S1 re-run **once** (Inertia version rotates
on deploys) — legacy would just fail. HTML parsing uses `scraper` (html5ever) for the two `find`
operations and `regex` for the script scraping, matching legacy semantics exactly (including "last
`<script>` containing `masterPlaylist` wins" ordering, which we make explicit as "first match in
document order", the BeautifulSoup behaviour).

### 8.4 Entry shapes

Preserved bit-for-bit so existing rows and NFOs keep working:

```
id     = "sc_<title_id>"                       (movie)
       | "sc_<title_id>_<episode_id>"          (episode)
title  = "<Name>"                              (movie)
       | "<Name> S01E02"                       (episode, no ep name)
       | "<Name> S01E02 - <ep name>"           (episode with name)
url    = the /watch/<id>[?e=<ep>] page url
state  = {"base_url": "...", "title_id": 1234, "episode_id": 5678}
meta   = { series, season, episode, episode_title, plot, year, tags, runtime_secs, poster }
```

`extract_season` (`/titles/{id}-{slug}/season-{n}`) emits a `GroupMeta{ kind: Season, title: "<Name> Season <n>" }`
and streams one `entry` per episode. **Change C-12:** the legacy path performed 3+ HTTP round trips
*per episode* during resolution (≈60 requests for a 20-episode season) purely to obtain an m3u8 it
then discarded. We do **zero** embed/stream requests during resolution — the season JSON already
contains every episode id, name and number — so resolving a season is 2 requests instead of ~60,
and the just-in-time re-extraction at download time is unchanged. `extract_title` for a TV title
resolves each season's JSON (1 request each) and emits a `Series` group with season sub-grouping in
`playlist.title`.

### 8.5 Download engines

`download()` re-extracts the m3u8 just in time (S1–S4 with a fresh jar), then:

```
out_path = out_dir / f"{sanitize(title)}.mp4"          # `[<>:"/\|?*]` -> "_", trailing ". " trimmed
headers  = { User-Agent, Referer: embed_url, Origin: iframe_origin, [Cookie] }
if SC_USE_FFMPEG -> ffmpeg engine
else             -> nm3u8 engine, on failure: cleanup partials, Phase("N_m3u8DL-RE failed, retrying with ffmpeg…"), ffmpeg engine
```

Legacy also wrote `<title>.info.json` for the (unwired) NFO generator. **Change C-13:** we do not
write `.info.json` as a side effect; the NFO hook (§16.2) writes `<title>.nfo` directly from
`Outcome::provider_meta`, and `.info.json` is written only when `AULOS_SC_WRITE_INFOJSON=true` for
users whose `YTDL_OPTIONS` `Exec` scripts consume it.

**N_m3u8DL-RE engine** — identical argv to legacy:

```
N_m3u8DL-RE <m3u8> --save-dir <out_dir> --save-name <safe_title>
  --tmp-dir <tmp> --thread-count <SC_THREAD_COUNT> --auto-select --del-after-done
  --no-log --mux-after-done format=mp4:muxer=ffmpeg --log-level INFO
  -H "User-Agent: …" -H "Referer: …" -H "Origin: …" [-H "Cookie: …"]
```

Output resolution order: `out_path` → newest `glob(<safe_title>*.mp4)` by mtime → **gapless
fallback mux**.

**Gapless fallback mux** (`mux.rs`), the legacy trick preserved with its rationale as a doc comment:
if `out_path` is missing but a segment dir `<out_dir>/<safe_title>/` exists, collect
`*.{m4s,ts,mp4,m4a,aac}` recursively, sort by **natural (numeric-aware) filename order** — never
mtime, because parallel segment downloads scramble mtimes — binary-concatenate in 1 MiB chunks into
`<seg_dir>/_merged.ts`, then

```
ffmpeg -y -i _merged.ts -map 0 -c copy -bsf:a aac_adtstoasc -movflags +faststart <out_path>
```

with a 600 s timeout. `-f concat` is explicitly **not** used: it pads each segment to its container
duration, producing a ~64 ms A/V gap and one dropped frame per join. On success the segment dir is
removed. `natural_cmp` is a hand-rolled digit-run comparator with property tests.

**ffmpeg engine**:

```
ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 <m3u8>     # 30 s, tolerated failure
ffmpeg -y -headers "<CRLF-joined headers>" -i <m3u8> -c copy -bsf:a aac_adtstoasc -progress pipe:1 <out_path>
```

`-progress pipe:1` gives `out_time_ms`, `total_size`, `speed=<x>x` on stdout; stderr is drained to a
20-line ring for error tails. Per `progress` block (≥ 250 ms apart) we emit
`Metrics { downloaded_bytes: total_size, percent_hint: out_time/duration*100, eta: (duration-out_time)/speed, speed: speed*(size/time) }`.
Using `percent_hint` from the *time* ratio (not the byte estimate) is strictly better than legacy's
`total_bytes_estimate` reverse-engineering and makes the bar smooth.

### 8.6 Progress parsing for N_m3u8DL-RE

Spectre.Console repaints several frames per read, and the first is usually `0/100 0.00%`, so
**last match wins** — preserved. `progress.rs`: strip ANSI CSI/OSC sequences, `\r` → `\n`, then run
four `regex` patterns and take the last capture of each:

| Pattern | Yields |
|---|---|
| `(\d+)/(\d+)\s+([\d.]+)%` | `fragment_index`, `fragment_count`, `percent_hint` |
| `([\d.]+)\s*(KB\|MB\|GB)\s*/\s*([\d.]+)\s*(KB\|MB\|GB)` | `downloaded_bytes`, `total_bytes` (1024-based) |
| `([\d.]+)\s*(KB\|MB\|GB)ps` | `speed` |
| `(\d{2}):(\d{2}):(\d{2})(?=\s\|$)` | `eta` |

**Change C-14:** legacy abused `downloaded_bytes`/`total_bytes` as *segment counters* when no size
pattern matched, which made the iOS byte fields lie. We put segment counts in
`fragment_index`/`fragment_count` (their actual meaning) and set `percent_hint` from the
percentage the tool already prints. Byte fields stay `null` until real sizes appear.

### 8.7 SC provider surface

| Aspect | Value |
|---|---|
| `capabilities` | resolve ✓, playlists ✓, streaming_resolve ✓, subtitles ✗, chapters ✗, thumbnails ✗, audio_extract ✗, cancel `ProcessGroup`, **quality_advisory ✓**, nfo_capable ✓, **needs_network_at_download ✓**, resume ✗ |
| `limits` | `max_concurrent: SC_MAX_CONCURRENT_DOWNLOADS`, **`uses_global_slot: false`**, `max_concurrent_resolves: 2`, `min_request_interval: 250 ms` |
| `catalog` | one `download_type: video`, one `format: mp4`, one `quality: best` labelled "Source" + `notice: "StreamingCommunity serves a single source rendition; the quality selector is ignored."` |
| naming | ignores `OUTPUT_TEMPLATE*` (legacy behaviour, documented in the catalog as `naming: "provider"`) |
| `preflight` | probe `N_m3u8DL-RE --version` and `ffmpeg -version`; missing N_m3u8DL-RE ⇒ degrade to ffmpeg-only (not a hard failure) |

---

## 9. The `command` community plugin format

Goal: **add a site in any language, no rebuild, no restart.**

### 9.1 Discovery and lifecycle

```
$AULOS_PLUGINS_DIR/                       (default /config/plugins)
  kinotek/
    plugin.toml                           required
    resolve.py                            any executable / interpreted file
    download.sh
    icon.png                              optional, served at api/v2/providers/command:kinotek/icon
    README.md                             optional
```

- Discovery at boot and on `notify` events (debounced 1 s) over `$AULOS_PLUGINS_DIR`.
- Each directory with a `plugin.toml` becomes provider id `command:<dirname>` (dirname must match
  `^[a-z0-9][a-z0-9_-]{0,31}$`).
- Manifest is parsed and **validated** (see §9.3); failures register the plugin as
  `Degraded(reason)` so it is visible in `healthz` and `GET api/v2/providers` rather than silently
  absent. This is the single most common plugin-author complaint and it is cheap to fix.
- `ReloadReport { added, updated, removed, failed: Vec<(name, error)> }` is broadcast as a WS
  `providers` frame, so the iOS catalog updates live.
- Hot reload never touches running jobs: a job holds `Arc<CommandProvider>` for its lifetime; the
  registry swaps the `Arc` for new jobs.

### 9.2 Complete `plugin.toml` schema

| Key | Type | Req | Default | Meaning |
|---|---|---|---|---|
| `manifest_version` | int | ✓ | — | must be `1`; anything else ⇒ `Degraded("unsupported manifest_version")` |
| `name` | string | ✓ | — | display name |
| `version` | string | ✓ | — | semver-ish, shown to clients |
| `description` | string | | `""` | shown in the catalog |
| `homepage` | string | | — | |
| `authors` | [string] | | `[]` | |
| **`[match]`** | | ✓ | | at least one of `hosts`/`host_regex` |
| `match.hosts` | [string] | | `[]` | host suffix match, case-insensitive; score 100 |
| `match.host_regex` | string | | — | anchored regex over the normalised host; score 150 |
| `match.path_regex` | string | | — | if set and it matches, score is promoted to 250 |
| `match.schemes` | [string] | | `["http","https"]` | |
| `match.exclude_path_regex` | string | | — | veto (returns `Match::NONE`) |
| **`[capabilities]`** | | | | all default `false` unless noted |
| `capabilities.resolve` | bool | | `false` | `false` ⇒ synthetic single-entry resolve (§4.6) |
| `capabilities.playlists` | bool | | `false` | |
| `capabilities.streaming_resolve` | bool | | `true` when `resolve` | resolve output is read line-by-line |
| `capabilities.subtitles` / `chapters` / `thumbnails` / `audio_extract` | bool | | `false` | catalog hints |
| `capabilities.quality_advisory` | bool | | `true` | |
| `capabilities.nfo_capable` | bool | | `false` | |
| `capabilities.cancel` | enum | | `"process_group"` | `process_group` \| `cooperative` \| `none` |
| **`[limits]`** | | | | |
| `limits.max_concurrent` | int | | `1` | per-provider semaphore |
| `limits.uses_global_slot` | bool | | `true` | `false` ⇒ own limit only |
| `limits.max_concurrent_resolves` | int | | `1` | |
| `limits.min_request_interval_ms` | int | | `0` | politeness |
| `limits.resolve_timeout_secs` | int | | `AULOS_PLUGIN_TIMEOUT_RESOLVE` | |
| `limits.download_stall_secs` | int | | `600` | no progress ⇒ kill |
| `limits.download_hard_timeout_secs` | int | | `0` (none) | |
| `limits.max_output_bytes` | int | | `67108864` | `RLIMIT_FSIZE`-adjacent guard on stdout |
| `limits.memory_bytes` | int | | `0` (none) | `RLIMIT_AS` |
| **`[resolve]`** | | | | required iff `capabilities.resolve` |
| `resolve.command` | [string] | ✓ | — | argv; element 0 resolved against the plugin dir then `PATH` |
| `resolve.format` | enum | | `"json_lines"` | `json_lines` \| `json` |
| `resolve.stdin` | enum | | `"none"` | `none` \| `json` (the whole resolve request as one JSON line) |
| `resolve.cwd` | string | | plugin dir | |
| **`[download]`** | | ✓ | | |
| `download.command` | [string] | ✓ | — | argv template |
| `download.cwd` | string | | plugin dir | |
| `download.stdin` | enum | | `"none"` | `none` \| `json` |
| `download.expect_output` | enum | | `"path_template"` | `path_template` (use `out_dir/out_name.ext`) \| `result_frame` (plugin prints a `result` line) \| `newest_in_dir` |
| `download.output_ext` | string | | `"mp4"` | used by `path_template` and `{out_name}` |
| `download.overwrite` | bool | | `true` | |
| **`[progress]`** | | | `{kind="none"}` | |
| `progress.kind` | enum | ✓ | — | `json_lines` \| `regex` \| `none` |
| `progress.source` | enum | | `"stdout"` | `stdout` \| `stderr` \| `both` |
| `progress.strip_ansi` | bool | | `true` | |
| `progress.cr_as_newline` | bool | | `true` | Spectre/ffmpeg style repaints |
| `progress.last_match_wins` | bool | | `true` | per read chunk |
| `progress.min_interval_ms` | int | | `250` | client-side of the coalescer |
| `progress.patterns` | [string] | ✓ if `regex` | — | regexes with named groups from `{percent, downloaded, total, speed, eta, status, fragment_index, fragment_count, msg}` |
| `progress.units` | table | | `{}` | e.g. `speed = "auto"`, `downloaded = "auto"`, `eta = "hms"`; `auto` understands `KB/MB/GB/KiB/MiB/GiB` suffixes (1024-based, matching N_m3u8DL-RE) |
| `progress.status_map` | table | | `{}` | maps captured `status` text → Aulos status |
| **`[env]`** | | | | |
| `env.pass` | [string] | | `[]` | env var names inherited from the server process |
| `env.set` | table | | `{}` | literal env; values may use the same templates as argv |
| **`[headers]`** | | | `{}` | name → template; exposed to the child as `AULOS_HEADER_<NAME>` and as `{headers_curl}` / `{headers_crlf}` templates |
| **`[catalog]`** | | | derived | the provider's client catalog (§10.4); when absent a single `video/mp4/best` type is synthesised |
| **`[[catalog.download_types]]`** | array of tables | | | `id`, `label`, `[[formats]]` with `id`, `label`, `qualities = [{id, label}]`, optional `codecs`, `notice` |

**Templates** available in `resolve.command`, `download.command`, `env.set`, `headers`:

| Token | Value |
|---|---|
| `{url}` | the item url |
| `{url_host}`, `{url_path}`, `{url_query}` | parsed parts |
| `{entry_id}` | `MediaEntry.key.id` |
| `{entry_title}` | raw title |
| `{out_dir}`, `{tmp_dir}` | absolute, already created |
| `{out_name}` | sanitised basename without extension (prefix + template-resolved) |
| `{out_path}` | `{out_dir}/{out_name}.{output_ext}` |
| `{output_ext}` | from manifest |
| `{download_type}`, `{format}`, `{quality}`, `{codec}` | selection |
| `{subtitle_language}`, `{subtitle_mode}` | selection |
| `{state}` | provider state as compact JSON (from `resolve`) |
| `{state.<key>}` | one field of the state object, JSON-scalar-stringified |
| `{playlist_index}`, `{playlist_count}`, `{playlist_title}` | when in a group |
| `{cookies_file}` | path to `STATE_DIR/cookies.txt` if present, else empty string |
| `{headers_curl}` | `-H "K: V"` pairs, shell-safe |
| `{headers_crlf}` | `K: V\r\n…` blob (ffmpeg style) |
| `{plugin_dir}` | the plugin's own directory |

Substitution is **argv-level, never shell-level**: each argv element is templated then passed
verbatim to `execvp`. There is no `sh -c`; a plugin that wants a shell writes
`command = ["/bin/sh", "-c", "…"]` explicitly and owns the consequences. Unknown tokens are a
validation error at load time, not a silent empty string.

### 9.3 Validation performed at load

| Check | Failure mode |
|---|---|
| `manifest_version == 1` | Degraded |
| `name`/`version` non-empty; dirname pattern | Degraded |
| at least one of `match.hosts` / `match.host_regex` | Degraded |
| all regexes compile; `host_regex` is anchored (auto-anchored with a warning if not) | Degraded |
| `download.command` non-empty; argv[0] resolves to an existing executable (plugin dir, then `PATH`) | Degraded |
| every template token is known; `{state.x}` only when `capabilities.resolve` | Degraded |
| `progress.patterns` compile and use only known group names; at least one group | Degraded |
| `catalog` ids are `^[a-z0-9_]+$`, unique | Degraded |
| plugin dir is not world-writable, files not setuid | Degraded (refuse to execute) |
| `limits.*` within hard caps (`max_concurrent ≤ 32`, timeouts ≤ 24 h) | clamp + warn |

### 9.4 Execution

**resolve**: spawn argv, `EnvPolicy::Clear` + `env.pass`/`env.set`, cwd from manifest, stdin the
JSON request when `resolve.stdin = "json"`, stdout parsed as JSON Lines (or one JSON doc). Accepted
line shapes:

```json
{"t":"group","key":"kinotek:series:914","title":"The Long Dusk — Season 2","kind":"season","expected":8}
{"t":"entry","key":"kinotek:ep:914-2-01","url":"https://kinotek.example/w/914?e=1",
 "title":"The Long Dusk S02E01 — Ashes","duration":2731.0,"thumbnail":"https://…/1.jpg",
 "series":{"series":"The Long Dusk","season":2,"episode":1,"episode_title":"Ashes"},
 "state":{"stream_id":"a91f","dc":"eu-3"},
 "meta":{"plot":"…","year":2025,"tags":["drama"],"runtime_secs":2731}}
{"t":"note","message":"2 episodes are region-locked and were skipped"}
{"t":"error","code":"unavailable","message":"title 914 not found","retryable":false}
```

A bare object without `t` is accepted as `t:"entry"` (author ergonomics). `key` is optional and
defaults to `sha256(url)[..16]`. Unknown fields are ignored; unknown `t` values produce one warning
and are skipped, so the format can grow.

**download**: spawn argv the same way. Progress is parsed per `[progress]`. Success criteria by
`expect_output`:

| `expect_output` | Success |
|---|---|
| `path_template` | exit 0 **and** `{out_path}` exists and is non-empty |
| `result_frame` | exit 0 **and** a `{"t":"result","path":"…","size":…,"artifacts":[…]}` line was printed |
| `newest_in_dir` | exit 0 **and** at least one file in `{out_dir}` newer than job start; newest wins |

Non-zero exit ⇒ `ProviderError::Tool { tool: plugin, code, tail: last 2 KiB of stderr }`. The
`tail` is surfaced to the user (cleaned of ANSI) — plugin authors need that.

**Isolation** (best-effort, documented as such): cleared environment, no inherited fds beyond
stdio, `setsid`/new process group, `RLIMIT_AS`/`RLIMIT_FSIZE`/`RLIMIT_NOFILE`/`RLIMIT_CPU` from
`[limits]`, `nice(5)`, cwd pinned, and `out_dir`/`tmp_dir` are the only paths we hand over. We do
**not** claim a security boundary: a plugin runs as the server user and can do anything that user
can (risk R7). The docs say so in bold, and `GET api/v2/providers` shows every plugin's argv so an
operator can audit what is installed.

### 9.5 Example plugin: `kinotek` (hypothetical site)

`/config/plugins/kinotek/plugin.toml`:

```toml
manifest_version = 1
name        = "Kinotek"
version     = "0.3.1"
description = "Kinotek.example series and films via their public HLS API"
homepage    = "https://github.com/example/aulos-plugin-kinotek"
authors     = ["someone <someone@example.org>"]

[match]
hosts      = ["kinotek.example", "kinotek.test"]
host_regex = '^(www\.)?kinotek\.(example|test)$'
path_regex = '^/(w|series)/\d+'
schemes    = ["https"]

[capabilities]
resolve            = true
playlists          = true
streaming_resolve  = true
subtitles          = true
quality_advisory   = false
nfo_capable        = true
cancel             = "process_group"

[limits]
max_concurrent             = 2
uses_global_slot           = true
max_concurrent_resolves    = 1
min_request_interval_ms    = 400
resolve_timeout_secs       = 45
download_stall_secs        = 300
max_output_bytes           = 33554432

[resolve]
command = ["python3", "resolve.py", "{url}"]
format  = "json_lines"
stdin   = "none"

[download]
command = [
  "python3", "download.py",
  "--url", "{url}",
  "--state", "{state}",
  "--quality", "{quality}",
  "--subs", "{subtitle_language}",
  "--out", "{out_path}",
  "--tmp", "{tmp_dir}",
]
expect_output = "result_frame"
output_ext    = "mkv"

[progress]
kind             = "regex"
source           = "both"
strip_ansi       = true
cr_as_newline    = true
last_match_wins  = true
min_interval_ms  = 250
patterns = [
  '(?P<percent>[\d.]+)%\s+(?P<downloaded>[\d.]+\s*[KMG]i?B)\s*/\s*(?P<total>[\d.]+\s*[KMG]i?B)',
  '(?P<speed>[\d.]+\s*[KMG]i?B)/s',
  'ETA\s+(?P<eta>\d{1,2}:\d{2}(:\d{2})?)',
  'stage=(?P<status>fetch|mux|done)',
]
units = { downloaded = "auto", total = "auto", speed = "auto", eta = "hms" }
status_map = { fetch = "downloading", mux = "postprocessing", done = "finished" }

[env]
pass = ["HTTPS_PROXY", "NO_PROXY"]
set  = { KINOTEK_UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36", PYTHONUNBUFFERED = "1" }

[headers]
Referer    = "https://kinotek.example/w/{state.title_id}"
User-Agent = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36"

[[catalog.download_types]]
id    = "video"
label = "Video"
  [[catalog.download_types.formats]]
  id    = "mkv"
  label = "MKV (source)"
  qualities = [
    { id = "best",  label = "Best available" },
    { id = "1080",  label = "1080p" },
    { id = "720",   label = "720p" },
  ]
  [[catalog.download_types.formats]]
  id     = "mp4"
  label  = "MP4 (remux)"
  notice = "Remuxed with ffmpeg; adds ~10 s per title."
  qualities = [{ id = "best", label = "Best available" }]

[[catalog.download_types]]
id    = "captions"
label = "Subtitles only"
  [[catalog.download_types.formats]]
  id        = "srt"
  label     = "SRT"
  qualities = [{ id = "best", label = "Best" }]
```

`resolve.py` (contract, ~40 lines of any language): read argv[1], print one `group` line and one
`entry` line per episode, exit 0. `download.py`: fetch, write to `--out`, print progress lines
matching the regexes on stderr, print
`{"t":"result","path":"…","size":123,"artifacts":[{"role":"subtitle","path":"…","language":"en"}]}`
on stdout, exit 0.

`plugins/examples/kinotek/` in the repo ships exactly this, plus a `tests/plugin_kinotek.rs`
integration test that runs it against a `wiremock` server — which doubles as the plugin author's
template.

---

## 10. The format/quality catalog

The legacy `get_available_formats()` list lived in `main.py`, was hard-coded, and was pushed to
clients via a `formats` Socket.IO event. iOS caches it in the app group and falls back to a
hard-coded copy. Two problems: it is provider-blind (offers `2160` for a StreamingCommunity item
that only has one rendition), and it cannot describe anything a plugin adds.

### 10.1 Types (`aulos-core::catalog`)

```rust
pub struct FormatCatalog {
    pub provider: ProviderId,
    pub version: u32,                            // bumped on any change; part of the ETag
    pub naming: NamingPolicy,                    // Template | Provider  (SC ignores OUTPUT_TEMPLATE)
    pub download_types: Vec<DownloadTypeSpec>,
    pub notices: Vec<Notice>,
}
pub struct DownloadTypeSpec {
    pub id: String, pub label: String,
    pub formats: Vec<FormatSpec>,
    pub default_format: String,
    pub options: Vec<OptionSpec>,                // subtitle_language, split_by_chapters, …
}
pub struct FormatSpec {
    pub id: String, pub label: String,
    pub qualities: Vec<QualitySpec>,
    pub default_quality: String,
    pub codecs: Vec<CodecSpec>,                  // empty ⇒ codec not applicable
    pub notice: Option<String>,
    pub flags: FormatFlags,                      // { advisory, requires_ffmpeg, lossy_remux, slow }
}
pub struct QualitySpec { pub id: String, pub label: String, pub notice: Option<String> }
pub struct OptionSpec { pub id: String, pub label: String, pub kind: OptionKind, pub default: Value,
                        pub choices: Vec<Choice>, pub help: Option<String> }
pub enum OptionKind { Bool, Int { min: i64, max: i64 }, Enum, Text { pattern: Option<String> }, Path }
```

### 10.2 The `ytdlp` catalog

Exactly the legacy matrix, now with labels and per-entry notices:

| download_type | format | qualities |
|---|---|---|
| `video` | `any` | best, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| `video` | `mp4` | best, **best_remux**, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| `video` | `ios` | best |
| `audio` | `m4a` | best, 192, 128 |
| `audio` | `mp3` | best, 320, 192, 128 |
| `audio` | `opus` / `wav` / `flac` | best |
| `captions` | `srt`, `txt`, `vtt`, `ttml`, `sbv`, `scc`, `dfxp` | best |
| `thumbnail` | `jpg` | best |

Codecs (`video` only): auto, h264, h265, av1, vp9. Options: `subtitle_language` (Text, pattern
`^[A-Za-z0-9][A-Za-z0-9-]{0,34}$`), `subtitle_mode` (Enum, `captions` only),
`split_by_chapters` (Bool), `chapter_template` (Text), `playlist_item_limit` (Int 0..=10000),
`folder` (Path, only when `CUSTOM_DIRS`), `custom_name_prefix` (Text),
`ytdl_options_presets` (Enum multi, choices from the preset registry),
`ytdl_options_overrides` (Text/JSON, present only when `ALLOW_YTDL_OPTIONS_OVERRIDES`).
`best_remux` carries `notice: "Re-encodes audio after download (slower, fixes SponsorBlock drift)"`
and `flags.slow = true`. `worst` carries the honest
`notice: "Selector currently resolves to the best available stream"` (legacy quirk, §19.3 C-15).

### 10.3 Merged view and `?url=`

```rust
pub struct MergedCatalog {
    pub etag: String,                    // sha256 of the canonical JSON, 16 hex chars
    pub providers: Vec<FormatCatalog>,
    pub union: FormatCatalog,            // provider = "*"; union of ids, intersection of notices
    pub defaults: CatalogDefaults,       // from config: OUTPUT_TEMPLATE_CHAPTER, limits, flags
}
```

`GET api/v2/catalog` → merged. `GET api/v2/catalog?url=<encoded>` → the catalog of the provider that
`Registry::select` would pick, plus `{"provider":"streamingcommunity","match":{"score":250,"reason":"host_and_path"}}`.
**This is the feature that makes the iOS share sheet honest**: paste an SC link and the quality
picker collapses to "Source" with an explanation, paste a YouTube link and the full matrix appears —
with no client release.

### 10.4 Plugin catalogs

A plugin's `[catalog]` table deserialises straight into `FormatCatalog` (defaults filled in). If
absent, a single `video/mp4/best` type is synthesised with
`notice: "This plugin does not advertise formats; quality selections are ignored."`

### 10.5 Validation is catalog-driven

`AddRequest` validation is a *lookup in the selected provider's catalog*, not a hard-coded match
arm. Adding a format to `ytdlp` therefore requires one catalog edit, and a plugin gets validation
for free. The v1 shim keeps the legacy hard-coded per-type lists as an extra pre-check so a legacy
client's 400s stay byte-identical.

### 10.6 Wire shape (also delivered as a WS `catalog` frame on connect)

```json
{
  "etag": "9f2b41c0d7e5a318",
  "defaults": { "download_type": "video", "format": "mp4", "quality": "best",
                "chapter_template": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
                "playlist_item_limit": 0, "subscription_check_interval": 60,
                "allow_ytdl_options_overrides": false, "custom_dirs": true },
  "presets": ["archive", "sponsorblock"],
  "providers": [
    { "provider": "ytdlp", "version": "2026.8.30.232658.dev0", "naming": "template",
      "capabilities": { "playlists": true, "subtitles": true, "quality_advisory": false, "…": "…" },
      "download_types": [
        { "id": "video", "label": "Video", "default_format": "mp4",
          "formats": [
            { "id": "mp4", "label": "MP4", "default_quality": "best",
              "flags": { "advisory": false, "requires_ffmpeg": true, "slow": false },
              "qualities": [ { "id": "best", "label": "Best" },
                             { "id": "best_remux", "label": "Best (remux)",
                               "notice": "Re-encodes audio after download" },
                             { "id": "1080", "label": "1080p" } ],
              "codecs": [ { "id": "auto", "label": "Auto" }, { "id": "h264", "label": "H.264" } ] }
          ],
          "options": [ { "id": "split_by_chapters", "label": "Split by chapters", "kind": "bool", "default": false } ] }
      ] },
    { "provider": "streamingcommunity", "naming": "provider",
      "capabilities": { "quality_advisory": true, "…": "…" },
      "download_types": [ { "id": "video", "label": "Video", "default_format": "mp4",
        "formats": [ { "id": "mp4", "label": "MP4",
          "notice": "StreamingCommunity serves one source rendition; quality is ignored.",
          "flags": { "advisory": true },
          "qualities": [ { "id": "best", "label": "Source" } ], "codecs": [] } ] } ] },
    { "provider": "command:kinotek", "version": "0.3.1", "…": "…" }
  ]
}
```

---

## 11. `aulos-store` — SQLite

### 11.1 Topology

One SQLite file, WAL, `synchronous = NORMAL`, `busy_timeout = 5000`, `foreign_keys = ON`,
`journal_size_limit`, `mmap_size = 64 MiB`.

```
callers ──Store (Clone handle)──┬── writes ──► mpsc(1024) ──► Writer task (dedicated OS thread,
                                │                              one rusqlite::Connection, statement cache)
                                └── reads  ──► r2d2 pool (4 conns, read-only) via spawn_blocking
```

Writes are serialised through one connection — SQLite allows exactly one writer anyway, and this
removes all `SQLITE_BUSY` handling. Every write command carries a `oneshot` reply channel; callers
that do not need the result (progress-free by construction — we never write progress) use
`Store::fire_and_forget` with an error-logging drop guard. The writer batches: it drains up to 256
commands or 5 ms, wraps them in one `IMMEDIATE` transaction, commits once. Adding a 500-item
playlist becomes ~2–4 transactions instead of 500 whole-file JSON rewrites (legacy pain point #5).

```rust
#[derive(Clone)]
pub struct Store { tx: mpsc::Sender<Cmd>, reads: Pool<SqliteConnectionManager>, seq: Arc<AtomicU64> }

impl Store {
    pub async fn open(path: &Path, cfg: &StoreConfig) -> Result<Self, StoreError>;   // runs migrations
    // items
    pub async fn insert_items(&self, rows: Vec<NewItem>) -> Result<Vec<ItemId>, StoreError>;
    pub async fn patch_item(&self, id: ItemId, p: ItemPatch) -> Result<Seq, StoreError>;
    pub async fn patch_items(&self, batch: Vec<(ItemId, ItemPatch)>) -> Result<Seq, StoreError>;
    pub async fn delete_items(&self, ids: &[ItemId]) -> Result<usize, StoreError>;
    pub async fn list_items(&self, f: ItemFilter) -> Result<Vec<ItemView>, StoreError>;
    pub async fn get_item(&self, id: ItemId) -> Result<Option<ItemView>, StoreError>;
    pub async fn claim_inflight(&self) -> Result<Vec<ItemView>, StoreError>;  // restart recovery
    // groups / artifacts / subscriptions / telegram / eventlog / kv
    pub async fn upsert_group(&self, g: NewGroup) -> Result<GroupId, StoreError>;
    pub async fn group_counters(&self, id: GroupId) -> Result<GroupCounters, StoreError>;
    pub async fn add_artifacts(&self, id: ItemId, a: Vec<Artifact>) -> Result<(), StoreError>;
    pub async fn append_events(&self, evs: &[StateEvent]) -> Result<Seq, StoreError>;
    pub async fn events_since(&self, since: Seq, limit: usize) -> Result<EventsSince, StoreError>;
}
```

`ItemPatch` is a struct of `Option<T>` fields with a `dirty: FieldMask` bitset, so an
`UPDATE items SET status=?, msg=? WHERE id=?` writes only what changed.

### 11.2 DDL (migration 0001)

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
-- rows: schema_version, created_at, imported_from_json, seq_high_water, instance_id

CREATE TABLE groups (
  id           TEXT PRIMARY KEY,               -- GroupId (ULID)
  provider     TEXT NOT NULL,
  provider_key TEXT,                           -- playlist/series id, for idempotent re-adds
  kind         TEXT NOT NULL,                  -- playlist|channel|season|series
  title        TEXT NOT NULL,
  source_url   TEXT NOT NULL,
  expected     INTEGER,                        -- entries announced by the provider
  status       TEXT NOT NULL,                  -- resolving|active|done|error|canceled
  created_at   INTEGER NOT NULL,               -- epoch ms
  updated_at   INTEGER NOT NULL,
  seq          INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX groups_provider_key ON groups(provider, provider_key)
  WHERE provider_key IS NOT NULL;

CREATE TABLE items (
  id                   TEXT PRIMARY KEY,        -- ItemId (ULID) — sorts by creation time
  group_id             TEXT REFERENCES groups(id) ON DELETE SET NULL,
  provider             TEXT NOT NULL,
  entry_key            TEXT NOT NULL,           -- provider-scoped stable media id
  url                  TEXT NOT NULL,
  title                TEXT NOT NULL,
  status               TEXT NOT NULL,
  -- selection (flat: queried by the UI and by v1)
  download_type        TEXT NOT NULL,
  format               TEXT NOT NULL,
  quality              TEXT NOT NULL,
  codec                TEXT NOT NULL DEFAULT 'auto',
  subtitle_language    TEXT NOT NULL DEFAULT 'en',
  subtitle_mode        TEXT NOT NULL DEFAULT 'prefer_manual',
  split_by_chapters    INTEGER NOT NULL DEFAULT 0,
  chapter_template     TEXT,
  playlist_item_limit  INTEGER NOT NULL DEFAULT 0,
  folder               TEXT,
  custom_name_prefix   TEXT,
  -- provider handoff + options (JSON; validated on read, never trusted)
  provider_state       TEXT NOT NULL DEFAULT '{}',
  tuning               TEXT NOT NULL DEFAULT '{}',  -- {presets:[],overrides:{}}
  entry_meta           TEXT NOT NULL DEFAULT '{}',  -- EntryMeta for NFO/outtmpl
  playlist_index       INTEGER,
  playlist_count       INTEGER,
  playlist_title       TEXT,
  -- results
  filename             TEXT,                    -- relative to the serving root
  size                 INTEGER,
  msg                  TEXT,
  error_code           TEXT,
  error_message        TEXT,
  -- lifecycle
  source_kind          TEXT NOT NULL,           -- api|v1|telegram|subscription|plugin
  source_ref           TEXT,                    -- telegram chat id / subscription id
  attempts             INTEGER NOT NULL DEFAULT 0,
  auto_start           INTEGER NOT NULL DEFAULT 1,
  created_at           INTEGER NOT NULL,
  started_at           INTEGER,
  finished_at          INTEGER,
  clear_after          INTEGER,                 -- epoch ms; CLEAR_COMPLETED_AFTER
  seq                  INTEGER NOT NULL
) STRICT;
CREATE INDEX items_status_seq  ON items(status, seq);
CREATE INDEX items_group       ON items(group_id, playlist_index);
CREATE INDEX items_url         ON items(url);
CREATE INDEX items_entry       ON items(provider, entry_key);
CREATE INDEX items_clear_after ON items(clear_after) WHERE clear_after IS NOT NULL;

CREATE TABLE artifacts (
  id        INTEGER PRIMARY KEY,
  item_id   TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
  role      TEXT NOT NULL,                     -- media|subtitle|chapter|thumbnail|infojson|nfo|other
  path      TEXT NOT NULL,                     -- relative to the serving root
  size      INTEGER,
  language  TEXT,
  label     TEXT
) STRICT;
CREATE UNIQUE INDEX artifacts_unique ON artifacts(item_id, path);

CREATE TABLE subscriptions (
  id                    TEXT PRIMARY KEY,
  name                  TEXT NOT NULL,
  url                   TEXT NOT NULL,
  enabled               INTEGER NOT NULL DEFAULT 1,
  check_interval_minutes INTEGER NOT NULL DEFAULT 60,
  download_type         TEXT NOT NULL DEFAULT 'video',
  format                TEXT NOT NULL DEFAULT 'any',
  quality               TEXT NOT NULL DEFAULT 'best',
  codec                 TEXT NOT NULL DEFAULT 'auto',
  folder                TEXT NOT NULL DEFAULT '',
  custom_name_prefix    TEXT NOT NULL DEFAULT '',
  auto_start            INTEGER NOT NULL DEFAULT 1,
  playlist_item_limit   INTEGER NOT NULL DEFAULT 0,
  split_by_chapters     INTEGER NOT NULL DEFAULT 0,
  chapter_template      TEXT NOT NULL DEFAULT '',
  subtitle_language     TEXT NOT NULL DEFAULT 'en',
  subtitle_mode         TEXT NOT NULL DEFAULT 'prefer_manual',
  tuning                TEXT NOT NULL DEFAULT '{}',
  last_checked          INTEGER,                -- epoch ms
  next_check_at         INTEGER,                -- epoch ms (scheduler source of truth)
  consecutive_failures  INTEGER NOT NULL DEFAULT 0,
  error                 TEXT,
  created_at            INTEGER NOT NULL,
  seq                   INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX subscriptions_url ON subscriptions(url);
CREATE INDEX subscriptions_due ON subscriptions(enabled, next_check_at);

CREATE TABLE subscription_seen (
  subscription_id TEXT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
  entry_key       TEXT NOT NULL,
  seen_at         INTEGER NOT NULL,
  PRIMARY KEY (subscription_id, entry_key)
) STRICT, WITHOUT ROWID;
CREATE INDEX subscription_seen_age ON subscription_seen(subscription_id, seen_at DESC);

CREATE TABLE telegram_chats (
  chat_id  INTEGER PRIMARY KEY,
  config   TEXT NOT NULL,                       -- JSON, same keys as legacy telegram_bot_config.json
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE event_log (
  seq       INTEGER PRIMARY KEY,                -- global monotonic; == items.seq / groups.seq values
  kind      TEXT NOT NULL,                      -- added|delta|completed|removed|group|subscription|catalog
  item_id   TEXT,
  group_id  TEXT,
  payload   TEXT NOT NULL,                      -- the exact JSON body the WS frame carries
  at        INTEGER NOT NULL
) STRICT;
CREATE INDEX event_log_at ON event_log(at);

CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;  -- cookies meta, pot state, etc.
```

`seq` allocation: a single `AtomicU64` in the `Store` handle, initialised from
`max(schema_meta.seq_high_water, max(event_log.seq))` at open, persisted every 256 allocations and
on clean shutdown. Restart never re-uses a `seq`, so `?since=` is safe across restarts (and a
`since` newer than our high-water yields a full snapshot).

`event_log` is trimmed to `AULOS_EVENT_LOG_KEEP` rows by the writer every 1000 appends
(`DELETE FROM event_log WHERE seq <= (SELECT MAX(seq) - ? FROM event_log)`).

### 11.3 Legacy importer

Runs automatically when `AULOS_DB_PATH` does not exist and `STATE_DIR` contains any of the legacy
files. Reads `schema_version: 1` **and** `2` (v1 rows are upconverted with the same field defaults
`DownloadInfo.__setstate__` applied). Pickle/shelve is out of scope (BRIEF).

| Legacy file | → | Notes |
|---|---|---|
| `queue.json` | `items` with `status='queued'`, `auto_start=1` | ordered by legacy `timestamp` (ns) so ULIDs are minted in the original order |
| `pending.json` | `items` with `status='queued'`, `auto_start=0` | |
| `completed.json` | `items` with `status='finished'` or `'error'` (from the record) | `filename`, `size`, `chapter_files` → `artifacts` |
| `subscriptions.json` | `subscriptions` + `subscription_seen` (one row per id) | `seen_ids` list becomes rows; `next_check_at = now + jitter` |
| `telegram_bot_config.json` | `telegram_chats` | keys parsed as i64; unparseable keys logged and skipped |
| `cookies.txt` | `kv['cookiefile']` + runtime override | presence check only |

Provider attribution for imported rows: `entry.extractor` containing `streamingcommunity` ⇒
`provider='streamingcommunity'` with `provider_state` rebuilt from `_sc_base_url` + the url regex;
everything else ⇒ `provider='ytdlp'`. `entry_key` = legacy `id`. Import is one transaction; on any
error the DB file is deleted and the process exits non-zero with the offending file+record, and the
legacy files are left untouched (so the operator can roll back to the Python image).
Import writes `schema_meta.imported_from_json = <iso8601>` and renames each source to
`<name>.json.imported` **only after** a successful commit.

---

## 12. `aulos-queue`

### 12.1 Actor topology

```
              ┌──────────────────────── QueueHandle (Clone) ────────────────────────┐
              │  add(AddRequest) -> Vec<ItemId>      cancel(ids)   start(ids)       │
              │  clear(ids)      cancel_add(job)     retry(ids)                     │
              └───────────────────────────┬─────────────────────────────────────────┘
                                          │ mpsc(4096) Cmd
                                          ▼
                              ┌───────────────────────┐
                              │  Scheduler task       │  owns: pending order, slot bookkeeping,
                              │  (single, no locks)   │  cancel tokens, group aggregates
                              └───┬──────────┬────────┘
                    spawn         │          │        spawn
        ┌───────────────────────┐ │          │ ┌───────────────────────────┐
        │ Resolve task (N≤4)   │◄┘          └►│ Download task (N≤slots)   │
        │ provider.resolve()   │              │ provider.download()       │
        └──────────┬───────────┘              └────────────┬──────────────┘
                   │ ResolveSink                           │ ProgressSink
                   ▼                                       ▼
        ┌──────────────────────────────────────────────────────────────┐
        │  ProgressBus task: coalescer + normaliser + differ           │
        │  ticks every AULOS_WS_BATCH_MS                               │
        └────────┬──────────────────────┬──────────────────────────────┘
                 │ StateEvent           │ ItemPatch batches
                 ▼                      ▼
        broadcast::Sender<Arc<Frame>>   Store writer
                 │
                 ├──► WS connections (each with its own lagged-recovery path)
                 ├──► Notifier fan-out (Telegram, future APNs)
                 └──► Hook dispatcher (jellyfin debounce, nfo, audio-sync)
```

Channel inventory:

| Channel | Type | Capacity | Overflow policy |
|---|---|---|---|
| `Cmd` | `mpsc` | 4096 | `try_send` → 503 `busy` (never blocks a request handler) |
| provider events | `mpsc` | 256/job | provider is slowed via `Notify` (lossless) |
| provider metrics | `AtomicCell` | 1 | latest-wins (lossy by design) |
| `StateEvent` → subscribers | `broadcast` | 1024 | a lagged WS client receives `{"t":"resync"}` and re-snapshots |
| hook jobs | `mpsc` | 256 | oldest dropped with a warning (hooks are best-effort) |
| store writes | `mpsc` | 1024 | back-pressures the scheduler (correct: durability first) |

### 12.2 Add path (async, BRIEF §5)

`POST api/v2/downloads` handler work: validate against the catalog (§10.5), resolve `out_dir`
(containment-checked), `Store::insert_items` for **one** row in `status='resolving'`, return
`202 {"id": …}`. Everything else is background:

1. Scheduler receives `Cmd::Add { item_id, url, request, job }`.
2. `Registry::select(url, hint)`; store `provider` on the row.
3. Acquire a resolve permit (global + per-provider). While waiting, the item stays `resolving`.
4. Run `provider.resolve(...)` with a `ResolveSink` that:
   - on `group()`: `upsert_group`, mark the original row as the **group anchor** (its
     `group_id` = the new group, `status` stays `resolving`, and it is *converted* to a group
     placeholder that the client renders as a folder row);
   - on `entry()`: buffer; flush to `Store::insert_items` in batches of 64 or every 200 ms;
     emit `added` frames in the same batches.
5. Single-entry resolution *reuses* the original row (no add/remove churn on the client) — its
   `title`, `entry_key`, `provider_state` are patched and it goes `resolving → queued`.
6. Playlist resolution converts the anchor row into a group and creates N children. The anchor's
   own item row is deleted and a `removed` frame is emitted **in the same WS batch** as the
   `group` + first `added` frames, so the client never sees a flash.
7. `truncated` ⇒ a `note` on the group.

Cancel-add: `POST api/v2/jobs/{job_id}/cancel` cancels the `CancellationToken` of the resolve task;
buffered-but-unflushed entries are dropped, already-created children stay (matching legacy
"Canceled - added N items before cancel", but now with an exact count in the response).

### 12.3 Scheduling

Pending order is `(auto_start DESC, seq ASC)` — FIFO within priority, deterministic, and identical
to the ULID order the client sees. The scheduler keeps an in-memory `VecDeque<ItemId>` rebuilt from
the DB at boot; the DB is the source of truth, the deque is a cache.

For each dequeued item: acquire per-provider permit → acquire global permit (unless
`uses_global_slot == false`) → check the cancel token → spawn the download task. Permits are held
by the task and released on drop, **after** the store write of the terminal state but **not**
during hook dispatch (fixing legacy pain point #11, where `_post_download_cleanup` ran inside the
semaphore).

### 12.4 Per-download task

```
1. mkdir out_dir/tmp_dir (idempotent)
2. sink.event(Status(Preparing))  → store patch (status, started_at)
3. if provider.capabilities.needs_network_at_download: nothing special — provider handles it
4. res = select! { provider.download(job, &sink, cancel.clone()) , cancel.cancelled() }
5. terminal:
     Ok(outcome)  -> status=Finished, filename=relpath(outcome.primary), size, artifacts
     Err(Canceled)-> status=Canceled  (item is KEPT, unlike legacy which dropped it)
     Err(e)       -> status=Error, error_code=e.code(), error_message=e.user_message();
                     if e.retryable() != No && attempts < AULOS_MAX_ATTEMPTS(=2): requeue with backoff
6. store patch + `completed` frame (prompt, unbatched)
7. group aggregate recompute + `group` frame
8. hooks.dispatch(item) — outside the slot
9. if clear_after: register with the autoclear wheel
```

### 12.5 Cancellation

One `CancellationToken` per item, in a `DashMap<ItemId, CancellationToken>` owned by the scheduler.
`cancel(ids)`:

| Item state | Action |
|---|---|
| `queued` | remove from the deque, `status=Canceled`, `completed` frame (terminal) |
| `resolving` | cancel the resolve token; children already created are left, group noted |
| `preparing`/`downloading`/`postprocessing` | `token.cancel()` ⇒ provider `select!` fires ⇒ `Child::shutdown` (SIGTERM to `-pgid`, grace, SIGKILL) ⇒ `Err(Canceled)` |
| terminal | 409 `conflict` (legacy silently ignored; being explicit lets the client stop guessing) |

Partial-file cleanup: the queue removes `tmp_dir` entries it created, and any `Outcome`-less file
the provider reported via `ProviderEvent::Artifact` before cancellation, but only inside
`tmp_dir`/`out_dir` and only paths it has actually seen — never a glob.

### 12.6 Retry policy

| Error class | Policy |
|---|---|
| `Network`, `throttled` | up to 2 automatic retries, backoff 30 s / 120 s + ±20 % jitter |
| `BotCheck` | 1 automatic retry after 60 s (POT may have just restarted); then `error` |
| `Tool` (N_m3u8DL-RE) | handled internally by the provider (ffmpeg fallback); no queue retry |
| `AuthRequired`, `GeoRestricted`, `Unsupported`, `NoFormat`, `Unavailable` | no retry |
| `NotYetLive` | reschedule at `release_ts - 60 s` (max 7 days out); shows as `queued` with a `msg` |
| `Timeout`, `Postprocessing`, `Disk`, `Contract`, `Other` | no retry |
| manual `POST items/{id}/start` | always allowed from a terminal state; `attempts` reset |

### 12.7 Groups

The scheduler keeps `HashMap<GroupId, GroupCounters { total, queued, active, finished, error, canceled, bytes_done, bytes_total }>`
updated on every item transition, and emits a `group` delta at most once per batch tick. The client
renders a whole 500-item playlist as one row with `percent = finished/total*100` plus
`active_percent` (mean of active children) — no per-child traffic needed until the row is expanded
(`GET api/v2/groups/{id}/items?limit=&cursor=`).

### 12.8 Restart recovery

At boot, before the API binds: `claim_inflight()` returns every item in a non-terminal, non-queued
state (`resolving|preparing|downloading|postprocessing`). Each is patched to `queued` with
`msg = "Requeued after server restart"` and `attempts` **not** incremented. Then normal scheduling
starts, bounded by the usual slots. **Change C-4:** legacy re-added everything at once, before any
client connected. Here recovery is ordinary queue work, rate-limited by `MAX_CONCURRENT_DOWNLOADS`,
and visible in the snapshot the first client receives. Items with `auto_start=0` return to the
pending list instead.

### 12.9 Hook dispatch

`hooks.dispatch(item)` sends `HookJob { item: ItemView, outcome_meta: Value }` to
`aulos-hooks::Dispatcher`, which runs each enabled hook in order with its own timeout, collecting
`HookOutcome`s onto the item (`artifacts` for NFO, a `msg` note on failure). Hook failures never
change the item's terminal status — a Jellyfin outage must not turn a good download into an error.

---

## 13. `aulos-api`

All routes under `<URL_PREFIX>`; `Content-Type: application/json` everywhere;
`X-Request-Id` echoed/generated; CORS from `CORS_ALLOWED_ORIGINS` with `Access-Control-Allow-Methods`
included (legacy omitted it).

### 13.1 v2 REST

| Method | Path | Body | Success | Errors |
|---|---|---|---|---|
| POST | `api/v2/downloads` | `AddRequestV2` (§13.2) | `202 {"id":"01JC…","group_hint":null,"job_id":"01JC…"}` | 400 validation/overrides/path, 503 busy |
| POST | `api/v2/downloads/batch` | `{"items":[AddRequestV2, …]}` (≤ 200) | `202 {"ids":[…],"job_ids":[…],"rejected":[{"index":3,"error":{…}}]}` | 400 if all rejected |
| GET | `api/v2/state` | `?since=<seq>&limit=` | `200` delta list or full snapshot (§13.3) | 400 bad seq |
| GET | `api/v2/items` | `?status=&group_id=&limit=&cursor=&order=` | `200 {"items":[ItemView],"next_cursor":null,"seq":123}` | — |
| GET | `api/v2/items/{id}` | — | `200 ItemView` | 404 |
| DELETE | `api/v2/items/{id}` | `?delete_files=true` | `204` | 404 |
| POST | `api/v2/items/delete` | `{"ids":[…],"delete_files":false}` | `200 {"deleted":["…"],"missing":["…"]}` | — |
| POST | `api/v2/items/cancel` | `{"ids":[…]}` | `200 {"canceled":[…],"not_cancelable":[{"id":…,"status":"finished"}]}` | — |
| POST | `api/v2/items/start` | `{"ids":[…]}` | `200 {"started":[…],"missing":[…]}` | — |
| GET | `api/v2/groups` | `?limit=&cursor=` | `200 {"groups":[GroupView]}` | — |
| GET | `api/v2/groups/{id}/items` | `?limit=&cursor=` | `200 {"items":[ItemView]}` | 404 |
| POST | `api/v2/groups/{id}/cancel` | — | `200 {"canceled":N}` | 404 |
| POST | `api/v2/jobs/{job_id}/cancel` | — | `200 {"added_before_cancel":N}` | 404 |
| GET | `api/v2/catalog` | `?url=` | `200 MergedCatalog` / provider catalog, `ETag` | — |
| GET | `api/v2/providers` | — | `200 {"providers":[{id,display_name,kind,version,state,capabilities,limits,argv?}]}` | — |
| GET | `api/v2/resolve-preview` | `?url=` | `200 {"provider":"…","score":250,"reason":"host_and_path","runner_up":{…}}` | 400 |
| POST | `api/v2/providers/reload` | — | `200 ReloadReport` | — |
| GET | `api/v2/config` | — | `200` client-safe config (superset of legacy `frontend_safe`, correctly typed) | — |
| GET | `api/v2/presets` | — | `200 {"presets":[{"name":"archive","keys":["download_archive"]}]}` | — |
| GET | `api/v2/custom-dirs` | — | `200 {"download_dir":[…],"audio_download_dir":[…]}` (cached, refreshed off-loop) | 404 when `CUSTOM_DIRS=false` |
| GET/POST/DELETE | `api/v2/cookies` | multipart field `cookies` | `200 {"has_cookies":true,"bytes":1234,"updated_at":…}` | 413, 409 |
| GET | `api/v2/subscriptions` | — | `200 {"subscriptions":[SubView]}` | — |
| POST | `api/v2/subscriptions` | subscribe body | `201 SubView` | 400, 409 duplicate |
| PATCH | `api/v2/subscriptions/{id}` | partial | `200 SubView` | 400, 404 |
| DELETE | `api/v2/subscriptions/{id}` | — | `204` | 404 |
| POST | `api/v2/subscriptions/check` | `{"ids":[…]}` or `{}` | `202 {"job_id":"01JC…","count":3}` | 400 |
| GET | `api/v2/jobs/{job_id}` | — | `200 {"kind":"subscription_check","state":"running","done":1,"total":3}` | 404 |
| GET | `api/v2/version` | — | `200 {"version":"2026.09.04","yt_dlp":"2026.8.30…","url_prefix":"/","protocol":2,"capabilities":{…}}` | — |
| GET | `healthz` | `?verbose=1` | `200`/`503` (§13.6) | — |
| GET | `api/v2/debug/options` | `?item_id=` or the full add body | `200` the merged option dict, with a per-key `source` layer label | — |
| GET | `download/*`, `audio_download/*` | — | file, `Accept-Ranges: bytes`, `ETag`, `Last-Modified` | 404 |

`api/v2/version.capabilities` is what lets the client stop guessing: `{"async_add":true,
"stable_ids":true,"ws_v2":true,"deltas":true,"since_query":true,"retry":true,"groups":true,
"providers":true,"file_urls":true}` — iOS ask #10 and #15 in one response.

### 13.2 `AddRequestV2`

```json
{
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "download_type": "video",
  "format": "mp4",
  "quality": "1080",
  "codec": "auto",
  "folder": "Music/Live",
  "custom_name_prefix": "",
  "playlist_item_limit": 0,
  "auto_start": true,
  "split_by_chapters": false,
  "chapter_template": null,
  "subtitle_language": "en",
  "subtitle_mode": "prefer_manual",
  "ytdl_options_presets": ["sponsorblock"],
  "ytdl_options_overrides": {},
  "provider": null,
  "source": { "kind": "api", "ref": null }
}
```

Every field except `url` is optional and defaults from `api/v2/config`. `provider` forces a
provider (`Match::FORCED`). Unknown fields are **rejected** (`serde(deny_unknown_fields)`) so a
typo'd option is not silently ignored — with one exception: the v1 shim strips legacy keys first.

### 13.3 WebSocket v2 (`<prefix>ws`)

Connect → `hello` → `snapshot` → `catalog` → `config` → then frames. Every frame:
`{"t": <type>, "seq": <u64>, …}`. Server→client only; the client may send `{"t":"ping"}` and
`{"t":"resume","since":<seq>}`.

**`hello`**
```json
{"t":"hello","seq":41207,"protocol":2,"server":"aulos 0.1.0","batch_ms":250,
 "heartbeat_ms":20000,"snapshot_follows":true,"instance_id":"01JBZ…"}
```

**`snapshot`** — same item shape as REST (BRIEF §3):
```json
{"t":"snapshot","seq":41207,"items":[ /* ItemView … */ ],
 "groups":[{"id":"01JC…","title":"Mix - lofi","kind":"playlist","status":"active",
            "total":500,"finished":128,"active":3,"error":1,"canceled":0,
            "percent":25.6,"bytes_done":9836789760,"bytes_total":38400000000,
            "created_at":1772668000000,"seq":41200}],
 "subscriptions":[ /* SubView … */ ]}
```

**`delta`** — batched, changed fields only, `id` always present:
```json
{"t":"delta","seq":41215,"at":1772668812810,
 "items":[{"id":"01JC3Q7ZK8V0Q4E7P2W6R5T9XN","percent":43.2,"speed":11534336.0,"eta":37,
           "downloaded_bytes":25165824},
          {"id":"01JC3Q8A1C2D3E4F5G6H7J8K9L","status":"postprocessing","percent":99.9,
           "msg":"Merging formats","speed":null,"eta":null}],
 "groups":[{"id":"01JC…","finished":129,"percent":25.8}]}
```

**`added`** (prompt, not batched) — full `ItemView` objects, array so a playlist batch is one frame:
```json
{"t":"added","seq":41216,"items":[ /* ItemView */ ]}
```

**`completed`** (prompt) — full objects with terminal state, cleaned error:
```json
{"t":"completed","seq":41220,
 "items":[{"id":"01JC…","status":"error","percent":61.4,
           "error":{"code":"bot_check","message":"Sign in to confirm you're not a bot"},
           "…":"…"}]}
```

**`removed`** (prompt): `{"t":"removed","seq":41221,"ids":["01JC…","01JD…"]}`
**`group`** (prompt for create/terminal, batched for counters): the group object as in `snapshot`.
**`catalog`**: `{"t":"catalog","seq":…,"etag":"9f2b41c0d7e5a318","catalog":{…}}`
**`config`**: `{"t":"config","seq":…,"config":{…}}` (also re-sent when `YTDL_OPTIONS_FILE` reloads)
**`providers`**: `{"t":"providers","seq":…,"providers":[…],"reload":{"added":["command:kinotek"],"failed":[]}}`
**`subscription`**: `{"t":"subscription","seq":…,"op":"upserted"|"removed","subscription":{…}|null,"id":"…"}`
**`ytdl_options`**: `{"t":"ytdl_options","seq":…,"success":true,"msg":"","update_time":1772668000.0}` (legacy parity)
**`job`**: `{"t":"job","seq":…,"job_id":"…","kind":"resolve"|"subscription_check","state":"running","done":2,"total":3}`
**`resync`**: `{"t":"resync","seq":…,"reason":"lagged"}` — the client must re-`GET api/v2/state` or reconnect
**`pong`**: `{"t":"pong","seq":…,"at":…}`

`GET api/v2/state?since=<seq>`:
```json
{"mode":"delta","from":41207,"seq":41221,"frames":[ /* the exact frames replayed from event_log */ ]}
{"mode":"snapshot","seq":41221,"items":[…],"groups":[…],"subscriptions":[…],
 "reason":"since_too_old"}   // when since < event_log floor, or since > our high-water
```

Batching detail: one `tokio::time::interval` per **server** (not per connection) drives the
ProgressBus; each connection has its own `broadcast::Receiver`. A slow client's `broadcast` lag
turns into one `resync`, never into memory growth. Frames are serialised **once**
(`Arc<str>` of the JSON) and shared across all connections.

### 13.4 v1 compatibility shim

A thin translation layer with **no logic of its own** beyond mapping.

| v1 route | v2 call | Response mapping |
|---|---|---|
| `POST add` | `POST api/v2/downloads` after `_migrate_legacy_request` (§legacy 2.3) + `parse_download_options` validation | `200 {"status":"ok"}`; validation failures become `200 {"status":"error","msg":…}` **and** the legacy 400s where legacy returned 400, byte-identical `reason` strings |
| `GET history` | `list_items` | `{"done":[…],"queue":[…],"pending":[…]}` with v1 item shape: `id`=our `entry_key`-derived legacy id, `url`, `title`, `status` via `Status::v1_name()`, `percent`, `speed`, `eta`, `msg`, `filename`, `size`, byte/fragment fields; `queue` = active, `pending` = `queued && !auto_start`, `done` = terminal |
| `POST delete` | `items/delete` or `items/cancel` by `where` | ids accepted as **either** ULIDs or urls (a `url → newest item` lookup) — this is what keeps the shipped iOS build working |
| `POST start` | `items/start` | `{"status":"ok"}` |
| `GET version` | `api/v2/version` | `{"yt-dlp":…,"version":…}` plus new `url_prefix` (additive, harmless) |
| `POST subscribe`, `GET subscriptions`, `POST subscriptions/{update,delete,check}` | the v2 subscription routes | legacy `{"status":"ok","subscription":{13 public keys}}`; `subscriptions/check` returns `{"status":"ok"}` **immediately** (change C-9) |
| `GET presets` | `api/v2/presets` | `{"presets":["a","b"]}` sorted |
| `POST cancel-add` | cancels the newest resolve job of the caller | `{"status":"ok"}` |
| `POST upload-cookies` / `delete-cookies` / `GET cookie-status` | `api/v2/cookies` | legacy bodies verbatim |

Not provided: Socket.IO, `GET /` SPA, `robots.txt` static (a static `User-agent: *\nDisallow: /download/\nDisallow: /audio_download/` is served; `ROBOTS_TXT` file override kept), `custom_dirs` socket event (available as REST).
`Status::v1_name()` maps `canceled → "error"` with `msg = "Canceled"` because v1 clients have no
`canceled` case (iOS `DownloadStatus` has none); v2 clients get the real value.

### 13.5 Static files

`tower-http::ServeDir` over `DOWNLOAD_DIR` and `AUDIO_DOWNLOAD_DIR` with
`ServeDir::new(..).precompressed_gzip(false)`, range support (needed for iOS playback — ask #14),
`show_index = DOWNLOAD_DIRS_INDEXABLE`, and a symlink/`..` guard. `ItemView::download_url` is
`PUBLIC_HOST_URL + percent_encode(filename)` (or `PUBLIC_HOST_AUDIO_URL` for audio types), so the
client never builds paths itself.

### 13.6 `healthz`

```json
{
  "status": "degraded",
  "version": "2026.09.04",
  "uptime_secs": 8123,
  "queue": { "active": 3, "queued": 128, "resolving": 1, "slots": {"global": "3/3", "streamingcommunity": "0/1"} },
  "store": { "ok": true, "wal_bytes": 2097152, "write_queue_depth": 0 },
  "providers": [
    { "id": "ytdlp", "state": "ready", "yt_dlp": "2026.8.30.232658.dev0", "recent_error_rate": 0.05 },
    { "id": "streamingcommunity", "state": "degraded", "detail": "N_m3u8DL-RE not found; using ffmpeg" },
    { "id": "command:kinotek", "state": "ready", "version": "0.3.1" }
  ],
  "pot": { "supervised": true, "state": "running", "pid": 41, "restarts": 2, "last_probe_ms": 4, "healthy": true },
  "telegram": { "enabled": true, "state": "polling" },
  "subscriptions": { "count": 12, "due": 0, "failing": 1 }
}
```

HTTP `200` when every *required* component is up (store + API + at least one ready provider);
`503` when the store is unwritable or `ytdlp` failed preflight. `pot.healthy == false` is
`degraded`, not `503` — YouTube may still work. The Docker `HEALTHCHECK` hits
`<URL_PREFIX>healthz` (legacy hit the SPA index and ignored `URL_PREFIX`).

---

## 14. `aulos-telegram`

`teloxide` with the `throttle` adapter (its token-bucket respects Telegram's per-chat and global
limits, which is exactly the hard part of live progress editing).

| Concern | Design |
|---|---|
| Startup gating | `TELEGRAM_BOT_ENABLED` + token + `TELEGRAM_ALLOWED_CHAT_IDS` (comma i64 list). Missing config ⇒ one `warn!` and the bot task is not spawned; the server still starts (legacy parity) |
| Commands | `/start`, `/config` — same text, same inline keyboards, same callback grammar `cfg:menu:{main,format,quality,limit}`, `cfg:toggle:split`, `cfg:set:{format,quality,limit}:<v>` |
| Keyboard source | the **merged catalog** (§10), not a hard-coded list — a new plugin format appears in the bot with no code change |
| Per-chat config | `telegram_chats` table (was `telegram_bot_config.json`), same JSON keys, same defaults |
| Authorization | `chat_id ∈ allowed` else silent ignore + `warn!` |
| URL extraction | `regex` `https?://[^\s<>()\[\]{}"']+`, trailing `.,;:!?)]}>'"` trimmed, order-preserving dedupe, cap `TELEGRAM_MAX_URLS_PER_MESSAGE` with the same over-cap message |
| SSRF guard | scheme ∈ {http,https}; host required; reject `localhost`, `*.local`; if the host parses as an IP, reject loopback/private/link-local/multicast/reserved/unspecified. **Change C-10:** also resolve the host and reject when *any* resolved address is private (legacy only checked literal IPs), behind `AULOS_TELEGRAM_RESOLVE_GUARD=true` |
| Attribution | `source: {kind:"telegram", ref:"<chat_id>"}` on the `AddRequest` (BRIEF §11) — no contextvars, so playlist children, subscription adds and web adds are all attributed correctly |
| Live progress | one message per (chat, job) where a job = one item or one group. Edited at most every `AULOS_TELEGRAM_PROGRESS_EDIT_MS` (3 s) **and** only when the rendered text changed. Rate-limit errors (`RetryAfter`) push the next edit out by the requested delay and double the local floor for that chat until success |
| Message body | `⬇️ <title>\n▓▓▓▓▓░░░░░ 51 % · 11.0 MB/s · ETA 0:37\n<provider> · <format>/<quality>` ; group jobs show `128/500 done · 1 failed` |
| Terminal | edit the same message to `✅ <title>\n<size> · <duration>\n<download_url>` or `❌ <title>\n<error.message>` (already cleaned — iOS ask #12) |
| Stall / hard timeout | from the item's own `updated_at` (the queue already tracks it), not a 15 s poll: the notifier receives `StateEvent`s and arms `tokio::time::sleep_until` timers. `⚠️ stalled for Ns` once, `⏱️ taking longer than expected (Ns)` once. Neither cancels (legacy parity), but the message now carries an inline **Cancel** button that does |
| Notifier | `impl aulos_core::Notifier for TelegramNotifier` — the only integration point. APNs later = a second impl (BRIEF out-of-scope hook) |

```rust
// aulos-core::notifier
#[async_trait]
pub trait Notifier: Send + Sync {
    fn interest(&self) -> Interest;      // which sources/events this notifier wants
    async fn on_event(&self, ev: &StateEvent);
    async fn on_terminal(&self, item: &ItemView);
}
```

---

## 15. `aulos-subscriptions`

Same data model and the same 13-key public projection as legacy
(`id,name,url,enabled,check_interval_minutes,download_type,codec,format,quality,folder,last_checked,seen_count,error`),
plus additive `next_check_at`, `consecutive_failures`, `provider`.

| Concern | Design |
|---|---|
| Scheduler | one `tokio::time::sleep_until(next_check_at)` per subscription (a `JoinSet` + a `BinaryHeap` timer wheel, rebuilt from `subscriptions_due`), **not** a 60 s global tick |
| First check | `now + uniform(5s, 45s)` at boot per subscription (legacy: `+60 s` for all, then all at once) |
| Jitter | `next = last + interval ± min(interval*0.1, 5 min)` — prevents a thundering herd of 12 channel checks |
| Backoff | `consecutive_failures` → `interval * 2^min(f,5)` capped at 6 h, and `last_checked` **is** updated on failure (fixes the legacy 60 s hot-retry, pain point #17) |
| Concurrency | `Semaphore(AULOS_SUB_CHECK_CONCURRENCY=3)`; a slow feed no longer blocks the others |
| `POST check` | returns `202 {"job_id":…}` immediately; progress via `GET api/v2/jobs/{id}` and WS `job` frames (change C-9) |
| Extraction | `Registry::select(url)` then `provider.resolve()` with `ResolveOpts { flat: true, playlist_end: SUBSCRIPTION_SCAN_PLAYLIST_END, purpose: Subscription }`. Subscriptions therefore work for StreamingCommunity seasons and plugins too — legacy was yt-dlp-only (change C-11) |
| Media-entry filter | port of `_is_media_entry`: not playlist/channel/multi_video, no nested entries, has a url, and when the extractor key contains `playlist|channel|tab` requires one of `duration,timestamp,release_timestamp,upload_date,view_count,live_status,availability` |
| Channel-of-tabs | port of the `_depth < 1` recursion into the first ≤ 5 child urls |
| New-item detection | `entry_key ∉ subscription_seen` **plus** any seen entry with `live: Live` (re-queue a started stream). Seen set is a table, so the check is an index lookup, not an O(n) list scan + whole-file rewrite |
| Backfill suppression | on subscribe, insert every visible `entry_key` into `subscription_seen` without queueing, **except** `live: Upcoming` (legacy parity) |
| Seen cap | `SUBSCRIPTION_MAX_SEEN_IDS` enforced by `DELETE … WHERE seen_at < (SELECT seen_at … LIMIT 1 OFFSET ?)` after each check |
| Add-single-video guard | `_type == video` ⇒ `409 conflict` with the exact legacy message `"This URL points to a single video, not a channel or playlist. Use Download instead."` |
| Errors | `error` cleared only by a fully successful check (legacy parity); per-item queue errors joined `"; "` (first 3) |
| Validation | `PATCH` accepts `enabled`, `check_interval_minutes`, `name` (legacy) **plus** every download-selection field, and returns `400` (never 500) on bad types (fixes pain point #25) |

---

## 16. `aulos-hooks`

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn id(&self) -> &'static str;
    fn enabled(&self) -> bool;
    fn wants(&self, item: &ItemView) -> bool;
    async fn run(&self, ctx: &HookCtx<'_>) -> Result<HookOutcome, HookError>;
    fn timeout(&self) -> Duration;
}
```

### 16.1 Jellyfin

`POST {JELLYFIN_URL}/Library/Refresh` with `Authorization: MediaBrowser Token="<key>"`,
`Accept: application/json`, no body, `JELLYFIN_SYNC_TIMEOUT_SECONDS`.
**Change C-5 (BRIEF §13):** when `JELLYFIN_LIBRARY_ID` is set, prefer
`POST /Items/{id}/Refresh?Recursive=true&metadataRefreshMode=<JELLYFIN_METADATA_REFRESH_MODE|Default>&imageRefreshMode=<JELLYFIN_IMAGE_REFRESH_MODE|Default>`
and fall back to the global refresh on 404. **Debounce:** a `tokio` timer coalesces all completions
within `AULOS_JELLYFIN_DEBOUNCE_SECONDS` (30 s) into one request, with a trailing edge so the last
completion in a burst is always covered. Failures are logged, retried once after 5 s, then dropped.

### 16.2 NFO

Runs when `AULOS_NFO_ENABLED` **and** the provider declares `nfo_capable` **and** the item has
`entry_meta`. Writes `<media basename>.nfo` next to the media using `quick-xml`:
`episodedetails` when `series|season|episode` are present, else `movie`; elements `title`,
`originaltitle`, `showtitle`, `season`, `episode`, `plot`, `year`, `premiered`, `dateadded`,
`studio`, `director`, `uniqueid type="<provider>"`, `website`, ≤ 20 `tag`, `runtime` (whole
minutes). **Change C-13:** it reads `Outcome::provider_meta`/`entry_meta` in memory instead of a
sidecar `.info.json`, and therefore does not delete anything. The artifact is recorded so
`DELETE ?delete_files=true` removes it too (fixes legacy orphan-file bug, pain point #20).

### 16.3 Audio-sync re-encode (port of `audio_sync_fix.py`)

Runs when `download_type=video && format=mp4 && quality=best_remux` — the same trigger legacy wired
as a late `Exec` postprocessor. Now in-process Rust spawning ffmpeg, so no `/app/app/...` hard-coded
path and no dependency on `Exec` being allowed:

```
ffprobe -v error -select_streams v -show_entries stream=codec_type -of csv=p=0 <file>   # skip if no video
ffprobe -v error -show_entries format=duration -of csv=p=0 <file>                        # 30 s timeout
timeout = max(600s, ceil(duration/2))  ; 1800 s if duration unknown
ffmpeg -y -loglevel warning -i <file> -map 0 -dn -ignore_unknown -c copy -c:a aac -b:a 256k \
       -movflags +faststart <tmp in same dir>     ; then rename(tmp, file)
```

Non-zero exit / timeout ⇒ the temp file is removed and a `msg` note is attached; the item stays
`finished` (legacy made the `Exec` postprocessor fail, which surfaced as a download error even
though a perfectly good file existed — change C-6).

---

## 17. `aulos-server` (binary)

```
main()
 ├─ init tracing (EnvFilter from LOGLEVEL; JSON when AULOS_LOG_FORMAT=json; third-party
 │  targets hyper/h2/rustls/teloxide/notify dampened to WARN — legacy dampenThirdPartyLoggers)
 ├─ Config::from_env(std::env::vars())?            → exit 1 with the aggregated report
 ├─ Store::open(AULOS_DB_PATH) + migrations + legacy import (if needed)
 ├─ Registry::builder()
 │     .with(ScProvider::new(&cfg)?)               // order 0
 │     .with_plugins_from(&cfg.plugins_dir)        // order 1..N, dir-sorted
 │     .with_fallback(YtDlpProvider::new(&cfg)?)   // order last
 │     .preflight_all().await                      // degraded, never fatal (except ytdlp)
 ├─ Queue::spawn(store.clone(), registry.clone(), cfg.clone())
 ├─ Subscriptions::spawn(store, queue, registry)
 ├─ Hooks::spawn(cfg)                              → registered with the queue
 ├─ Telegram::spawn(cfg, queue, store)             → registered as a Notifier
 ├─ Supervisor::spawn("bgutil-pot", ["server"])    // §17.1
 ├─ Reloader::spawn(notify watcher)                // §17.2
 ├─ axum::serve(listener, router).with_graceful_shutdown(shutdown_signal())
 └─ shutdown: stop accepting → cancel WS → queue.drain(grace=20s) → store.flush() → reap children
```

### 17.1 bgutil-pot supervisor

Generic `Supervisor { program, args, env, restart: Backoff { 1s → 2 → 4 … 60s, reset after 60 s up }, health: Option<HealthProbe> }`.
stdout/stderr are line-forwarded into `tracing` with `target = "bgutil_pot"` (legacy dumped them to
`/tmp/bgutil-pot.log` unsupervised — pain point #28). State (`running|restarting|failed`, pid,
restart count, last probe latency) feeds `healthz`. `AULOS_POT_SUPERVISE=false` lets an operator run
it externally. On shutdown: SIGTERM to its process group, 5 s grace, SIGKILL.

### 17.2 Hot reload

One `notify` (v8) recommended watcher, debounced 250 ms, watching:

| Path | Action |
|---|---|
| `YTDL_OPTIONS_FILE` | re-read + validate; on success swap the `Arc<JsonMap>` layer and broadcast `ytdl_options{success:true}`; on failure **keep the previous good value**, log, broadcast `{success:false,msg}`. Legacy semantics preserved: the file must be the same inode (`same_file`) or path, and create/modify/remove all count |
| `YTDL_OPTIONS_PRESETS_FILE` | same (change C-8 — legacy did not watch it) |
| `AULOS_PLUGINS_DIR/**` | `Registry::reload_plugins`, broadcast `providers` |
| `STATE_DIR/cookies.txt` | update the `cookiefile` runtime override |

Reload never affects a running job: layers are `Arc`-swapped and each job captured its merged option
dict at spawn time.

### 17.3 Packaging notes

Multi-stage: `rust:1.95-slim` + `cargo-chef` (recipe layer) → `debian:bookworm-slim` runtime with
`python3`, `pip install --break-system-packages --no-deps yt-dlp==<nightly pin>`, the BgUtils yt-dlp
plugin zip unpacked into site-packages, `deno`, `ffmpeg`, `N_m3u8DL-RE`, `bgutil-pot`, `tini`,
`gosu`, `ca-certificates`, `curl`. `cmake`+`clang` only in the builder (BoringSSL for `wreq`).
`crates/aulos-provider-ytdlp/python/ytdlp_runner.py` is copied to `/app/python/`.
Entrypoint preserves `PUID/PGID/UID/GID/UMASK/CHOWN_DIRS` semantics exactly; `bgutil-pot` is no
longer started there (the server supervises it). `HEALTHCHECK` → `curl -fsS localhost:$PORT$URL_PREFIX`healthz.
CI: `fmt --check`, `clippy --all-targets -- -D warnings`, `test`, `cargo deny`, buildx amd64+arm64
to GHCR, and the ported `update-yt-dlp.yml` (now also bumping a `YTDLP_VERSION` build arg and
running the shim selftest in CI so a nightly that breaks the protocol fails the PR).

---

## 18. Sequences

### 18.1 Add a single video

```
iOS  POST api/v2/downloads {url, format:mp4, quality:1080}
api  validate against catalog(ytdlp)  → resolve out_dir → Store.insert_items(1 row, resolving)
api  202 {"id":"01JC…","job_id":"01JD…"}                                    (~4 ms, no network)
ws   added [ItemView{status:"resolving", percent:0, title:"<url>"}]         (prompt frame)
sched Registry.select → ytdlp(score 1) → acquire resolve permit
ytdlp spawn shim(mode=extract, noplaylist) → hello → resolved{type:video} → entry → result
sched patch item {title, entry_key, provider_state, status:queued}
ws   delta [{id, status:"queued", title:"Never Gonna Give You Up"}]         (next 250 ms tick)
sched acquire ytdlp permit + global permit → spawn download task
job  Status(Preparing) → ws delta [{id, status:"preparing"}]
ytdlp spawn shim(mode=download) → progress frames ~10/s → sink.metrics (lossy)
bus  every 250 ms: delta [{id, percent, speed, eta, downloaded_bytes}]
ytdlp pp{Merger,started} → Status(Postprocessing) → delta [{status:"postprocessing"}]
ytdlp pp{MoveFiles,finished} → artifact(media) → result{ok}
job  patch {status:finished, filename, size, finished_at} + artifacts
ws   completed [ItemView{status:"finished", percent:100, download_url:"download/Rick.mp4"}]
hooks jellyfin debounce armed (30 s); nfo skipped (ytdlp nfo_capable but AULOS_NFO_ENABLED only
      writes when entry_meta has series/plot — configurable)
```

Wall-clock to first client feedback: **one round trip**, versus multi-second yt-dlp extraction in
legacy. This is iOS ask #1 and deletes the entire background-upload machinery.

### 18.2 Add a 500-item playlist

```
t=0      POST api/v2/downloads {url: …playlist?list=PL…}   → 202 {"id": anchor, "job_id": J}
t=4ms    ws added [anchor{status:"resolving"}]
t=40ms   resolve permit acquired; shim spawned
t=310ms  shim `resolved{type:playlist,count:500}` → Store.upsert_group
         ws  group  {id:G, title:"Mix - lofi", total:0, status:"resolving"}   (prompt)
             removed [anchor]                                                 (same batch)
t=340ms  entries arrive from the lazy playlist; buffered, flushed every 64 entries / 200 ms
         Store.insert_items(64) in ONE transaction   → ws added [64 ItemViews]  (prompt, one frame)
         ws group {id:G, total:64}
t~4.2s   result{count:500} → group{total:500,status:"active"}; 8 insert transactions total
t=4.3s   scheduler starts 3 children (MAX_CONCURRENT_DOWNLOADS); 497 stay `queued`
steady   delta frames carry only the 3 active children + one group counter object
         → ~600 bytes / 250 ms, independent of playlist size
```

Legacy equivalent: 500 sequential blocking extractions in the event-loop thread pool, 500 whole-file
`queue.json` rewrites with 1000 fsyncs, 500 unthrottled full-object Socket.IO broadcasts, and an
`/add` request that does not return until all of it finishes.

If `playlist_item_limit = 50`: the queue passes `playlist_end: 50` to the provider **and** stops
accepting entries at 50 (belt and braces, as legacy did with slice + `playlistend`), and the group
carries `truncated: true` + `note: "Limited to the first 50 items"`.

### 18.3 Cancel mid-download

```
iOS  POST api/v2/items/cancel {"ids":["01JC…"]}
api  Cmd::Cancel → 200 {"canceled":["01JC…"],"not_cancelable":[]}         (~1 ms)
sched token.cancel()
job  select! wakes on cancel → provider.download's own select! returns
ytdlp Child::shutdown(Canceled): kill(-pgid, SIGTERM)
shim  SIGTERM handler → KeyboardInterrupt inside yt-dlp → .part flushed
      → error{code:"canceled"} → bye → exit 130
      (if still alive after AULOS_KILL_GRACE_MS=5s: kill(-pgid, SIGKILL) — reaches ffmpeg/deno too)
job  Err(Canceled) → patch {status:"canceled", msg:"Canceled", finished_at}
     tmp files created by this job under tmp_dir removed
ws   completed [ItemView{status:"canceled", percent:43.2}]                 (prompt)
sched permits released; next queued item starts
```

The item is **kept** as `canceled` (legacy dropped it entirely, emitting a `canceled` event with a
bare url and leaving the client to guess). A canceled item can be retried with
`POST items/start`.

### 18.4 Server restart with in-flight downloads

```
SIGTERM
 ├─ listener stops accepting; WS connections get {"t":"resync","reason":"shutdown"} and close(1001)
 ├─ queue.drain(grace = AULOS_SHUTDOWN_GRACE=20s):
 │    every running job's token is canceled → providers SIGTERM their groups → Err(Canceled)
 │    each item is patched to status:"queued", msg:"Interrupted by server shutdown"
 │    (NOT canceled — the user did not ask; attempts is not incremented)
 ├─ ProgressBus flushes; event_log appended; seq_high_water persisted
 └─ store writer commits, WAL checkpointed (PRAGMA wal_checkpoint(TRUNCATE)), children reaped

boot
 ├─ Store::open; migrations; seq restored from schema_meta/event_log
 ├─ claim_inflight(): any row still in resolving/preparing/downloading/postprocessing (e.g. after
 │  SIGKILL or a power cut) → status:"queued", msg:"Requeued after server restart"
 ├─ orphan sweep: files in tmp_dir older than 24 h with no owning item → deleted (logged)
 ├─ providers preflight; POT supervisor starts; API binds
 └─ first client connects → snapshot shows 128 queued, 0 active, then the scheduler fills 3 slots
```

Note the deliberate asymmetry: a *graceful* shutdown re-queues; an *unclean* one also re-queues via
`claim_inflight`. Either way the client sees a coherent snapshot and no ghost "downloading" rows —
legacy left `queue.json` rows at whatever status they had and then auto-restarted them all at once.

`.part`/`.tmp` reuse: `ytdlp` resumes automatically (`continuedl` default), so a large interrupted
download resumes rather than restarting — a user-visible win over legacy's SIGKILL + orphaned parts.

### 18.5 Subscription tick

```
timer  next_check_at reached for sub S (jittered)
sub    acquire check permit (3) → Registry.select(S.url) → provider.resolve(flat, playlist_end=50)
       ws job {job_id, kind:"subscription_check", state:"running", done:0, total:1}
       entries → filter _is_media_entry → new = entry_key ∉ subscription_seen  ∪  {seen && live}
case A 3 new entries:
       queue.add_batch(3 AddRequests with source {kind:"subscription", ref:S.id})
       Store: insert 3 items + 3 subscription_seen rows + sub patch (last_checked, next_check_at,
              consecutive_failures=0, error=null)   — ONE transaction
       ws added [3 ItemViews]; ws subscription {op:"upserted", …seen_count+3}
case B extraction failed (403):
       consecutive_failures += 1 → next_check_at = now + interval*2^f (cap 6 h) ± jitter
       error = "HTTP Error 403: Forbidden"; last_checked = now      (fixes the legacy hot-retry)
       ws subscription {op:"upserted", error:…, next_check_at:…}
case C url now resolves to a single video:
       error = "This URL points to a single video, not a channel or playlist. Use Download instead."
       backoff as in B
```

`POST api/v2/subscriptions/check {}` enqueues all enabled subs onto the same permit-limited path and
returns `202 {"job_id":…,"count":12}` immediately; the WS `job` frames report `done/total`.

### 18.6 Telegram message with 3 URLs

```
msg    "look at these https://youtu.be/a https://youtu.be/b https://kinotek.example/w/914"
bot    chat_id ∈ allowed? yes
       extract 3 urls (order-preserving dedupe, ≤ TELEGRAM_MAX_URLS_PER_MESSAGE)
       SSRF-validate each; suppose all pass
       chat config → Selection {video, mp4, best, …}
       POST-equivalent: queue.add_batch([3 AddRequests], source {kind:"telegram", ref:"<chat>"})
bot    reply "Queued 3 link(s) with current chat config." (one message)
       create ONE live message per item (3 messages) or ONE aggregate message when >2 items
       (config `AULOS_TELEGRAM_AGGREGATE_FROM=3`): "⬇️ 3 downloads · 0 done"
sched  a,b → ytdlp; kinotek → command:kinotek (score 150) → different per-provider semaphores,
       so the plugin download does not consume a ytdlp slot if its manifest says so
notify TelegramNotifier receives StateEvents; edits the live message at most every 3 s and only on
       text change; on RetryAfter(7) it sleeps 7 s and doubles that chat's floor until success
       terminal: "✅ 3 downloads · 3 done" with per-item lines and download_urls; failures show the
       cleaned error message and an inline Retry button (→ POST items/start)
```

Invalid urls produce the legacy-shaped `Ignored invalid links:\n- <url> (<reason>)` message.
Playlist urls in a Telegram message produce a group; the live message tracks group counters.

### 18.7 `YTDL_OPTIONS_FILE` edit

```
t=0    operator writes /config/ytdl.json (editor writes a temp file and renames → notify sees
       Create+Remove on the target path, both accepted, same as legacy's watchfiles filter)
t=250ms debounce fires → read file → serde_json parse
 ok:   validate it is a JSON object; Arc-swap the `file` option layer
       recompute the option-layer fingerprint; broadcast
       ws ytdl_options {success:true, msg:"", update_time:1772668000.0}
       ws config {…}                      (in case a client-visible default changed)
       log info "YTDL_OPTIONS_FILE reloaded (12 keys, 3 changed: format, cookiefile, postprocessors)"
 bad:  keep the previous layer; log error;
       ws ytdl_options {success:false, msg:"YTDL_OPTIONS_FILE contents is invalid", update_time:…}
running jobs: unaffected (each captured its merged dict at spawn)
queued jobs: pick up the new layer when they start — same as legacy
GET api/v2/debug/options?url=…  now shows, per key, which layer won
```

### 18.8 A new site arrives (the extensibility acceptance test)

```
user   drops /config/plugins/kinotek/{plugin.toml,resolve.py,download.py}
notify debounced 1 s → Registry.reload_plugins
       manifest parsed + validated; argv[0] resolved; regexes compiled; catalog merged
       ws providers {added:["command:kinotek"], failed:[]}
       ws catalog  {etag:"…"}                      → iOS pickers update with no app release
user   shares https://kinotek.example/series/914 from Safari
api    POST api/v2/downloads → Registry.select → command:kinotek (score 250: host + path)
       GET api/v2/catalog?url=… would have shown MKV/MP4 and its qualities
resolve resolve.py prints group + 8 entry lines → group + 8 children
download download.py per child, progress scraped by the manifest regexes
```

No Rust was written, no image was rebuilt, no restart happened.

---

## 19. Legacy behaviour map

### 19.1 Spec §1–§12 → where it lives

| Legacy spec area | Where it lives here |
|---|---|
| §1 Config, `_DEFAULTS`, `%%` indirection, `_BOOLEAN`, `URL_PREFIX`/host-url normalisation | `aulos-core::config` (`Config::from_env`, one-shot typed parse, aggregated errors) |
| §1.2 `TELEGRAM_BOT_TOKEN`, `TELEGRAM_ALLOWED_CHAT_IDS`, `METUBE_VERSION` | `Config` (all env reading is centralised; nothing reads `std::env` elsewhere) |
| §1.3 `YTDL_OPTIONS`/presets load, file-over-env, runtime overrides, hot reload | `aulos-provider-ytdlp::options` (layers) + `aulos-server::reload` (`notify`) |
| §1.4 `frontend_safe()` | `GET api/v2/config` (typed: ints are ints — change C-1) + WS `config` frame |
| §1.5 Logging, third-party dampening, DEBUG→yt-dlp verbose | `aulos-server::telemetry`; `debug` flag in the shim job |
| §2 REST routes | `aulos-api::v2` (new) + `aulos-api::v1` (byte-compatible shim, §13.4) |
| §2 `text/plain` JSON bodies | **changed** to `application/json` (C-2) |
| §2.2 `/add` validation matrix | `aulos-core::request` + catalog-driven validation (§10.5); v1 shim keeps the exact legacy strings |
| §2.3 `_migrate_legacy_request` | `aulos-core::request::migrate_v1` (same table, unit-tested) |
| §3 Socket.IO events | replaced by WS v2 (§13.3). Mapping table in §19.2 |
| §4 `DownloadInfo` fields | `aulos-core::item::ItemView` (superset; always-present keys) |
| §4.1 Status vocabulary/transitions | `aulos-core::status::Status` (8 values incl. `postprocessing`/`canceled`) + transition matrix |
| §5.1 three `PersistentQueue`s | `items` table + `status` column; `queue/pending/done` are *views*, not files |
| §5.2 `AtomicJsonStore`, schema_version 2, quarantine, `_PERSISTED_DOWNLOAD_FIELDS`, entry compaction | `aulos-store::importer` (read-only, one-shot) — the running system never writes JSON |
| §5.3 semaphores, per-download process, manager queue, thread-pool blocking | `aulos-queue::slots` + `aulos-provider::proc` (one child, JSON-lines pipe, zero blocked threads) |
| §5.3 `_calculate_progress_percent` | `aulos-core::progress::ProgressNormalizer` (all four rules + monotonic clamp) |
| §5.3 `_post_download_cleanup` | per-download task tail (§12.4), moved **outside** the concurrency slot |
| §5.4 add flow, playlist expansion, `_add_generation`, `_canceled_urls` | `aulos-queue` add path (§12.2) + `job_id`-scoped resolve cancellation |
| §5.4 playlist field injection (`playlist_index` zero-padding, `n_entries`, `__last_playlist_index`) | `aulos-provider-ytdlp` builds these into `ProviderState.outtmpl_fields`; the shim receives them pre-resolved in `outtmpl` |
| §5.5 `__calc_download_path`, `OUTPUT_TEMPLATE*` selection, `_resolve_outtmpl_fields`, `_sanitize_path_component` | `aulos-core::paths` + `aulos-core::outtmpl`; containment uses `Path::starts_with` on canonicalised components (fixes the `startswith` prefix bug, C-3) |
| §5.6 `clear`/`cancel`/`start_pending` | `items/delete`, `items/cancel`, `items/start` |
| §5.7 `get_custom_dirs` 5 s memo + recursive glob | `aulos-api::v2::custom_dirs`: a background refresher task (interval 30 s, `spawn_blocking` walk with `walkdir`, depth cap 8) serving a cached snapshot — never on the request path (C-16) |
| §5.8 Jellyfin post-completion hook | `aulos-hooks::jellyfin` (+ debounce, + targeted refresh) |
| §5.9 ffmpeg timeout scaling | `aulos-hooks::audio_sync` (`max(600, ceil(dur/2))`, 1800 unknown) |
| §6 `dl_formats.get_format`/`get_opts` | `aulos-provider-ytdlp::options` (port; golden-file tested) |
| §7 Subscriptions (model, persistence, scheduling, extraction, backfill) | `aulos-subscriptions` + `subscriptions`/`subscription_seen` tables |
| §7.1 `to_public_dict()` 13 keys | `aulos-subscriptions::projection` (same keys + additive fields) |
| §8 Telegram bot (handlers, callback grammar, per-chat config, URL regex, SSRF guard, monitor loop) | `aulos-telegram` |
| §9 StreamingCommunity (detection, Inertia, embed, `window.streams`, JIT m3u8, N_m3u8DL-RE, ffmpeg, gapless mux, ANSI progress) | `aulos-provider-sc` |
| §10 Jellyfin sync / NFO generator / audio_sync_fix | `aulos-hooks::{jellyfin,nfo,audio_sync}` |
| §11 BgUtils POT (sidecar + yt-dlp plugin + deno) | `aulos-server::supervisor` (+ Dockerfile keeps the plugin in site-packages; the shim reports it in `hello`) |
| §12 Dockerfile, entrypoint PUID/PGID/UMASK/CHOWN_DIRS, CI workflows | `docker/` + `.github/workflows/` (§17.3) |
| §13 pain points 1–29 | addressed; see §19.3 and the risk register |

### 19.2 Socket.IO → WS v2 event mapping

| Legacy event | v2 |
|---|---|
| `all` (`[[key,info]…], [[key,info]…]`, double-encoded string) | `snapshot` frame, flat `ItemView` objects, same shape as REST (iOS ask #4) |
| `added` | `added` frame (array; full objects) |
| `updated` (per progress hook, full object, broadcast) | `delta` frame, batched at `AULOS_WS_BATCH_MS`, changed fields only (iOS ask #5) |
| `completed` | `completed` frame (prompt) |
| `canceled` (bare url string) | `completed` frame with `status:"canceled"` (the item survives) |
| `cleared` (bare url string) | `removed` frame with ULIDs |
| `configuration` | `config` frame + `GET api/v2/config` |
| `custom_dirs` | `GET api/v2/custom-dirs` (not pushed — it changes rarely and cost a directory walk per connect) |
| `ytdl_options_changed` | `ytdl_options` frame (same payload keys) |
| `subscriptions_all` / `subscription_added` / `subscription_updated` / `subscription_removed` | `snapshot.subscriptions` + `subscription` frame with `op` |
| `formats` (implicit; iOS consumed it) | `catalog` frame + `GET api/v2/catalog` (per provider — §10) |

### 19.3 Intentional changes

| # | Change | Why it is better for the user |
|---|---|---|
| C-1 | `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` are emitted as **integers**, not strings | clients stop writing 3-branch flexible decoders (iOS ask #7); the v1 shim keeps strings for the old shape |
| C-2 | `Content-Type: application/json` on every JSON response; 4xx for client errors; `401`, never a redirect | the share-extension classifier's "2xx with HTML means expired session" heuristic disappears (iOS ask #2) |
| C-3 | Path containment via canonicalised component comparison, not `startswith` | `/downloads-evil` no longer passes as inside `/downloads` |
| C-4 | Restart re-queues in-flight items through the normal scheduler instead of auto-restarting everything at once | a restart with 40 in-flight items no longer spawns 40 processes; the user sees an honest queue |
| C-5 | Jellyfin: targeted `Items/{JELLYFIN_LIBRARY_ID}/Refresh` when configured, plus a 30 s debounce | a 500-item playlist triggers ~1 refresh instead of 500 full-library scans; the documented-but-inert env vars finally work |
| C-6 | The audio-sync re-encode failing no longer fails the download | a good file stops being reported as an error |
| C-7 | Extraction options are pinned-last for **both** the add path and subscriptions | `YTDL_OPTIONS` can no longer silently break subscription scanning (legacy asymmetry) |
| C-8 | `YTDL_OPTIONS_PRESETS_FILE` is watched | matches what the README always claimed |
| C-9 | `POST subscriptions/check` returns immediately with a job handle | the request no longer hangs for minutes; one slow feed no longer blocks the others |
| C-10 | SSRF guard optionally resolves hostnames before accepting | `http://internal.corp/` shaped attacks via a shared Telegram group are covered |
| C-11 | Subscriptions run through the provider registry | you can subscribe to a StreamingCommunity series or a plugin feed, not only yt-dlp sources |
| C-12 | SC season resolution does 2 requests instead of ~60 | adding a 20-episode season is seconds, not a minute, and is far less likely to be rate-limited |
| C-13 | NFO is written from in-memory metadata; no `.info.json` side effect, nothing deleted | no mystery sidecar files; NFO actually works (legacy never wired it) |
| C-14 | N_m3u8DL-RE segment counts go to `fragment_*`, percent to `percent`; byte fields stay null until real | the byte counters stop lying; the progress bar is monotonic and smooth |
| C-15 | `quality:"worst"` is marked advisory in the catalog with a notice | legacy silently returned the *best* stream for `worst`; now the UI says so (a real fix would change behaviour users may depend on — deferred, see §23) |
| C-16 | Custom-dirs listing is a background-refreshed cache | a multi-TB library no longer stalls the event loop on every client connect |
| C-17 | `canceled` is a real terminal status and the item is kept | "did my cancel work?" is answerable; retry is possible |
| C-18 | Dedupe checks **all** non-terminal items (not just the `queue` collection) by `(provider, entry_key)`, and re-adding a finished item is allowed but returns the existing id when it is still active | fixes the legacy double-pending bug (pain point #18) without blocking legitimate re-downloads |
| C-19 | `tmpfilename` is only updated when the frame carries it | partial-file cleanup actually finds the partial file (pain point #23) |
| C-20 | Every optional field is serialised as `null` rather than omitted | no more "the TypeScript/Swift type says non-optional but the key is missing" |
| C-21 | Healthcheck hits `healthz` (honouring `URL_PREFIX`) and covers store + providers + POT | a dead POT sidecar or an unwritable DB is now visible to Docker/monitoring (pain point #28) |
| C-22 | `subtitle_files`/`chapter_files` are persisted as `artifacts` and deleted with the item | they survive restarts and `DELETE_FILE_ON_TRASHCAN` no longer orphans files (pain point #20) |

### 19.4 Preserved-on-purpose quirks

`SC_MAX_CONCURRENT_DOWNLOADS` acquired outside the global slot; SC ignores `OUTPUT_TEMPLATE`;
`format.startswith("custom:")` escape hatch in `get_format`; `null` in a preset clears a key;
preset application order = request order; `best_remux` pops a user `format`;
`writethumbnail` is only added when the user has not set it; caption `txt` maps to `srt` +
post-conversion; subscription backfill suppression except `is_upcoming`; re-queue of already-seen
`is_live` entries; the exact `get_format` selector strings (byte-for-byte).

---

## 20. Dependencies

| Crate | Version | Why |
|---|---|---|
| `tokio` | `1` (`rt-multi-thread`, `macros`, `process`, `signal`, `fs`, `time`, `sync`, `io-util`) | mandated runtime; `process` gives async child stdio, `signal` the shutdown path |
| `axum` | `0.8` | mandated; native `ws` extractor, typed extractors, `tower` integration |
| `tower` / `tower-http` | `0.5` / `0.6` | `ServeDir` with ranges, CORS, request-id, trace, timeout, body-limit layers — all already written |
| `hyper` / `hyper-util` | `1` | transitive; pinned so TLS/HTTP behaviour is explicit |
| `serde` / `serde_json` | `1` / `1` | everything on the wire and in the option pipeline; `serde_json::Map` preserves insertion order with `preserve_order` off (we sort deliberately) |
| `serde_with` | `3` | `DisplayFromStr` for the flexible v1 numeric fields in the shim, `skip_serializing_none` avoidance |
| `rusqlite` | `0.3x` with `bundled`, `serde_json`, `functions` | mandated store; `bundled` removes the libsqlite3 build dependency and pins the engine version |
| `r2d2` + `r2d2_sqlite` | `0.8` / `0.2x` | tiny, boring read-only connection pool; the write path needs no pool |
| `rusqlite_migration` | `1` | ordered, tested schema migrations without a macro framework |
| `ulid` | `1` | mandated id scheme; time-sortable, 26-char Crockford base32 |
| `tokio-util` | `0.7` | `CancellationToken` (mandated by BRIEF §10), `LinesCodec`, `FramedRead` for the shim pipe |
| `futures-util` | `0.3` | `StreamExt`/`select` combinators in the provider crates |
| `async-trait` | `0.1` | `dyn Provider` / `dyn Hook` / `dyn Notifier` object safety |
| `thiserror` | `2` | mandated: library errors |
| `anyhow` | `1` | mandated: binary errors |
| `tracing` / `tracing-subscriber` | `0.1` / `0.3` (`env-filter`, `json`) | mandated structured logging, per-item spans, request ids |
| `url` | `2` | one normalised URL type across matching, SSRF guard and template rendering |
| `regex` | `1` | plugin match/progress patterns, ANSI-stripped SC parsing, Telegram URL extraction |
| `toml` | `0.9` | `plugin.toml` parsing (serde-native, good error spans for author feedback) |
| `notify` | `8` | mandated hot reload of `YTDL_OPTIONS_FILE`, presets and the plugins dir |
| `notify-debouncer-full` | `0.5` | editors write-and-rename; raw `notify` events need debouncing to be usable |
| `teloxide` | `0.13`+ (`macros`, `throttle`) | mandated bot framework; the `throttle` adapter is what makes 3 s live edits safe |
| `reqwest` | `0.12` (`json`, `rustls-tls`, `stream`) | Jellyfin, POT probe, SC `plain` mode; no OpenSSL dependency |
| `wreq` | `6`/latest (feature `sc-impersonate`) | Chrome TLS/HTTP2 impersonation for StreamingCommunity (successor to `rquest`); the only realistic way to keep the Inertia endpoints reachable |
| `scraper` | `0.2x` | html5ever-backed `#app[data-page]` and `<iframe src>` extraction — same semantics as BeautifulSoup |
| `quick-xml` | `0.37` | NFO writing with correct escaping |
| `strip-ansi-escapes` | `0.2` | N_m3u8DL-RE Spectre.Console frames |
| `shell-words` | `1` | quoting when rendering `{headers_curl}` and when logging plugin argv |
| `nix` | `0.29`+ (`signal`, `process`, `resource`) | `killpg`, `setrlimit`, `waitpid` for the process-group lifecycle |
| `libc` | `0.2` | `pre_exec` bits `nix` does not cover |
| `jiff` | `0.2` | timestamps, `dateadded` formatting, subscription scheduling arithmetic (correct, small, no `chrono` `time`-crate churn) |
| `rand` | `0.9` | subscription jitter, backoff jitter |
| `dashmap` | `6` | cancel-token map read by many tasks, written by one |
| `bytes` | `1` | shared `Arc`-backed WS frame payloads |
| `walkdir` | `2` | custom-dirs listing and orphan sweeps, on `spawn_blocking` |
| `percent-encoding` | `2` | `download_url` construction |
| `mime_guess` | `2` | `Content-Type` for served files |
| `sha2` | `0.10` | catalog ETag, synthetic entry keys |
| `humansize` | `2` | Telegram message rendering |
| `tempfile` | `3` | atomic file replacement in the audio-sync hook; tests |
| **dev** `insta` | `1` | snapshot tests for every WS frame, REST body and merged option dict |
| **dev** `wiremock` | `0.6` | SC scrape pipeline, Jellyfin, POT probe |
| **dev** `proptest` | `1` | `natural_cmp`, progress normaliser monotonicity, template renderer |
| **dev** `assert_cmd` + `predicates` | `2` / `3` | shim selftest and plugin example execution |
| **dev** `tokio-test` | `0.4` | deterministic time in the scheduler and subscription tests |
| **build** `cargo-chef`, `cargo-deny` | — | layer-cached Docker builds; licence/advisory gate in CI |

Deliberately **not** used: `sqlx` (compile-time DB connection for SQLite adds build friction for no
benefit here), `figment`/`config` (env-only config with legacy names is 200 lines of explicit code
and better errors), `once_cell` (edition 2024 has `std::sync::LazyLock`), `socketioxide` (BRIEF
forbids Socket.IO), `pyo3` (§7.1), `chrono` (`jiff` covers it), `lazy_static`, `structopt`.

---

## 21. Risk register

| # | Risk | Likelihood × Impact | Mitigation |
|---|---|---|---|
| R1 | **yt-dlp nightly breaks the shim** (a hook signature, an option name, an exception class moves). Bumped automatically every 3 days with auto-merge. | Medium × High | `hello.protocol` handshake; the shim only touches `progress_hooks`, `postprocessor_hooks`, `YoutubeDL(params)`, `extract_info` — the most stable surface. CI runs the shim selftest **and** a live extract against a stable public URL on every yt-dlp bump PR; failure blocks auto-merge. `healthz` reports `ytdlp: degraded` with the selftest error, and the queue fails fast with a clear message instead of hanging. |
| R2 | **Shim protocol drift between Rust and Python** in a partial image update | Low × High | Both sides constant `PROTOCOL = 1`; mismatch ⇒ exit 64 and a `Degraded` provider at boot, not a per-job mystery. The shim is shipped inside the same image layer as the binary; the Dockerfile copies both from the same build context. |
| R3 | **`wreq`/BoringSSL fails to build or breaks on arm64** | Medium × High | Feature-gated (`sc-impersonate`, default on); `AULOS_SC_HTTP=plain|curl` fallbacks; CI builds both feature combinations for amd64 and arm64; `preflight` degrades SC rather than failing the server. |
| R4 | **StreamingCommunity changes its site** (Inertia props, `window.streams`, domain) | High × Medium | Scrape steps are separate, individually unit-tested functions with `wiremock` fixtures captured from real pages; every step failure produces a distinct error code so logs say *which* step broke; `AULOS_SC_EXTRA_HOSTS` for mirrors; the version cache retries once on 409/404. If it breaks, users can drop in a `command` plugin as a stopgap without waiting for a release — which is itself the strongest mitigation. |
| R5 | **A community plugin hangs, floods stdout, or fills the disk** | Medium × Medium | Per-plugin `max_concurrent`, resolve/stall/hard timeouts, `RLIMIT_AS`/`RLIMIT_FSIZE`/`RLIMIT_CPU`/`RLIMIT_NOFILE`, a 4 MiB line cap and a `max_output_bytes` budget, process-group kill, and `nice(5)`. Repeated failures mark the plugin `Degraded` (circuit breaker: 5 failures in 10 min ⇒ 10 min cool-down) and surface in `healthz`. |
| R6 | **Malicious `plugin.toml`** (someone shares a "codec pack") | Low × High | No shell by default; cleared env; explicit `env.pass` allow-list; refusal to run world-writable or setuid files; `GET api/v2/providers` exposes every argv so an operator can audit; docs state plainly that a plugin runs with the server's privileges. |
| R7 | **Plugins are not a security boundary** and someone assumes they are | Medium × High | Documented in bold in the plugin guide and in `GET api/v2/providers` (`"isolation": "process-only"`). A future hardening path (bubblewrap/`seccomp`/a dedicated uid) is sketched in §23 but explicitly out of scope now. |
| R8 | **SQLite write contention or WAL growth** under a 500-item burst | Low × Medium | Single writer with 5 ms/256-command batching; WAL checkpoint on shutdown and every 1000 transactions; `journal_size_limit`; progress never written; `healthz` exposes `wal_bytes` and `write_queue_depth`. |
| R9 | **Legacy import corrupts or loses state** | Medium × High | Import is one transaction; on any failure the new DB is removed, the process exits non-zero, and the legacy files are untouched (roll back by re-pulling the Python image). Sources are renamed `.imported` only after commit. A `--import-dry-run` flag prints the plan. Golden-file tests over real captured `queue.json`/`completed.json`/`subscriptions.json`. |
| R10 | **v1 shim divergence breaks the shipped iOS build** during cutover | Medium × High | The shim is defined by tests, not by prose: a `tests/v1_golden/` directory of request/response pairs captured from the Python server, replayed against the Rust server byte-for-byte (modulo `Content-Type` and the additive keys, both allow-listed). Cutover plan: run both images behind the reverse proxy on different ports and diff `GET history`. |
| R11 | **Process-group kill leaves zombies or kills the wrong group** | Low × High | `process_group(0)` makes each child its own leader; we only ever `killpg(-child_pid)`; one reaper task per child; a `Drop` guard SIGKILLs and reaps. An integration test spawns `sh -c 'sleep 300 & wait'` and asserts the grandchild dies. |
| R12 | **WS backpressure / a slow client** grows memory | Low × Medium | `broadcast` with a fixed 1024 capacity; frames are `Arc`-shared and serialised once; a lagged receiver gets one `resync` and re-snapshots; per-connection send timeout closes dead sockets. |
| R13 | **`seq` reuse after an unclean shutdown** would make `?since=` lie | Low × High | `seq` high-water persisted every 256 allocations *and* derivable from `max(event_log.seq)`; on open we take the max of both; `instance_id` in `hello` lets a client detect a different server generation and force a snapshot. |
| R14 | **POT sidecar death** silently breaks YouTube | Medium × Medium | Supervised with backoff, logs into `tracing`, health probe in `healthz`, and `bot_check` errors carry a hint plus one automatic retry after 60 s. |
| R15 | **Telegram rate limits** cause dropped or delayed notifications | Medium × Low | `teloxide` `throttle` adapter, per-chat edit floor with `RetryAfter` honouring and exponential local backoff, text-change suppression, and aggregation above `AULOS_TELEGRAM_AGGREGATE_FROM` items. |
| R16 | **N_m3u8DL-RE absent or ABI-broken** on an arch | Low × Medium | `preflight` probes it; absence degrades SC to ffmpeg-only (a supported path) rather than failing downloads. |
| R17 | **Scope creep** — this design has three provider implementations plus a plugin format | High × Medium | Implementation order is fixed: `core`→`store`→`provider`+`fake`→`queue`→`api v2`→`ytdlp`→`v1 shim`→`sc`→`telegram`→`subscriptions`→`hooks`→`command`. Every stage is shippable; the `command` provider is last because it is the only one with no legacy user waiting for it. |
| R18 | **Two clients, one protocol change** (iOS must ship before v1 can be dropped) | High × Low | v1 shim has no sunset date in this design; it is ~600 lines and fully test-pinned. `api/v2/version.capabilities` lets the client feature-detect and migrate endpoint by endpoint. |

---

## 22. Testing strategy

| Layer | What |
|---|---|
| `aulos-core` unit | config parsing (every legacy default, every `%%` indirection, every bad-token case), boolean tokens, status transition matrix, `ProgressNormalizer` (the four legacy rules + monotonicity + source-tag reset, `proptest`), legacy request migration table, path containment (including the `/downloads-evil` case), outtmpl field pre-resolution |
| `aulos-provider-ytdlp` unit | `get_format` golden strings for the full (type × format × quality × codec) matrix; `get_opts` golden JSON for every branch; option-layer precedence (env vs file vs preset order vs overrides vs pinned); `null`-clears-a-key; `best_remux` pops `format`; uncoercible-option rejection; error-taxonomy regex table |
| shim | `pytest`-free: `assert_cmd` runs `ytdlp_runner.py` with `mode=selftest`, with a malformed job (exit 2), with an unknown protocol (exit 64), and with a `file://` URL for a real extract+download of a bundled 1 s test clip. A `--replay <transcript.jsonl>` mode lets Rust-side tests exercise every frame type without Python at all |
| `aulos-provider` unit | match scoring and tie-breaks; registry select with hints and degraded providers; template renderer (all tokens, unknown-token rejection, `proptest` for injection safety); progress regex parsers with real captured N_m3u8DL-RE/ffmpeg/kinotek output; `Child` process-group kill (grandchild dies), stall watchdog, line-cap |
| `aulos-provider-sc` | `wiremock` fixtures for S1–S4 captured from real pages; token/expires/`h=1` assembly; entry id/title shaping for movie/episode/season/title; `natural_cmp` (`proptest`); gapless mux against synthetic TS segments (assert exact byte concatenation and one ffmpeg invocation) |
| `command` plugin | the shipped `plugins/examples/kinotek` runs end-to-end against `wiremock`; manifest validation table (one test per rejection reason); a deliberately hostile plugin (infinite stdout, `sleep 1d`, 1 GiB write) asserts the limits fire |
| `aulos-store` | migration from empty; legacy import golden files (real `queue.json`/`completed.json`/`subscriptions.json`/`telegram_bot_config.json`, both schema 1 and 2); import failure rollback; `seq` monotonicity across reopen; `event_log` trim; 500-insert batching (assert transaction count) |
| `aulos-queue` integration | the `fake` provider with a scripted timeline (BRIEF §17): single add, 500-item playlist, cancel at 40 %, provider error with retry, restart recovery (`claim_inflight`), slot accounting incl. `uses_global_slot: false`, group aggregate correctness |
| `aulos-api` integration | `axum::Router` + `tower::ServiceExt::oneshot`; `insta` snapshots of every REST body and every WS frame type; `?since=` (delta, snapshot-fallback, future-seq); WS lag → `resync`; the `tests/v1_golden/` replay corpus |
| `aulos-subscriptions` | `tokio-test` paused time: jitter distribution, backoff growth and cap, concurrency limit, backfill suppression, `is_live` re-queue, seen-cap trimming, single-video rejection |
| `aulos-telegram` | handler tests against a mocked Bot; callback grammar; URL extraction + SSRF table (the exact legacy accept/reject set plus the resolver cases); live-message edit throttling with injected `RetryAfter` |
| `aulos-hooks` | Jellyfin debounce (N completions in 30 s ⇒ 1 request, trailing edge), targeted vs global refresh with a 404 fallback, NFO XML snapshots for movie and episode, audio-sync timeout scaling and the "failure does not fail the item" invariant |
| e2e | `tests/e2e/run.sh` guarded by `AULOS_E2E=1`: build the image, `docker run` it with a temp volume, POST a real public URL, assert the WS frame sequence and the produced file, then restart the container mid-download and assert recovery |
| CI gates | `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`, `cargo deny check`, a `no_unwrap` lint (`clippy::unwrap_used` denied outside `#[cfg(test)]`), and the shim selftest |

---

## 23. Open questions

1. **`quality: "worst"` (C-15).** Legacy silently returns the *best* stream (`get_format` emits no
   `worst*` selector). Do we fix the selector (behaviour change for anyone relying on it) or keep
   the quirk with a catalog notice? Proposal: keep the quirk in v1, fix it in v2 behind
   `AULOS_FIX_WORST_SELECTOR=true`, default off for one release.
2. **Group representation on the wire.** This design converts the anchor item into a `group` row and
   deletes the anchor item. The alternative is keeping the anchor as an item with `kind: "group"`.
   The former is cleaner for the store, the latter means the client needs only one list type. Which
   does the iOS rewrite prefer?
3. **v1 `id` for `history`.** Legacy `DownloadInfo.id` is the extractor's id (with the prefix). The
   shipped iOS build keys deletes on `url`. Do we expose the ULID as v1 `id` (safer, and the client
   already falls back), or the legacy extractor id (byte-identical, but a second identity to carry)?
4. **`canceled` in v1.** Mapped to `error` + `msg:"Canceled"` here. Acceptable, or should canceled
   items be hidden from `GET history` entirely for v1 clients?
5. **Plugin distribution.** Is a plain directory drop enough, or do we want
   `POST api/v2/providers/install` from a git URL / tarball with a signature check? The trait and
   registry support it; it is a policy decision with real supply-chain consequences.
6. **Plugin sandboxing beyond rlimits.** `bubblewrap` (if present) or a dedicated low-privilege uid
   would make plugins a genuine boundary. Worth the packaging cost and the "why can't my plugin see
   /media" support load?
7. **Resolution caching.** Should `resolve` results be cached (e.g. 5 min per url) so a re-add or a
   Telegram double-paste does not re-hit the network? It helps the user and the site; it also risks
   stale titles. Proposal: cache only *negative* results (unsupported/unavailable) for 60 s.
8. **APNs.** The `Notifier` trait is ready. Does the iOS side want device-token registration in this
   server (a new table + endpoint) or a webhook to a separate push service?
9. **Auth.** The brief is silent: today Authelia sits in front and the server trusts everything. Do
   we want an optional `AULOS_API_TOKEN` bearer check so the server is not naked if the proxy is
   misconfigured? (Cheap: one middleware, one env var, `401` with the JSON envelope.)
10. **`DELETE_FILE_ON_TRASHCAN` scope.** We now know every artifact, so deleting an item can remove
    subtitles, chapters, NFO and thumbnails too. That is what users expect, but it is strictly more
    destructive than legacy. Default on or off?
11. **Retry limit and dead-lettering.** `AULOS_MAX_ATTEMPTS=2` with per-class backoff is proposed.
    Should exhausted items land in a distinct `error` sub-state (`error_permanent`) so the UI can
    offer "retry all transient failures"?
12. **Multi-arch `wreq`.** If arm64 BoringSSL proves painful, is `AULOS_SC_HTTP=curl` (shipping
    `curl-impersonate` in the image, ~8 MiB) an acceptable default for arm64 only?
