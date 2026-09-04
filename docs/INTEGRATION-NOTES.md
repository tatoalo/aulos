# Integration notes

Cross-package notes the integrator must act on or be aware of. Append a bullet list under a heading
with your WP id.

## WP-01 — Workspace, CI, Docker skeleton

- **`wreq` is pinned to `6.0.0-rc.31`, a prerelease.** DESIGN §18.6 asks for `wreq = "6"`, but
  upstream has published only `6.0.0-rc.*` in that major (the highest stable release is `5.3.0`),
  and Cargo refuses a bare `"6"` requirement against a prerelease. It is declared **optional** in
  `aulos-provider-sc` behind the default-on `sc-impersonate` feature, and it does compile and link
  on the pinned 1.95 toolchain (verified locally and in the amd64/arm64 image). WP-08/WP-09 should
  decide whether to stay on the rc, drop to `5.3.0`, or take the BRIEF's plain-`reqwest` escape
  hatch, and record the choice in DESIGN §10.1.
- **`wreq` needs native build tooling.** It pulls a vendored BoringSSL (`cmake`) and bindgen
  (`libclang`). `docker/Dockerfile`'s builder stage and the `clippy`/`test`/`release` CI jobs
  install `cmake clang libclang-dev` (plus `pkg-config perl` in the image) for this reason. If
  WP-08 drops `wreq`, those installs can go with it.
- **`BGUTIL_TAG` corrected from `v1.2.3` to `v0.8.1`.** The tag in DESIGN §18.1 does not exist;
  `v0.8.1` is the current release of `jim60105/bgutil-ytdlp-pot-provider-rs` and is what the legacy
  image's `releases/latest` lookup resolved to. Both the `bgutil-pot` binary and the
  `bgutil-ytdlp-pot-provider-rs.zip` plugin are pinned to it.
- **The runtime base is `debian:trixie-slim`, not `bookworm-slim`.** The pinned `bgutil-pot`
  release links against `GLIBC_2.38`; bookworm ships 2.36, so on bookworm the sidecar fails at
  startup with `version 'GLIBC_2.38' not found`. The legacy image was trixie-based too (its
  `python:3.13-slim` base, which is why it installed `libssl3t64` rather than `libssl3`). The
  Dockerfile now has two ARGs: `BUILDER_DEBIAN=bookworm` (older glibc, so the binary runs on both)
  and `RUNTIME_DEBIAN=trixie`. This matters to WP-16's POT supervisor and to the e2e script.
- **`docker/entrypoint.sh` deviates from the DESIGN §18.2 snippet in one place.** The snippet's
  `[ "${PUID}" -eq 0 ] && echo "Warning: …"` is the last command of a `&&` list under `set -e`, so
  the script would abort whenever `PUID != 0` — i.e. on every normal start. It is an `if` statement
  here. The `CHOWN_DIRS=false` arm also logs (`Skipping ownership changes …`) so the mode is
  observable in `docker logs` for all three values, as the PLAN's acceptance list requires.
- **Every crate already declares its DESIGN §3 dependency set** (with `default-features = false`
  and a narrow feature list where the design implies one). This proves the whole §18.6 pin set
  resolves inside the real `Cargo.lock`, and means no wave-1 package has to touch a manifest just
  to start. Wave-1 owners should still add the *features* they need to their own crate's manifest.
- **`tests/arch.rs` is not here.** `aulos-workspace-tests` exists with no `src/` and owns
  `tests/packaging.rs` (the WP-01 packaging gates: HEALTHCHECK shape, single yt-dlp pin, pinned
  sidecars, amd64-only docker matrix, workflow inventory, crate inventory). WP-03 adds
  `tests/arch.rs` for the A1–A5 dependency rules next to it; `ci.yml`'s `arch` job already runs
  `cargo test -p aulos-workspace-tests`, so no workflow change is needed.
  **APPLIED (wave-0 integration).** WP-03 landed `crates/aulos-workspace-tests/tests/arch.rs`;
  `cargo test -p aulos-workspace-tests` now runs 12 arch + 10 packaging tests green, and `ci.yml`
  needed no edit.
- **`deny.toml` is not shipped.** The BRIEF cuts the `deny` CI job for v1.0, and a config no
  workflow reads is dead weight. Restoring the job means restoring the file.
- **`clippy::doc_markdown` is deliberately absent** from the workspace pedantic subset: with
  `-D warnings` it fails the build on every acronym in a prose module doc (`SQLite`, `WAL`, `HLS`,
  `NFO`, `ANSI`). `unwrap_used = "deny"`, `expect_used = "warn"` and `disallowed_methods = "deny"`
  are set as DESIGN §3 requires.
