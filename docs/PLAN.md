# Aulos Server — Implementation Plan (PLAN.md)

Status: **binding**. Derived from `docs/DESIGN.md`; the wire contract is `docs/PROTOCOL.md`.

**All 18 work packages (WP-00…WP-17) are complete, and so is the final integration pass.**
This file has no per-package status table of its own; the one that is kept up to date is in
`docs/STATUS.md`, and the deviations each package took from DESIGN are in
`docs/INTEGRATION-NOTES.md`. Read the package sections below as the specification each one was
built against, not as a to-do list.

18 work packages in three waves (4 · 8 · 6). Every package is scoped to roughly one engineer-day of focused
work, states the exact public interfaces it must expose or consume (signatures copied verbatim from
DESIGN.md), names its dependencies on other packages, and lists acceptance tests plus a definition
of done.

---

## 0. Ground rules for every package

These are not repeated per package. A package is not done unless all of them hold.

| Rule | Detail |
|---|---|
| Green gates | `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace --locked` all pass. |
| No `unwrap` | `unwrap_used = "deny"` at the workspace level; `#[cfg(test)]` is exempt. `expect_used = "warn"` with a justification comment where used. |
| Errors | `thiserror` in libraries, `anyhow` only in `aulos-server`. Every library error type implements `fn code(&self) -> ErrorCode` and `fn retryable(&self) -> bool`. |
| Tracing | Structured fields, never eager formatting on a hot path. Every job carries a `job{item_id, provider, url_host}` span; every request carries `request_id`. |
| Secrets | Any type that can hold a token, key, cookie or proxy URL uses the `Redact` newtype in its `Debug`/`Display`. |
| Dependency direction | Strictly downward per DESIGN §3. **No provider crate may depend on `aulos-store` or `aulos-queue`** — enforced by `tests/arch.rs` (WP-03). |
| Public API | Everything listed under "Interfaces" is `pub` and documented with `///`. Signatures must match DESIGN.md exactly; a deliberate deviation requires a DESIGN.md edit in the same PR. |
| Serde discipline | No `skip_serializing_if` on any wire type. `#[serde(deny_unknown_fields)]` is **forbidden** on request types. |
| Wire changes | Any change to a wire type requires the `print-schema` snapshot to be updated in the same commit, with the diff called out in the PR body. |
| Fixtures | Test fixtures live under `crates/<crate>/tests/fixtures/`; captured HTML, JSON and transcripts are checked in, never fetched at test time. |
| Branch / PR | One branch per package, named `wp-NN-<slug>`. The PR body links the DESIGN.md sections it implements. |

**Wave discipline.** WP-00 has no code dependency on anything and can run on day one in parallel
with WP-01; the rest of wave 0 must compile, pass all gates, and be **merged to `master`** before any
wave 1 package starts — it is the shared vocabulary, and a churning `aulos-core` would serialise
seven parallel packages into one queue. Wave 1 packages are mutually independent and can all run in
parallel. Wave 2 packages depend on wave 1 and on each other as stated.

---

# Wave 0 — Scaffold (must be merged before wave 1)

## WP-00 — Legacy capture: the three golden corpora

**Crates:** none. `tools/capture/`, `tests/v1_golden/`, `crates/aulos-provider-ytdlp/tests/golden/`,
`crates/aulos-core/tests/golden/`
**Depends on:** nothing (it runs against the **legacy Python image**, not against this workspace)
**Size:** ~1 day

### Why this is a work package and not a footnote

R1 — *"the v1 shim is subtly wrong and the shipped iOS build silently shows a broken queue during
cutover"* (M/H, the top risk) — and R21 are mitigated **only** by three checked-in corpora, and
every package that consumes them (WP-02, WP-06, WP-15) treats them as an input that already exists.
Nothing produced them. A corpus that no package owns is an unmitigated top risk, so it gets a
deliverable, an owner and acceptance criteria like anything else. It must be captured **while the
Python server is still running**, which is another reason it cannot be deferred: after the cutover
the source of truth is gone.

### Scope

- `tools/capture/README.md`: how to stand up the legacy image
  (`ghcr.io/tatoalo/metube_pot@<digest>`) against a scratch `STATE_DIR`, seeded so every route has
  something to return (a queued item, a pending item, a finished item, a failed item, one
  subscription, one two-episode SC season, uploaded cookies).
- `tools/capture/capture_v1.sh` — drives the running legacy server and writes
  `tests/v1_golden/<name>/{request.json,response.json,meta.json}`. `meta.json` records the method,
  path, status, **`Content-Type`** and the response headers, because the shim's documented deltas
  are header-level (DESIGN §11.1). The route checklist it must cover, one directory each:

  | Route | Cases to capture |
  |---|---|
  | `POST add` | ok (video), ok (playlist), duplicate, **unsupported URL**, **`Invalid/empty data was given.`**, a geo-blocked URL if one is reachable, each `_migrate_legacy_request` row (6), the five §11.2.1 leniencies, a bad `download_type`, a bad `playlist_item_limit`, `auto_start` as `true`/`"true"`/`"false"` |
  | `GET history` | empty, one of each status, ≥ 600 done rows (the window-vs-whole-set case of DESIGN §11.4) |
  | `POST delete` | `where=queue`, `where=done`, a url key, an unknown key, a missing `where` |
  | `POST start` | a pending id, an unknown id, `ids: null` (captures the legacy 500) |
  | `GET version`, `GET presets`, `GET robots.txt`, `POST cancel-add`, `GET cookie-status` | as-is |
  | `POST upload-cookies` | ok, no file, **1 000 001 bytes** (the decimal-cap boundary of DESIGN §16.6) |
  | `POST delete-cookies` | ok, nothing to delete |
  | `POST subscribe` | ok, duplicate, single-video URL, bad interval type, interval `0` |
  | `GET subscriptions`, `POST subscriptions/{update,delete,check}` | ok plus every 400 reason and the `enabled` 500 |

- `tools/capture/dump_formats.py` — imports the legacy `app/dl_formats.py` and iterates the **whole**
  legal `(download_type, codec, format, quality)` space, writing
  `crates/aulos-provider-ytdlp/tests/golden/formats.json` (selector strings) and `opts.json` (one
  entry per `get_opts` branch, canonical JSON). This is the file WP-06 diffs against.
- `tools/capture/dump_progress_vectors.py` — drives the legacy `_calculate_progress_percent` over
  the legacy unit-test cases plus a generated sweep (exact totals, estimates, fragment bounds, the
  bogus 1 KiB/1 KiB frame, source changes) into `crates/aulos-core/tests/golden/percent.json`.
  This is the file WP-02's `Normalizer` test replays.
- `tests/v1_golden/MANIFEST.json` — provenance, and it is not optional: the legacy git commit
  (`fd35a66` at the time of writing), the image digest, the capture timestamp, the `STATE_DIR`
  fixture description, the legacy `yt-dlp` version, and the `URL_PREFIX` used. A corpus without
  provenance cannot be re-derived and cannot be argued with.
- `tests/v1_golden/harness.rs` is **not** here — it is WP-15's, which owns the replay. This package
  ships data plus the tools that regenerate it.
- Secret hygiene: the capture scripts scrub `Cookie`, `Authorization` and any `Set-Cookie` header,
  and the seeded subscription/URLs are public. `gitleaks` runs over the corpus in CI.

### Interfaces

None (no Rust). The contract is the on-disk layout above, and it is what WP-02, WP-06 and WP-15
consume.

### Acceptance tests

- `tools/capture/verify.py`, run in CI (not against the network): every directory in
  `tests/v1_golden/` has all three files and parses; `MANIFEST.json` is present and has every field;
  **every route in the checklist above has at least one directory** and every 400/500 case named in
  the table is present — a route-coverage assertion, so a half-finished capture fails loudly rather
  than silently shrinking the shim's test surface.
- `formats.json` is non-empty and covers every tuple the DESIGN §6.6 catalog admits (the same
  cross-check WP-06 runs, asserted here so a gap is caught before WP-06 depends on it).
- `percent.json` is non-empty and contains at least one vector per row of the DESIGN §4.7 table.
- No captured file contains a `Cookie`, `Authorization` or `Set-Cookie` header value.

### Definition of done

The three corpora are checked in with provenance, CI verifies their shape and coverage on every
push, and WP-02, WP-06 and WP-15 can be written against them without touching the legacy server
again.

---

## WP-01 — Workspace, CI, Docker skeleton

**Crates:** the workspace root, `docker/`, `.github/workflows/`
**Depends on:** nothing
**Size:** ~1 day

### Scope

- Cargo workspace at `aulos_server/Cargo.toml`: `resolver = "3"`, `edition = "2024"`,
  `rust-version = "1.95"`, `[workspace.dependencies]` pinning every crate in DESIGN §18.6, and
  `[workspace.lints.clippy]` with `unwrap_used = "deny"`, `expect_used = "warn"`,
  `disallowed_methods = "deny"`.
- All eleven crates created per BRIEF's repository layout **plus the dev-only
  `aulos-workspace-tests`** (DESIGN §3: `publish = false`, no `src/`, it exists to own
  `tests/arch.rs` and is the target of `cargo test -p aulos-workspace-tests arch` in
  DESIGN §18.4) — twelve workspace members. Each has a `lib.rs` (or `main.rs` for
  `aulos-server`) that compiles and a `//! ` module doc naming its responsibility. `aulos-server`
  is a `clap` skeleton exposing `serve | check-config | import | doctor | print-schema |
  repair-ids | healthcheck`, where every subcommand except `serve` prints `not implemented` and
  exits 0, and `serve` binds nothing. **The subcommand is optional and defaults to `serve`** — a
  bare `aulos-server` with no argument must run, because the entrypoint execs
  `aulos-server "$@"`.
- `docker/Dockerfile` exactly as DESIGN §18.1 (it will build a stub binary) — including
  `CMD ["serve"]` and `HEALTHCHECK CMD ["/usr/local/bin/aulos-server","healthcheck"]`, **not** a
  shell `curl` interpolating `${URL_PREFIX}` (which would produce `…:8081metubehealthz` for
  `URL_PREFIX=metube`, the exact input C16 exists to fix) — `docker/entrypoint.sh` as DESIGN §18.2,
  `docker/compose.example.yml` as DESIGN §18.3.
- `.github/workflows/ci.yml` with the jobs from DESIGN §18.4 (`fmt`, `clippy`, `test`, `arch`,
  `deny`, `python`, `coverage`, `schema`, `gitleaks`); `coverage`, `schema` and `python` may be
  no-op-but-present until their inputs exist. `docker.yml` building **`linux/amd64` only**.
  `dev-build.yml`, `update-yt-dlp.yml` (with the §18.5 hardening), `update-sidecars.yml`,
  `upstream-sync-check.yml`, `upstream-sync-label.yml`, `release.yml`.
- `deny.toml`, `rustfmt.toml`, `.gitignore` (covering `.env`, `*.db`, `target/`), `README.md`
  stub, `plugins/examples/` directory with a `.gitkeep`.

### Interfaces

```rust
// crates/aulos-server/src/cli.rs
#[derive(clap::Parser)]
#[command(name = "aulos-server")]
pub struct Cli {
    /// Absent => Cmd::Serve. A bare subcommand enum would REQUIRE an argument, and the
    /// entrypoint execs `aulos-server "$@"`, so the container would fail to start (DESIGN §18.1).
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(clap::Subcommand)]
pub enum Cmd {
    Serve,
    CheckConfig,
    Import { state_dir: PathBuf, db: PathBuf, dry_run: bool, force: bool, skip_corrupt: bool },
    Doctor,
    PrintSchema { json: bool },
    /// DESIGN §3.1 / §4.1 / R8 — the documented recovery for the allocator boot check.
    RepairIds { db: Option<PathBuf>, dry_run: bool },
    /// DESIGN §3.1 / §18.1 — the container HEALTHCHECK. Loads Config so URL_PREFIX is normalised.
    Healthcheck,
}

impl Cli { pub fn command(self) -> Cmd { self.cmd.unwrap_or(Cmd::Serve) } }
```

### Acceptance tests

- `cargo build --workspace` and `cargo test --workspace` succeed on a clean checkout.
- `cargo clippy --all-targets --all-features -- -D warnings` is clean.
- `docker build -f docker/Dockerfile .` produces an image; `docker run <img> aulos-server doctor`
  exits 0 with the `not implemented` line.
- **`docker run <img>` with no command starts the server** (the `CMD ["serve"]` + `Option<Cmd>`
  path), and so does overriding the entrypoint to run the binary bare.
- The `HEALTHCHECK` is the `healthcheck` subcommand, asserted by a grep of the Dockerfile, and a
  container test with `URL_PREFIX=metube` reports `healthy` — the regression test for the raw
  `${URL_PREFIX}` interpolation bug.
- CI is green on the branch, and the `docker.yml` build matrix contains **only** `linux/amd64`
  (asserted by a grep in the `arch` job).
- `docker/entrypoint.sh` shellcheck-clean; a container test asserts the `CHOWN_DIRS` mode line is
  logged for each of `true`, `false`, `recursive`.

### Definition of done

The workspace compiles, CI is green, the image builds, and every crate directory exists with a
documented `lib.rs`. Nothing in wave 1 needs to create a crate.

---

## WP-02 — `aulos-core`: domain, config, catalog

**Crates:** `aulos-core`
**Depends on:** WP-01, and WP-00 for `tests/golden/percent.json`
**Size:** ~1 day (large but mechanical; if two engineers are available, split at the
`config`/`ytdl_options` boundary into WP-02a *domain* and WP-02b *config*)

### Scope

Everything in DESIGN §4, §5, §6.6 (types only), §2.2.1 (the `EventRouter`) and §17.1–17.2 (loading
only, no watcher).

`aulos-core` is where **every** type that appears in a `DomainEvent` payload or in a cross-crate
port must live, because `DomainEvent` is declared here and the dependency graph is strictly
downward (DESIGN §3). That is not stylistic: putting `SubscriptionView` in `aulos-subscriptions`,
`HealthView` in `aulos-server` or `ReloadReport` in `aulos-provider` produces three dependency
cycles that `tests/arch.rs` rejects.

- `id`: `ItemId`, `GroupId`, `SubId`, `Seq`, `Ord0`, `BootId`, `ConnId` (a `u64` newtype; the
  atomic counter that mints them lives in `aulos-api`) with the serde attributes and the
  `HiLoAllocator` **trait** (the implementation is WP-04).
- `status`: the closed `Status` enum, its predicates, `v1()`, `TerminalStatus`
  (`{Finished, Error, Canceled}` with `TryFrom<Status>` — the type a community manifest's
  `on = [...]` parses into, WP-10), and the legal-transition table of DESIGN §4.2 with a
  `debug_assert`-backed `fn can_transition(from, to) -> bool`, **including the pause edges**
  (`Queued(auto_start=true) → Queued(auto_start=false)` and
  `Preparing|Downloading|Postprocessing → Queued(auto_start=false)`).
- `item`: `Kind`, `Item`, `ItemView`, `RequestView` (DESIGN §4.6.1), `FileRef`, `SelectionView`,
  `SourceRef`, `SourceKind`.
- `request`: `DownloadType`, `Codec`, `SubtitleMode`, `SubtitleLang`, `FormatId`, `QualityId`,
  `Selection`, `DownloadRequest`, and `validate()` against a catalog.
- `catalog`: `FormatCatalog`, `DownloadTypeSpec`, `FormatSpec`, `QualitySpec`, `FormatFlags`,
  `OptionSpec`, `OptionKind`, `Choice`, `MergedCatalog`, `NamingPolicy`, the `ytdlp` catalog
  constant of DESIGN §6.6 (**`video`/`ios` carries all nine heights**, `captions` carries all seven
  formats), and `FormatCatalog::bot_formats()` — the documented projection to the legacy nine-entry
  Telegram list (DESIGN §12.2).
- `error`: `ErrorCode` (`#[non_exhaustive]`, `snake_case`, **including `socketio_removed` → 501**,
  which ships on day one and therefore has to be in the enum, the `print-schema` snapshot and the
  `aulos_http_errors_total{code}` label set), `WireError` (**including `field`** — the one struct
  serves both the HTTP envelope and `Item.error`, PROTOCOL §1.5/§2.3), the `Redact` newtype.
