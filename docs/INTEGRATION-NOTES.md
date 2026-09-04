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
  **UPDATED (wave-1 integration).** WP-07 landed `python/ytdlp_runner.py` and
  `tests/shim_contract.py`, so the `ruff`/`py_compile` step and the shim contract step are both
  live and pass locally. Only `crates/aulos-provider-ytdlp/tests/smoke_extract.sh` (the
  `update-yt-dlp.yml` real-`mode=extract` smoke) and `tests/e2e/run.sh` still no-op; both need a
  built image, so they stay **open for WP-18**.
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
  **APPLIED (wave-1 integration) — verified, no change needed.** All three consumers build their
  frames through `aulos_core::progress`: `aulos-provider-ytdlp` (`frames.rs`, `progress.rs`,
  `runner.rs`), `aulos-provider-sc` (`progress.rs`) and the `command` plugin grammar
  (`aulos-provider/src/command/progress.rs`). No `as u64` cast on a provider-supplied number
  anywhere.
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
  **APPLIED (wave-1 integration) — verified, no change needed.** `runner.rs` spawns through
  `Child::spawn_command(&spec, cmd)` with the fd-3 pipe added by `command-fds`; there is no
  `Command::spawn` anywhere in the crate.
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
  **DECIDED (wave-1 integration): keep the workspace-root layout.** One corpus in one place, read
  by both `aulos-core` and `aulos-provider-ytdlp`, is the better arrangement; PLAN WP-06's
  per-crate paths are the thing that is wrong, not the tree. No files moved.
- **`get_opts` does not emit the legacy `Exec` audio-sync postprocessor** — the one deliberate
  behaviour change of DESIGN §9.8 (Δ C9). **WP-11 owes the other half**: the in-process
  `audio_sync` hook must fire for a finished `{video, mp4, best_remux}` item, or that selection
  silently loses the A/V-desync fix it had in legacy. The exact dict we no longer emit is public as
  `opts::legacy_audio_sync_exec()`, and `tests/golden_opts.rs` asserts that this is the *only*
  difference from the captured Python output, so the delta cannot widen unnoticed.
  **APPLIED (wave-1 integration).** WP-11 delivered the other half, and the two halves are now
  tied together by a test neither crate could own:
  `crates/aulos-workspace-tests/tests/audio_sync_delta_c9.rs` sweeps **every**
  `(download_type, format, quality)` the `ytdlp` catalog admits and asserts that the legacy `Exec`
  is gone from all of them and that `AudioSyncHook::applies` claims exactly one — `{video, mp4,
  best_remux}` — plus that a failed, cancelled or file-less job is never re-encoded. Before this,
  a disagreement between the two halves would have silently dropped the A/V-desync fix with no
  test going red.
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

## WP-08 — `aulos-provider-sc`: HTTP client, scrape pipeline, entries

- **`wreq` 6.0.0-rc.31 compiles and links on the pinned 1.95 toolchain**, so the BRIEF's
  plain-`reqwest` escape hatch was **not** taken and `sc-impersonate` stays the default. No
  `docs/DESIGN.md §10.1` edit is needed for that. `cargo clippy/test -p aulos-provider-sc` is green
  with the feature on *and* with `--no-default-features` (88 unit + 5 integration tests in both
  combinations). WP-01's `cmake clang libclang-dev` installs in the Dockerfile builder and in CI
  must stay.
- **`wreq-util` is missing from DESIGN §18.6 and that costs the real Chrome fingerprint preset.**
  `wreq` itself ships only `Emulation`/`EmulationBuilder`; the named browser profiles (`Chrome131`
  and friends, i.e. the actual JA3/JA4 + HTTP/2 fingerprint tables) live in the separate
  `wreq-util` crate. Since §18.6 does not list it and WP-08 may not add an unlisted workspace
  dependency, `http::impersonate` hand-builds the profile from the `curl-impersonate` `chrome`
  target's cipher list, curve list and sigalg list plus Chrome's four HTTP/2 SETTINGS values. That
  is BoringSSL with a Chrome-shaped ClientHello — much closer than rustls, but **not** a
  byte-exact JA3/JA4 match, and the HTTP/2 pseudo-header and SETTINGS *order* are `wreq`'s
  defaults rather than Chrome's. **Recommended integrator action:** add
  `wreq-util = "3"` to `[workspace.dependencies]` and to the `aulos-provider-sc` row of DESIGN §3 /
  the `arch.rs` budget, then replace `chrome_emulation()`'s body with the crate's Chrome preset —
  it is a ~10-line change in one function, and `the_chrome_profile_is_accepted_by_boringssl`
  already guards it.
  **APPLIED (wave-1 integration).** `wreq-util = "3.0.0-rc.14"` is in `[workspace.dependencies]`
  (a bare `"3"` cannot match a prerelease, exactly as with `wreq`), declared `optional` in
  `aulos-provider-sc` behind `sc-impersonate`, and added to the `aulos-provider-sc` row of both
  DESIGN §3 and `tests/arch.rs`. `http::impersonate` now sends the real Chrome 131 profile
  (`wreq_util::Emulation::Chrome131` through `wreq::IntoEmulation`), with this crate's own header
  set layered on top so the impersonating and the plain client present one identical header map;
  the hand-built cipher/curve/sigalg strings and the four HTTP/2 SETTINGS constants are gone.
  DESIGN §10.1 and §18.6 record the crate and both rc pins. A second guard,
  `the_fingerprint_and_the_user_agent_claim_the_same_chrome`, fails if the profile version and
  `USER_AGENT`/`sec-ch-ua` ever drift apart — a JA3/JA4 from one Chrome behind a user agent from
  another is a worse signal to a bot filter than none.
  `ClientBuilder::build()` constructs the BoringSSL connector eagerly, so `WreqClient::new()`
  catches a rejected cipher/curve string at boot and falls back to `wreq`'s default TLS options
  with a WARN and `impersonating() == false` — a bad profile can never become a silent
  per-download failure.
- **`ScHttp` has a third method, `cookie_header()`, with a default implementation.** DESIGN §10.1
  and PLAN WP-08 list only `get` and `impersonating`, but `jit::fresh_stream` has to hand
  `N_m3u8DL-RE`/ffmpeg the session jar as one `Cookie:` header (legacy read it off `curl_cffi`'s
  session, `streamingcommunity.py:442`). The default returns `""`, so the design signature is
  still all an implementor must provide.
- **CI does not exercise the `plain` client.** PLAN WP-08 requires "CI runs both feature
  combinations" and `ci.yml` currently runs default features only (plus `--all-features` for
  clippy), so the `reqwest` path — the one that will actually run wherever BoringSSL is
  unavailable — is untested there. WP-08 does not own `.github/`; the fix is one line in the
  `test` job of `.github/workflows/ci.yml`:
  `- run: cargo test -p aulos-provider-sc --no-default-features --locked`.
  **APPLIED (wave-1 integration).** That line is now in `ci.yml`'s `test` job, immediately after
  `cargo test --workspace --locked`. It passes locally.