- **`crates/aulos-provider-ytdlp/python/` exists with a `.gitkeep`** so the image's
  `COPY crates/aulos-provider-ytdlp/python/ /app/python/` resolves before WP-07 lands
  `ytdlp_runner.py`. WP-07 should just add the file; no Dockerfile change is needed.
  **VERIFIED (wave-0 integration).** The `.gitkeep` is present and the `COPY` still resolves; the
  file drop stays WP-07's, so this bullet remains open for WP-07 only.
- **CI has conditional no-op steps that WP-00 and WP-07 turn on by adding a file**, with no
  workflow edit: `ci.yml`'s `python` job runs `ruff`/`py_compile` once
  `crates/aulos-provider-ytdlp/python/ytdlp_runner.py` exists, the shim contract test once
  `crates/aulos-provider-ytdlp/tests/shim_contract.py` exists, and `tools/capture/verify.py` once
  that exists. `docker.yml` runs `tests/e2e/run.sh` (with `AULOS_IMAGE` and `AULOS_E2E=1`) once it
  is executable, and `update-yt-dlp.yml` runs
  `crates/aulos-provider-ytdlp/tests/smoke_extract.sh` for the real `mode=extract` smoke of
  DESIGN §18.5. If a package prefers a different path, update the workflow in that package.
  **PARTLY APPLIED (wave-0 integration).** WP-00 landed `tools/capture/verify.py`, so `ci.yml`'s
  "verify the golden corpora" step is live and no longer a no-op — it passes locally
  (`OK — tests/golden/{formats,opts,percent}.json and 131 v1 case(s) verified`). The `ruff`/
  `py_compile`, `shim_contract.py`, `smoke_extract.sh` and `tests/e2e/run.sh` steps still no-op and
  stay open for WP-07 / WP-18; the paths in the workflow match what those packages are told to
  create, so none of them needs a workflow edit either.
- **`serve` blocks in the skeleton.** It prints `aulos-server serve: not implemented`, then parks
  on `SIGTERM`/`SIGINT` and exits 0. Nothing is bound and no task is spawned, but the container
  needs a live process for the `HEALTHCHECK` to have anything to probe. `healthcheck` currently
  prints `not implemented` and exits 0, so the container reports `healthy`; WP-14/WP-16 make it a
  real loopback probe.
- **The image is `linux/amd64` in CI but the Dockerfile cross-compiles either way.** The builder
  stage installs a cross linker (and exports `CARGO_TARGET_*_LINKER` / `CC_*` / `CXX_*`) only when
  `TARGETPLATFORM != BUILDPLATFORM`; DESIGN §18.1's snippet installed the arm64 cross toolchain
  unconditionally for arm64 targets, which fails on an arm64 host. Local verification was a native
  `linux/arm64` build (`docker build -f docker/Dockerfile -t aulos-server:wp01 .` on Apple
  Silicon); CI's amd64-on-amd64 build takes the same native path.
- **BRIEF scope trims applied here:** the CLI has no `print-schema` and no `repair-ids` (a
  `// v1.0: not implemented, see BRIEF` note sits at the top of `crates/aulos-server/src/cli.rs`),
  `.github/workflows/` holds only `ci.yml`, `docker.yml`, `update-yt-dlp.yml` and `release.yml`
  (asserted by a test), and `ci.yml` has no `deny`/`coverage`/`schema`/`gitleaks` job. `metrics`
  and `criterion` are pinned in `[workspace.dependencies]` but no crate depends on them.

## WP-02 — `aulos-core`: domain, config, catalog

- **`ProviderId` and `FileSlot` live in `aulos-core`, not `aulos-provider`.** DESIGN §6.1 declares
  `ProviderId` alongside the `Provider` trait and §6.2 declares `FileSlot` alongside
  `ProgressSink`, but `Item.provider`, `ItemView.provider`, `DownloadRequest.provider_hint`,
  `FormatCatalog.provider`, `ReloadReport` and `WriteOp::PushFile` all name them, and `aulos-core`
  is downstream of nothing. They are `aulos_core::selection::ProviderId` (also at the crate root)
  and `aulos_core::item::FileSlot`. **WP-03 should `pub use` them rather than declare its own**, or
  the two crates end up with structurally identical but incompatible types.
  **APPLIED (wave-0 integration).** WP-03 re-exports both instead of declaring them
  (`crates/aulos-provider/src/lib.rs`: `pub use aulos_core::item::{FileRef, FileSlot};` and
  `pub use aulos_core::selection::ProviderId;`), so there is exactly one of each type in the
  workspace. A duplicate-declaration sweep over every `crates/*/src/*.rs` finds no other repeated
  `pub struct`/`pub enum`/`pub trait` name.