- `progress`: `RawProgress`, `ProgressCell`, `Normalizer`.
- `paths`: `RelDir`, `RelPath`, `Paths`, `contain(base, candidate)` doing **component-wise**
  containment on canonicalised paths with symlink-escape rejection, and
  `sanitize_path_component`.
- `prefix`: the `Prefix` newtype — the only thing in the process allowed to build a path.
- `clock`: the `Clock` trait plus `SystemClock` and `FakeClock`.
- `config`: `RawEnv`, `Config`, `load()`, `ConfigError`, the complete `DEFAULTS` table of
  DESIGN §17.3, `%%` indirection with a cycle check, the legacy boolean token set, prefix
  normalisation, per-key numeric leniency, and unknown-`AULOS_*` rejection.
- `ytdl_options`: `YtdlOptions` and `layer()`, with the exact legacy error strings.
- `event`: `DomainEvent`, `AddReason`, `RemoveReason`, `Notice`, `Level`, **and the `EventRouter`
  fan-out of DESIGN §2.2.1** — `EventSender`, `EventInbox`, `SubscriberSpec`, `DropPolicy`,
  `EventFilter`, `EventRouter::{new, subscribe, spawn}`. This is the piece that makes four
  independent event consumers possible at all: an `mpsc::Receiver` has one owner, so the router
  owns it and hands each subscriber its own bounded inbox of `Arc<DomainEvent>`.
- `subscription`: `SubscriptionRecord` (persisted by WP-04) and `SubscriptionView` (the wire
  projection carried by `DomainEvent::SubscriptionChanged`).
- `telegram`: `ChatConfig` (persisted by WP-04) and `normalize_download_selection`.
- `health`: `HealthRegistry`, `HealthView`, `ComponentStatus` — written by `aulos-server` (WP-17)
  and read by `aulos-api` (`ApiState.health`, WP-14); `aulos-server` depends on `aulos-api`, so
  the registry cannot live there.
- `reload`: `ReloadReport` (produced by `Registry::reload_commands`, carried by
  `DomainEvent::ProvidersReloaded`).
- `ports`: the `HookStore` trait of DESIGN §7.1/§13 — `entry_blob` / `drop_entry_blob` /
  `set_size` — plus `PortError`. It is **implemented by `aulos-queue::EngineHookStore`** (WP-12),
  not by `Store`, because both writes must be engine-mediated so the item cache and the delta
  baseline see them (DESIGN §13.3); `aulos-hooks` consumes the trait (WP-11) and therefore needs
  neither a store nor a queue dependency.
- `subscription` additionally declares **`SubscriptionsHandle`** and its `SubCmd`, `SubChanges`,
  `CheckJob`, `SubError` and `SubsHealth` (DESIGN §14.1). The handle is a clone of an
  `mpsc::Sender<SubCmd>` with no logic; putting it here is what lets `aulos-api`'s `ApiState` hold
  it **without** depending on `aulos-subscriptions`, which removes the only wave-1 → wave-2 edge
  that would otherwise force WP-16 to land before WP-14 can compile.
- `FieldUpdate<T>` (`Keep | Clear | Set`), the single three-state patch type every nullable-column
  `WriteOp` field uses (DESIGN §7.1), and `HookPhase` (`PreTerminal | PostTerminal`, DESIGN §13).

### Interfaces

```rust
pub fn load(env: &RawEnv) -> Result<Config, Vec<ConfigError>>;
impl EventRouter {
    pub fn new(capacity: usize) -> (Self, EventSender);
    pub fn subscribe(&mut self, spec: SubscriberSpec) -> EventInbox;
    pub fn spawn(self) -> JoinHandle<()>;
}
impl EventSender { pub async fn publish(&self, ev: DomainEvent);
                   pub fn try_publish(&self, ev: DomainEvent) -> Result<(), TryPublishError>; }
impl EventInbox  { pub async fn recv(&mut self) -> Option<Arc<DomainEvent>>;
                   pub fn dropped(&self) -> u64; }
impl FormatCatalog { pub fn bot_formats(&self) -> Vec<BotFormat>; }
impl Status { pub const fn is_terminal(self) -> bool; pub const fn is_active(self) -> bool;
              pub const fn is_running(self) -> bool; pub const fn v1(self) -> &'static str; }
impl Normalizer { pub fn apply(&mut self, m: &RawProgress, status: Status) -> f64; }
impl YtdlOptions { pub fn layer(&self, presets: &[Box<str>],
                                overrides: &Map<String, Value>) -> Map<String, Value>; }
pub fn contain(base: &Path, candidate: &Path) -> Result<PathBuf, PathError>;
pub trait Clock: Send + Sync { fn now_ms(&self) -> i64; fn instant(&self) -> Instant; }
pub trait HiLoAllocator: Send + Sync { fn next(&self) -> i64; fn current(&self) -> i64; }
```

### Acceptance tests

- `rstest` case per row of DESIGN §17.3: default, valid parse, invalid parse, and (for booleans)
  every accepted token plus a rejected one.
- `%%` indirection: single hop, two hops, a cycle (fatal), an unknown target (fatal).
- `URL_PREFIX`: `""` → `/`; `metube` → `/metube/` with a WARN; `/metube` → `/metube/`;
  `/metube/` unchanged.
- Unknown `AULOS_FOO` is fatal; unknown `RANDOM_VAR` is ignored.
- `contain`: `/downloads` + `Music` ok; `..` rejected; `/downloads-evil` rejected (the legacy
  `startswith` bug); a symlink pointing outside rejected.
- `Normalizer`: **`tests/golden/percent.json`, produced by WP-00** from the legacy
  `_calculate_progress_percent`, replays byte-for-byte, plus a `proptest` asserting monotonicity
  over arbitrary status/progress sequences and a reset on a `source_tag` change.
- Validation matrix: every legal `(download_type, codec, format, quality)` tuple accepted, and one
  rejected case per row with the **exact legacy message string**.
- `YtdlOptions::layer`: env-then-file precedence, presets applied in request order, overrides last,
  `null` preserved as a key-clearing value.
- Serde: `insta` snapshot of a fully populated `ItemView` asserting **every** key is present and
  no `Option::None` is omitted (`request` and its nine keys included); a compile-time test
  asserting the serializer's field list and the diff macro's field list are identical.
- `WireError` serialises all five keys, `field` included, and the same struct decodes an
  `Item.error` payload and an HTTP envelope's `error` minus `request_id`.
- `RequestView`: an override key matching the secret pattern serialises as `"«redacted»"` while
  the key set is preserved.
- Catalog: `video`/`ios` offers the nine heights and `captions` offers all seven formats (a
  regression test against the legacy `main.py:610-620` set and `VALID_SUBTITLE_FORMATS`);
  `bot_formats()` returns **exactly** the nine legacy entries in order, with `thumbnail` (not
  `jpg`), `any` carrying the `audio` pseudo-quality and `mp4` carrying `best_remux`.
- `EventRouter`: three subscribers with different filters each receive only their discriminants
  and in publish order; a full `DropPolicy::DropNewest` inbox drops the **newest** event and
  increments `dropped()` while the other subscribers are unaffected; a full `DropPolicy::Block`
  inbox applies backpressure to the producer (asserted with a `FakeClock` and a bounded producer);
  dropping every `EventSender` terminates every inbox after it drains; `subscribe()` after
  `spawn()` is rejected.
- `can_transition`: every legal edge of DESIGN §4.2 accepted (pause and start edges included) and
  a table of illegal ones rejected — **including that `Finished → Postprocessing` is rejected**,
  since the pre-terminal hook phase of DESIGN §13 deliberately does not need it.
- `FieldUpdate`: a round-trip table proving `Keep` / `Clear` / `Set` are distinguishable after
  serde, so no writer can express "unchanged" and "null" with the same value.

### Definition of done

`aulos-core` has no async runtime dependency beyond `tokio::sync` types, every type in DESIGN §4–§6.6
exists with the documented serde attributes, and `cargo doc` for the crate reads as a usable
reference.

---

## WP-03 — `aulos-provider`: trait, registry, sink, process helpers, fake provider

**Crates:** `aulos-provider`
**Depends on:** WP-02
**Size:** ~1 day

### Scope

DESIGN §6.1–6.4, §6.5.3 (the spawn helper only), §2.3 (the stderr ring), plus the fake provider
from BRIEF §17 and the architecture test.

- `provider`: the `Provider` trait, `Match`, `ProviderError` (every variant, with `code()` and
  `retryable()`), `ProviderHealth`.
- `entry`: `MediaEntry`, `EntryKind`, `LiveStatus`, `EntryHints`.
- `sink`: `ProgressSink`, `ProgressSinkFactory` (`for_item(ItemId) -> ProgressSink`, the type
  WP-11's dispatcher is handed so a hook's `phase_percent` flows through the ordinary aggregator
  path), `ProgressMsg`, `Stage`, `FileSlot`.
- `registry`: `Registry`, `Selected`, `MatchReason`, `ProviderState`, `pick`, `by_id`,
  `catalog_for`, `merged_catalog`, and the `Degraded` semantics of DESIGN §6.4 including the
  5-failures-in-10-minutes circuit breaker. `reload_commands` returns
  `aulos_core::reload::ReloadReport` (the type lives in `aulos-core` because
  `DomainEvent::ProvidersReloaded` carries it, DESIGN §3).
- `proc`: the shared child-process helper used by all three providers — `SpawnSpec`, `Child`,
  `process_group(0)`, a **mandatory** stderr drain into a capped `StderrRing`, a line reader with a
  configurable cap, `kill_group(SIGTERM → grace → SIGKILL)`, a `Drop` guard that does the same,
  rlimit application, and `nice`.
- `humansize`: the shared 1024-based `KB/KiB/MB/MiB/GB/GiB` and `hms` parsers.
- `fake` (feature `fake`): `FakeProvider` driven by TOML timelines.
- `tests/arch.rs`, **owned by the `aulos-workspace-tests` crate** (DESIGN §3; that is what makes
  `cargo test -p aulos-workspace-tests arch` in DESIGN §18.4 a real target): parse every
  `Cargo.toml` and enforce **all five** rules of DESIGN §3 — A1 no `aulos-provider*` crate depends
  on `aulos-store` or `aulos-queue`; A2 only `aulos-store` depends on `rusqlite`; A3 only
  `aulos-api` and `aulos-server` depend on `axum`; A4 only `aulos-server` depends on `aulos-api`;
  A5 only `aulos-server` depends on `anyhow`. It also asserts each crate's declared dependencies
  are a **subset** of its row in the DESIGN §3 table, which is what catches a crate quietly
  acquiring a dependency the design did not budget for.
- The subset rule ignores **the ubiquitous five** — `serde`, `serde_json`, `thiserror`, `tracing`,
  `async-trait` — which DESIGN §3 permits everywhere and omits from the rows. Without that
  exemption the gate would reject WP-06 through WP-09 and WP-14 on their first commit, since a
  provider crate has to name `aulos_core::Config` and `serde_json::Value` in public signatures and
  Rust has no implicit transitive `use`. The exemption list lives in one `const` in `arch.rs` and
  is asserted to match the DESIGN §3 paragraph verbatim.

### Interfaces

```rust
#[async_trait] pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn matches(&self, url: &Url) -> Match;
    fn catalog(&self) -> Arc<FormatCatalog>;
    async fn resolve(&self, url: &Url, ctx: ResolveCtx<'_>) -> Result<Vec<MediaEntry>, ProviderError>;
    async fn download(&self, ctx: DownloadCtx<'_>, sink: ProgressSink) -> Result<Outcome, ProviderError>;
    fn own_slots(&self) -> Option<usize> { None }
    async fn probe(&self) -> ProviderHealth { ProviderHealth::Ok }
}
#[derive(Clone)]
pub struct ProgressSinkFactory { /* mpsc::Sender<ProgressMsg> */ }
impl ProgressSinkFactory { pub fn for_item(&self, id: ItemId) -> ProgressSink; }
impl ProgressSink {
    pub fn progress(&self, p: RawProgress);
    pub async fn stage(&self, s: Stage, msg: Option<Box<str>>);
    pub async fn file(&self, slot: FileSlot, f: FileRef);
    pub fn log(&self, level: Level, msg: &str);
}
impl Registry {
    pub fn pick(&self, url: &Url, hint: Option<&ProviderId>) -> Selected;
    pub fn by_id(&self, id: &ProviderId) -> Option<&Arc<dyn Provider>>;
    pub fn catalog_for(&self, url: &Url) -> (ProviderId, Arc<FormatCatalog>, MatchReason);
    pub fn merged_catalog(&self) -> Arc<MergedCatalog>;
    pub fn reload_commands(&mut self, dir: &Path) -> ReloadReport;
}
pub enum Step { Wait(Duration), Stage(Stage), Progress { percent: f64, speed: Option<f64>, eta: Option<i64> },
                File { slot: FileSlot, name: String, size: u64 },
                Finish { filename: String, size: u64 }, Fail(ErrorCode), Hang,
                ExpandPlaylist(usize) }
```

### Acceptance tests

- Match scoring: a plugin at 250 beats SC at 200 beats ytdlp at 1; ties break by registration
  order; `Match::Forced` beats everything; an `exclude_path_regex` veto returns `No`.
- `Degraded`: a degraded provider still matches, and a job routed to it fails immediately with
  `provider_degraded` and the reason — it does **not** fall through to `ytdlp`. Five failures in
  ten minutes trip the breaker; the eleventh minute allows one probationary attempt.
- `proc::Child`: spawn `sh -c 'sleep 300 & wait'`, kill the group, assert **the grandchild is
  dead** within the grace period; assert a `SIGKILL` follows a non-responsive `SIGTERM`; assert a
  child that writes 1 MiB to stderr does not deadlock (the drain test); assert the line cap kills
  and reports `contract`.
- `humansize`: a table of 1024-based units and `hms` durations.
- `FakeProvider`: a scripted timeline under `FakeClock` runs a "10-minute download" in
  microseconds and emits the expected `Stage`/`Progress`/`Finish` sequence; `Hang` triggers the
  caller's stall path; `ExpandPlaylist(500)` returns 500 entries.
- `tests/arch.rs` fails when a dependency is deliberately added for each of the five rules
  (a `rusqlite` dep on `aulos-api`, an `axum` dep on `aulos-queue`, an `aulos-store` dep on
  `aulos-provider-sc`, an `aulos-api` dep on `aulos-telegram`, an `anyhow` dep on `aulos-core`),
  and **passes on the real tree** — asserted for every crate, including the provider crates with
  their `aulos-core` + `tokio` + `nix` + `command-fds` rows, so the gate and the table are proven
  consistent rather than assumed to be.

### Definition of done

A new provider can be written against this crate with no other workspace change, the fake provider
makes the wave-2 integration suite possible, and the architecture test is enforcing the boundary.

---

# Wave 1 — Independent crates (all parallel)

## WP-04 — `aulos-store`: schema, actor, allocators, reads

**Crates:** `aulos-store`
**Depends on:** WP-02
**Size:** ~1 day

### Scope

DESIGN §7.1–7.4.

- `schema`: `migrations/0001_init.sql` exactly as DESIGN §7.2, wired through `rusqlite_migration`,
  plus the checked-in `schema.sql` snapshot and the pragma set.
- `actor`: the single writer thread draining up to 256 jobs into one transaction, extending the
  batch until `AULOS_DB_FLUSH_MS`, short-circuiting on `Durability::Sync`, using `prepare_cached`
  everywhere.