- **`crates/aulos-provider-sc/src/engines.rs` is the seam WP-09 must fill.**
  `ScProvider::download` delegates to `engines::download(&self, ctx, sink)`, which currently logs
  an ERROR and returns `ProviderError::ToolMissing("N_m3u8DL-RE")` — deliberately a loud failure
  rather than `Ok(Outcome::default())`, so an SC item can never be marked `finished` with nothing
  on disk. WP-09 replaces that function and adds `nm3u8dl`, `ffmpeg`, `mux` and `progress`
  alongside it. Everything WP-09 needs is already public: `jit::fresh_stream` →
  `StreamTarget { m3u8, headers, cookies }`, `ScProvider::http()`, `ScProvider::base_of()`,
  `ScState::from_json` (the persisted `base_url`/`title_id`/`episode_id`) and
  `ScState::to_legacy_info_json` (the **legacy flat** sidecar of DESIGN §10.3 note 2, key order
  matching the Python dict literal). `own_slots()` already returns
  `SC_MAX_CONCURRENT_DOWNLOADS`.
- **`indexmap` was wanted and dropped.** The DESIGN §3 row for this crate does not budget it and
  `tests/arch.rs` enforces the row, so the two insertion-ordered maps (the cookie jar and the
  vixcloud query string) are `Vec<(String, String)>` with a linear scan. Both hold a handful of
  entries; no behaviour differs. Nothing to do — recorded only so the next person does not
  re-add it.
- **`reqwest`'s `cookies` feature is now enabled for this crate** (member-level, not workspace),
  which is what pulls `cookie`, `cookie_store`, `publicsuffix` and `psl-types` into `Cargo.lock`.
  The session must send back the `cf_clearance`/`sid` cookies the site sets, exactly as
  `curl_cffi`'s `Session` did.
- **Two deliberate behaviour differences from DESIGN, both narrower than they look.**
  1. A single `/watch/` URL **still performs the S3/S4 embed and stream hops** during resolution.
     DESIGN §10.4 removes them only from *season* resolution, and legacy used them as the validity
     probe that decided between "queue this" and "hand the URL back to yt-dlp"; keeping them is
     what makes an unplayable watch page fail with `Unsupported` (→ the §6.4 runner-up retry)
     instead of queueing an item that can never download. Cost: 2 extra requests per single add.
     Season and title resolution do **zero** embed/stream requests, as designed, and
     `a_twenty_episode_season_resolves_in_exactly_two_requests` asserts the count.
  2. `resolve()` for a flattened multi-season title **re-stamps `playlist_index`/`playlist_count`
     as one global `1..=N` run** rather than restarting per season, so a group's child counters and
     the output template see one sequence. Legacy had no such field.
- **The SC catalog is one advisory `mp4`/`best` entry labelled "Source"** with
  `naming: "provider"`, `flags.advisory = true`, `flags.requires_ffmpeg = true` and an explanatory
  `notice` on both the format and the quality (DESIGN §6.6, PLAN WP-08). Its `options` array is
  **empty**: `NamingPolicy::Provider` means this provider names and places the file, so
  advertising `folder`/`custom_name_prefix`/`chapter_template` would be a lie. WP-14 should note
  that a v1 client posting `{video, any, 1080}` for an SC URL passes the legacy pre-check and then
  finds no such format in this catalog — the v1 shim needs to map any legacy video selection onto
  `mp4`/`best` for a provider whose catalog is advisory, or catalog validation will reject a
  request legacy accepted.
- **Scrape failures carry a stable `provider_code`.** `ScErrorCode` (`sc_version_unreadable`,
  `sc_version_rejected`, `sc_no_embed_url`, `sc_no_iframe`, `sc_no_stream`, `sc_nothing_resolved`,
  …) is what tells an operator "the site rotated its Inertia version" from "Cloudflare blocked us"
  from "the vixcloud page changed shape". WP-12/WP-14 should pass it as
  `ProviderError::to_wire(&id, Some(code.as_str()))`; `ScError::code()` is public for that.
- **`ScState` is the §7.6.3a target shape.** WP-05's importer should build its translated blob with
  `ScState { base_url, title_id, episode_id, needs_m3u8_extraction, season_number, episode_number,
  episode, series, ext, extractor, extractor_key, legacy }` and `ScState::to_json()`; `legacy` is
  `#[serde(default, skip_serializing_if = "Map::is_empty")]`, and `from_json` tolerates a blob
  missing every optional field, so a row whose ids could not be derived still round-trips.

## WP-09 — `aulos-provider-sc`: download engines, gapless mux, progress parsing

- **No change is needed in any crate WP-09 does not own.** `aulos-provider`'s `proc::Child`,
  `SpawnSpec`, `ProgressSink`, `Outcome` and `ProviderError` covered the whole engine surface as
  designed; `aulos-core`'s `RawProgress`/`Normalizer`/`RelPath` covered progress and naming. The
  only edits outside the four new engine modules are additive and inside this crate:
  `ScProvider` gained an `engine: EngineCfg` field, `ScProvider::engine()` and
  `ScProvider::with_engine()`, and `src/testing.rs` gained the engine fixtures.
- **The PLAN WP-09 interfaces each take one more argument than the sketch**, all of them the same
  thing — the binaries and knobs to run with, so the suite can drive the engines with stand-in
  scripts and no `N_m3u8DL-RE` installed:
  `download_nm3u8(cfg, ctx, target, names, tmp, sink)`,
  `download_ffmpeg(cfg, ctx, target, names, tmp, sink)`,
  `gapless_mux(ffmpeg_bin, seg_dir, out)`. `natural_cmp(a, b)` and
  `parse_nm3u8_frame(chunk)` are exactly as specified. `EngineCfg::from_config(&Config)` is what
  the binary should use; the three binary names default to `N_m3u8DL-RE`, `ffmpeg`, `ffprobe` and
  are not configurable by env var (nothing in DESIGN §17.3 makes them so).
- **`Outcome.entry_final` carries the legacy flat `.info.json` object** for every successful SC
  download, so WP-11's NFO hook can generate its XML without re-reading the sidecar from disk.
- **The SC engines never look at `OUTPUT_TEMPLATE`** unless `AULOS_SC_USE_OUTPUT_TEMPLATE=true`.
  That opt-in path resolves `%(field)s` / `%(field)02d` over the entry's own fields with a small
  in-crate renderer — this crate never runs yt-dlp, so there is no template engine to delegate to,
  and an unknown field renders `NA` the way yt-dlp renders one. Anyone who needs the full grammar
  should route the item through `ytdlp` instead.
- **Partial cleanup now also runs after a *final* failure**, not only between the two engines.
  Legacy left the truncated `.mp4` on disk, where a library scanner saw a finished-looking file for
  an item in `error`. Cancel behaviour is unchanged (kill the process group, then clean up).
- **`_merged.ts` is excluded from the segment scan.** Legacy would have re-concatenated its own
  previous output if the fallback mux ran twice in one segment directory.