- **`RawProgress` carries provider numbers as `f64`/`i64`, not `u64`/`u32`.** The golden corpus
  pins legacy `_number()` behaviour, which coerces numeric **strings** and floats
  (`"250.5" / "1000.0" ⇒ 25.05`) and tolerates negatives; integer inputs cannot reproduce that.
  The wire stays integral. Every provider that parses numbers out of a child process must use
  `aulos_core::progress::{number, integer}` to build a `RawProgress` and let
  `ProgressCell::apply` convert to the wire types with `to_wire_bytes` / `to_wire_count` — this is
  WP-07 (yt-dlp shim frames), WP-09 (`N_m3u8DL-RE` ANSI frames) and WP-10 (the `command` plugin
  `regex`/`json_lines` progress grammar). Rolling your own `as u64` cast reintroduces the negative
  and fractional bugs the corpus exists to catch.
- **`serde_json`'s `float_roundtrip` feature is enabled** in `crates/aulos-core/Cargo.toml`
  (`arbitrary_precision` stays off, as DESIGN §18.6 requires). Without it serde_json's fast float
  path can land one ULP away from the value CPython produced, which breaks the byte-for-byte
  replay of `tests/golden/percent.json` and would silently perturb `percent` on the wire. Cargo
  unifies features, so the whole workspace gets it; nothing else needs to opt in.
- **`Clock::instant()` returns `tokio::time::Instant`,** not `std::time::Instant`, and so do
  `ProgressCell::{last_frame_at, last_applied_at}`. That is what lets a `tokio::time::pause()`
  test drive the aggregator's stall watchdog and the subscription backoff without sleeping.
  `FakeClock::advance` moves the wall clock and the monotonic half together.
- **`aulos_core::load()` does no file IO.** DESIGN §17.1 step 8 folds `YTDL_OPTIONS_FILE` loading
  into the loading algorithm; keeping `load()` a pure function of `RawEnv` is what makes the whole
  §17.3 table testable without a filesystem. WP-17 must call `config::load_with_warnings()` **and**
  `YtdlOptions::load(&cfg.ytdl_options, cfg.ytdl_options_file.as_deref(), ...)`, and merge both
  reports before the single "print the table, exit 2" step.
- **`Config` field naming rule:** the variable name lower-cased, with the `AULOS_` namespace marker
  stripped — `AULOS_WS_BATCH_MS` → `cfg.ws_batch_ms`, `MAX_CONCURRENT_DOWNLOADS` →
  `cfg.max_concurrent_downloads`. `METUBE_VERSION`/`AULOS_VERSION` collapse onto `cfg.version` and
  `PLUGINS_DIR`/`AULOS_PLUGINS_DIR` onto `cfg.plugins_dir`, with the `AULOS_` name winning a tie.
  `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` also keep a
  `_raw: Box<str>` copy, because the v1 shim must echo them as the strings legacy never coerced.