- `readers`: the `ReadPool` of `SQLITE_OPEN_READ_ONLY` connections behind a semaphore.
- `alloc`: the **reserve-before-use** hi/lo allocator for `ord` and `seq`, **seeded per counter as
  DESIGN §4.1 specifies** — `ord` from `max(meta.ord_hwm, COALESCE(MAX(items.ord), -1) + 1)`, `seq`
  from `meta.seq_hwm` and its `meta.seq_hwm_witness` (there is no `seq` column; comparing the frame
  counter to the record-order column is meaningless) — plus the two boot consistency checks and the
  `aulos-server repair-ids` message. **`meta.seq_hwm_witness` is one of the `meta` keys in the
  DESIGN §7.2 DDL comment** and is written on every graceful shutdown, so the DDL snapshot test and
  the boot check agree on what the schema contains. `Store::ord_allocator()` / `seq_allocator()` expose them as
  `Arc<dyn HiLoAllocator>` so `EventHub::new` (WP-13) can be handed the `seq` counter; the `ord`
  and `seq` fields themselves stay private.
- `items`, `subscriptions`, `telegram`, `kv`: **all eighteen `WriteOp` variants** of DESIGN §7.1
  and every typed read, including `v1_done(limit)` (the v1 shim's uncapped-by-default history
  source, DESIGN §11.4) backed by an index on `(status, ord)`. The variants added since the first
  draft are the ones the feature set actually needs and none of them is optional:
  `SetAutoStart` (pause/start change **only** `auto_start`, DESIGN §8.7 — with no such variant the
  whole pause feature is unimplementable against this API), `SetSource` (`source` is on the wire and needs a
  write path of its own; boot recovery used to write `kind:"restart"` and a retry `kind:"retry"`,
  until DESIGN §4.4 made the origin permanent — the variant stays, nothing in the engine writes it
  any more, and rows an older build wrote still carry those two values),
  `SetSize` (a hook that rewrites the produced file changes the size but not the filename,
  DESIGN §13.3) and `DropEntryBlob` (the NFO hook, DESIGN §13.2).
- `SetStatus` takes `FieldUpdate<Box<str>>` / `FieldUpdate<WireError>` (`Keep | Clear | Set`) and an
  `Option<bool> auto_start`, and implements the **timestamp table** of DESIGN §7.1: `updated_at`
  always, `started_at` on the first `Preparing` only, `finished_at` on a terminal status,
  `finished_at = NULL` on a terminal → non-terminal transition with `started_at` kept. "Unchanged"
  and "set to null" are never the same value.
- `entry_blob(id)` / typed size and blob writes are exposed as ordinary reads and `WriteOp`s;
  **`aulos_core::ports::HookStore` is not implemented here** — it is implemented by
  `aulos-queue::EngineHookStore` (WP-12) so that a hook's writes pass through the engine's item
  cache and the delta baseline (DESIGN §13.3). This crate is what that implementation delegates
  its read to.
- `PRAGMA quick_check` at open; `wal_checkpoint(TRUNCATE)` + `PRAGMA optimize` on close; the WAL
  byte gauge for `healthz`.
- `print-schema` support: the DDL dump and the JSON Schema of every wire type.

### Interfaces

```rust
pub struct Store { /* … */ }
impl Store {
    pub async fn write(&self, ops: Vec<WriteOp>, d: Durability) -> Result<(), StoreError>;
    pub async fn read<T: Send + 'static>(&self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static) -> Result<T, StoreError>;
    pub async fn items(&self, f: ItemFilter) -> Result<Page<Item>, StoreError>;
    pub async fn item(&self, id: ItemId) -> Result<Option<Item>, StoreError>;
    pub async fn boot_state(&self) -> Result<BootState, StoreError>;
    pub async fn resolve_v1_token(&self, tok: &str) -> Result<Vec<ItemId>, StoreError>;
    pub async fn subscriptions(&self) -> Result<Vec<SubscriptionRecord>, StoreError>;
    pub async fn seen(&self, sub: &SubId) -> Result<HashSet<Box<str>>, StoreError>;
    pub async fn telegram_chats(&self) -> Result<HashMap<i64, ChatConfig>, StoreError>;
    pub async fn due_clears(&self, now: UnixMs) -> Result<Vec<ItemId>, StoreError>;
    pub async fn v1_done(&self, limit: Option<u32>) -> Result<Vec<Item>, StoreError>;
    pub fn next_ord(&self) -> Ord0;
    pub fn next_seq(&self) -> Seq;
    pub fn ord_allocator(&self) -> Arc<dyn HiLoAllocator>;
    pub fn seq_allocator(&self) -> Arc<dyn HiLoAllocator>;
    pub fn wal_bytes(&self) -> u64;
}
pub enum WriteOp { /* the 18 variants of DESIGN §7.1, verbatim */ }
pub enum Durability { Batched, Sync }
// NOTE: `impl HookStore for Store` deliberately does NOT exist — see the scope note above.
```

### Acceptance tests

- Migrations from an empty file produce a schema byte-identical to the `schema.sql` snapshot
  (`insta`).
- Every one of the eighteen `WriteOp`s round-trips through a typed read, `SetAutoStart`,
  `SetSource`, `SetSize` and `DropEntryBlob` included.
- `SetStatus` semantics: `msg: Keep` leaves the column, `msg: Clear` nulls it, `msg: Set(x)` writes
  it — three distinguishable outcomes asserted for both `msg` and `error`; `auto_start: None`
  leaves the column and `Some(v)` writes it; and one case per row of the timestamp table,
  including a retry (`error → queued`) asserting `error` is cleared, `finished_at` is nulled and
  `started_at` survives.
- Batching: 500 `InsertItems` submitted concurrently commit in **≤ 8 transactions** (assert the
  count via a `commit_hook`), and no ops are lost or reordered within an id.
- Allocator crash safety: reserve a block, drop the allocator mid-block, reopen, and assert **no
  value is re-issued** and the sequence is still increasing. A `proptest` over interleaved
  reserve/crash schedules.
- Boot check, both counters and the right comparison for each: seed **`meta.ord_hwm`** below
  `COALESCE(MAX(items.ord), -1) + 1` and assert the open **fails** with the
  `aulos-server repair-ids` message; then run `repair-ids` and assert the next open succeeds and
  no `ord` is re-issued. Separately, delete / corrupt `meta.seq_hwm` and assert the open fails.
  A negative test asserts a healthy DB with a large `MAX(items.ord)` and a correctly-ahead
  `meta.ord_hwm` opens fine — the earlier draft of this test compared `seq_hwm` against
  `MAX(items.ord)`, which are different counters, and would have failed every healthy database
  with more than 1024 frames' worth of history.
- `v1_done`: ordering by `(ord, id)`, terminal statuses only, `limit = None` returns everything,
  and a `limit` keeps the **most recent** rows.
- `resolve_v1_token`: a ULID, a url, a media id, an unknown token, and a url matching two rows.
- `PruneSeen` boundaries: keep 0, keep 1, keep exactly N, keep N+1.
- `STRICT` tables reject a wrong-typed write (a negative test proving the guard is live).
- `503` mapping: a deliberately locked DB produces `StoreError::Busy`.

### Definition of done

The store is the only thing in the workspace that touches `rusqlite`, the writer is a single thread
with no `Connection` crossing an `await`, and `print-schema` emits both the DDL and the wire
schemas.

---

## WP-05 — `aulos-store`: legacy importer + `import` / `check-config` CLI

**Crates:** `aulos-store` (module `import`), `aulos-server` (two subcommands)
**Depends on:** WP-04
**Size:** ~1 day

### Scope

DESIGN §7.6 in full, plus wiring `aulos-server import [--dry-run] [--force]` and
`aulos-server check-config`.

- `import::legacy_model`: serde types for `schema_version` 1 and 2 of all five files, the
  `__metube_bytes__` / `__metube_datetime__` wrappers, and the `LegacyV1 → LegacyV2` migration
  table.
- Identity assignment (global chronological `ord`, `Ulid::from_datetime`, `media_id`, duplicate-URL
  resolution), status mapping, `CLEAR_COMPLETED_AFTER` application, subscription and
  `subscription_seen` import, telegram chat import through the selection normaliser.
- **StreamingCommunity entry translation (DESIGN §7.6.3a)**: legacy `_sc_base_url` /
  `_sc_needs_m3u8_extraction` → the v2 `state` object, `title_id`/`episode_id` derived from the
  legacy `id` (`^sc_(\d+)(?:_(\d+))?$`) with a watch-URL fallback and an `sc_ids_unresolved`
  warning when both fail, and every remaining legacy key carried through as `state.legacy`. Without
  this an imported queued SC item cannot be downloaded (the JIT extractor reads `state.base_url`)
  and its NFO would be generated from legacy key names.
- **The three-class failure taxonomy of DESIGN §7.6.1**, unambiguously: a *record* error is always
  a warning and never fails the import; a *file* error obeys `AULOS_IMPORT_ON_ERROR ∈ fail | skip`
  (`--skip-corrupt` sets `skip`) — `fail` rolls back and exits non-zero, `skip` commits without
  that file, downgrades the error to a `file_skipped` warning and marks
  `healthz.components.importer` degraded for the process's life; a *fatal* error always rolls back.
- One transaction; on a fatal error, or a file error under the default policy, rollback,
  **delete the DB file**, and exit non-zero with the report. `meta.imported_*` plus the
  `.aulos-imported` marker on success.
- The report struct, its JSON serialisation, and the INFO table rendering.
- `--dry-run` against `:memory:`.
- Shelf (pickle) detection with the exact actionable message.

### Interfaces

```rust
pub struct ImportReport { pub imported_at: i64, pub state_dir: PathBuf,
                          pub files: Vec<FileReport>, pub warnings: Vec<Warning>,
                          pub errors: Vec<ImportError>, pub seen_ids_imported: u64,
                          pub items: BTreeMap<Status, u64> }
pub async fn import(state_dir: &Path, store: &Store, opts: ImportOpts)
    -> Result<ImportReport, ImportFatal>;
pub struct ImportOpts { pub dry_run: bool, pub force: bool, pub on_error: OnError,
                        pub clear_completed_after_s: u64, pub max_seen_ids: u32 }
pub enum OnError { Fail, Skip }
```

### Acceptance tests

The fixture corpus under `tests/fixtures/state/` with directories `v1`, `v2`, `mixed`, `corrupt`,
`bad-record`, `sc-entry`, `shelf-present`, `empty`, each holding realistically shaped files.

- `v2`: exact `ord` assignment, exact status mapping per DESIGN §7.6.3, `media_id` preserved,
  `chapter_files`/`filename`/`size` preserved on completed rows.
- `v1`: every row of the `__setstate__` migration table.
- `mixed`: a `queue.json` at v2 and a `completed.json` at v1 in the same import.
- `corrupt` under `AULOS_IMPORT_ON_ERROR=fail` (the default): the report has a `file_invalid`
  error, the DB file **does not exist** afterwards, and the exit code is non-zero.
- `corrupt` under `skip` / `--skip-corrupt`: the DB **exists**, every other file's records are
  present, the error appears as a `file_skipped` **warning**, the exit code is 0, and the importer
  health component reports `degraded` naming the file. This is the documented escape from the
  restart loop that a single corrupt legacy JSON file would otherwise cause (DESIGN §19.4).
- `bad-record`: one malformed element inside a valid envelope ⇒ a `record_skipped` warning, every
  other record in that file imported, the DB committed and the exit code 0 **under both policies**.
- `sc-entry`: a queued SC episode and a queued SC movie ⇒ the translated `state` object matches an
  `insta` snapshot, `title_id`/`episode_id` are derived, `state.legacy` carries the leftover keys,
  and a cross-crate test asserts the NFO hook renders identical XML from the imported blob and
  from a freshly resolved one.
- `shelf-present`: the actionable message appears in `errors`.
- Duplicate URL across `queue.json` and `completed.json` ⇒ the terminal record wins and a
  `duplicate_url` warning is recorded.
- **T2 invariant:** after every successful and every failed import, `sha256` of each input file is
  unchanged, and no new file exists in the fixture directory besides `.aulos-imported`.
- Idempotence: a second import against an existing DB is refused unless `--force`.
- `--dry-run` writes nothing (assert via a read-only bind mount in the CLI test) and prints a
  report identical to the real run's.
- `assert_cmd` tests for `import --dry-run`, `import`, `import --skip-corrupt`, and `check-config`
  (exit 0 on a valid env, exit 1 with a table on an invalid one, secrets shown as `«redacted»`).
- Coverage floor: `aulos-store::import` at **90 %** lines, enforced in CI.

### Definition of done

The rehearsal step of the cutover runbook (DESIGN §19.2) can be executed against a copy of the real
`STATE_DIR` and produces a clean report, and the legacy files are provably untouched.

---

## WP-06 — `aulos-provider-ytdlp`: formats, options, outtmpl

**Crates:** `aulos-provider-ytdlp` (modules `formats`, `opts`, `outtmpl`)
**Depends on:** WP-02, WP-03, and WP-00 for `tests/golden/{formats,opts}.json`
**Size:** ~1 day

### Scope

DESIGN §9.8 and Appendix A.6 — a literal, table-driven port of `dl_formats.py`.

- `formats::get_format(download_type, codec, format, quality) -> Result<String, FormatError>`
  reproducing the whole legacy decision table: `custom:` checked **first**, the `ios` selector
  chain, `best_remux → "bestvideo+bestaudio/best"`, the codec filter map, `vres`/`vfmt`/`afmt`
  composition, and the quirk that `quality == "worst"` emits no `worst*` selector.
- `opts::get_opts(...) -> Map<String, Value>` reproducing every branch: the audio postprocessor
  chain with the `writethumbnail` guard and the **string** `preferredquality`; the thumbnail
  branch; the `best_remux` branch's `opts.remove("format")` + `merge_output_format` +
  `FFmpegVideoConvertor` (with the late `Exec` step **omitted** — it is the WP-10 hook); the
  captions branch's per-mode `subtitleslangs` ordering and the `txt → srt` mapping; and the
  prepended/user/late postprocessor ordering.
- `outtmpl`: the `OutTmpl` type, the playlist/channel template swap, `sanitize_path_component`
  application, and the `mode=outtmpl` job construction that delegates evaluation to yt-dlp's own
  `evaluate_outtmpl` (the shim call itself is WP-07; this package defines the job and consumes
  the result).
- The catalog for `ytdlp` per DESIGN §6.6, including labels, `flags` and the `best_remux` and
  `worst` notices.

### Interfaces

```rust
pub fn get_format(dt: DownloadType, codec: Codec, format: &str, quality: &str)
    -> Result<String, FormatError>;
pub fn get_opts(dt: DownloadType, format: &str, quality: &str, user: Map<String, Value>,
                subtitle_language: &SubtitleLang, subtitle_mode: SubtitleMode) -> Map<String, Value>;
pub struct OutTmpl { pub default: String, pub chapter: String }
pub fn build_outtmpl(cfg: &Config, req: &DownloadRequest, hints: &EntryHints) -> OutTmplJob;
pub fn ytdlp_catalog() -> Arc<FormatCatalog>;
```

### Acceptance tests

- `tests/golden/formats.json` — **produced by WP-00** from the legacy Python and checked in with
  provenance — contains every legal `(download_type, codec, format, quality)` tuple mapped to its
  selector string. A single test iterates it and asserts equality. A diff fails CI. A second
  assertion cross-checks the tuple set against the DESIGN §6.6 catalog, so a catalog entry with no
  golden vector is a failure rather than a silent hole.
- `tests/golden/opts.json` — likewise for `get_opts`, one entry per branch, compared as canonical
  JSON.
- Unknown format / unknown download type produce the **exact legacy `ValueError` message text**.
- `custom:` precedence: `format = "custom:bestaudio"` wins over every other rule.
- Option layering interaction: a user `format` is popped for `best_remux`; a user
  `writethumbnail` suppresses the injected one; a user postprocessor list is preserved between the
  prepended and late lists.
- `outtmpl`: the playlist template replaces the default only when `playlist_index` is present;
  an empty `OUTPUT_TEMPLATE_PLAYLIST` keeps the default; Windows-invalid characters are replaced
  in string values only, not in numbers.
- The catalog matches the DESIGN §6.6 table exactly (`insta` snapshot).

### Definition of done

Every selector string this crate can produce is byte-identical to what the Python code produced,
proven by a checked-in golden file rather than by reading.

---

## WP-07 — `aulos-provider-ytdlp`: the Python shim and its Rust client