- **Two of the four gates were run for this crate only.** `cargo clippy -p aulos-provider-sc
  --all-targets -- -D warnings` and `cargo test -p aulos-provider-sc` are green (160 unit tests in the crate, plus WP-08's 5 integration tests);
  the workspace-wide gates were not run because other crates were mid-edit.
  **CLOSED (wave-1 integration).** The workspace-wide gates were run over the whole tree in this
  pass; the results are in the wave-1 section at the end of this file.
- **`mux::tests::a_real_ffmpeg_remuxes_the_concatenation` self-skips** when no `ffmpeg` is on
  `PATH` or in `/opt/homebrew/bin`, printing why. It generates its own two MPEG-TS segments with
  ffmpeg rather than checking a binary blob into the repository, so CI (which has no ffmpeg) just
  skips it. Every other engine test uses the checked-in `sh` stand-ins in
  `crates/aulos-provider-sc/tests/fixtures/sc/bin/` — **those files must keep their executable
  bit**; a `git checkout` that drops mode 100755 turns the whole engine suite into `ToolMissing`.

## WP-10 — `aulos-provider`: command plugins and the `[[hook]]` manifest

- **The whole package lives in `crates/aulos-provider/src/command/`**, as one module with six
  files (`manifest`, `template`, `progress`, `provider`, `hookspec`, `sha256`) rather than the
  three top-level modules PLAN names. `aulos_provider::command::*` re-exports everything, and the
  crate root re-exports the names other packages need (`CommandProvider`, `CommandPluginLoader`,
  `PluginManifest`, `ManifestError`, `HookSpec`, `HookFilter`, `HookAction`, `Template`,
  `TemplateCtx`, `Token`, `TokenScope`, `discover`, `load_manifest`). Only `crates/aulos-provider`
  and `plugins/examples/` were touched.
- **`HookAction::Http.method` is a local `HttpMethod` enum, not `http::Method`.** DESIGN §13.4
  writes `Method`, but DESIGN §3's dependency row for `aulos-provider` budgets for no HTTP crate
  and `tests/arch.rs` enforces that row as a subset rule. `HttpMethod` is
  `{Get, Post, Put, Patch, Delete, Head}` with `as_str()`/`parse()`; **WP-11 maps it to
  `reqwest::Method` in one line** (`Method::from_bytes(m.as_str().as_bytes())` or a six-arm match).
  **APPLIED (wave-1 integration) — verified, no change needed.** WP-11 did exactly that:
  `manifest_hook::reqwest_method` is the six-arm match.
- **WP-11 gets its hooks from one of two places.** `aulos_provider::command::discover(dir)` has the
  PLAN signature and returns `(Vec<Arc<dyn Provider>>, Vec<HookSpec>, ReloadReport)`; the registry
  path is `Registry::set_command_loader(Arc::new(CommandPluginLoader::with_env(env)))` plus
  `Registry::reload_commands(dir)`, and after every such reload
  `CommandPluginLoader::hooks() -> Vec<Arc<HookSpec>>` holds that scan's community hooks (and
  `::warnings()` its clamps). Use the loader path in `aulos-server`: it is the one that carries the
  `Degraded` **state** into the registry, which `discover`'s flat `Vec<Arc<dyn Provider>>` cannot.
  `HookSpec` is deliberately not `Clone` (it owns parsed templates), hence the `Arc`.
- **`HookSpec.id` is `hook:<dir>/<id>`**, `on` is `Vec<TerminalStatus>`, and `HookSpec::applies(&Item)`
  already combines `on` with `HookFilter::matches`. `HookFilter` treats an empty axis as "matches
  everything" and treats an item with **no provider yet** as *failing* a `when.provider` filter.
- **`limits.file_size_bytes` is a key WP-10 added.** DESIGN §6.5.3 requires `RLIMIT_FSIZE` to be
  applied but §6.5.1's table has no key for it, so there was nothing to apply. Default `0` (off);
  the rest of `[limits]` is exactly the design table.
- **`{cookies_file}` needs a `PluginEnv`.** `DownloadCtx` (DESIGN §6.1) carries no `Paths`, so
  `STATE_DIR/cookies.txt` cannot be derived at download time. `CommandProvider::new(manifest, env)`
  takes `PluginEnv { state_dir }` and `scan_with`/`CommandPluginLoader::with_env` thread it through;
  **WP-17 must pass `PluginEnv { state_dir: cfg.paths.state.clone() }`** or `{cookies_file}` renders
  empty. `PluginEnv::default()` is the empty-state-dir case, which is what `discover(dir)` uses.
- **Three additive additions to `crates/aulos-provider/src/proc.rs`** (no signature changed, nothing
  removed), all needed by the plugin download loop and all reusable:
  - `Lines::next_chunk() -> Result<Option<Vec<u8>>, ProcError>` — raw chunks, because
    `next_line()` deliberately does not treat a bare `\r` as a terminator, so a tool that repaints
    for a minute without printing a newline delivers nothing through it (`progress.cr_as_newline`).
  - `Child::take_stdout() -> Option<Lines<ChildStdout>>` — `stdout_lines()` borrows the whole
    `Child`, which makes "read stdout **or** notice the exit, whichever first" unwritable.
  - `SpawnSpec::stderr_tap(mpsc::Sender<Vec<u8>>)` — duplicates each stderr chunk into a channel
    **in addition** to the bounded ring, for `progress.source = "stderr" | "both"` and for the
    `max_output_bytes` budget (which counts stdout *and* stderr). The mandatory drain is untouched.
    WP-09 may find the tap useful for `N_m3u8DL-RE`'s stderr repaints.
  In the download loop the provider **drops its `SpawnSpec` right after `Child::spawn`**, leaving
  the drain task holding the only tap sender; that is what makes the post-exit `tap.recv()` end at
  end-of-stream instead of racing a task that may not have been polled yet, and therefore what
  makes the stderr tail in `error.message` deterministic.
- **No new crate dependency.** SHA-256 (`command::sha256`, the `media_id` default of §6.5.3),
  percent-encoding and JSON string escaping (`command::template::{percent_encode, json_escape}`)
  are implemented in-crate rather than pulling `sha2` / `percent-encoding`, because the §3
  dependency row for `aulos-provider` does not list them and `tests/arch.rs` enforces it as a
  subset rule. Both are covered by tests (NIST vectors for the digest). `wiremock` and `proptest`
  were added as **dev**-dependencies, which the arch rule exempts.
- **`Escape::{Percent, Json}` and `Template::render_escaped` implement DESIGN §13.4's escaping
  rules** — a placeholder inside `http.url` is percent-encoded, one inside a body/header is
  JSON-escaped. WP-11 should use them rather than re-deriving; the "JSON-escape only when the body
  parses as JSON" decision is WP-11's to make per request, which is why `render_escaped` takes the
  mode as an argument.