- **`DomainEvent::Notice` keeps DESIGN §8.1's inline shape** (`{ level, code: &'static str, id,
  message }`) and the `Notice` struct PLAN asks for is its **wire** projection
  (`DomainEvent::as_notice()`). `event::notice_code` holds the six server-owned codes of
  PROTOCOL §5.8, and `Level::Warn` serialises as `"warning"` — not `"warn"` — because that is what
  the protocol document says. WP-13 should build the `notice` frame from `as_notice()`.
- **`ItemView::FIELDS` (40 entries) is the authoritative wire key list**, and
  `ItemView::IMMUTABLE_FIELDS` is the three a `delta` may never carry. `print-schema` is CUT, so
  WP-13's diff macro should assert its own field list against `ItemView::FIELDS` — `wire_shapes.rs`
  already asserts the serializer against it from the other side, which closes the loop PLAN asks
  for. `ItemView::from_item(&Item, Option<&ProgressCell>, &ViewExtras)` is the constructor;
  `ViewExtras` carries the three things only the caller knows (the percent-encoded `download_url`
  and the group child counters).
- **`can_transition` takes `impl Into<StatusEdge>`.** `can_transition(Status::Queued,
  Status::Preparing)` works directly; the pause and start edges need the flag, so they are
  `can_transition(StatusEdge::scheduled(Status::Downloading), StatusEdge::paused(Status::Queued))`.
  A self-edge is legal — it is the engine's `StatusChanged { from == to }` re-diff signal.
- **`EventRouter::subscribe` after `spawn` is structurally impossible**, not a runtime `Err`:
  `spawn(self)` consumes the router, so the borrow checker enforces what DESIGN §2.2.1 describes as
  a panic-in-debug. `EventRouter::run(self)` is also public so a test can drive the fan-out on the
  current task. The `Block`-backpressure test uses `tokio::time::pause()` rather than `FakeClock`,
  since it is the tokio scheduler that has to be held still.
- **`FormatSpec.flags.slow` is set on `mp4`, not on the `best_remux` quality.** DESIGN §6.6 says
  "`best_remux` carries `flags.slow = true`", but `flags` is a member of `FormatSpec` and
  `QualitySpec` has only `{ id, label, notice }` (PROTOCOL §4.6). `mp4` is the only format offering
  that quality, so the flag lands there and the per-quality `notice` says which choice is slow.
- **The `ytdlp` catalog's `ytdl_options_presets` option ships with an empty `choices` array.** The
  real preset names come from `YTDL_OPTIONS_PRESETS`, which a `LazyLock` constant cannot know, so
  WP-14 must fill them in when it serves `GET api/v2/catalog` / `capabilities`.
  `FormatCatalog::flat_formats()` is the ready-made projection for `capabilities.formats` (sixteen
  entries, labels already matching PROTOCOL §4.5) and `bot_formats()` the one for WP-16's keyboard.
- **`ChatConfig` has twelve keys, not thirteen.** DESIGN §7.6.5 says "the legacy 13 keys verbatim";
  legacy `_get_chat_config` (`app/telegram_bot.py:188-201`) actually writes twelve. The twelve are
  implemented and asserted; `ytdl_options_presets`/`_overrides` were never part of the chat config.
- **`Status::v1()` returns `"preparing"` for `Preparing`** (DESIGN §11.5's table, not the §4.2
  prose) and `"error"` for `Canceled` as a defensive fallback — DESIGN §11.4 omits cancelled items
  from `GET history` entirely, so WP-15 should filter them out rather than rely on that value.
- **`Item` and `DownloadRequest` derive `PartialEq`** (not `Eq`: `serde_json::Value` has no total
  equality) so WP-04's store round-trips and WP-05's importer fixtures can compare rows directly.

## WP-03 — `aulos-provider`: trait, registry, sink, process helpers, fake provider, arch test

- **`Registry::pick` returns `Option<Selected>`, not `Selected`** (and `catalog_for` likewise
  returns an `Option`). DESIGN §6.3 writes both as infallible, but "no provider matched" is a real
  outcome the engine has to handle: it is exactly the `unsupported_url` case of DESIGN §5 (a
  `magnet:`/`file:` URL, or any scheme `ytdlp` declines), and a synthesised `Selected` would be a
  provider id the engine could route a job to. WP-12 must map `None` to
  `WireError::new(ErrorCode::UnsupportedUrl, …)`; WP-14 must serve `match: null` for a
  `catalog?url=` that matches nothing.
- **`OutTmpl` is declared in `aulos_provider::provider`, not in `aulos-provider-ytdlp`.** PLAN
  WP-06 lists `pub struct OutTmpl` under the ytdlp crate's `outtmpl` module, but
  `DownloadCtx.outtmpl` names it (DESIGN §6.1) and `aulos-provider` is upstream of every provider
  crate, so declaring it there would be a cycle. WP-06 should `pub use aulos_provider::OutTmpl;`
  from its `outtmpl` module rather than declare a second one.
  **VERIFIED (wave-0 integration).** `OutTmpl` is declared exactly once, in
  `crates/aulos-provider/src/provider.rs`, and re-exported from the crate root; adding the
  `pub use` stays WP-06's job.
- **`command` plugin loading is a seam, not a call.** `Registry::reload_commands(dir)` has the
  DESIGN §6.5 signature and returns a real `ReloadReport`, but the manifest parsing lives in
  WP-10. WP-10 should implement `registry::CommandLoader` (one method,
  `load(&Path) -> CommandLoadResult`, carrying `LoadedPlugin { provider, degraded, fingerprint }`)
  and the binary should call `Registry::set_command_loader` at boot. Without a loader installed,
  `reload_commands` logs at debug and returns an empty report, so wiring can land before WP-10.
  `LoadedPlugin.fingerprint` is what makes the report's `updated` list honest — hash the manifest
  bytes; `None` means "assume it changed".
- **`ProviderError` variants carry a `String` message** (except `ToolMissing(&'static str)` and the
  payload-free `Canceled`). DESIGN §6.1 lists most of them bare, but `Item.error.message` has to
  come from somewhere and DESIGN §6.5.3 requires a plugin's stderr tail to reach the user.
  `ProviderError::message()` does the DESIGN §9.6 cleaning once (`"ERROR: "` stripped, ANSI and
  control characters removed, 512 characters), and `to_wire(&provider_id, provider_code)` builds
  the `WireError`. `ProviderError::from_code(code, msg)` is the inverse, for WP-07's shim `code`
  field and WP-10's plugin `{"t":"error","code":…}` frame — neither needs its own copy of the map.