**Crates:** `aulos-provider-ytdlp` (modules `runner`, `progress`, `errmap`, plus
`python/ytdlp_runner.py`)
**Depends on:** WP-03, WP-06
**Size:** ~1 day

### Scope

DESIGN §9.1–9.7 in full.

- `python/ytdlp_runner.py` (~200 lines, no third-party import beyond `yt_dlp`): one JSON job on
  stdin; the protocol on **fd 3**; stdout redirected to a devnull-backed fd for the whole run;
  stderr left raw; `hello`/`resolved`/`entry`/`progress`/`pp`/`artifact`/`phase`/`info`/`log`/
  `result`/`bye`/`error` frames with the `{"v","t","n","ts"}` envelope; the `coerce` mechanism; the
  four `mode`s (`extract`, `download`, `outtmpl`, `selftest`); the strict-retry rule; the
  per-`stream` progress rate limit; the `policy` block's caption/thumbnail local decisions
  (including the `.srt → .txt` conversion); and the ordered error-classification table.
- `runner.rs`: `RunnerHandle`, spawn with fd 3 via `command-fds`, the `select!` over fd-3 lines,
  stderr, `child.wait()`, cancellation and the two timers; the frame-sequence gap check; the 8 MiB
  line cap; `killpg` with grace.
- `progress.rs`: mapping the `progress` frame onto `RawProgress` including the non-sticky
  `tmpfilename` rule and the `stream`-derived `source_tag`.
- `errmap.rs`: shim `code` → `ProviderError`.
- `--replay <transcript.jsonl>` mode on the Rust reader.
- The `Provider` impl for `ytdlp`: `matches` returns `Weak(1)`, `resolve`, `download`, `probe`
  (which runs `mode=selftest`).

### Interfaces

```rust
pub struct YtdlpProvider { /* … */ }
impl YtdlpProvider { pub fn new(cfg: Arc<Config>, python: PathBuf, runner: PathBuf) -> Self; }
pub struct RunnerHandle { /* … */ }
pub async fn run_job(job: Job, sink: &ProgressSink, cancel: &CancellationToken)
    -> Result<RunnerOutcome, ProviderError>;
pub enum RunnerOutcome { Extracted { entries: Vec<MediaEntry>, truncated: bool },
                         Downloaded(Outcome), OutTmpl(Vec<String>) }
```

### Acceptance tests

- `assert_cmd` against the shim: `mode=selftest` exits 0 and prints `hello` + `result`; a malformed
  job exits 2; a bad `protocol` exits 64; an unknown `coerce` name yields `error{code:"bad_job"}`.
- A real `file://` extract **and** download of a bundled 1-second clip (ffmpeg-generated at test
  time) produces the expected frame sequence and a playable output file.
- **fd-3 isolation:** a job whose options include a postprocessor that prints to stdout, plus a
  stub plugin that prints on import, must not corrupt the stream — the Rust side sees a clean
  transcript. This is the regression test for the whole design decision.
- `--replay` transcripts under `tests/fixtures/transcripts/` covering: a normal download; a
  multi-stream merge (asserting the `source_tag` reset); a 500-entry streamed extract; a `pp`
  sequence with `MoveFiles` + `SplitChapters` + captions; every error code in the §9.6 table; a
  frame-sequence gap (⇒ `contract`); a missing `bye` (⇒ `contract`); an oversized line (⇒ kill).
- Cancellation: a hanging job is killed within `AULOS_KILL_GRACE_MS`; its ffmpeg grandchild dies
  too; `.part` files are cleaned.
- Stderr: a job that writes 1 MiB to stderr completes normally (the drain works) and the last 8 KiB
  is available in the error tail.
- `ruff` and `py_compile` gate the shim in CI.

### Definition of done

A real YouTube download works end-to-end through the shim inside the image, the `--replay` suite
covers every frame type without network or Python, and stdout pollution from a plugin cannot break
the protocol.

---

## WP-08 — `aulos-provider-sc`: HTTP client, scrape pipeline, entries

**Crates:** `aulos-provider-sc` (modules `http`, `inertia`, `watch`, `season`, `embed`, `jit`)
**Depends on:** WP-03
**Size:** ~1 day

### Scope

DESIGN §10.1–10.4.

- `http`: the `ScHttp` trait with a `wreq` implementation behind feature `sc-impersonate` and an
  always-present `reqwest` implementation; `AULOS_SC_HTTP=auto|impersonate|plain` selection; the
  boot WARN and `impersonating` reporting.
- `inertia`: the site-version fetch (`div#app[data-page]` → `.version`), a 30-minute TTL cache with
  single-flight, and the one-shot retry on 403/404/409.
- `watch`, `season`: entry construction with the **bit-for-bit** legacy id/title/state shapes, and
  **season resolution in 2 requests** — zero embed or stream requests during resolution.
- `embed`, `jit`: the embed-iframe hop and the `window.streams` / `masterPlaylist` token/expires
  extraction with query-param preservation and the `h=1` rule; the just-in-time re-extraction used
  at download time, with **no** debug `GET` of the m3u8.
- `matches`: **host and path** (DESIGN §10.2) — hostname *contains* `streamingcommunity` (or an
  `AULOS_SC_EXTRA_HOSTS` entry) **and** the path contains `/watch/`, `/titles/` or `/season-` ⇒
  `Strong(200)`; an SC host with any other path ⇒ `Match::No`, so yt-dlp gets it exactly as legacy
  did (legacy detected by host but dispatched by path and returned `None` otherwise). A scrape that
  resolves nothing returns `ProviderError::Unsupported`, which the engine retries once through the
  runner-up (DESIGN §6.4, WP-12).
- The SC catalog per DESIGN §6.6 (one advisory `mp4`/`best` entry labelled "Source" with the
  explanatory notice) and `NamingPolicy::Provider`.

### Interfaces

```rust
#[async_trait] pub trait ScHttp: Send + Sync {
    async fn get(&self, req: ScReq) -> Result<ScRes, ScError>;
    fn impersonating(&self) -> bool;
}
pub struct ScProvider { http: Arc<dyn ScHttp>, /* … */ }
impl ScProvider { pub fn new(cfg: Arc<Config>) -> Result<Self, ScInitError>; }
pub async fn fresh_stream(http: &dyn ScHttp, base: &Url, watch_url: &Url)
    -> Result<StreamTarget, ScError>;
pub struct StreamTarget { pub m3u8: Url, pub headers: Vec<(String, String)>, pub cookies: String }
pub fn sc_catalog() -> Arc<FormatCatalog>;
```

### Acceptance tests

All against `wiremock` with **captured real HTML/JSON** under `tests/fixtures/sc/`.

- Version extraction from a real `/it` page; a malformed page yields a distinct error code.
- Version drift: a 409 on the Inertia call invalidates the cache, re-runs S1 **once**, and
  succeeds; a second 409 fails with a distinct code.
- Watch page: `/watch/123` and `/watch/123?e=456` produce the exact legacy `media_id` and `title`
  strings for a movie, an episode with a name, and an episode without one.