- **A plugin's terminal `status_map` target is advisory and is dropped.** `status_map = { done =
  "finished" }` parses into `StatusTarget::Terminal`, but the provider does **not** forward it to
  the sink: the engine writes the terminal status from `download()`'s return value (DESIGN §6.2),
  and a plugin cannot change an item's status. Only the three running stages reach `ProgressSink`.
- **`capabilities.streaming_resolve` is parsed and its reading strategy honoured** (resolve stdout
  is consumed line by line as the child prints it, so a 500-entry album does not buffer), but
  `Provider::resolve` returns a `Vec<MediaEntry>`, so children cannot literally be published to the
  engine before the child exits. Making that real needs a channel on `ResolveCtx`; WP-12 can add one
  additively and this provider will fill it.
- **Load-time warnings are a first-class output, not just a log line.** `PluginManifest.warnings`,
  `Scan.warnings` and `CommandPluginLoader::warnings()` carry every clamp and every unset `${VAR}`
  as `{key, message}`; **WP-14 should surface them next to `ReloadReport.failed` in `healthz` and
  `GET api/v2/providers`**, because a mistyped `${PLEX_TOKEN}` otherwise produces a silent 401
  forever. `CommandProvider::audit() -> Vec<Vec<String>>` is the unrendered argv list DESIGN §6.5.3
  requires that endpoint to publish.
- **`ManifestError::reason()` is the `Degraded(reason)` string** — one line, capped at 400
  characters, prefixed with the dotted key (`download.command[2]: unknown token {out_dirr} at
  offset 3`). `ManifestError::has_partial_match()` says whether a matcher could still be built from
  the file: a semantic rejection becomes a `DegradedProvider` that **still claims its URLs** (via
  `command::manifest::partial_match`), while an unreadable or syntactically broken file becomes a
  `ReloadFailure` only. Both appear in the report either way.
- **`plugins/examples/` ships two directories and a plugin-author guide.**
  `plugins/examples/bandcamp/` is the DESIGN §6.5.4 manifest plus `resolve.py` / `download.py`, run
  end to end against `wiremock` by `crates/aulos-provider/tests/plugin_example.rs` — that test is
  the author's template and will go red if the example rots. It **skips itself with a printed note
  when `python3` is not on `PATH`** rather than failing; the image and CI both have it.
  `plugins/examples/media-server-hooks/plugin.toml` is the §13.4 Plex/Emby/ntfy/command file, and
  `plugin_manifest.rs` loads it and asserts all four hooks.
- **A `{` only starts a token when a token-shaped name and a `}` follow it.** `{"title": "{title}"}`
  is therefore a usable JSON hook body with no escaping, while `{out_dirr}` is still a load-time
  error; `{{`/`}}` remain available as explicit brace escapes. This is the one place the template
  grammar is looser than a strict reading of §6.5.1, and it is what makes the §13.4 `body`
  examples writable.
- **`{headers_curl}` is the only token that changes the argv length**, and it does so from the
  manifest's `[headers]` table, never from the entry — which is the invariant
  `tests/plugin_template.rs` proves with a `proptest` over hostile titles. It expands to `-H`/`K: V`
  pairs only when it *is* the whole argv element; embedded in a larger element it renders as a
  shell-quoted joined string, for the `["/bin/sh","-c","curl {headers_curl} …"]` case.

## WP-07 — `aulos-provider-ytdlp`: the Python shim and its Rust client

- **`ProviderError::Unsupported`'s `Display` prefixes its message.** It is
  `#[error("unsupported url: {0}")]` while every other message-bearing variant is `#[error("{0}")]`,
  so `ProviderError::message()` returns `"unsupported url: Invalid/empty data was given."`.
  DESIGN §8.4 and §11.7 require the string `Invalid/empty data was given.` **byte-identical** on the
  wire (the v1 shim echoes it as `{"status":"error","msg":…}`, and the iOS build matches on it), and
  the same applies to the verbatim `Unsupported resource "<etype>"`. Either
  `aulos-provider`'s variant should become `#[error("{0}")]` — a one-line, behaviour-only change in
  a file WP-07 does not own — or the engine must special-case `ErrorCode::UnsupportedUrl` when it
  builds the `WireError`. The runner already produces the exact text
  (`aulos_provider_ytdlp::runner::EMPTY_DATA`); only the `Display` impl adds the prefix.
  `tests/replay.rs::an_extraction_that_yielded_nothing_uses_the_verbatim_legacy_message` asserts
  `contains` rather than `==` and points here.
  **APPLIED (wave-1 integration).** The first option was taken: `ProviderError::Unsupported` is
  now `#[error("{0}")]` like every other message-bearing variant, so no engine has to special-case
  `ErrorCode::UnsupportedUrl` to satisfy DESIGN §8.4/§11.7. The variant's doc comment records why
  the prefix may not come back, `provider.rs::the_verbatim_legacy_messages_are_undecorated`
  asserts both required strings survive `Display`, `message()` and `to_wire()` untouched, and the
  replay test was tightened from `contains` to `assert_eq!(e.message(), EMPTY_DATA)`.
- **Per-line child-stderr logging is deferred to the end of the job.** DESIGN §9.1 asks for stderr
  lines to reach `tracing` at DEBUG (WARN for `^(ERROR|WARNING)`) with `target = "ytdlp.child"` *as
  they arrive*. `aulos_provider::proc::Child` owns the (mandatory, deadlock-avoiding) stderr drain
  and exposes a bounded `StderrRing` rather than a line stream, so `runner::drain_stderr` does the
  classification once when the job ends. Same information, later. An additive
  `SpawnSpec::stderr_line_hook(Box<dyn Fn(&str)>)` in `aulos-provider` would restore the real-time
  behaviour for every provider at once; nothing depends on it today.
  **APPLIED (wave-1 integration).** `SpawnSpec::stderr_line_hook(impl Fn(u32, &str))` now exists
  (the pid is an argument because the drain starts inside `Child::spawn`, before a caller could
  learn it), and the runner installs `runner::log_child_line` through it — so DESIGN §9.1's
  per-line DEBUG/WARN classification happens **as the line arrives**, and `drain_stderr`'s
  end-of-job replay is gone. The ring and `stderr_tap` are untouched, and
  `tests/proc.rs::the_stderr_line_hook_sees_lines_while_the_child_is_still_running` proves the
  lines land before the child exits, exactly once each. Every provider can use it now.
- **`RunnerOutcome` has a fourth variant, `Selftest(ShimIdentity)`.** The PLAN interface lists
  three. `Provider::probe` runs `mode = selftest` and needs the `hello` payload (yt-dlp version,
  interpreter version, plugin list, POT availability), which `healthz.components.ytdlp_runner` and
  `GET <p>version` also read. Adding a variant keeps `run_job`'s signature exactly as DESIGN §9.7
  writes it. `RunnerHandle::identity()` is the accessor WP-14/WP-16 want; it is shared across
  clones of a handle.
- **`Provider::resolve` and `Provider::probe` get no `ProgressSink` from the trait**, but the runner
  needs one to forward `log` frames. `YtdlpProvider::detached_sink()` builds one over a channel
  whose receiver is dropped immediately — `ProgressSink` documents a closed channel as a no-op. If
  WP-12 wants resolution logs on an item's event stream, `ResolveCtx` needs a sink field (additive).
- **`Job::download_root` is not serialised and must be set by the caller.** `Outcome::filename` is
  documented as relative to *the item's download root* (`DOWNLOAD_DIR` or `AUDIO_DOWNLOAD_DIR`), not
  to `DownloadCtx::out_dir`, because `download_url = PUBLIC_HOST_URL + filename`.
  `YtdlpProvider::download` sets it from `cfg.paths.root_for(download_type)`; a caller driving
  `RunnerHandle` directly must do the same or the produced path degrades to a basename.