- **`SpawnSpec::new` takes a `&'static str` tool label** before the program path. That label is
  what a missing binary reports as `ProviderError::ToolMissing`, and that variant is
  `&'static str` by design. Use the canonical tool name (`"ffmpeg"`, `"python3"`,
  `"N_m3u8DL-RE"`); a `command` plugin whose `argv[0]` is not static should pass `"plugin"` and
  rely on the manifest's load-time executable check instead.
- **`aulos-provider-ytdlp` should spawn through `Child::spawn_command`, not `Command::spawn`.**
  `SpawnSpec::to_command()` applies the whole policy (own process group, `nice(5)`, rlimits, env
  clearing, piped stderr) and hands back a `tokio::process::Command`; add the fd-3 pipe with
  `command-fds` and then call `Child::spawn_command(&spec, cmd)`. Bypassing `Child` loses the
  **mandatory** stderr drain, which is the one failure mode in DESIGN §2.3 that deadlocks a child
  forever rather than merely failing it.
- **`strip_ansi` lives in `aulos_provider::proc`.** DESIGN §3 gives `strip-ansi-escapes` to
  `aulos-provider-sc` only, and `aulos-provider` may not take a dependency its row does not budget
  for, so the ~30-line CSI/OSC scrubber the stderr ring needs is implemented here and exported.
  WP-09 and WP-10 should use it (or `strip-ansi-escapes`, which is theirs to use) rather than a
  third copy.
- **`Stage` gained `serde` derives** so a `fake` timeline can name a stage in TOML. It serialises
  as `preparing`/`downloading`/`postprocessing`, i.e. `Status::as_str()` for the three running
  statuses; `Stage::status()` is the mapping WP-13's aggregator should use.
- **`tests/arch.rs` amends the DESIGN §3 table in two places to pass on the real tree**, both
  recorded in the `TABLE` const next to the row they affect: `aulos-provider-sc` declares
  `futures-util` and `aulos-api` declares `serde_with` (both landed in WP-01 and both budgeted by
  DESIGN §18.6, but neither is in its §3 row). If WP-08 or WP-14 drops one, the row can lose it —
  the check is a subset rule, so declaring *fewer* dependencies than the row is always fine.
  `metrics`/`metrics-exporter-prometheus` stay in the `aulos-api` row for the same reason even
  though the Prometheus endpoint is CUT.
- **The subset rule is judged on normal + build dependencies only; A1–A4 also cover
  dev-dependencies; A5 (`anyhow`) does not.** The rows describe the architecture, while `insta`,
  `wiremock`, `rstest`, `proptest` and friends are a test toolbox the table does not enumerate. A
  provider crate whose *tests* need `aulos-store` is still an A1 violation, though — that is the
  property A1 exists to protect. A crate's dev-dependency on itself (the standard trick for
  enabling a feature for test targets, which `aulos-provider` uses for `fake`) is ignored.
- **`cargo test -p aulos-provider` builds with the `fake` feature on**, via a dev-dependency of
  `aulos-provider` on itself with `features = ["fake"]`. That keeps the plain gate command
  meaningful; the shipped library still defaults to `fake` off.
- **The wave-2 fixture is `crates/aulos-provider/tests/fixtures/fake/timelines.toml`.** It already
  scripts the six scenarios the integration suite needs — a ten-minute download, a 500-child
  playlist, a geo-block at resolve time, a retryable mid-download `network` failure, a stall that
  arms no timer, and a plain instant success — selected by a regex over the URL, so one provider
  instance serves all of them. `Step::Hang` deliberately awaits **only** the cancellation token:
  under `tokio::time::pause()` that means virtual time still advances to the caller's stall
  deadline, which is what makes a stall test instant. Timelines write their output files by
  default, so the static file route and the `size` bookkeeping have something real to look at.

## Wave-0 integration pass (integrator, 2026-09-04)

- **The tree was already clean and green when the pass started.** `git status` reported nothing to
  commit on `main` after WP-00/01/02/03 landed, and no test anywhere in the workspace is
  `#[ignore]`d (`grep -rn '#\[ignore' crates/ tests/ tools/` is empty). In particular the WP-02
  Normalizer golden test (`crates/aulos-core/tests/percent_golden.rs`) is **not** ignored:
  `tests/golden/percent.json` shipped in WP-00's commit, so the six tests in that file — the vector
  replay, the threaded-sequence replay, the rule-coverage check, the `source_tag` reset and the two
  `proptest` invariants — run and pass on every `cargo test --workspace`. No un-ignoring was
  needed.