- **Season in 2 requests:** a 20-episode season fixture is resolved with exactly **two** HTTP
  calls (asserted by the mock's request count) and yields 20 entries with correct
  season/episode numbers and titles.
- Title page: a movie delegates to the watch path; a TV title flattens every season into one
  playlist.
- Stream extraction: `window.streams` with `active:true`, with only one inactive entry, and the
  `masterPlaylist` `url:` fallback; existing query params preserved; `h=1` added **only** when
  `canPlayFHD` is true; `token`/`expires` appended.
- No debug `GET`: the mock asserts the m3u8 URL is never fetched during resolution or JIT.
- `plain` mode passes the same suite (CI runs both feature combinations).
- `ScInitError` with `AULOS_SC_HTTP=impersonate` and the feature off produces a `Degraded`
  registration, not a panic.
- `matches` table: `/watch/123`, `/titles/9-slug`, `/titles/9-slug/season-2` on an SC host ⇒
  `Strong(200)`; `/search?q=x`, `/`, `/browse` on an SC host ⇒ **`Match::No`** (the regression test
  for the legacy path-dispatch fall-through); a non-SC host ⇒ `Match::No`; an
  `AULOS_SC_EXTRA_HOSTS` mirror with a dispatchable path ⇒ `Strong(200)`.
- A scrape that yields no extractable entry returns `ProviderError::Unsupported` (not `Other`),
  which is the variant the engine's runner-up retry keys on.

### Definition of done

Every scrape step is an independently testable function with a distinct error code, the season path
does 2 requests, and the entry shapes are byte-identical to legacy so existing DB rows and NFOs
keep working.

---

## WP-09 — `aulos-provider-sc`: download engines, gapless mux, progress

**Crates:** `aulos-provider-sc` (modules `nm3u8dl`, `ffmpeg`, `mux`, `progress`)
**Depends on:** WP-03, WP-08
**Size:** ~1 day

### Scope

DESIGN §10.5.

- `nm3u8dl`: the byte-identical legacy argv, the ANSI/OSC-stripping last-match-wins progress
  parser, and the automatic ffmpeg retry on a non-zero exit after partial cleanup.
- `ffmpeg`: the CRLF header blob, the `ffprobe` duration probe, the
  `-c copy -bsf:a aac_adtstoasc -progress pipe:1` invocation, and the `out_time_ms`/`total_size`/
  `speed` parsing at most every 0.5 s.
- `mux`: the gapless fallback — collect `.m4s/.ts/.mp4/.m4a/.aac`, sort with a hand-written
  numeric-aware comparator (**not** `natord`), binary-concatenate in 1 MiB chunks, then a
  single-input ffmpeg remux with `+faststart` and a 600 s timeout. `-f concat` must never appear.
- Output naming: `<out_dir>/<sanitised title>.mp4` plus `<…>.info.json`, ignoring
  `OUTPUT_TEMPLATE*` unless `AULOS_SC_USE_OUTPUT_TEMPLATE=true`.
- Partial cleanup on cancel/failure.
- `own_slots() = Some(SC_MAX_CONCURRENT_DOWNLOADS)`.

### Interfaces

```rust
pub(crate) async fn download_nm3u8(ctx: &DownloadCtx<'_>, t: &StreamTarget, sink: &ProgressSink)
    -> Result<Outcome, ProviderError>;
pub(crate) async fn download_ffmpeg(ctx: &DownloadCtx<'_>, t: &StreamTarget, sink: &ProgressSink)
    -> Result<Outcome, ProviderError>;
pub(crate) async fn gapless_mux(seg_dir: &Path, out: &Path) -> Result<u64, ProviderError>;
pub fn natural_cmp(a: &str, b: &str) -> Ordering;
pub(crate) fn parse_nm3u8_frame(chunk: &str) -> Option<RawProgress>;
```

### Acceptance tests

- `parse_nm3u8_frame` against **real captured** Spectre.Console output containing several repaints
  per read: the last match wins, a leading `0/100 0.00%` frame never pins progress at zero,
  segment counts land in `fragment_index`/`fragment_count`, byte fields stay `null` until real
  sizes appear, and `KB/MB/GB` are 1024-based.
- ffmpeg progress parsing from a captured `-progress pipe:1` stream, including the derived
  `total_bytes_estimate` and `eta`.
- `natural_cmp`: a table including `seg2 < seg10`, zero-padded and unpadded mixes, and equal
  prefixes; plus a `proptest` asserting it is a total order.
- `gapless_mux`: synthetic segment files with scrambled mtimes — assert the concatenated bytes are
  exactly the natural-order concatenation, that exactly **one** ffmpeg process is spawned, and that
  the command line contains no `-f concat`.
- The N_m3u8DL-RE argv is asserted string-for-string against the legacy argv.
- Failure path: a stubbed N_m3u8DL-RE exiting non-zero triggers cleanup and the ffmpeg retry, and
  the `msg` sequence matches the legacy strings.
- Cancellation mid-download removes the partial mp4, the segment directory and the temp
  directories.
- Output naming: default keeps `<title>.mp4` + `.info.json`;
  `AULOS_SC_USE_OUTPUT_TEMPLATE=true` uses the template.

### Definition of done

An SC season downloads end-to-end against a local HLS fixture, the gapless mux is byte-exact, and
the progress parser cannot be fooled by repaint frames.

---

## WP-10 — `aulos-provider`: command plugins and the `[[hook]]` manifest

**Crates:** `aulos-provider` (modules `manifest`, `command`, `hookspec`)
**Depends on:** WP-03
**Size:** ~1 day

### Scope

DESIGN §6.5 in full and §13.4 (parsing and validation only; execution of hooks is WP-11).

- `manifest`: the complete `plugin.toml` model of DESIGN §6.5.1 with `serde` + `toml`, the template
  tokenizer with **unknown-token rejection at load time**, and every validation check of §6.5.2
  mapping to `Degraded(reason)` or a clamp+WARN.
- `command`: the `Provider` implementation — discovery, `json_lines`/`json` resolve parsing (with
  the bare-object-as-entry ergonomic), the three `expect_output` modes, `[progress]` parsing
  (regex and json_lines, `strip_ansi`, `cr_as_newline`, `last_match_wins`, `units`, `status_map`,
  `min_interval_ms`), header exposure, and the isolation policy of §6.5.3 (cleared env with
  `env.pass`, rlimits, `nice(5)`, own pgid, no shell, refusal to execute world-writable or setuid
  files).
- `hookspec`: parsing and validating `[[hook]]` tables into a `HookSpec` that WP-11 executes.
  `on` parses into `aulos_core::status::TerminalStatus` (WP-02); `HookFilter` is defined here, in
  this crate, as the three optional allow-lists of DESIGN §13.4.
- `plugins/examples/bandcamp/` — the manifest from DESIGN §6.5.4 plus a ~40-line `resolve.py` and
  `download.py`; and `plugins/examples/media-server-hooks/plugin.toml` from DESIGN §13.4.

### Interfaces

```rust
pub struct PluginManifest { /* the full §6.5.1 schema */ }
pub fn load_manifest(dir: &Path) -> Result<PluginManifest, ManifestError>;
pub fn discover(dir: &Path) -> (Vec<Arc<dyn Provider>>, Vec<HookSpec>, ReloadReport);
pub struct CommandProvider { /* … */ }
pub struct HookSpec { pub id: Arc<str>, pub on: Vec<TerminalStatus>, pub ordering: i16,
                      pub debounce_ms: u64, pub max_wait_ms: u64, pub timeout_ms: u64,
                      pub retries: u8, pub when: HookFilter, pub action: HookAction }
/// DESIGN §13.4 `when.*`. An empty vec means "no filter on this axis" (matches everything).
#[derive(Default)]
pub struct HookFilter { pub provider: Vec<Arc<str>>,
                        pub download_type: Vec<DownloadType>,
                        pub folder_prefix: Vec<Arc<str>> }
impl HookFilter { pub fn matches(&self, item: &Item) -> bool; }
pub enum HookAction { Http { method: Method, url: Template, headers: Vec<(String, Template)>,
                             body: Template },
                      Command { argv: Vec<Template>, cwd: PathBuf } }
pub fn render(t: &Template, ctx: &TemplateCtx) -> Result<String, TemplateError>;
```

### Acceptance tests

- One test per rejection reason in DESIGN §6.5.2, each asserting the resulting `Degraded` message.
- Template renderer: every token in the §6.5.1 table resolves; an unknown token is a **load-time**
  error; `{state.x}` without `capabilities.resolve` is rejected; a title containing spaces, quotes
  and `;` cannot inject an argv element (a `proptest` over hostile titles asserting the argv length
  is invariant).
- The shipped `bandcamp` example runs end-to-end against `wiremock`: a group plus three entries
  from `resolve`, then a download producing a `result` frame and a file.
- `expect_output`: all three modes, each with a success and a failure case.
- Progress: regex mode against captured hostile output (ANSI, `\r` repaints, partial lines);
  `json_lines` mode; `status_map` translating `mux → postprocessing`; `units="auto"` parsing
  1024-based suffixes; `min_interval_ms` throttling.
- A deliberately hostile plugin: infinite stdout (⇒ `max_output_bytes` kills it), `sleep 1d`
  (⇒ the stall timeout fires), a 1 GiB write (⇒ `RLIMIT_FSIZE`), and a world-writable directory
  (⇒ refusal to execute).
- Circuit breaker: five failures in ten minutes ⇒ `Degraded`; the provider still matches and its
  items fail with `provider_degraded`.
- `[[hook]]` parsing: the Plex, Emby, ntfy and command examples all load; `on` outside the closed
  set, an unparseable `http.url` and a `debounce_ms` over an hour are each rejected; `${ENV}`
  interpolates at load time; a hook-only manifest with no `[match]`/`[download]` is valid.

### Definition of done

A third party can add a site with one directory and no recompile, a broken manifest is visible in
`healthz` rather than silently absent, and the `[[hook]]` schema BRIEF §13 mandates is parsed and
validated.

---

## WP-11 — `aulos-hooks`: dispatcher, jellyfin, nfo, audio-sync, community hooks

**Crates:** `aulos-hooks`
**Depends on:** WP-03, WP-10
**Size:** ~1 day

### Scope

DESIGN §13 in full.

- `dispatcher`: the `Hook` trait (with **`phase()`**) and `HookCtx` (DESIGN §13 — the fields are
  `item`, `entry`, `out_dir`, `file`, `sink`, `store`, `cfg`, `batch`, `cancel`, `clock`),
  deterministic `ordering()`, a per-hook inbox with concurrency 2, the rule that a hook failure
  never changes item status, failure counting for `healthz`, and `AULOS_HOOKS_ENABLED`. The
  dispatcher's event source is an `EventInbox` from the `EventRouter` (DESIGN §2.2.1) filtered to
  **`Finishing | Completed`**, capacity 256, `DropPolicy::DropNewest` — **not** an exclusive
  `mpsc::Receiver<DomainEvent>`, which cannot coexist with the aggregator's and Telegram's.
- **The two phases** of DESIGN §13. On `Finishing` the dispatcher runs every applicable
  `HookPhase::PreTerminal` hook in `ordering()` order and then sends
  `EngineCmd::HooksFinished { id, outcome }` — **always**, including when a hook fails, times out
  or panics, because the engine cannot finalise the item until it arrives. On `Completed` it runs
  the `PostTerminal` hooks. A per-hook timeout bounds the pre-terminal phase so a wedged hook
  cannot park an item in `postprocessing` forever.
- `jellyfin`: the trailing debounce with the `max_wait` cap; the global vs targeted request; the
  one-shot fallback on 400/404 with a WARN naming the bad library id; **all four** verbatim legacy
  message shapes (`JELLYFIN_URL is required`, `JELLYFIN_API_KEY is required`,
  `Jellyfin refresh failed with HTTP {code}: {details}`,
  `Jellyfin refresh request failed: {err}`); the precondition behaviour of DESIGN §13.1 —
  `JELLYFIN_SYNC_ENABLED=true` with a blank URL or key logs the corresponding message once at boot,
  marks the component `degraded` with it as `detail`, and makes `applies()` return false so every
  completion is a silent no-op; 3 attempts with 2 s/8 s backoff; the `healthz` component.
- `nfo`: the `quick-xml` port reading `ctx.entry` (the DB blob, never the on-disk sidecar), element
  order and content exactly as the legacy generator, the opt-in `.info.json` deletion, and
  `HookStore::drop_entry_blob` after a successful write. It must render correctly from an
  **imported** SC blob (the `state` shape of DESIGN §10.3, with metadata under `state.legacy`) as
  well as from a freshly resolved one.
- `audio_sync`: **`phase() == PreTerminal`** (DESIGN §13.3 — legacy ran it as a late yt-dlp `Exec`
  postprocessor, i.e. before the item was terminal, and a post-terminal port would have to move a
  `finished` item back to `postprocessing`, which §4.2 forbids); the ffprobe guards, the
  duration-scaled timeout, the exact ffmpeg argv, the atomic rename, the size update **through
  `HookStore::set_size`** (engine-mediated, so it lands *before* the single `completed` frame and
  every client sees the post-re-encode size), `phase`/`phase_percent` from `-progress pipe:1`, and
  the invariant that a failure leaves the item `finished` with the original file. Its `applies()`
  reads the prospective outcome from `ctx.batch[0].status`, since the row is still
  `postprocessing`.
- `manifest_hook`: executing a `HookSpec` — HTTP with placeholder encoding rules, or a command with
  WP-10's isolation; debounce batching with `{count}`, `{titles_json}`, `{filenames_json}`;
  retries.

### Interfaces

```rust
#[async_trait] pub trait Hook: Send + Sync {
    fn id(&self) -> Arc<str>;
    fn ordering(&self) -> i16;
    fn applies(&self, item: &Item) -> bool;
    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError>;
}

/// DESIGN §13. Borrowed for one `run`; a hook holds no state of its own.
pub struct HookCtx<'a> {
    pub item: &'a Item,
    pub entry: Option<&'a EntryBlob>,
    pub out_dir: &'a Path,
    pub file: Option<&'a Path>,
    pub sink: &'a ProgressSink,              // from ProgressSinkFactory (WP-03)
    pub store: &'a dyn HookStore,            // aulos_core::ports (WP-02), impl'd by Store (WP-04)
    pub cfg: &'a Config,
    pub batch: &'a [BatchEntry],             // len 1 unless debounced; drives {count}/{titles_json}
    pub cancel: &'a CancellationToken,
    pub clock: &'a dyn Clock,
}
pub struct BatchEntry { pub id: ItemId, pub title: Arc<str>, pub filename: Option<Arc<str>>,
                        pub status: TerminalStatus, pub error: Option<WireError> }

pub struct HookDispatcher { /* … */ }
impl HookDispatcher {
    pub fn new(cfg: Arc<Config>, specs: Vec<HookSpec>, clock: Arc<dyn Clock>) -> Self;
    pub fn spawn(self, events: EventInbox, sink: ProgressSinkFactory,
                 store: Arc<dyn HookStore>) -> JoinHandle<()>;
    pub fn health(&self) -> HooksHealth;
}
```

### Acceptance tests

- Jellyfin debounce with a `FakeClock`: 20 completions inside 30 s ⇒ **one** request; a completion
  every 25 s for 10 minutes ⇒ a request every 300 s (the `max_wait` cap); the trailing edge fires
  after the last completion.
- Targeted vs global: with `JELLYFIN_LIBRARY_ID` set the URL and query string match DESIGN §13.1
  exactly; a 404 falls back **once** to `/Library/Refresh` and logs the bad id; a 500 retries twice
  and then gives up.
- Error message shapes asserted string-for-string against **all four** legacy formats, including
  the two preconditions; and the precondition path asserted end to end: `JELLYFIN_SYNC_ENABLED=true`
  with a blank `JELLYFIN_URL` produces exactly one boot WARN, a `degraded` component carrying
  `JELLYFIN_URL is required`, and **zero** HTTP requests across 20 completions.
- NFO: `insta` XML snapshots for a movie and for an episode, generated from captured SC entries;
  element order asserted; `.info.json` survives by default and is deleted when the knob is on;
  `entry_json` is dropped after a successful write.
- Audio-sync: against a real 2-second generated mp4 — a successful re-encode replaces the file and
  updates `size`; a missing video stream is skipped; a forced ffmpeg failure leaves the original
  file intact and the item `finished`; the timeout is `max(600, ceil(dur/2))` and `1800` when the
  duration is unknown; `phase_percent` moves during the run.
- Ordering and phases: with all three built-ins applicable, the observed run order is
  audio_sync → nfo → jellyfin, and a community hook at `ordering = 50` lands between nfo and
  jellyfin. audio_sync runs on `Finishing` and the other two on `Completed`, asserted by a fake
  engine that records the command sequence: `Finishing` → audio_sync → `HooksFinished` →
  `Completed` → nfo → jellyfin. `HooksFinished` is still sent when the pre-terminal hook returns
  an error, panics, or exceeds its timeout (three separate cases), because an item that never
  finalises is worse than a failed re-encode.
- Community hooks against `wiremock`: the Plex GET (URL and token), the Emby POST (headers), the
  ntfy POST (body placeholders), and a command hook (argv). Debounced batching produces one call
  with the right `{count}` and `{titles_json}`. A non-2xx retries then gives up, and the item is
  unaffected.
- A hook that panics is caught, counted, and does not take down the dispatcher.
- `HookCtx` plumbing: a fake `HookStore` records exactly the `set_size` / `drop_entry_blob` calls
  the built-ins are documented to make, and the whole suite runs with **no SQLite and no engine at
  all** — the structural proof that `aulos-hooks` needs neither an `aulos-store` nor an
  `aulos-queue` dependency (`tests/arch.rs`, WP-03). The real implementation of the port is
  `aulos-queue::EngineHookStore` (WP-12); the cross-crate test that a `set_size` actually produces
  a `delta` lives there, not here.
- A saturated dispatcher inbox drops the **newest** `Completed` event, increments
  `aulos_event_dropped_total{subscriber="hooks"}`, and does not stall the aggregator.

### Definition of done

NFOs are actually written (which the legacy script never was), a 500-item playlist produces a
handful of Jellyfin scans instead of 500, a failed audio-sync no longer turns a good download into
an error, and BRIEF §13's community hook surface works end-to-end.

---

# Wave 2 — Wiring, binary, end-to-end

## WP-12 — `aulos-queue`: the engine

**Crates:** `aulos-queue` (modules `engine`, `cmd`, `slots`, `priority`, `resolve`, `run`,
`cancel`, `groups`, `recovery`, `clear`)
**Depends on:** WP-03, WP-04
**Size:** ~1 day

### Scope

DESIGN §8.1–8.11, excluding the aggregator and hub (WP-13).

- `EngineCmd` **verbatim from DESIGN §8.1**, which includes `Pause`,
  `CancelResolve { scope: CancelScope }`, `WaitResolved`, `Unwatch`, `ConnClosed`, `HookWrite`,
  `HooksFinished` and the singular `Delete { delete_file }` (one spelling on the wire, in the
  handle signature and in the command — DESIGN §8.10); `AckActions` is the
  `oneshot::Sender<ActionsResult>` type alias, `Duplicate`, `Skipped`/`SkipReason`,
  `ResolveReport`, `HookWrite`, the single-task engine with owned state, and the item cache.
  `DomainEvent` itself is WP-02's; the engine holds an `EventSender` and publishes.
- **The watch registry**: `watchers: HashMap<ConnId, HashSet<GroupId>>` plus a
  `HashMap<GroupId, u32>` refcount, mutated only by `Watch` / `Unwatch` / `ConnClosed`. Nothing
  else in the process holds a connection→groups map, and `ConnClosed` is what the WS task's `Drop`
  guard sends on **every** close path (DESIGN §15.4 step 5a). `watch` affects only which `added`
  pages a connection receives; it never changes what a `delta` contains, because frames are shared
  (DESIGN §8.6).
- `EngineHookStore` (module `hookstore`): the `aulos_core::ports::HookStore` implementation
  WP-11 consumes. `entry_blob` delegates to the store's read pool; `set_size` and
  `drop_entry_blob` become `EngineCmd::HookWrite`s, so the engine writes `SetSize` /
  `DropEntryBlob`, updates its item cache and republishes with `StatusChanged { from == to }` so
  the aggregator emits a `delta` for the changed field — which is the whole point
  (DESIGN §7.1, §13.3): a hook writing SQLite directly would leave `size` stale in the published
  snapshot, in the Aggregator's `last_sent` and on every connected client until a restart.
- **The pre-terminal hook handshake** (DESIGN §13): on `EngineCmd::Finished`, if any applicable
  hook is `PreTerminal`, write `SetStatus { Postprocessing, msg: Set(<label>) }`, publish
  `DomainEvent::Finishing`, and finalise only on `HooksFinished` (or after the dispatcher's
  timeout, logged at WARN). Otherwise finalise immediately. No new status and no new transition
  edge — the engine uses the existing `Downloading → Postprocessing → Finished` path.
- **`WaitResolved`**: a `HashMap<ItemId, Vec<oneshot::Sender<…>>>` of waiters, answered when an id
  leaves `resolving` and answered immediately for ids already out of it. The caller (the v1 shim,
  DESIGN §11.2) owns the deadline, so a slow resolve cannot pin engine state.
- The add path: synchronous validation, folder handling, ULID mint, `ord` allocation,
  `canonical_key`, dedupe, one batched `Sync` write, `ack`, `Added`.
- Resolution: the bounded pool, the deadline, single-entry promotion, **in-place group promotion**
  reusing the id and `ord`, batched child inserts, redirect depth capping, and the
  `playlist_item_limit` double application. Zero entries ⇒ `error` with the verbatim
  `Invalid/empty data was given.`; an unmappable root `_type` ⇒ the verbatim
  `Unsupported resource "<etype>"` (DESIGN §8.4, §11.7). **`ProviderError::Unsupported` with a
  `Ready` runner-up retries the resolve once through that provider** when
  `AULOS_RESOLVE_FALLTHROUGH` is on (DESIGN §6.4) — the legacy StreamingCommunity → yt-dlp
  fall-through — and every other `ProviderError` is terminal. **`pre_error` children are inserted as `queued` with
  `auto_start = false` and a non-null `error`** (DESIGN §8.4) — never as `status = error`, which
  would put an upcoming livestream in the shipped client's Failed section and stop it ever starting.
- Insert status is **always `resolving`**, including when `auto_start = false`; it is the *end* of
  resolution that produces `queued(auto_start=false)` (DESIGN §8.3).
- Scheduling with `Priority` classes, the `own_slots` bypass, and the bounded lookahead.
- Cancellation for every state including groups; idempotence; immediate status persistence.
- **`Pause`** per DESIGN §8.7: `queued(auto_start=true)` is un-scheduled; a running job is killed
  with the same `killpg` sequence as cancel but **keeps** its `*.part`/`*.ytdl` (SC partials are
  still removed, its token is dead) and lands in `queued(auto_start=false)` with `attempt`
  unchanged; `resolving` and terminal items report `NotPausable`.
- **`CancelResolve`** with `CancelScope::All` (what the v1 route sends — legacy's `cancel_add()`
  took no argument, so there is no generation for a v1 caller to supply) and
  `CancelScope::Generation(n)` for a v2 caller that kept the `generation` from its `AddOutcome`.
- The retry policy driven only by `code()`/`retryable()`.
- Group `GroupAcc` with the byte-weighted percent, the status roll-up over the closed enum, and the
  5-minute drift recompute.
- Boot recovery per the DESIGN §8.9 table, plus `AULOS_RESTART_POLICY` and the orphan-temp scan.
- `ClearScheduler` querying SQLite so auto-clear covers items outside the memory window.
- Watchdogs driven off `last_frame_at` with `sleep_until`.

### Interfaces

```rust
pub enum EngineCmd { /* the DESIGN §8.1 variants, verbatim */ }
pub struct EngineHandle { tx: mpsc::Sender<EngineCmd> }
pub enum Action { Start, Pause, Cancel, Retry, Delete }
pub struct Duplicate { pub url: Arc<str>, pub existing_id: ItemId }
pub struct Skipped { pub id: ItemId, pub reason: SkipReason }
pub enum SkipReason { NotFound, AlreadyTerminal, NotCancelable, NotStartable, NotRetryable, NotPausable }
pub enum CancelScope { All, Generation(u64) }
impl EngineHandle {
    pub async fn add(&self, reqs: Vec<DownloadRequest>, src: SourceRef) -> Result<AddOutcome, AddError>;
    pub async fn actions(&self, a: Action, ids: Vec<ItemId>, delete_file: Option<bool>)
        -> ActionsResult;
    pub async fn cancel_resolve(&self, scope: CancelScope) -> ActionsResult;
    /// The v1 shim's bounded pre-resolve (DESIGN §11.2). The CALLER applies the timeout.
    pub async fn wait_resolved(&self, ids: Vec<ItemId>) -> Vec<ResolveReport>;
    pub async fn watch(&self, conn: ConnId, groups: Vec<GroupId>, done: bool) -> Vec<Arc<ItemView>>;
    pub async fn unwatch(&self, conn: ConnId, groups: Vec<GroupId>);
    /// Sent from the WS session's Drop guard on every close path (DESIGN §15.4).
    pub async fn conn_closed(&self, conn: ConnId);
}
pub struct Engine { /* … */ }
impl Engine {
    pub fn new(store: Store, registry: Arc<RwLock<Registry>>, cfg: Arc<Config>,
               ytdl: Arc<ArcSwap<YtdlOptions>>, clock: Arc<dyn Clock>,
               events: EventSender, progress: mpsc::Sender<ProgressMsg>)
        -> (Self, EngineHandle);
    pub async fn recover(&mut self) -> Result<RecoveryReport, EngineError>;
    pub fn spawn(self) -> JoinHandle<()>;
}
pub fn canonical_key(p: &ProviderId, url: &Url, media_id: Option<&str>) -> Box<str>;
/// DESIGN §7.1 / §13.3. Implements `aulos_core::ports::HookStore` by routing both writes through
/// the engine, so a hook's `set_size` is visible in the published snapshot and in the next delta.
pub struct EngineHookStore { /* EngineHandle + Store */ }
```

### Acceptance tests

All integration tests use the `fake` provider and a `FakeClock`; none touch the network.

- Add: a single video; a batch of 50; a duplicate (returns the existing id in `active` mode, `409`
  in `strict`, a new item in `off`); a duplicate with a **different selection** creates a second
  item; a playlist child that duplicates another child is **not** deduped.
- Group promotion: assert the group keeps the anchor's `id` **and** `ord`, that `kind` flips, that
  no `Removed` event is emitted, and that children are inserted in batches of 100.
- 500-item expansion: `≤ 8` store transactions, and the first children are schedulable within
  100 ms of resolution completing.
- Priority: with 497 `Bulk` children queued, a new `Interactive` add starts on the next free slot,
  not last. A `Retry` beats an `Interactive`.
- Slots: `own_slots` providers do not consume a global permit; a saturated SC pool does not block
  yt-dlp items (the lookahead test); the global cap is never exceeded.
- Cancel: from each of `resolving`, `queued`, `preparing`, `downloading`, `postprocessing`, and
  terminal (a no-op); a group cancel cascades; every cancel is idempotent; a `Hang` timeline plus a
  cancel leaves no process and no `.part` file (a `/proc` scan and a directory scan).
- Pause: from `queued(auto_start=true)` (no process was running, nothing removed); from
  `downloading` (no process left, **the `.part` file is still there**, `attempt` unchanged, status
  `queued` with `auto_start=false`); a following `Start` re-runs the job; from `resolving` and from
  terminal ⇒ `NotPausable`; pausing an already-paused item is idempotent; a group pause cascades to
  every pausable child in one transaction and the roll-up becomes `queued`.
- `cancel_resolve(CancelScope::All)` aborts every in-flight resolve and marks not-yet-created
  children cancelled; `Generation(n)` touches only that generation's work and leaves a concurrent
  add running.
- Add with `auto_start=false` inserts `resolving`, **not** `queued`, and only becomes
  `queued(auto_start=false)` when resolution completes.
- A `pre_error` child (an `is_upcoming` entry) is `queued(auto_start=false)` with
  `error.code == not_yet_live` and the legacy scheduled-start text verbatim; it is **not** counted
  as a group error, it is never auto-retried, and `Start` runs it.
- Retry: only `retryable()` codes auto-retry; the backoff curve is `30·2^n ± 20 %`;
  `AULOS_AUTO_RETRY_MAX=0` disables it; `attempt` increments.
- Groups: byte-weighted percent for 49 small children plus one 4 GB child is dominated by bytes,
  not by count; the count-weighted fallback engages when totals are unknown; the status roll-up
  produces only the eight legal values; the drift recompute corrects a deliberately corrupted
  accumulator.
- Boot recovery: seed the DB with every status and assert the DESIGN §8.9 table exactly, including
  group counter recomputation and `clear_after` re-arming, under both `resume` and `pause`.
- Clear: an item whose `clear_after` has passed and which is **not** in the memory window is still
  deleted.
- Watchdog: a drop storm (a deliberately saturated progress channel) does **not** trip the stall
  watchdog, while a genuinely stalled job does.
- `WaitResolved`: an id already out of `resolving` answers immediately; an id still resolving
  answers on the transition, once, with `Ok` for a success and the `WireError` for a failure; a
  caller that drops its receiver does not leak a waiter; two callers waiting on the same id both
  get an answer.
- Runner-up fall-through: a fake provider that matches `Strong(200)` and answers `Unsupported`
  results in **one** retry through the `Weak(1)` runner-up and a successful item, with
  `msg = "Retrying with <id>"`; the same provider answering `AuthRequired` or `Degraded` is
  terminal with no retry; `AULOS_RESOLVE_FALLTHROUGH=false` makes even `Unsupported` terminal; and
  a runner-up that also answers `Unsupported` does not retry a third time.
- Zero-entry and unmappable-`_type` resolutions produce the verbatim legacy strings.
- Pre-terminal hooks: with a fake dispatcher, `Finished` on an item with a `PreTerminal` hook
  writes `postprocessing` and publishes `Finishing` **without** a terminal write; the terminal
  write and the `Completed` event happen only on `HooksFinished`; a dispatcher that never answers
  is finalised by the timeout path with a WARN; and an item with no `PreTerminal` hook finalises in
  one step (no `Finishing` at all).
- `EngineHookStore`: `set_size` produces a `SetSize` write, an updated item cache entry **and** a
  `delta` carrying the new `size` (the regression test for the writeback that used to bypass the
  engine); `drop_entry_blob` produces `DropEntryBlob`; `entry_blob` reads through the store.
- `watch`/`unwatch`/`conn_closed`: watching a group returns its children (and only its
  non-terminal children with `done: false`); a second `watch` from the same conn is idempotent;
  `unwatch` for an unwatched group is a no-op; `conn_closed` drops every group that connection
  held and decrements the refcount, and the last `unwatch` of a group drops its entry entirely.

### Definition of done

Every queue behaviour in DESIGN §8 has a test that runs in milliseconds with no network, and the
engine holds no `Mutex`.

---

## WP-13 — `aulos-queue`: aggregator, event hub, replay ring, published snapshot

**Crates:** `aulos-queue` (modules `aggregator`, `hub`, `ring`, `publish`)
**Depends on:** WP-12
**Size:** ~1 day

### Scope

DESIGN §15.1–15.3, §15.5.

- `aggregator`: the cells, the per-item `Normalizer`, `last_sent`, the dirty set, the 250 ms tick
  and the 25 ms urgent deadline, **the urgency classifier of DESIGN §15.1** (any change to a text
  field — `msg`, `title`, `phase` — is urgent, alongside `Stage`/`File`/`Added`/`Completed`/
  `Removed`/`Notice`; purely numeric changes are batched), the fixed flush order **with `removed`
  grouped by reason** (one frame per distinct `RemoveReason`, in the order `deleted`, `cleared`,
  `auto_cleared`, `group_cascade` — DESIGN §15.1, PROTOCOL §5.7: the aggregator tracks a reason per
  id while the frame carries one reason, so a delete and a `CLEAR_COMPLETED_AFTER` expiry in the
  same window need two frames), the empty-batch-emits-nothing rule, frame splitting with a
  round-robin cursor and tick backoff, and the `last_frame_at`-before-drop rule. `Finishing` is
  **not** in this subscriber's filter — it is a hooks-only event and must produce no frame. Its event source is an `EventInbox` (DESIGN §2.2.1) with
  `DropPolicy::Block`, not an exclusive `mpsc::Receiver<DomainEvent>`.
- **Delta by diff:** the generated `diff(old, new, writer)` over the `ItemView` field list, with a
  compile-time assertion that the diff list and the serialiser list are identical.
- `publish`: `Published`, `StateView`, the done window, `truncated`, and the `by_id` reuse
  optimisation.
- `hub`: `WireFrame`, `EventHub::publish` (serialise once), the broadcast channel, and the
  `resume()` decision function with `boot_id`, `UpToDate`, `Snapshot` (including the
  `since > head` case) and `Merged`.
- `ring`: the dual-representation replay ring bounded by frames **and** bytes, `floor` advancement,
  and `merge_after` with the five merge rules (`removed` folds **per reason**). The ring is shared
  by every client, so a client `ack` **never** trims it: `floor` moves only on the frame and byte
  bounds, and `ack` feeds lag reporting alone (DESIGN §15.3, PROTOCOL §5.11).

### Interfaces

```rust
pub struct EventHub { /* … */ }
impl EventHub {
    /// `seq` comes from `Store::seq_allocator()` (WP-04) — the store keeps its `ord`/`seq` fields
    /// private and exposes exactly these two accessors so the hub can be handed one.
    pub fn new(seq: Arc<dyn HiLoAllocator>, boot_id: BootId, cfg: &Config) -> Self;
    pub fn publish(&self, kind: FrameKind, body: impl Serialize) -> Seq;
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<WireFrame>>;
    pub fn resume(&self, since: Seq, boot: Option<BootId>) -> Resume;
    pub fn head(&self) -> Seq;
    pub fn boot_id(&self) -> BootId;
}
pub enum Resume { Snapshot, UpToDate, Merged { from: Seq, to: Seq, frames: Vec<Arc<WireFrame>> } }
pub struct StateView(Arc<ArcSwap<Published>>);
impl StateView { pub fn load(&self) -> arc_swap::Guard<Arc<Published>>; }
pub struct Aggregator { /* … */ }
impl Aggregator {
    pub fn new(hub: EventHub, cfg: Arc<Config>, clock: Arc<dyn Clock>) -> (Self, StateView);
    pub fn spawn(self, rx: mpsc::Receiver<ProgressMsg>, events: EventInbox,
                 engine: EngineHandle) -> JoinHandle<()>;
}
```

### Acceptance tests

- Diff correctness: mutate one field at a time across the whole `ItemView` and assert the emitted
  delta contains exactly that key plus `id`. A `proptest` over random field mutations asserting
  `apply(last_sent, delta) == new` for every field, which is the structural proof that a stale
  field is impossible.
- Immutable fields: `selection`, `folder` and `request` never appear in any emitted delta
  (asserted over the whole `proptest` corpus, and a `debug_assert` fires if they ever differ).
- Absent vs null: clearing `speed` emits `"speed": null`; an unchanged `speed` emits no key.
- Idle: no progress for 10 ticks emits **zero** frames.
- A stalled download at a constant 43.2 % emits **zero bytes** after the first frame.
- Promptness: a `Stage` mutation is flushed within 25 ms while the periodic tick is not reset;
  numeric progress waits for the 250 ms tick. **A frame that changes only `msg`** — the shim's
  `phase` frame (DESIGN §9.3) and the SC engine's `"Starting N_m3u8DL-RE download..."` /
  `"N_m3u8DL-RE failed, retrying with ffmpeg..."` transitions (§10.5) — is also flushed within
  25 ms, and so is a `title` change on `resolving → queued`; a numeric-only frame arriving in the
  same window does not pull the tick forward.
- Ordering: a flush containing all four kinds emits **`added`, `completed`, `removed`, `delta`** in
  that order — identical to PROTOCOL.md §4.3, §6.3 and §7, which are normative. A flush containing
  removals with two different reasons emits **two** `removed` frames, in the documented reason
  order, both in the `removed` position; a flush with removals of one reason emits one; the REST
  `state?since=` form of the same window is an **array** of `{ids, reason}` groups.
- `ack`: a client acking the head does **not** advance `floor`, and a second client's `?since=`
  below that point still merges — the regression test for a shared ring being trimmed by one
  client's optimisation. Plus the case the
  order exists for: an item **added and removed inside the same 250 ms window** leaves the client
  with no row at all (with `removed` emitted first it would leave a permanent ghost row), and
  `republish` happens after every frame so a REST reader can never see a published snapshot newer
  than the socket.
- Splitting: 1000 dirty items produce 5 frames of 200 with a persistent cursor such that every
  item is emitted within 4 ticks (no starvation); the tick backoff engages after 4 ticks and is
  logged once.
- Serialise-once: 5 subscribers receive `Arc`s that are pointer-equal.
- `resume`: `UpToDate` when `since == head`; `Snapshot` on a `boot_id` mismatch; `Snapshot` when
  `since < floor`; **`Snapshot` when `since > head`** (the post-restore case); `Merged` otherwise.
- `merge_after`: a `proptest` asserting that applying the merged frames to a client state is
  equivalent to applying every original frame in order; plus explicit cases for
  added-then-removed (both dropped) and delta-then-completed (completed wins).
- Ring bounds: a burst of large `added` frames evicts by bytes before the frame cap and advances
  `floor`.
- `Published`: 500 items are served from `StateView::load()` with **zero** store reads (asserted by
  a counting store wrapper); `by_id` is pointer-equal across a tick with no membership change.
- Memory: the DESIGN §15.5 bounds are asserted for the ring and the done window.

### Definition of done

The delta protocol cannot produce a stale client field (proven by the round-trip `proptest`), a
connect serves 500 items without touching the database, and `?since=` is correct across a restart.

---

## WP-14 — `aulos-api`: v2 REST, WebSocket, files, health, auth

**Crates:** `aulos-api` (modules `lib`, `v2/*`, `ws`, `files`, `health`, `metrics`, `error`,
`cors`, `trace`, `auth`)
**Depends on:** WP-12, WP-13. **Not WP-16:** `ApiState.subs` is
`aulos_core::subscription::SubscriptionsHandle` (DESIGN §3, §14.1), a clone of an mpsc sender with
no logic, so this package compiles and its `api/v2/subscriptions*` tests run against a fake
receiver. `aulos-subscriptions` supplies the real one in WP-16 without `aulos-api` ever depending
on it.
**Size:** ~1 day (large; if two engineers are available, split at the REST/WS boundary into
WP-14a *REST + files + health + auth* and WP-14b *WebSocket + `state?since=`*)

### Scope

Everything in PROTOCOL.md §1–§9 except the v1 shim.

- The router built entirely through the `Prefix` newtype; the error envelope as an axum
  `IntoResponse`; `X-Request-Id` and `X-Aulos-Seq` on every response; the CORS layer with legacy
  reflection for v1 and full methods for v2; the trace layer keyed on `ENABLE_ACCESSLOG`.
- `auth`: cookie passthrough (no-op), optional `AULOS_TRUSTED_PROXY_AUTH_HEADER`, optional
  `AULOS_API_TOKEN` bearer with constant-time comparison, and the same token accepted on the WS
  upgrade via subprotocol or `?token=`. **Never a redirect.**
- Every v2 endpoint in PROTOCOL.md §4, including `capabilities` and `catalog?url=` with `ETag`,
  `state?since=` with `ETag`/`304`, paged `items`, `actions`, **`downloads/cancel-resolve`**,
  `subscriptions`, `cookies`, `custom-dirs` (bounded, off-loop, cached),
  `ytdl-options[/reload]`, `import-report`, `providers`, `plugins/reload`, `resolve-preview`,
  `debug/options`, `presets`.
- The `202` add body carries **`generation`** (PROTOCOL §4.1), and
  `POST api/v2/downloads/cancel-resolve` maps `{"generation": n}` → `CancelScope::Generation(n)`
  and `{}` → `CancelScope::All`. Without the field on the wire and the route to consume it,
  `CancelScope::Generation` is unconstructible by any client and a v2-only deployment
  (`AULOS_V1_ENABLED=false`) loses a capability legacy had: aborting a 500-item playlist add.
  `capabilities.features` advertises `cancel_resolve`.
- The cookie routes enforce the legacy **1 000 000-byte** decimal cap with the byte-identical
  message (DESIGN §16.6), and `<p>socket.io/*` answers `501 socketio_removed` from the same error
  envelope as everything else.
- The unknown-request-field `warnings` array, and the absence of `deny_unknown_fields`.
- `ws`: the connection task of DESIGN §15.4 — subscribe-then-snapshot ordering, the optional
  `hello` grace, two tasks with a shared token, keepalive, `Lagged` resync with a lag budget, the
  send timeout, the client cap, the frame-size cap, and the six client frames. The snapshot builder
  also emits the **`ytdl_options` and `health` blocks** (PROTOCOL §5.3) read from the
  `ArcSwap<YtdlOptions>` and `HealthRegistry`, because both frames are transition-only and a client
  connecting into a degraded server would otherwise learn nothing. A `Lagged` resync sends
  `unwatch` for the connection's groups first, and the session's **`Drop` guard sends
  `conn_closed`** so no close path can leak a watch (DESIGN §15.4 step 5a). `ack` is accepted and
  used only for lag reporting.
- `files`: component-wise containment, `Range`/`If-Range`/`ETag`/`Last-Modified`,
  `mime_guess`, the JSON directory listing behind `DOWNLOAD_DIRS_INDEXABLE`, and symlink-escape
  rejection.
- `health`: `healthz` (with `?probe=deep`, rate-limited), `livez`, and the optional `metrics`.
- `GET <p>` returning the small JSON identity document; `robots.txt`.

### Interfaces

```rust
// Every field's type comes from a crate `aulos-api` declares in DESIGN §3:
//   EngineHandle/StateView/EventHub  <- aulos-queue
//   Store                            <- aulos-store   (typed reads only; never `Store::read`,
//                                       which would require naming rusqlite and tests/arch.rs
//                                       forbids that dependency)
//   Registry                         <- aulos-provider
//   Config/YtdlOptions/HealthRegistry<- aulos-core
//   SubscriptionsHandle              <- aulos-core  (DESIGN §3, §14.1 — NOT aulos-subscriptions:
//                                       it is an mpsc sender handle, so putting it in core keeps
//                                       aulos-api off this crate and off WP-16's critical path)
pub struct ApiState { pub engine: EngineHandle, pub state: StateView, pub hub: EventHub,
                      pub store: Store, pub registry: Arc<RwLock<Registry>>,
                      pub cfg: Arc<Config>, pub ytdl: Arc<ArcSwap<YtdlOptions>>,
                      pub health: Arc<HealthRegistry>, pub subs: SubscriptionsHandle,
                      pub conn_ids: Arc<AtomicU64> }   // mints ConnId for `watch`/`unwatch`
pub fn router(state: ApiState) -> axum::Router;
pub fn v2_router(state: ApiState) -> axum::Router;
pub fn ws_router(state: ApiState) -> axum::Router;
pub struct ApiError(pub ErrorCode, pub String, pub Option<&'static str>);
impl axum::response::IntoResponse for ApiError { /* the §1.5 envelope */ }
```

### Acceptance tests

In-process `axum::Router` driven by `reqwest` and `tokio-tungstenite`.

- `insta` snapshot of the response body **and** the header set for every endpoint, success and
  failure. One test per `ErrorCode` asserting the status and the envelope shape.
- `POST downloads`: single, batch with `defaults`, every validation failure with its `field`, an
  unknown field landing in `warnings` (**not** a 400), a duplicate reported in `duplicates`, and
  the batch cap producing 413.
- The item returned by `POST` is present in `GET api/v2/state` with `status: "resolving"` **before**
  the response is read (the async-add guarantee).
- `actions`: every action including `pause`, idempotence, and every `skipped` reason
  (`not_pausable` included). Pausing a running item then `start`ing it resumes rather than
  restarts (`attempt` unchanged), and the frame sequence is `delta(auto_start=false)` →
  `delta(status=preparing)`.
- `state?since=`: delta, `up_to_date`, snapshot on a `boot` mismatch, snapshot on a too-old cursor,
  snapshot when `since > seq`; `ETag`/`If-None-Match` producing 304 with an empty body; `removed`
  serialised as an **array of `{ids, reason}` groups** (`[]` when empty, never `null`), with a case
  covering two reasons in one window.
- `snapshot` (WS and REST) always carries `ytdl_options` and `health`; a test degrades the POT
  component and asserts a **freshly connecting** client sees it in the snapshot without any
  `health` frame having been sent.
- `downloads/cancel-resolve`: `{}` cancels every in-flight resolution; `{"generation": n}` from a
  prior `202` cancels that add only and leaves a concurrent add running; an unknown generation is
  `200` with `canceled: 0`; a malformed body is `400`.
- `capabilities`: the `formats` array matches PROTOCOL.md §4.5 exactly — which is **sixteen**
  entries, with `ios` carrying all nine heights and `captions` carrying all seven legacy formats
  (`srt, txt, vtt, ttml, sbv, scc, dfxp`) — and a second assertion cross-checks it against the
  DESIGN §6.6 catalog table and PROTOCOL.md §8, so the three can never drift apart. `actions`
  contains `start, pause, cancel, retry, delete`. `ETag` yields a 304.
- `catalog`: the full `OptionSpec` / `OptionKind` / `Choice` / `FormatFlags` / `QualitySpec` wire
  shapes of PROTOCOL.md §4.6 are emitted and snapshot-tested, `naming` is only ever `"template"` or
  `"provider"`, and the `ytdlp` video catalog's `options` array is the one documented there (so the
  iOS "richer add options" ask is actually implementable from the document).