- **Partial-file cleanup happens in this crate, for this provider only.** On cancel, timeout or any
  contract violation the runner removes every `.part`/`.ytdl` it saw in a `progress` frame, bounded
  to the job's own `download_dir` / `temp_dir` / download root. WP-12's `_post_download_cleanup`
  port (Δ C18: tmp dir, SC segment dir) is still needed for the paths a provider never reports.
- **`bot_check` is classified before `auth_required`**, one row earlier than the DESIGN §9.6 table
  lists it. The canonical YouTube string `Sign in to confirm you're not a bot` matches both regexes;
  `auth_required` would tell the user to upload cookies when the real signal is "the POT sidecar is
  not working". The specific pattern wins over the generic one — worth reflecting in §9.6 if the
  table is ever re-ordered.
- **The stderr tail quoted inside an error *message* is the last ~360 characters, not the whole
  8 KiB.** `ProviderError::message()` caps at 512 characters and truncates from the front, so
  quoting the whole tail would fill the message with the oldest, least useful noise. The full
  retained tail still reaches the logs.
- **The shim gained two job fields DESIGN §9.2 does not list**, both optional and both defaulting to
  the old behaviour: `policy.hard_timeout_ms` arms a `SIGALRM` watchdog inside the shim (so a wedged
  job produces a clean `error{code:"timeout"}` transcript instead of an undiagnosed `SIGKILL` — this
  is what §9.6's "the shim's own watchdog" row refers to), and `policy.pot_url` is echoed back in
  `hello.pot.url` so `healthz` can report which endpoint the POT plugin will use.
- **`RunnerHandle::with_env_var` exists for the test suite**, which points `PYTHONPATH` at
  `tests/fixtures/pystub`. Production sets only `PYTHONUNBUFFERED` and `PYTHONDONTWRITEBYTECODE`;
  the shim's environment is otherwise inherited, because `YTDL_*`, the proxy variables and the POT
  plugin's own settings are part of its contract.
- **`command-fds` needs its `tokio` feature.** `crates/aulos-provider-ytdlp/Cargo.toml` enables it
  (`features = ["tokio"]`) so `CommandFdExt` applies to `tokio::process::Command`; the root
  `[workspace.dependencies]` entry is untouched. The crate's `tokio` features also gained `net`
  (for `tokio::net::unix::pipe::Receiver`, the async reader for fd 3) and `fs` (for
  `RunnerHandle::replay`).
- **The image must ship the shim at `/app/python/ytdlp_runner.py`.** `docker/Dockerfile` already
  copies it there and `runner::DEFAULT_RUNNER_PATH` matches; `YtdlpProvider::new(cfg, python,
  runner)` takes both paths so the binary can override them.
- **CI needs no change.** The `python` job of `.github/workflows/ci.yml` already runs `ruff check` +
  `py_compile` on `python/ytdlp_runner.py` and then `tests/shim_contract.py`; both files now exist,
  so the two "skipping" notices go away. `shim_cli.rs` also runs `shim_contract.py` from
  `cargo test`, so the two cannot drift.
- **The optional real-yt-dlp smoke is `tests/real_ytdlp.rs`** and skips itself unless the chosen
  interpreter can `import yt_dlp` *and* `ffmpeg` is on `PATH`. Run it with
  `AULOS_TEST_PYTHON=<venv>/bin/python cargo test -p aulos-provider-ytdlp --test real_ytdlp`; it
  generates a one-second clip with ffmpeg and extracts and downloads it over `file://`.

## WP-05 — `aulos-store` legacy importer + `import` / `check-config` CLI

- **`aulos_core::Status` gained `PartialOrd, Ord`** (a derive added to the existing enum in
  `crates/aulos-core/src/status.rs`; no signature changed). The PLAN's `ImportReport.items` is a
  `BTreeMap<Status, u64>`, which needs `Ord`. Derived order is declaration order, which is also the
  order DESIGN §7.6.6 prints the counts in. Nothing reads it as a severity or a progression.
- **`WriteOp::SetMeta { key, value }` is new** (`WriteOp::NAMES` is now 20 entries, handled by the
  new `aulos-store` module `meta`). DESIGN §7.6.6 requires `meta.imported_from`, `meta.imported_at`
  and `meta.import_report` to land **in the same transaction as the rows**, and the two existing
  `meta` writers (the schema seed and the id allocators) both write outside the writer actor on
  their own connections. `tests/writes.rs` gained the matching round-trip case. Consumers read the
  keys through `Store::meta()`; the key names are re-exported as `aulos_store::{IMPORTED_FROM,
  IMPORTED_AT, IMPORT_REPORT}` and `aulos_store::import::stored_report(&store)` decodes the report.
- **`canonical_key` lives in `aulos-store`, not `aulos-queue`** —
  `aulos_store::canonical_key(provider: &str, url: &str, media_id: Option<&str>) -> Box<str>`
  (module `import::canonical`). `items.canonical_key` is `NOT NULL` and DESIGN §7.6.2 step 5
  requires the imported value to come from "the same function used at runtime", but `aulos-store`
  is upstream of `aulos-queue` and its DESIGN §3 row deliberately excludes `url`. **WP-11 must
  delegate rather than reimplement**:
  ```rust
  pub fn canonical_key(p: &ProviderId, url: &Url, media_id: Option<&str>) -> Box<str> {
      aulos_store::canonical_key(p.as_str(), url.as_str(), media_id)
  }
  ```
  Two implementations would silently defeat dedupe for every pre-cutover URL.
  **CARRIED FORWARD (wave-1 integration).** `aulos-queue` is still the WP-01 stub (one `lib.rs`,
  no `canonical_key`), so there is nothing to make delegate yet. This is **WP-12's** to honour when
  it writes the engine — the bullet is addressed to "WP-11", but the function belongs to
  `aulos-queue`.
- **`healthz.components.importer` (WP-17):** `ImportReport::is_degraded()` and
  `ImportReport::skipped_files()` are the DESIGN §7.6.1 "degraded for the life of the process"
  inputs, and `warnings.len()` / `imported_at` are the other two fields of the §16.3 payload.
  `GET <p>api/v2/import-report` (WP-14) should serve `import::stored_report(&store)`.
- **The boot path (WP-17)** should call `aulos_store::import::import(state_dir, &store, opts)` only
  when the DB file did not exist, then on `Err` call `store.close()`, `import::delete_db_files(&db)`
  when `fatal.should_delete_db()`, print `fatal.report.render_table()` and exit non-zero.
  `ImportOpts` is built from `Config` exactly as `crates/aulos-server/src/import_cmd.rs` does it.
- **`--dry-run` uses a throwaway database file, not `:memory:`.** DESIGN §7.6.6 says `:memory:`;
  the store's pragma set requires `journal_mode = WAL` (which an in-memory database refuses) and
  the read pool opens its own connections, so two `:memory:` handles would be two different
  databases. The CLI creates a scratch DB under the OS temp dir and deletes it before returning, so
  the `--db` path is never created and `STATE_DIR` is never written to. Same rehearsal, same
  guarantees, and the real `STRICT`/`UNIQUE` checks still run.
- **Still owed by another package: the SC cross-crate assertion.** PLAN WP-05 asks for a test that
  the NFO hook renders identical XML from an imported blob and from a freshly resolved one.
  `aulos-hooks` is still a stub, and `aulos-store` must not dev-depend on a provider crate, so the
  store side pins the translated `state` with an `insta` snapshot
  (`crates/aulos-store/tests/snapshots/import__sc_state.snap`) and the sc-entry fixture
  (`crates/aulos-store/tests/fixtures/state/sc-entry/queue.json`) is checked in for reuse.
  **WP-10 (or `aulos-workspace-tests`) should add:** `ScState::from_json(imported_blob)` succeeds,
  and `to_legacy_info_json` / the NFO XML match a freshly resolved entry.
  **APPLIED (wave-1 integration).** Added as
  `crates/aulos-workspace-tests/tests/sc_import_equivalence.rs`, which is where it had to go:
  `aulos-hooks` may not depend on a provider crate and no provider crate may depend on the store
  (A1), so only the dev-only crate can see all three sides. It resolves `/it/watch/9?e=456` for
  real through `watch::resolve_watch` over a loopback `wiremock` server and the SC crate's own
  checked-in fixtures, **derives** the legacy `queue.json` row from that resolution (so the two
  sides cannot drift apart by someone editing one fixture), runs the real importer, and then
  asserts `ScState::from_json(imported_blob)` parses, that the ids / season / episode / series /
  `base_url` all match, that `to_legacy_info_json` is equivalent either way, and that
  `nfo::render` produces a byte-identical document from the imported blob and from a fresh one.
  `aulos-workspace-tests` gained the dev-dependencies that needs; the arch gate permits it (the
  subset rule is judged on shipped dependencies, and A1 constrains provider crates, not this
  one).
- **Imported rows are attributed `source = { kind: "api_v1", ref: null }` and
  `provider = "ytdlp"`** (or `"streamingcommunity"`). Legacy persisted no attribution at all, and
  every legacy record had already been through `extract_info`, so a null provider would say
  "never resolved" and make an imported row's `canonical_key` disagree with a fresh add's.
- **A legacy `shelve` file is fatal only when its JSON counterpart is missing.** Legacy never
  deleted the shelf after migrating it (`app/ytdl.py:1149`), so a real `STATE_DIR` usually holds
  both `queue` and `queue.json`; making mere presence fatal (a literal reading of DESIGN §7.6.1)
  would break every real cutover. A shelf beside readable JSON is a `shelf_ignored` warning.
- **`aulos-server serve` now installs its `SIGTERM`/`SIGINT` handlers before printing its announce
  line** (`main.rs`). The line used to be printed first, so a supervisor that signalled immediately
  raced the installation and killed the process instead of shutting it down; the WP-01 CLI test
  started failing as soon as the binary grew. WP-17 should keep that ordering.

## WP-11 — `aulos-hooks`: dispatcher, jellyfin, nfo, audio-sync, community hooks

- **`HooksFinished` needs an adapter in `aulos-server`, and the engine must keep the `Outcome`.**
  `aulos-hooks` may not depend on `aulos-queue` (DESIGN §3), so it cannot construct
  `EngineCmd::HooksFinished`. The seam is `aulos_hooks::HookFinalizer` (one method,
  `async fn hooks_finished(&self, id: ItemId)`). Neither crate can `impl` it for the other's type,
  so **WP-16 must define a newtype over the engine handle in `aulos-server` and pass it to
  `HookDispatcher::with_finalizer`** — without it the pre-terminal phase ends in a DEBUG log and a
  `best_remux` item never finalises. The trait carries **only the id**: `DomainEvent::Finishing`
  carries an `Arc<ItemView>` and no outcome, so the outcome never crosses the event boundary. WP-12
  therefore has to park it engine-side (`pending_hooks: HashMap<ItemId, Box<Outcome>>`) on the
  `Finished → Finishing` transition and pair it back up on `HooksFinished`, which is what
  DESIGN §13's "the engine finalises with the outcome it already had" requires anyway.
- **`Finishing` must be published only on the success path.** The dispatcher treats a `Finishing`
  event as a prospective `TerminalStatus::Finished`, because `EngineCmd::Finished` is the success
  command (`Failed` is a different one) and `ItemView.status` still reads `postprocessing` at that
  point. If WP-12 ever publishes `Finishing` for a failed or cancelled job, the event needs to grow
  the outcome (`Finishing { view, outcome }`) — otherwise `audio_sync` would re-encode a file a
  failed download left behind. **The dispatcher always answers a `Finishing` event**, including when
  no pre-terminal hook applies, so a disagreement about `applies()` can never wedge an item.
- **Two deliberate signature deviations from PLAN WP-11 / DESIGN §13**, both documented in
  `crates/aulos-hooks/src/hook.rs`:
  1. `HookCtx.item` is `&ItemView`, not `&Item`. The dispatcher's only event source is an
     `EventInbox`, whose `Finishing`/`Completed` payloads are `Arc<ItemView>`, and
     `ports::HookStore` deliberately exposes no item read. Every field the four hooks need is on
     `ItemView`.
  2. `Hook::applies(&self, item: &ItemView, outcome: TerminalStatus)` takes the outcome explicitly.
     DESIGN §13 says a `PreTerminal` hook's `applies()` reads "the prospective outcome carried by
     `HookCtx.batch[0].status`", which a one-argument `applies(&item)` cannot see. The value passed
     is the same one that lands in `BatchEntry.status`.
- **`HookFilter::matches` (WP-10) takes `&Item` and is therefore unusable from this crate.**
  `ManifestHook::filter_matches` reimplements the same three axes against `ItemView`, with identical
  semantics (including "an item with no provider fails a `when.provider` filter rather than passing
  it"). A one-line additive `HookFilter::matches_view(&ItemView)` in `aulos-provider`, with both
  callers delegating to it, would remove the duplication; it is deliberately not done here because
  WP-10 owns that file.
  **APPLIED (wave-1 integration).** `HookFilter::matches_view(&ItemView)` now exists, both public
  entry points delegate to one private `HookFilter::passes(provider, download_type, folder)`, and
  `ManifestHook::filter_matches` is a one-line call into it — the second copy of the three axes is
  gone. `hookspec.rs::the_item_and_the_view_forms_of_a_filter_agree` cross-checks the two forms
  over five filters x five items, including the unresolved item, which is the case with the
  surprising answer.
- **`aulos_provider::proc::Child::wait` does not join the stderr drain task**, so reading
  `child.stderr().tail(..)` the instant `wait` returns is a race — the tail came back empty about one
  run in twenty in this crate's tests. `ffprobe::settled_stderr` works around it with a bounded
  1 ms poll on the failure path only. Every other consumer of `proc` has the same race; an additive
  `Child::wait_drained()` that awaits the drain's `JoinHandle` would fix it once.
  **APPLIED (wave-1 integration).** `Child` now keeps the drain's `JoinHandle` and exposes
  `wait_drained()` (wait, then join) and `drained()` (join only, for the `select!` loops that
  already have the exit status). The join is bounded by `Child::DRAIN_JOIN_GRACE` (250 ms),
  because a pipe closes only when *every* writer closes it — a grandchild that inherited stderr
  must not be able to wedge the caller, which
  `wait_drained_is_not_wedged_by_a_grandchild_holding_stderr` proves. All three workarounds are
  deleted: `aulos-hooks`' `ffprobe::settled_stderr`, `aulos-provider-sc`'s `engines::settled_tail`
  (now `settled_tail(&mut Child)`, a join rather than a 100 ms poll) and the ytdlp runner's read
  of the tail straight after `wait`. `wait_drained_settles_the_stderr_tail_without_polling` runs
  the race 25 times.
- **JSON escaping in a community `[[hook]]` applies to *string* tokens only.** DESIGN §13.4 says a
  placeholder in a `body`/header "is JSON-escaped when the body parses as JSON"; taken literally that
  breaks `{"count": {count}, "titles": {titles_json}}`, because `{count}` is a number and the two
  `*_json` tokens are JSON arrays. `ManifestHook` escapes the string-valued context fields and
  leaves the structural ones alone (`manifest_hook::ManifestHook::value_ctx`), which is what makes
  the DESIGN §13.4 example bodies valid JSON. Whether the body is JSON is decided from the
  *template*, once, at construction — never from a rendered value, so a title with a quote in it
  cannot change how the next body is escaped.
- **A phase-only progress frame would reset the aggregator's monotonic `percent` floor (WP-13).**
  `Normalizer::apply` resets on a `source_tag` change and `ProgressCell::apply` overwrites the byte
  counters from every frame, so a naive `{phase, phase_percent}`-only frame would drop a 99.9 %
  item to 0 % for the whole re-encode. `audio_sync` therefore publishes a constant non-zero
  `source_tag` (`audio_sync::SOURCE_TAG`) and repeats the finished byte counts. If WP-13 would
  rather special-case a frame that carries only `phase`/`phase_percent`, `audio_sync::frame` is the
  one place to simplify.
- **`healthz` component keys are exactly the hook ids** (DESIGN §16.3): `audio_sync`, `nfo`,
  `jellyfin`, and `hook:<dir>/<id>` per community hook. `HookDispatcher::health()` /
  `HooksHealthHandle::health()` return them ready-made — `HooksHealth::apply(&HealthRegistry)`
  publishes every one, and `HooksHealth::events_dropped` is what WP-14's
  `components.events.dropped.hooks` should carry. Detail fields follow one rule: `runs_total` and
  `failures_total` always; `last_success_at` / `last_error` when there is one; `pending` for a hook
  with a debounce window; `phase` for a pre-terminal hook. That reproduces the three payloads in
  DESIGN §16.3 exactly. Take `health_handle()` **before** `spawn` consumes the dispatcher.
- **Suggested WP-16 wiring**:
  `HookDispatcher::new(cfg, hook_specs, clock).with_finalizer(engine_adapter).with_cancel(shutdown_token)`,
  then `.spawn(router.subscribe(SubscriberSpec::hooks()), sink_factory, Arc::new(EngineHookStore))`.
  `hook_specs` is the concatenation of `PluginManifest.hooks` over the loaded plugin directory. The
  dispatcher's `run` returns once every `EventSender` is dropped: each debounced hook flushes its
  trailing batch first, unless the cancellation token is already cancelled (the DESIGN §16.4 grace
  has expired), in which case the tail is dropped with a WARN.
- **There is no config knob for the ffmpeg/ffprobe paths** (DESIGN §17.3 has none), so `audio_sync`
  resolves both through `PATH`. `AudioSyncHook::with_tools(MediaTools { .. })` exists for the tests
  and is where such a knob would land.
- **The `notify`-driven plugin reload does not reach hooks yet.** The dispatcher takes its
  `Vec<HookSpec>` once, at construction, so a `SIGHUP` or a `POST api/v2/plugins/reload` re-scan
  updates providers but not community hooks. DESIGN §13.4 does not require live hook reload; if
  WP-16 wants it, the shape is a `HookDispatcher` command channel, not a shared mutable hook list.
- **`FakeClock::default()`'s doc comment in `aulos-core` says 2026-09-04, but its epoch value
  (`1_772_582_400_000`) is 2026-03-04.** Harmless, but every snapshot stamped from it reads March;
  this crate's NFO snapshots pass an explicit `now_ms` instead.
  **APPLIED (wave-1 integration).** The **comment** was the wrong half: the value is load-bearing
  for every `insta` snapshot stamped from that clock, so it must not move. It is now
  `aulos_core::clock::DEFAULT_FAKE_EPOCH_MS`, documented as 2026-03-04, with
  `the_default_epoch_is_the_date_it_claims` asserting the constant against the date it names, so
  the two cannot drift again.
- **`plugins/examples/media-server-hooks/plugin.toml` is now covered by a test.**
  `crates/aulos-hooks/tests/community.rs::the_shipped_example_manifest_loads_into_four_hooks` loads
  the shipped file through WP-10's real loader and asserts its four ids, the debounce values and the
  `${PLEX_TOKEN}` interpolation, so an edit to the example that breaks it fails CI.

---

## Wave-1 integration pass (integrator, 2026-09-04)

### The tree

- **Nothing was uncommitted and no code was discarded.** `git status` showed a clean tree after
  WP-04/05/06/07/08/09/10/11 landed. Four git-ignored tool caches were sitting in it —
  `.ruff_cache/` at the root, `crates/aulos-provider-ytdlp/python/{.ruff_cache,__pycache__}/` and
  `crates/aulos-provider-ytdlp/tests/fixtures/scripts/.ruff_cache/` — and were removed; the
  `.gitignore` entries wave 0 added already cover them, so nothing else was needed. The
  `Cargo.lock` that WP-04 and WP-06 deliberately left out of their commits is committed here, with
  the whole graph resolved and `cargo test --workspace --locked` green, so CI's `--locked` cannot
  fail on it.
- **The `sh` stand-ins keep mode 100755** (`crates/aulos-provider-sc/tests/fixtures/sc/bin/*.sh`,
  `crates/aulos-provider-ytdlp/tests/fixtures/scripts/*.py`, both `plugins/examples/bandcamp`
  scripts), as WP-09 warned they must — checked with `git ls-files -s`.

### Gates, all green on `rustc 1.95.0 (59807616e 2026-04-14)`

| Gate | Result |
|---|---|
| `cargo fmt --all` | no diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` (the CI form) | clean |
| `cargo test --workspace` / `--locked` | **996 passed, 0 failed**, 57 test binaries |
| `cargo test -p aulos-workspace-tests` | 12 arch + 10 packaging + 3 Δ C9 + 1 SC-equivalence |
| `cargo test -p aulos-provider-sc --no-default-features` (the `plain` client) | 165 passed |
| `python3 crates/aulos-provider-ytdlp/tests/shim_contract.py` | every check passed |
| `python3 tools/capture/verify.py` | `OK — … 131 v1 case(s) verified` |
| `ruff check` + `py_compile` on `python/ytdlp_runner.py` | clean |

**No test was deleted, weakened or `#[ignore]`d to get there.** The single "ignored" line in the
run is a ```` ```ignore ```` *documentation* block in `aulos-store`'s `import::canonical` module
doc — the illustrative `aulos-queue::canonical_key` delegation snippet, which cannot compile until
WP-12 creates that function. Two assertions were made *stricter*: the replay test on the verbatim
`Invalid/empty data was given.` string went from `contains` to `==`, and
`a_megabyte_of_stderr_does_not_deadlock_the_child` lost its 2-second polling loop in favour of the
new deterministic join.

### What was applied

Every open request in this file is now annotated inline with what happened to it — sixteen
bullets. Nine were real changes (below); three needed no code, because the package they were
addressed to had already honoured them (marked *verified*); one was a decision to keep the tree as
it is; two were status updates on wave-0 bullets; and one is carried forward to WP-12.

| Request (owner) | Change |
|---|---|
| `ProviderError::Unsupported`'s `Display` decorates a wire-verbatim string (WP-07 → `aulos-provider`) | `#[error("{0}")]`, plus a test on both DESIGN §8.4 strings |
| `Child::wait` does not join the stderr drain, so the tail is a race (WP-11 → `aulos-provider`) | `Child::wait_drained()` / `Child::drained()`; **three** independently-invented poll-loop workarounds deleted |
| DESIGN §9.1's per-line child stderr logging is deferred to the end of the job (WP-07 → `aulos-provider`) | `SpawnSpec::stderr_line_hook`; the runner logs in real time again |
| `HookFilter::matches` is reimplemented against `ItemView` in `aulos-hooks` (WP-11 → WP-10's file) | `HookFilter::matches_view`, one predicate, both callers delegate, cross-checked |
| `wreq` cannot express a real Chrome fingerprint without `wreq-util` (WP-08 → integrator) | `wreq-util = "3.0.0-rc.14"` added to the workspace, DESIGN §3/§10.1/§18.6 and `arch.rs`; `Profile::Chrome131` replaces the hand-built tables |
| CI never exercises the plain `reqwest` SC client (WP-08 → `.github/`) | one line in `ci.yml`'s `test` job |
| The Δ C9 halves (WP-06 removed the `Exec`, WP-11 owns the hook) are untied | `tests/audio_sync_delta_c9.rs`, a full catalog sweep |
| The SC import/resolve equivalence assertion nobody could own (WP-05 → `aulos-workspace-tests`) | `tests/sc_import_equivalence.rs`, a real resolve + a real import |
| `FakeClock::default`'s doc says September, its value says March (WP-11 → `aulos-core`) | `DEFAULT_FAKE_EPOCH_MS`, documented correctly, asserted |

`aulos-workspace-tests` grew from "no dependencies at all" to a set of dev-dependencies
(`aulos-store`, `aulos-provider`, `aulos-provider-sc`, `aulos-provider-ytdlp`, `aulos-hooks`,
`serde_json`, `tempfile`, `url`, `wiremock`, `tokio`) so it can host the two cross-crate tests.
That is exactly the role DESIGN §3 gives it — its row in `arch.rs` stays `Some(&[])`, because the
subset rule is judged on *shipped* dependencies and this crate ships nothing.

### DESIGN.md edits made here

PLAN §0 wants a deliberate deviation recorded in DESIGN.md, and wave 0 carried five such
deviations in prose in this file instead. This pass adds one dependency, so it paid that debt for
its own change rather than growing it: DESIGN §3's `aulos-provider-sc` row now lists `wreq-util`
(and `futures-util`, which WP-01 added and `arch.rs` had been amending in a comment), §18.6 has a
`wreq-util` row, and §10.1 gained two rows — one explaining why `wreq-util` is *required* with
`wreq`, and one recording the pinned prerelease versions and the decision **not** to take the
BRIEF's plain-`reqwest` escape hatch, which is the §10.1 note WP-08 owed. **The other five wave-0
deviations are still documentation debt** (`ProviderId`/`FileSlot` in `aulos-core`,
`Registry::pick` returning `Option`, `OutTmpl` in `aulos-provider`, `FormatSpec.flags.slow` on
`mp4`, `ChatConfig`'s twelve keys), and so are the wave-1 ones recorded above in prose.

### Carried forward — every request in this file that is still open

All of these are addressed to packages that do not exist yet. None of them blocks wave 2 starting.

| Request | Owner |
|---|---|
| Map `Registry::pick` → `None` to `ErrorCode::UnsupportedUrl` | WP-12 |
| Park the `Outcome` engine-side on `Finished → Finishing` (`pending_hooks`) and pair it back on `HooksFinished`; publish `Finishing` **only** on the success path | WP-12 |
| Delegate `aulos-queue::canonical_key` to `aulos_store::canonical_key` — never a second implementation | WP-12 |
| Read `Store::options()` for `done_window`/`entry_max_bytes` rather than re-deriving them from `Config`; turn `set_size`/`drop_entry_blob` into `EngineCmd::HookWrite` | WP-12 |
| Call `OutTmplJob::merge_info` with the compacted entry blob, or `%(playlist_id)s`-style fields degrade to `NA` | WP-12 |
| Add a children channel to `ResolveCtx` if `capabilities.streaming_resolve` is to publish before the child exits; add a `ProgressSink` to `ResolveCtx` if resolution logs should reach the item's event stream | WP-12 |
| Port `_post_download_cleanup` for the paths no provider reports (tmp dir, SC segment dir) — Δ C18 | WP-12 |
| Build the `notice` frame from `DomainEvent::as_notice()`; assert the delta field list against `ItemView::FIELDS`; decide whether to special-case a `{phase, phase_percent}`-only frame (see `audio_sync::frame`) | WP-13 |
| Fill `ytdl_options_presets.choices` from `YTDL_OPTIONS_PRESETS`; serve `match: null` for a `catalog?url=` that matches nothing; surface `PluginManifest.warnings` next to `ReloadReport.failed` in `healthz` and `GET api/v2/providers`; serve `import::stored_report` at `GET api/v2/import-report` | WP-14 |
| Map any legacy video selection onto `mp4`/`best` for a provider whose catalog is advisory (the SC case), or catalog validation rejects a request legacy accepted | WP-15 |
| Filter `Canceled` items out of `GET history` rather than relying on `Status::v1()`'s defensive `"error"`; call `get_format_raw`/`get_opts_raw`, not the typed forms | WP-15 |
| Define the `HookFinalizer` newtype over the engine handle in `aulos-server` and pass it to `HookDispatcher::with_finalizer` — without it a `best_remux` item never finalises | WP-16 |
| Take `health_handle()` **before** `spawn`; wire the dispatcher as the WP-11 note spells out | WP-16 |
| Call `config::load_with_warnings()` **and** `YtdlOptions::load(...)`, merging both reports before the single exit-2 step | WP-17 |
| `Registry::set_command_loader(CommandPluginLoader::with_env(PluginEnv { state_dir }))` at boot, or `{cookies_file}` renders empty | WP-17 |
| Schedule the six-hourly checkpoint and `await store.close()` on shutdown; ask for `pub async fn checkpoint(&self)` rather than reaching into `schema` | WP-17 |
| Run the importer only when the DB file did not exist, and handle `ImportFatal` as the WP-05 note describes | WP-17 |
| Add `crates/aulos-provider-ytdlp/tests/smoke_extract.sh` and `tests/e2e/run.sh` — each turns on a CI step that no-ops today | WP-18 |