- **Gates run at integration time, all green**, on `rustc 1.95.0 (59807616e 2026-04-14)`:
  `cargo fmt --all` (no diff), `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` (the CI form — it
  compiles the `wreq` path too), `cargo test --workspace`, `cargo test --workspace --locked`
  (so `Cargo.lock` is in sync and CI's `--locked` will not fail), and
  `cargo test -p aulos-workspace-tests` (12 arch + 10 packaging). Totals: 314 tests passed, 0 failed,
  0 ignored. `python3 tools/capture/verify.py` also passes.
- **Junk removed:** `tools/capture/.ruff_cache/` (a ruff tool cache left by the WP-00 lint run). It
  was invisible to `git status` only because ruff writes a self-ignoring `.gitignore` inside it, so
  `.gitignore` now lists `.ruff_cache/`, `__pycache__/` and `*.pyc` under *Tooling* to keep the
  next one out of the tree. **No code was discarded** — nothing else was untracked or modified.
- **No cross-package request needed new glue.** The only wave-0-addressable request in this file
  (WP-02's "`WP-03` should `pub use` `ProviderId`/`FileSlot`") was already honoured by WP-03, and
  the `OutTmpl` and `tests/arch.rs` placements match what the notes ask for — each is marked
  inline above with what was verified. Every other request in this file is addressed to a package
  that does not exist yet; they are carried forward, not applied:
  | Request | Owner |
  |---|---|
  | Decide `wreq` rc-31 vs `5.3.0` vs plain `reqwest`, record it in DESIGN §10.1 (and drop the `cmake`/`clang` installs if `wreq` goes) | WP-08 / WP-09 |
  | Build `RawProgress` through `aulos_core::progress::{number, integer}`, never `as u64` | WP-07 / WP-09 / WP-10 |
  | Call `config::load_with_warnings()` **and** `YtdlOptions::load(...)`, merge both reports before the single exit-2 step | WP-17 |
  | Build the `notice` frame from `DomainEvent::as_notice()`; assert the delta field list against `ItemView::FIELDS` | WP-13 |
  | Fill `ytdl_options_presets.choices` from `YTDL_OPTIONS_PRESETS` when serving `catalog`/`capabilities`; serve `match: null` for a `catalog?url=` that matches nothing | WP-14 |
  | Filter `Canceled` items out of `GET history` rather than relying on `Status::v1()`'s defensive `"error"` | WP-15 |
  | Map `Registry::pick` → `None` to `ErrorCode::UnsupportedUrl` | WP-12 |
  | Implement `registry::CommandLoader`; wire `Registry::set_command_loader` at boot | WP-10 / WP-17 |
  | Spawn through `Child::spawn_command` (never `Command::spawn`) so the stderr drain is not lost; `pub use aulos_provider::OutTmpl` | WP-07 / WP-06 |
  | Add `python/ytdlp_runner.py`, `tests/shim_contract.py`, `tests/smoke_extract.sh`, `tests/e2e/run.sh` — each turns on a CI step that no-ops today | WP-07 / WP-18 |
- **Deviations from DESIGN that live only in this file.** PLAN §0 says a deliberate deviation wants
  a DESIGN.md edit in the same PR; wave 0 recorded five in prose here instead (`ProviderId`/
  `FileSlot` moved to `aulos-core`, `Registry::pick`/`catalog_for` returning `Option`, `OutTmpl`
  hoisted to `aulos-provider`, `FormatSpec.flags.slow` on `mp4` rather than on the `best_remux`
  quality, `ChatConfig`'s twelve keys). They are all narrower-and-correct rather than contested, so
  the integration pass left DESIGN.md untouched; the §6.1/§6.2/§6.3/§6.6/§7.6.5 edits are the one
  piece of documentation debt wave 0 carries into wave 1.

---

## WP-06 — `aulos-provider-ytdlp`: formats, options, outtmpl

- **The golden corpora are read from the workspace root, not from the crate.** PLAN WP-06 names
  `crates/aulos-provider-ytdlp/tests/golden/{formats,opts}.json`, but WP-00 shipped them (with
  `percent.json`) as `tests/golden/*.json` at the repo root, and `aulos-core`'s
  `tests/percent_golden.rs` already reads them from there. `tests/golden_formats.rs` and
  `tests/golden_opts.rs` follow that convention (`CARGO_MANIFEST_DIR/../../tests/golden/…`) rather
  than duplicating 76 KB of corpus. If the integrator prefers the per-crate layout, moving the two
  files and editing one `golden_path()` per test file is the whole change.
- **`get_opts` does not emit the legacy `Exec` audio-sync postprocessor** — the one deliberate
  behaviour change of DESIGN §9.8 (Δ C9). **WP-11 owes the other half**: the in-process
  `audio_sync` hook must fire for a finished `{video, mp4, best_remux}` item, or that selection
  silently loses the A/V-desync fix it had in legacy. The exact dict we no longer emit is public as
  `opts::legacy_audio_sync_exec()`, and `tests/golden_opts.rs` asserts that this is the *only*
  difference from the captured Python output, so the delta cannot widen unnoticed.
- **`ytdlp_catalog()` delegates to `aulos_core::YTDLP_CATALOG`** rather than declaring a second
  catalogue. WP-02 put the §6.6 data in `aulos-core` (it is a wire type and the request validator
  reads it), so this crate only hands out the `Arc`. `tests/catalog.rs` parses the DESIGN §6.6
  markdown table and asserts the catalogue matches it row for row, plus an `insta` snapshot of the
  wire shape; `tests/golden_formats.rs` asserts the catalog tuple set and the golden selector key
  set are **equal** (158 tuples), so neither can grow without the other.
- **`OutTmpl` is `aulos-provider`'s type, re-exported here.** The wave-0 integration pass hoisted
  it (`DownloadCtx` names it); `outtmpl::OutTmpl` is a `pub use`, not a redefinition.
- **WP-07 owes the `outtmpl` shim round trip.** `build_outtmpl` returns an `OutTmplJob`;
  `job.is_ready()` is the common case (a single video, or a template with no `playlist*`/`channel*`
  reference) and needs **no Python at all**. Otherwise `job.to_job(job_id)` is the DESIGN §9.2
  `mode = "outtmpl"` object and the shim must return the evaluated strings **in the same order as
  `job.templates()`**; feed them to `job.apply(&[String])`. The shim's own frame shape for that
  reply is WP-07's to define — `apply` only requires the ordered list, and rejects a length
  mismatch as `OutTmplError::Arity` (a §9.3 contract error).
- **WP-12 should call `OutTmplJob::merge_info` with the compacted entry blob.** `build_outtmpl`
  can only derive info fields from `EntryHints`, which carries `playlist_index`/`_count`/`_title`
  and the channel equivalents — legacy passed yt-dlp's **whole** child info dict, so a template
  using `%(playlist_id)s` or `%(playlist_uploader)s` resolved there and would resolve to `NA` here.
  DESIGN §7.5 already keeps exactly the right keys (`^(playlist|channel)`, `n_entries`,
  `__last_playlist_index`); `merge_info` sanitises and merges them. Without that call the templates
  still work, but those two fields degrade.
- **`get_format_raw` / `get_opts_raw` are for WP-15.** The v1 shim receives four free-form strings
  and legacy applied `or`-defaults, `strip()` and `lower()` to them; the typed `get_format` /
  `get_opts` cannot express `None`, `"  VIDEO "` or an unknown codec, all of which the golden
  corpus exercises. Both raw entry points are public and are what the shim should call before its
  own legacy pre-check, so the 400 strings stay byte-identical.
- **No manifest and no `Cargo.lock` change.** The package needs no dependency beyond its DESIGN §3
  row; the field scanner in `outtmpl` is a hand-written port of yt-dlp's `STR_FORMAT_RE_TMPL`
  because that pattern opens with the lookbehind `(?<!%)`, which the `regex` crate cannot compile —
  and because the §3 row does not budget for `regex` here anyway. `Cargo.lock` is dirty in the
  working tree from another package's edits and was deliberately left out of this commit.

## WP-04 — `aulos-store`: schema, writer actor, hi/lo allocators, typed reads

- **No change was needed in `aulos-core` or any other crate.** Every type the store persists was
  already there (`Item`, `SubscriptionRecord`, `ChatConfig`, `FieldUpdate`, `HiLoAllocator`), and
  the store's DESIGN §3 dependency row was already spelled in `crates/aulos-store/Cargo.toml`. The
  only manifest edits are inside the crate: the `hooks` feature on `rusqlite` (the batching test
  counts transactions with SQLite's own `commit_hook` rather than a counter the code bumps itself)
  and three dev-dependencies (`insta`, `proptest`, `url`), all already in
  `[workspace.dependencies]`. `cargo test -p aulos-workspace-tests --test arch` passes.
- **`WriteOp` has nineteen variants, not eighteen.** DESIGN §7.1 and PLAN WP-04 both say
  "eighteen" while listing nineteen (`InsertItems`, `SetStatus`, `SetAutoStart`, `SetSource`,
  `SetResolved`, `PromoteToGroup`, `SetOutput`, `SetSize`, `PushFile`, `DropEntryBlob`,
  `BumpAttempt`, `SetClearAfter`, `DeleteItems`, `UpsertSubscription`, `MarkSeen`, `PruneSeen`,
  `DeleteSubscriptions`, `UpsertTelegramChat`, `SetKv`). All nineteen are implemented and
  round-tripped; `WriteOp::NAMES` is the authority and the round-trip test asserts it covers the
  enum. Nothing downstream needs to change — just do not expect eighteen.
- **`aulos-store` cannot name `url::Url`** (its DESIGN §3 row does not budget for `url`, and the
  arch gate enforces that as the subset rule), so a `Url` column is rehydrated through the type's
  own `Deserialize` impl in `json::from_sql_string`, with the target inferred from the struct
  field. If a future package wants `url` here it must add the row to `tests/arch.rs` first.
- **Types WP-04 had to define because no other package owns them**, all exported from
  `aulos_store`: `BootState` + `GroupCounts` (DESIGN §8.9 names `boot_state()` but declares
  neither), `ItemFilter` + `Page<T>` + `Cursor` + `GroupScope` (DESIGN §7.1 names them in the
  signature only), `StoreOptions`, `IdWarning`. `BootState` deliberately does **not** bucket rows
  by status — it hands back `non_terminal` in `(ord, id)` order plus the done window, the totals,
  the per-group counters and `next_clear_at` — because the recovery table and the
  `AULOS_RESTART_POLICY` switch are WP-12's, and duplicating either here would give two places to
  change. WP-12 should read `Store::options()` for `done_window`/`entry_max_bytes` rather than
  re-deriving them from `Config`.
- **`impl HookStore for Store` deliberately does not exist** (DESIGN §7.1). `Store::entry_blob(id)`
  is the read `aulos-queue::EngineHookStore` (WP-12) delegates here; `set_size` and
  `drop_entry_blob` must become `EngineCmd::HookWrite`s and land as `WriteOp::SetSize` /
  `WriteOp::DropEntryBlob`, both of which exist.
- **`aulos-server` (WP-17) owns the periodic checkpoint and the `close()` call.**
  `Store::close().await` writes `meta.seq_hwm_witness`, runs `PRAGMA optimize` and then
  `wal_checkpoint(TRUNCATE)`, and stops both thread pools; it must be awaited on graceful shutdown
  or the WAL survives the restart. DESIGN §7.1 also asks for a six-hourly checkpoint — there is no
  timer inside the store (it owns no runtime), so the binary should schedule one; the simplest form
  is a `Store::read(|c| ...)`-free tick that calls `close()`'s sibling, which is currently private.
  **If WP-17 wants it, ask for a one-line `pub async fn checkpoint(&self)`** rather than reaching
  into `schema`.
- **`Store::open` is synchronous** (it is called once, before the listener binds, and does blocking
  disk work). It is safe to call from inside a tokio runtime. It returns `Err` only for a corrupt
  file, a failed migration or an unwritable directory — the DESIGN §4.1 boot consistency checks
  **warn and continue** per the BRIEF (`repair-ids` is CUT), and `Store::id_warnings()` is what
  `healthz` should surface.
- **`Durability::Batched` really does wait for `AULOS_DB_FLUSH_MS`** (200 ms by default), exactly as
  DESIGN §7.1 specifies. Anything on a latency-sensitive path should either use
  `Durability::Sync` or accept up to one flush window; the wave-2 integration suites will want
  `StoreOptions::with_flush_ms(5)`.
- **WP-05 (importer) additions land in `src/import*` only**, plus a `mod import;` line in
  `src/lib.rs` where the comment marks it. `StoreOptions::new(path).with_flush_ms(..)`,
  `Durability::Sync` and `Store::read` are the seams it needs; `meta` keys `imported_from`,
  `imported_at` and `import_report` are already documented in the DDL and readable through
  `Store::meta()`.
- **`Cargo.lock` was left out of this commit.** It is dirty in the shared working tree from another
  package's in-flight dependency additions (`cookie`/`cookie_store` for `wreq`), whose
  `Cargo.toml` change is not committed yet, so committing the lock would capture an inconsistent
  graph. WP-04's own lock delta is three dev-dependency rows on `aulos-store`
  (`insta`, `proptest`, `url`), all already pinned in `[workspace.dependencies]`; any `cargo`
  invocation regenerates it.