- `catalog?url=`: a YouTube URL returns the ytdlp catalog; an SC URL returns the advisory
  single-quality catalog with its notice and names the runner-up.
- WS: the full frame sequence for the three DESIGN §21 sequences; **no lost updates** across
  connect (mutate concurrently with the snapshot build and assert the mutation appears exactly
  once); `Lagged` on a deliberately slow reader produces one resync and the fast reader's frame
  timing is unaffected; the lag budget closes with 1013; the client cap; an oversized client frame
  closes with 1009; `hello` topic narrowing; `watch`/`unwatch` returning a group's children
  (`done: false` returns only the non-terminal ones); a client that never watched a large group
  still receives `delta` patches for its running children and its apply step ignores them (the
  documented PROTOCOL §5.4 exception); closing a socket in each of five ways (normal, 1001, 1009,
  1013, reader error) releases its watches via the session's `Drop` guard.
- **`stress_consistency`**: under a 50-item load, reconstruct client state purely from the frame
  stream and assert equality with the authoritative snapshot every 5 s. Any mismatch fails.
- Files: a `Range` request returns 206 with the right bytes; `If-Range` works; a traversal attempt
  and a symlink escape both 404; the JSON listing appears only when enabled.
- Auth: no token configured ⇒ open; a token configured ⇒ 401 without it, 200 with it, 401 with a
  wrong one; a configured proxy header missing ⇒ 401 with the envelope and **no** `Location`
  header; the WS accepts the token via subprotocol and via `?token=`.
- **The entire suite runs twice**, with `URL_PREFIX=/` and `URL_PREFIX=/metube/`.

### Definition of done

A Swift client written only from PROTOCOL.md works against this API, `stress_consistency` is green,
and the prefixed run passes identically.

---

## WP-15 — `aulos-api`: the v1 compatibility shim

**Crates:** `aulos-api` (module `v1`)
**Depends on:** WP-14, and WP-00 for `tests/v1_golden/`
**Size:** ~1 day

### Scope

DESIGN §11 and PROTOCOL.md §10 in full. Mounted iff `AULOS_V1_ENABLED`.

- Every route in DESIGN §11.1, including the `501` for `socket.io/*`, the `302` for `/`, and the
  `OPTIONS` handlers.
- `POST add`: the legacy-migration table, legacy-matrix-first validation with byte-identical 400
  reason strings, the permissive `auto_start`, the **five other §11.2.1 leniencies** (the singular
  `ytdl_options_preset`, a bare string in `ytdl_options_presets`, `ytdl_options_overrides` as a
  JSON string, an int-ish `playlist_item_limit`, an int-ish `check_interval_minutes`), the
  duplicate-as-`{"status":"ok"}` behaviour, and the always-200 success/business-error envelope.
- **The bounded synchronous pre-resolve of DESIGN §11.2 step 6**: submit `Add`, then
  `EngineHandle::wait_resolved(ids)` under a `AULOS_V1_ADD_RESOLVE_WAIT_MS` (default 10 s) timeout;
  every id resolved ⇒ `{"status":"ok","ids":[…]}`; any id in `error` ⇒ a `status: "error"` body
  whose `msg` is the failing messages joined with a comma and a space (legacy's own joiner); the
  window
  expiring ⇒ `{"status":"ok"}` plus a WARN and
  `aulos_v1_add_resolve_total{outcome="timeout"}`; `0` ⇒ answer immediately. This is what keeps the
  shipped share extension's only failure path alive during the overlap window — `AddResultClassifier`
  decides success purely by parsing this body, so without it a bad URL reads as "queued" and the
  "Couldn't add to Aulos" notification never fires (risk R24). The v2 add route is untouched.
- All strings the shim must emit byte-identically are enumerated in **DESIGN §11.7**, including the
  default `robots.txt` body and the four subscription 400 reasons; each one gets a literal
  assertion here rather than being compared only against the captured corpus.
- The id-resolution ladder and the `where` semantics.
- `GET history`: the three-array projection, group omission, `canceled` omission, `ord` ordering,
  the field-by-field projection table, `entry` omission, and the string-typed numeric config keys.
  **The sources are explicit** (DESIGN §11.4): `queue`/`pending` come from `StateView::load()`,
  which holds every non-terminal record by construction; `done` comes from
  `Store::v1_done(AULOS_V1_HISTORY_MAX)` — the **whole** completed set by default, because v1 has
  no `truncated`, no `done_total` and no cursor, and serving the in-memory 500-item window would
  silently drop thousands of rows out of the shipped client at cutover. A pre-download-problem item
  (an upcoming livestream) projects into `pending[]` with `status: "pending"` and its legacy `error`
  string.
- The subscription routes with the exact 13-key projection and float-second `last_checked`.
- The cookie routes with the legacy messages.
- `GET version` with the two additive keys.
- The `tests/v1_golden/` corpus and its replay harness.

### Interfaces

```rust
pub fn v1_router(state: ApiState) -> axum::Router;

/// The handler: `active` from `state.state.load()`, `done` from `state.store.v1_done(cap)`.
pub async fn history(state: &ApiState) -> Result<V1History, ApiError>;

/// Pure projection, so it is testable without a store. `active` is every non-terminal record;
/// `done` is the terminal page. Groups and `canceled` are filtered here.
pub fn project_history(active: &[Arc<ItemView>], done: &[Arc<ItemView>], cfg: &Config) -> V1History;
pub fn project_item(v: &ItemView, cfg: &Config) -> serde_json::Value;
pub fn migrate_legacy_request(body: &mut serde_json::Map<String, Value>);
```

### Acceptance tests

- **`tests/v1_golden/`**, the corpus **WP-00 captured** (with its `MANIFEST.json` provenance),
  replayed against the shim and compared field by field, with an explicit allow-list of documented
  deltas (`Content-Type`, additive keys including `ids`/`generation`, the omitted `entry`, the
  omitted groups and cancels, `400` instead of `500` on a bad `enabled`, and the pre-resolve
  timeout case). The harness asserts it covered **every** directory in the corpus, so a route the
  capture recorded can never be silently skipped.
- A **JSON-Schema check generated by `aulos-server print-schema`**, run against `/history`,
  `/version` and `/add`, encoding the shipped Swift models' expectations mechanically. In
  particular: `queue`, `pending` and `done` are always present; `status` is only ever one of the
  five legacy strings; `percent` decodes as a number.
- `migrate_legacy_request`: one case per row of the migration table.
- Id resolution: `url ?? id` from the shipped client; a url-only `clearCompleted`; a ULID; a legacy
  media id; a url matching two rows (both are affected); an unknown token (silently skipped).
- `auto_start`: `true`, `false`, `"true"`, `"false"`, `"1"`, `"0"`, `"on"`, `"off"`, and a garbage
  value (400).
- The other leniencies, one case each: `ytdl_options_preset: "sponsorblock"` behaves like
  `ytdl_options_presets: ["sponsorblock"]`; `ytdl_options_presets: "sponsorblock"` likewise;
  `ytdl_options_overrides: "{\"a\":1}"` parses (and `"{"` is a 400 with the legacy string);
  `playlist_item_limit: "5"` and `" 5 "` parse while `"x"` is a 400;
  `check_interval_minutes: "30"` parses on `/subscribe`.
- Pre-resolve: a URL that resolves ⇒ `{"status":"ok"}`; a URL whose resolution fails ⇒
  `{"status":"error","msg":…}` with the provider's cleaned message at HTTP **200**, and the item is
  left in the queue as `error`; two failing URLs in one add ⇒ the two messages joined with `", "`;
  a resolution slower than the window ⇒ `{"status":"ok"}` and the metric's `timeout` outcome;
  `AULOS_V1_ADD_RESOLVE_WAIT_MS=0` ⇒ an immediate `{"status":"ok"}` and no wait at all; and a
  validation failure still answers `400` **before** any waiting.
- Cookie upload: 1 000 000 bytes is accepted, 1 000 001 is rejected with the byte-identical
  `Cookie file too large (max 1MB)` (the decimal-vs-MiB regression), and the response messages
  match §11.7.
- `GET robots.txt` with `ROBOTS_TXT` unset returns exactly the three-line body of DESIGN §11.7.
- `where`: `"queue"` cancels then deletes so the item disappears; `"done"` deletes; a missing or
  invalid `where` is a 400.
- Groups and `canceled` items never appear in `history`; their children do.
- `done[]` completeness: seed 4 211 terminal rows, of which only 500 are in the memory window, and
  assert `GET history` returns **all** of them in `ord` order. With `AULOS_V1_HISTORY_MAX=1000`,
  assert exactly the 1 000 most recent are returned and the cap WARN is logged once.
- An upcoming-livestream item appears in `pending[]` with `status: "pending"` and the legacy
  scheduled-start text in `error` — never in `done[]`.
- `POST cancel-add` with an arbitrary body (and with no body at all) succeeds and cancels
  in-flight resolution; the shim passes `CancelScope::All` and never invents a generation.
- `POST start` on a failed item retries it.
- `socket.io` returns exactly `501` with the documented body, and does **not** hang.
- `AULOS_V1_ENABLED=false` makes every v1 route 404 while v2 is unaffected.
- Coverage floor: `aulos-api::v1` at **90 %** lines, enforced in CI.

### Definition of done

The **currently shipped** iOS build, pointed at the Rust server, can list, add and delete without a
single client change, proven by the golden corpus rather than by inspection.

---

## WP-16 — `aulos-subscriptions` and `aulos-telegram`

**Crates:** `aulos-subscriptions`, `aulos-telegram`
**Depends on:** WP-04, WP-12
**Size:** ~1 day (two independent crates; if two engineers are available, split into WP-16a
*subscriptions* and WP-16b *telegram*)

### Scope

DESIGN §14 and §12 in full.

Subscriptions: `SubscriptionRecord`, `SubscriptionView` **and `SubscriptionsHandle`** come from
`aulos-core` (WP-02) — the view because `DomainEvent::SubscriptionChanged` carries it, the handle
because `aulos-api`'s `ApiState` holds it and must not depend on this crate (DESIGN §3, §14.1); this
package implements the `Manager` that owns the receiving half; both projections (v2's 16 keys and
v1's 13);
the scheduler is an event **producer only** — it holds an `EventSender` and registers no
`EventInbox` (DESIGN §2.2.1) — and it needs `aulos-queue` to send `EngineCmd::Add` (§14.3 step 6),
which is why that dependency is declared in DESIGN §3; the per-subscription task in a `JoinSet` with a persisted `next_due`, jitter, bounded concurrency, a per-check timeout
and exponential backoff to 6 h; the check algorithm's ported parity rules (`is_media_entry`, tab
recursion, the single-video failure, unseen + already-seen-`is_live`, unmarked failures, seen
pruning); backfill suppression on subscribe; the add/update/delete guards and their exact messages;
routing through the provider registry.

Telegram: the actor and its 1 Hz tick; the byte-identical command texts and the **unchanged**
`cfg:` callback grammar over `FormatCatalog::bot_formats()` — the documented nine-entry projection
of the one shared `ytdlp` catalog (DESIGN §12.2), *not* the catalog's 16 ids: `thumbnail` (not
`jpg`), `any` carrying the `audio` pseudo-quality, `mp4` carrying `best_remux`, `ios` showing only
`best`, and captions deliberately unreachable from the keyboard exactly as in legacy; per-chat
config in SQLite; URL extraction, the max-URLs
message and the hardened SSRF guard; the selection normaliser; one batched `Add` with
`SourceRef{kind:"telegram"}`; the live board with the `governor` per-chat and global limiters,
`last_rendered` suppression and `RetryAfter` handling; the five discrete notification messages; the
`Notifier` trait with `AULOS_TELEGRAM_WATCH_ALL`.

### Interfaces

```rust
// aulos-core::subscription (WP-02) — declared there so aulos-api needs no dependency on this crate
pub struct SubscriptionsHandle { /* mpsc::Sender<SubCmd> */ }
impl SubscriptionsHandle {
    pub async fn list(&self) -> Vec<Arc<SubscriptionView>>;
    pub async fn create(&self, req: DownloadRequest, interval: u32) -> Result<Arc<SubscriptionView>, SubError>;
    pub async fn update(&self, id: &SubId, ch: SubChanges) -> Result<Arc<SubscriptionView>, SubError>;
    pub async fn delete(&self, id: &SubId) -> Result<(), SubError>;
    pub async fn check(&self, ids: Option<Vec<SubId>>) -> CheckJob;
    pub fn health(&self) -> SubsHealth;
}
pub struct Scheduler { /* … */ }
impl Scheduler { pub fn spawn(self) -> JoinHandle<()>; }

// aulos-telegram
pub struct TelegramActor { /* … */ }
impl TelegramActor {
    // `store` and `engine` are why `aulos-telegram` declares aulos-store and aulos-queue
    // in DESIGN §3; `events` is an EventInbox from the EventRouter (§2.2.1), not an exclusive
    // `mpsc::Receiver<DomainEvent>` — the aggregator and the hook dispatcher need one too.
    pub fn new(cfg: Arc<TelegramConfig>, store: Store, engine: EngineHandle,
               catalog: Arc<FormatCatalog>, clock: Arc<dyn Clock>) -> Result<Self, TgInitError>;
    pub fn spawn(self, events: EventInbox) -> JoinHandle<()>;
    pub fn health(&self) -> TelegramHealth;
}
#[async_trait] pub trait Notifier: Send + Sync {
    fn id(&self) -> &'static str;
    fn interested(&self, item: &ItemView) -> bool;
    async fn on_event(&self, ev: &DomainEvent);
}
```

### Acceptance tests

Subscriptions, with the fake provider and paused time:

- Backfill suppression marks everything seen without queueing, except `is_upcoming`.
- A subsequent check queues only unseen entries, plus already-seen `is_live` ones.
- Entries that fail validation are **not** marked seen and their first three messages join into
  `error`.
- The single-video URL is rejected with the exact legacy message and **counts as a failure**.
- The backoff curve: 1, 2, 4 … capped at `AULOS_SUB_BACKOFF_MAX_SECS`, with `last_checked` updated
  every time; a success resets it.
- Concurrency: with 10 subscriptions due simultaneously, at most `AULOS_SUB_CHECK_CONCURRENCY` run
  at once, and a slow one does not delay the others past its own permit.
- `next_due` and `consecutive_failures` survive a simulated restart.
- First check happens at ~`AULOS_SUB_FIRST_CHECK_DELAY_SECS` + jitter, and 40 subscriptions spread
  across the jitter window.
- Duplicate URL ⇒ `409` with the exact legacy message; an in-flight duplicate is also rejected.
- `update` accepts only the three legacy fields; a bad `enabled` is a 400.
- Both projections snapshot-tested: v2 has 16 keys, v1 has exactly the legacy 13 with float-second
  `last_checked`.
- The two WS envelopes of PROTOCOL §5.9 are snapshot-tested:
  `{"t":"subscription","seq":…,"subscription":{…16 keys…}}` and
  `{"t":"subscription_removed","seq":…,"ids":[…]}` — an **array**, even for one deletion, where
  legacy emitted a bare id string.

Telegram, against a mocked bot transport:

- Every command and callback text asserted byte-for-byte; the `cfg:` grammar including a quality
  reset when the format changes and an ignored out-of-list quality.
- `bot_formats()` drives the keyboard: the nine ids in legacy order, `any` offering `audio`, `mp4`
  offering `best_remux`, `thumbnail` shown as `thumbnail` while resolving to
  `(thumbnail, jpg, best)`, and `ios` offering only `best`. `cfg:set:quality:audio` on `any`
  stores the legacy pair and normalises to `(audio, m4a, best)`. An imported chat whose stored
  format is `captions` keeps working through `normalize_download_selection` even though the
  keyboard cannot select it.
- The URL extraction and SSRF table: the exact legacy accept/reject set plus the new `0.0.0.0/8`,
  IPv4-mapped-IPv6 and `[::1]` rejections, each with its reason string.
- Over the max-URLs limit produces the exact message and truncates.
- One message with three URLs produces **one** `Add` with three requests and
  `source.kind == "telegram"`.
- Board rendering snapshot-tested for a mix of active, queued, group and terminal lines, including
  the `+N more` overflow.
- Limiter: with a `FakeClock`, edits are at most one per 3 s per chat; an unchanged render issues
  **no** API call; an injected `RetryAfter(7)` sleeps and doubles the interval, and three
  successes halve it back; the global cap holds under 5 chats.
- The five discrete messages fire exactly once each per chat per job.
- `AULOS_TELEGRAM_WATCH_ALL=false` (the default) keeps a web add off the board; `true` reports it.
  A subscription is reported either way, and an `ios`-sourced item only with the knob on
  (DESIGN §12.6).

### Definition of done

A dead feed backs off to 6 h instead of hammering every 60 s, a `check` call returns immediately,
and the bot answers "how far along is it?" without tripping Telegram's rate limits.

---

## WP-17 — `aulos-server`: wiring, POT supervisor, CLI, e2e

**Crates:** `aulos-server`
**Depends on:** WP-05, WP-11, WP-14, WP-15, WP-16
**Size:** ~1 day

### Scope

DESIGN §16 in full plus the e2e harness.

- `main`/`wiring`: the exact startup order of DESIGN §16.1, with steps 5–12 completing **before**
  the listener binds; a `TaskTracker` around every spawned task; `SO_REUSEPORT`; TLS via
  `axum-server` when `HTTPS=true`.
- **`EventRouter` wiring**: construct it, `subscribe()` every consumer with the exact
  `SubscriberSpec` table of DESIGN §2.2.1 (`aggregator` 1024/`Block`, `hooks` 256/`DropNewest`
  filtered to **`Finishing | Completed`**, `telegram` 512/`DropNewest`), hand the `EventSender` to
  the engine, the subscription scheduler and the watchers, and `spawn()` it **after** the last
  `subscribe()`. This is the only place in the workspace that decides who receives what, and it is
  also where the `Finishing` filter is enforced: the aggregator must **not** receive it, because it
  would otherwise frame a status transition that has not been written yet.
- Wire `EngineHookStore` (WP-12) as the `Arc<dyn HookStore>` handed to `HookDispatcher::spawn`, so
  a hook's `set_size` reaches the engine rather than SQLite (DESIGN §13.3).
- `pot`: the supervisor of DESIGN §16.2 — own pgid, stdout/stderr into `tracing`, backoff with
  jitter and a healthy-uptime reset, the 15 s probe with a TCP fallback, the **force-restart after
  three consecutive probe failures**, the `failed` state after `AULOS_POT_MAX_RESTARTS`, and clean
  shutdown.
- `ConfigWatcher` and `PluginWatcher` per DESIGN §17.2 — the directory watch, the filename filter,
  the debounce, the poll fallback, `PollWatcher` selection, and the `SIGHUP` path.
- `signals`: the ten-step graceful shutdown, the `SIGHUP`/`SIGQUIT` handlers, and the panic policy
  (fatal in the engine and store actors, item-failing elsewhere).
- `HealthRegistry` (the type lives in `aulos-core`, WP-02 — `aulos-api` reads it and
  `aulos-server` depends on `aulos-api`, so it cannot live here) aggregating every component, the
  `health` frame on transitions, the `healthz`/`livez` payloads, and the **complete §16.7 metric
  inventory** behind `AULOS_METRICS_ENABLED`.
- `cli`: `check-config`, `doctor`, `print-schema`, `repair-ids` and `healthcheck` fully implemented
  (`import` came in WP-05).
- `tests/e2e/run.sh` gated on `AULOS_E2E=1`.

### Interfaces

```rust
pub async fn run(cfg: Config) -> anyhow::Result<()>;
pub struct PotSupervisor { /* … */ }
impl PotSupervisor {
    pub fn spawn(cfg: Arc<Config>, health: Arc<HealthRegistry>) -> (Self, JoinHandle<()>);
    pub fn state(&self) -> Arc<PotState>;
}
pub struct ConfigWatcher { /* … */ }
impl ConfigWatcher {
    pub fn spawn(targets: Vec<PathBuf>, cfg: Arc<Config>, ytdl: Arc<ArcSwap<YtdlOptions>>,
                 events: mpsc::Sender<DomainEvent>) -> anyhow::Result<JoinHandle<()>>;
}
pub struct HealthRegistry { /* … */ }
impl HealthRegistry {
    pub fn set(&self, component: &'static str, status: ComponentStatus, detail: serde_json::Value);
    pub fn snapshot(&self) -> Arc<HealthView>;
}
```

### Acceptance tests

- Startup order: an integration test asserting the listener does not accept a connection until
  recovery has completed (bind is observably last).
- Event routing: with all three subscribers registered, a single `Completed` reaches the
  aggregator, the hook dispatcher and Telegram; killing the Telegram task does not stall the other
  two; `subscribe()` after `spawn()` is rejected.
- POT supervisor: a fake sidecar that exits immediately is restarted with the expected backoff; one
  that hangs while its probe fails three times is **force-restarted**; ten restarts in ten minutes
  produce `failed` with the server still serving; a cancel of a download does **not** change the
  POT pid (the pgid isolation regression test); shutdown terminates it.
- `ConfigWatcher`: a `vim`-style write-and-rename triggers exactly one reload; five rapid `sed -i`
  edits trigger one; deleting the file keeps the last-good options and degrades the component, and
  re-creating it heals without a restart; with inotify disabled, the poll fallback still reloads;
  `POST ytdl-options/reload` works synchronously.
- Shutdown: with two downloads in flight, `SIGTERM` lets them run up to the grace period, then
  kills the groups, marks the items `queued`, checkpoints the WAL and exits 0 — asserted by a
  restart that resumes them.
- `SIGHUP` reloads options and re-scans plugins.
- `assert_cmd`: `check-config` on a valid and an invalid env; `doctor` with a tool removed from
  `PATH`; `print-schema` matching its snapshot; `repair-ids --dry-run` reporting the two counters
  and changing nothing, then `repair-ids` making a deliberately-behind DB open successfully;
  `healthcheck` exiting 0 against a running server, 1 against a stopped one, and **0 with
  `URL_PREFIX=metube`** (the normalisation regression the raw-`${URL_PREFIX}` healthcheck failed);
  a bare `aulos-server` with no subcommand starting the server.
- Config: an environment exporting `AULOS_E2E=1` (and `AULOS_E2E_ANYTHING`) starts normally
  instead of exiting 2, while `AULOS_WS_BATCH_MSEC` is still fatal.
- `metrics`: with `AULOS_METRICS_ENABLED=true`, every metric name in DESIGN §16.7 is present with
  the documented type and label set, and no metric is exported that is not in the table (both
  directions asserted, so the inventory cannot rot). The rows added for this design are included:
  `aulos_resolve_fallthrough_total{from,to}`, `aulos_v1_add_resolve_total{outcome}`,
  `aulos_hook_writebacks_total{hook,kind}`.
- `healthz` vs `metrics` reconciliation, asserted in **both** directions: every `healthz` path named
  in the DESIGN §16.7 table exists in the payload, and every component in the DESIGN §16.3 payload
  is reachable from the table. That means `components.events.dropped.<subscriber>` and one
  component per built-in hook (`jellyfin`, `nfo`, `audio_sync`) are present — the two the payload
  and the inventory used to disagree about, which WP-14's healthz snapshot test would otherwise
  have contradicted on its first run.
- **e2e (`AULOS_E2E=1`)**: build the image; run it with a temp volume; `healthz` green including
  `pot`; `POST api/v2/downloads` for a small public CC video; assert the WS sequence
  `added → delta → completed`; the file exists in the volume; `GET download/<name>` returns 200 and
  honours `Range`; `POST <p>add` (v1) works and `GET <p>history` has all three keys;
  `<p>socket.io` returns 501; restart the container mid-download and assert it resumes; assert
  `docker logs` contains no `ERROR`. A second profile seeds a legacy `STATE_DIR` and asserts the
  import report has zero errors.
- Load (`tests/load/`): 50 fake items at 50–200 ms with 5 WS clients — frames/s per client
  ≤ `1000/AULOS_WS_BATCH_MS`, zero `Lagged`, and a slow client not perturbing a fast one.

### Definition of done

`docker compose up` produces a server that passes the DESIGN §19.3 functional smoke list, the POT
sidecar is visibly supervised in `healthz`, and the e2e job is green in CI.

---

## Dependency graph

```
wave 0:   WP-00  (independent: runs against the legacy Python image)
                     │ corpora
          WP-01 ──► WP-02 ──► WP-03
                       │          │
wave 1:   ┌────────────┼──────────┴──────────────┬───────────┬───────────┐
          ▼            ▼                         ▼           ▼           ▼
        WP-04       WP-06 ──► WP-07           WP-08 ──► WP-09        WP-10 ──► WP-11
          │
          ▼
        WP-05
          │
wave 2:   └──────────► WP-12 ──► WP-13 ──► WP-14 ──► WP-15
                         │                              │
                         └──► WP-16 ───────────────────┴──► WP-17
```

**WP-00's edges are data, not code:** WP-02 consumes `percent.json`, WP-06 consumes
`formats.json`/`opts.json`, WP-15 consumes `tests/v1_golden/`. Each can start against a
placeholder, but none is *done* until it replays the real corpus, which is why WP-00 is scheduled on
day one and against the still-running Python server.

**There is no WP-16 → WP-14 edge.** `SubscriptionsHandle` lives in `aulos-core::subscription`
(DESIGN §3, §14.1) — it is a clone of an mpsc sender with no logic — so `aulos-api` holds it in
`ApiState` without depending on `aulos-subscriptions`, and `aulos-subscriptions` keeps its
`aulos-queue` dependency for `EngineCmd::Add`. That restores the stated discipline ("wave 1
packages are mutually independent; wave 2 depend on wave 1 and on each other as stated") on the one
edge where the earlier draft's header and this section contradicted each other: WP-14's dependency
set really is just WP-12 and WP-13.

Critical path: **WP-01 → WP-02 → WP-03 → WP-04 → WP-12 → WP-13 → WP-14 → WP-17** (8 packages), with
WP-00 off to the side and off the critical path.
With three engineers, wave 1's seven packages fit in roughly three days of wall clock, so the whole
plan lands in about two working weeks.

## Suggested ownership if three engineers are available

| Engineer | Wave 0 | Wave 1 | Wave 2 |
|---|---|---|---|
| A (core / realtime) | WP-01, WP-02 | WP-04, WP-05 | WP-12, WP-13, WP-14 |
| B (providers) | **WP-00**, WP-03 | WP-06, WP-07, WP-10 | WP-15 |
| C (integrations) | — (reviews WP-02) | WP-08, WP-09, WP-11 | WP-16, WP-17 |

WP-00 goes to engineer B for the same reason WP-15 does: the corpora it captures are exactly the
`dl_formats` and v1-shim semantics B will be held to later, and capturing them first is the cheapest
possible way to learn them.

Engineer B owns the v1 shim because whoever ported `dl_formats` and the shim protocol has the
sharpest picture of legacy field semantics, which is what the golden corpus tests.
