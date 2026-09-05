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
  | ~~Map `Registry::pick` → `None` to `ErrorCode::UnsupportedUrl`~~ **DONE by WP-12** | WP-12 |
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
  3. **(bug-2a fix, 2026-09-05)** `Hook::skip_reason(&self, item, outcome) -> Option<SkipReason>`
     sits alongside `applies`, and is what the dispatcher actually gates on. `applies` stays a
     required method — several `impl Hook` blocks live outside this crate (`aulos-server`'s
     `adapters.rs` stub and `tests/server.rs`) and must keep compiling — and every built-in defines
     it as `skip_reason(..).is_none()`. `skip_reason` has a default that derives the answer from
     `applies` with the generic reason `it does not apply to this item`, so an out-of-tree hook is
     still counted, just not specific. **If a later pass is free to touch every `impl Hook` in the
     workspace, invert them**: make `skip_reason` the required method and `applies` the provided
     one, which removes the one-line `applies` body from each built-in and the possibility of the
     two disagreeing.

- **The NFO hook was gated to `provider == "streamingcommunity"` and therefore never ran in
  production (bug 2a, 2026-09-05).** DESIGN §13 scoped it to SC on the reasoning that YouTube NFOs
  came from the user's own yt-dlp `Exec` postprocessor — which the temp-dir layout had already
  broken (the media file is in `/downloads/<ULID>/` at exec time while `writeinfojson` has written
  the sidecar to `/downloads/`), so the cutover produced no `.nfo` from either path. The hook now
  applies to every provider, reads the on-disk `<file>.info.json` when the row carries no metadata
  blob (which for a plain yt-dlp row is always, per DESIGN §7.5), and `AULOS_NFO_PROVIDERS`
  (comma list, default empty = all) is the opt-in narrowing. `tests/nfo_legacy_parity.rs` pins the
  output against XML captured from `jellyfin_nfo_generator.py` itself, so the port cannot drift
  from what legacy wrote for the same `.info.json`; the module doc there carries the regeneration
  command.
- **Skips are counted and named (bug 2a, second half).** The dispatcher logs `hook skipped` at
  DEBUG (hook id, item id, outcome, reason), counts `skipped_total` per hook and keeps
  `last_skip_reason`; a hook with `runs_total: 0` and a non-zero `skipped_total` also gets a
  `detail` string in `healthz`. `status` stays `ok`. `HookStat` grew the two fields, so anything
  matching it exhaustively needs them. A hook of the *other* phase is not counted as a skip — it
  was never offered the event.
  **Amended (review pass 2):** the two `healthz` keys are published **only once something has been
  skipped**. `HookStat` still carries `skipped_total: 0`, but the component detail omits it, so a
  hook that has never declined an event serialises to exactly the payload it did before this change
  — which is what keeps DESIGN §16.3's stock sample and the hand-assembled snapshot that pins it
  (`crates/aulos-api/tests/snapshots/rest_meta__healthz_stock.snap`) from disagreeing. That snapshot
  is built from a literal in `crates/aulos-api/tests/rest_meta.rs`, not from
  `aulos_hooks::dispatcher::health_of`, so it cannot fail when the real payload changes shape;
  **whoever owns `aulos-api` should consider deriving it from `health_of`**, or the two will drift
  again on the next field.
- **Review pass 2 on bug 2a — the hook must not write a file it cannot fill.** The first pass
  rendered unconditionally, so an install with no `.info.json` (nothing in aulos forces
  `writeinfojson`) got a `movie/title/plot` stub next to every download, which Jellyfin adopts as
  local metadata and which truncated any `.nfo` a user's own `Exec` postprocessor had written.
  Legacy wrote no file in that case, and now neither does this: `render` takes a non-optional blob,
  `run` writes only when one of the two sources yielded something, and the runs that yield nothing
  are counted as `wrote_nothing_total` in the hook's own `healthz` detail (same publish-once-non-zero
  rule). Two more things came out of the same review and are worth knowing outside this crate:
  `aulos-queue`'s `keeps_entry` is `provider == streamingcommunity`, so a **`command:<name>` plugin
  row has no blob at hook time either** — the NFO source table in DESIGN §13.2 said otherwise; and
  `aulos_provider::entry::outtmpl_info` keeps `channel` for a playlist/channel child, which the
  hook's "does this blob carry metadata?" test now discounts explicitly (`is_outtmpl_hint`) rather
  than relying on the engine's terminal drop to hide it. `nfo::render` also reproduces legacy's
  nested-`get` semantics exactly for a sidecar (`uploader` present-but-null writes no `<studio>`,
  a sidecar with no url writes no `<website>`, an absent `title` becomes `"Unknown Title"`); the row
  fills a gap only for a stored `entry_json`, which legacy never saw. `nfo::Source` is the new
  parameter that distinguishes the two.
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
| ~~Park the `Outcome` engine-side on `Finished → Finishing` (`pending_hooks`) …~~ **DONE by WP-12** | WP-12 |
| ~~Delegate `aulos-queue::canonical_key` to `aulos_store::canonical_key`~~ **DONE by WP-12** | WP-12 |
| ~~Read `Store::options()` …; turn `set_size`/`drop_entry_blob` into `EngineCmd::HookWrite`~~ **DONE by WP-12** | WP-12 |
| ~~Call `OutTmplJob::merge_info` with the compacted entry blob~~ **APPLIED (wave 2)** — see below | WP-12 |
| Add a children channel / a `ProgressSink` to `ResolveCtx` — **still open, and still conditional**: nothing in v1.0 publishes children before the resolve returns | WP-12 |
| ~~Port `_post_download_cleanup` … — Δ C18~~ **DONE by WP-12** (the per-item `tmp_dir` makes it one `remove_dir_all`; the boot orphan scan covers the rest) | WP-12 |
| ~~Build the `notice` frame …; assert the delta field list against `ItemView::FIELDS`; decide the `{phase, phase_percent}`-only frame~~ **DONE by WP-13** (`DIFF_FIELDS` is declared with `ItemView::FIELDS.len()`, so an unclassified field is a compile error) | WP-13 |
| Four requests: `ytdl_options_presets.choices`, `match: null`, `import-report` — **DONE by WP-14**. `PluginManifest.warnings` in `healthz`/`providers` — **APPLIED (wave 2)**, see below | WP-14 |
| ~~Map any legacy video selection onto `mp4`/`best` for an advisory catalog~~ **APPLIED (wave 2)** — see below | WP-15 |
| ~~Filter `Canceled` out of `GET history`~~ **DONE by WP-15** (`v1_status` returns `None`). `get_format_raw`/`get_opts_raw` — **satisfied by other means**, see below | WP-15 |
| Define the `HookFinalizer` newtype over the engine handle in `aulos-server` and pass it to `HookDispatcher::with_finalizer` — without it a `best_remux` item never finalises | WP-16 |
| Take `health_handle()` **before** `spawn`; wire the dispatcher as the WP-11 note spells out | WP-16 |
| Call `config::load_with_warnings()` **and** `YtdlOptions::load(...)`, merging both reports before the single exit-2 step | WP-17 |
| `Registry::set_command_loader(CommandPluginLoader::with_env(PluginEnv { state_dir }))` at boot, or `{cookies_file}` renders empty | WP-17 |
| Schedule the six-hourly checkpoint and `await store.close()` on shutdown; ask for `pub async fn checkpoint(&self)` rather than reaching into `schema` | WP-17 |
| Run the importer only when the DB file did not exist, and handle `ImportFatal` as the WP-05 note describes | WP-17 |
| Add `crates/aulos-provider-ytdlp/tests/smoke_extract.sh` and `tests/e2e/run.sh` — each turns on a CI step that no-ops today | WP-18 |

---

## WP-12 — `aulos-queue`: the engine

- **`EngineCmd` deviations from DESIGN §8.1, all four deliberate.**
  1. `Watch` / `Unwatch` / `ConnClosed` and the `ConnId` they carry are **absent**: the BRIEF scope
     trim CUTs the WS watch registry, so there is no connection→groups map anywhere in the process
     and `ItemView.children_inline` is always `true` on a group. `PLAN WP-12` still lists them; the
     BRIEF wins.
  2. `HooksFinished { id, outcome: Option<Box<Outcome>> }` — `Option`, because
     `aulos_hooks::HookFinalizer::hooks_finished` carries **only the id** (see the WP-11 note). The
     engine parks the `Outcome` in `pending_hooks` on the `Finished → Finishing` transition and
     pairs it back up here. `None` is what the wiring adapter sends.
  3. `File { id, slot, file }` added: DESIGN §8.1 omits it while §15.1 requires the aggregator to
     forward `ProgressMsg::File` "to the engine (persisted)". Without it `WriteOp::PushFile` has no
     caller.
  4. `ExpandNext { group }` and `Clear { delete_file }` added. `ExpandNext` makes a 500-child
     expansion interruptible between batches, which is what "`CancelResolve` marks the
     not-yet-created children of every in-flight expansion cancelled" requires; `Clear` is the only
     producer of `RemoveReason::Cleared`, which both the v1 "clear completed" call and the v2 clear
     route need.
- **`EngineHandle` has four methods DESIGN does not list, and WP-13/WP-14/WP-16 need all four.**
  `stage(id, stage, msg)` and `file(id, slot, file)` are what the aggregator calls when it forwards
  the two lossless `ProgressMsg` kinds (DESIGN §15.1). `clear(delete_file)` is the clear route.
  `tick()` runs the 1 Hz maintenance pass on demand — for a caller that has just moved the clock,
  and it is what makes every timer in this crate's tests instant instead of wall-clock bound.
- **WP-13 must call `EngineHandle::heartbeats().frame(id, now_ms)` on every progress frame it
  receives.** **VERIFIED (wave-2 integration):** the aggregator does, and `Engine::handle_stage`
  beats as well. The additive per-item beat on `ProgressSink` this bullet proposes is **not** done:
  `watchdog::tests::a_drop_storm_does_not_trip_the_stall_watchdog` proves the factory-wide counter
  already reproduces the discrimination DESIGN §4.7 asks for, so the change would be a tidier
  spelling of a property that already holds. Recorded, not applied. That is the whole input to the stall watchdog (DESIGN §8.11). DESIGN §4.7 asks for
  `last_frame_at` to be bumped "before the drop decision", but the drop decision is made inside
  `ProgressSink::progress` (`aulos-provider`), whose per-item state neither the engine nor the
  aggregator can see — only the factory-wide `ProgressSinkFactory::dropped()` counter is
  observable. The watchdog therefore treats **either** a heartbeat advance **or** a rise in that
  counter as liveness, which reproduces the discrimination the design asks for
  (`watchdog::tests::a_drop_storm_does_not_trip_the_stall_watchdog` proves it). An additive
  per-item beat on `ProgressSink` — one `Arc<AtomicI64>` bumped before the `try_send` — would let
  the sink record it exactly where DESIGN §4.7 says, and would make the global counter
  unnecessary. That is an `aulos-provider` change (WP-03's file), so it is not done here.
- **WP-16 must wire `Engine::with_pre_terminal(...)`.** **STILL OPEN — owner is WP-17**, which
  has not landed: the wiring lives in `aulos-server`, and `aulos-server` is still the WP-01
  skeleton. Without it a `best_remux` item never finalises, so this is on WP-17's critical path. The engine cannot evaluate
  `aulos_hooks::Hook::applies` (`aulos-queue` must not depend on `aulos-hooks`, DESIGN §3), so the
  seam is `aulos_queue::PreTerminalHooks` — one method,
  `fn label_for(&self, view: &ItemView) -> Option<Box<str>>`, returning the `msg` the engine writes
  while the phase runs. Implement it in `aulos-server` over `HookDispatcher`'s hook list
  (`hooks.iter().filter(|h| h.phase() == PreTerminal && h.applies(view, Finished)).next()`), and
  pass the same dispatcher's `HookFinalizer` adapter. **Without it every item finalises in one
  step** — correct for a build with no pre-terminal hook, and silently wrong for `audio_sync`.
- **A pre-terminal hook's `set_size` wins over the provider's outcome.** `EngineCmd::HookWrite`
  records the item in `hook_sized`, and `finalise_success` then keeps the row's size rather than
  the `Outcome`'s, so the single `completed` frame carries the post-re-encode value (DESIGN §13.3).
  `Finishing` is published **only** on the success path, as the WP-11 note requires.
- **`Engine::recover` publishes the whole recovered working set as one `Added(_, Created)`
  event, and WP-13 must treat that as its snapshot baseline.** The aggregator builds `Published`
  from the events it sees (DESIGN §15.1/§15.2); without this a client connecting after a restart
  would get an empty snapshot until something happened to change a row. It is one event carrying
  every view, never one per item, because the router's inbox is bounded at 4 096 and recovery may
  run before the router task is spawned.
- **`ViewExtras.download_url` is left `None` by the engine, so `aulos-api` must fill it.**
  `percent-encoding` is not in `aulos-queue`'s `tests/arch.rs` row (it *is* in `aulos-api`'s), and
  `aulos_core::ViewExtras`'s own documentation assigns the field to `aulos-api`. WP-14 therefore
  has to apply `PUBLIC_HOST_URL`/`PUBLIC_HOST_AUDIO_URL` plus percent-encoding when it serves a
  view — including views read straight out of WP-13's published snapshot — and the same goes for
  `FileRef.download_url` on the two artifact lists.
- **`OutTmpl` is built from the config templates only.** **APPLIED (wave-2 integration).** The
  pre-resolution now happens in the `ytdlp` provider, which is the only place that holds both the
  entry blob and the shim, exactly as this bullet proposes. Three coordinated pieces:
  `aulos_provider::outtmpl_info` is the shared DESIGN §7.5 key filter (`^(playlist|channel)`,
  `n_entries`, `__last_playlist_index`); `aulos_provider_ytdlp::outtmpl_job(cfg, req, entry)` is
  `build_outtmpl` plus `merge_info` over that subset; and `YtdlpProvider::download` calls it and
  `resolve_outtmpl` instead of using `ctx.outtmpl`. `DownloadCtx::outtmpl`'s doc no longer claims
  the fields are pre-resolved, because for the engine's copy they are not.

  **This exposed a second, larger DESIGN §7.5 violation, also fixed here.** `compact_entry`
  persisted the *whole* `state` object for a plain provider — for a yt-dlp playlist child that is
  the entire raw info dict, tens of kilobytes per row, bounded only by the 256 KB
  `AULOS_ENTRY_MAX_BYTES` cap. §7.5's table allows only the key set above, which is precisely what
  `outtmpl_job` needs, so the two problems had one fix. A non-object `state` is still kept
  verbatim (nothing in §7.5's rules applies to it, and the `fake` provider's script state is one). `Engine::outtmpl_for` reproduces legacy's
  prefix handling and the `OUTPUT_TEMPLATE_PLAYLIST`/`_CHANNEL` swap, but **not** legacy's
  `_resolve_outtmpl_fields` pre-resolution: that is `aulos_provider_ytdlp::outtmpl::OutTmplJob`,
  and `aulos-queue` may not depend on that crate (DESIGN §3, `tests/arch.rs`). The natural home for
  it is the ytdlp provider itself, which already holds both the `MediaEntry` and `OutTmplJob`;
  otherwise `%(playlist_id)s`-style fields degrade to `NA` (WP-06's note).
- **`DownloadCtx::tmp_dir` is a per-job directory, `<TEMP_DIR>/<item id>`, not the shared
  `TEMP_DIR` legacy passed.** `aulos-provider-sc` uses `ctx.tmp_dir` *as* its segment directory
  (`engines::temp_dir`), so two concurrent StreamingCommunity jobs sharing it would interleave
  their segments. It also makes the DESIGN §8.7 partial cleanup one `remove_dir_all`, and keeps a
  paused yt-dlp job's `.part` exactly where the resume looks for it. Boot recovery's orphan scan
  understands both shapes: a loose `*.part`/`*.ytdl` file and a scratch directory whose name is an
  `ItemId` no row claims.
- **`streamingcommunity` is named as a string constant in the engine**
  (`aulos_queue::entry::SC_PROVIDER`). DESIGN §7.5, §8.7 and §8.9 all state their rules per
  provider id — keep the entry blob until the NFO hook has run; remove the partials on a pause
  because the m3u8 token is dead — rather than as a capability. An additive
  `Provider::partials_resumable() -> bool` (default `true`) plus
  `Provider::keeps_entry_after_success() -> bool` would let the engine ask instead of knowing, and
  would drop the constant.
- **`GroupAcc` has one field DESIGN §8.6's struct does not: `active_percent`.** The count-weighted
  fallback is written there as `Σ_active(child.percent / 100)` without saying where that sum lives,
  and it has to be a field — walking 500 children per 250 ms tick is exactly what the accumulator
  exists to avoid. **WP-13 owns three of the nine fields** (`downloaded`, `speed`,
  `active_percent`): they are progress-derived, and progress never enters the engine (DESIGN §2.2).
  They are `pub` for that reason.
- **The five-minute drift pass corrects and WARNs; it does not `debug_assert_eq!`.** DESIGN §8.6
  asks for the assert in debug builds, but a debug build is exactly where the acceptance test for
  this path runs, and aborting the process is a strictly worse outcome than the corrected counter
  the pass exists to produce. `GroupAcc::correct` returns whether it drifted.
- **`Engine::write_status` chains a transition when the direct edge is illegal.** A provider is not
  obliged to report every stage — the `fake` provider's default script and any downloader that
  produces its file in one go hand the engine `Finished` while the row still reads `preparing`, and
  DESIGN §4.2 has no `Preparing → Finished` edge. `engine::status_chain` walks the forward run of
  the happy path, all hops land in **one** transaction, and **one** frame is published, so the
  persisted column is legal at every step while the wire never shows a state the item held for a
  microsecond. `Resolving → Preparing` is still refused: the only edge out of `resolving` is to
  `queued`, and inventing a chain through it would let a resolve result overwrite a cancel.
- **The dedupe index holds two keys per resolved item.** DESIGN §8.5 says the canonical target is
  the `media_id` "once resolved", but a re-add arrives with a URL and nothing else, so the
  URL-derived key has to stay in the index alongside the `media_id`-derived one — otherwise
  re-adding the URL of an item that is still queued creates a second item, which is the exact
  legacy bug §8.5 exists to close. `Engine::drop_dedupe` is therefore a value scan, bounded by the
  live queue.
- **`canonical_key` delegates to `aulos_store::canonical_key`** (WP-05's note), and
  `aulos_queue::DedupeKey` implemented `Hash` by hand because `aulos_core::Selection` derived `Eq`
  but not `Hash`. **APPLIED (wave-2 integration):** `Selection` now derives `Hash` — all four of
  its fields already did, and they are all canonical, so the derived hash and the hand-written one
  are the same function — and `DedupeKey` derives it too. Twelve lines and one import gone.
- **`AddError` is one variant per HTTP status, not DESIGN §8.3's six.** `Invalid { index, errors }`
  carries every failing field as a `WireError`, so the API layer maps `errors[0].code` straight onto
  the §8.3 table (`validation_failed`, `unknown_preset`, `overrides_disabled`, `folder_invalid`,
  `unsupported_url` → 400; `Duplicate` → 409 in strict mode; `TooManyUrls` → 413). `index` says
  which request in a batch failed, which DESIGN's shape could not express. A batch fails **whole**:
  that is what "`AddError` → HTTP status" means, and partial acceptance would need a different
  `AddOutcome`.
- **`Registry::pick` → `None` is mapped to `unsupported_url`** with the verbatim
  `Unsupported resource "<url>"` (WP-03's request), on both the add path and the resolve path.
- **Cancel writes the terminal status immediately and ignores the job's later report.** `RunSlot`
  carries a `settled` flag; a cancel or a pause sets it, and the task's own `Finished`/`Failed` is
  then discarded rather than overwriting what the user asked for. This is what makes the HTTP
  response independent of `SIGKILL` (DESIGN §8.7).
- **`/proc` is not scanned by the cancel test.** The acceptance list asks for a `/proc` scan plus a
  directory scan; the `fake` provider spawns no process, so there is nothing to scan for, and the
  process-group kill itself is `aulos_provider::proc`'s own tested surface (WP-03). The directory
  scan is asserted, on both a `.part` and a `.ytdl` file.

## WP-13 — `aulos-queue`: aggregator, event hub, replay ring, published snapshot

- **`WireFrame.text` is `bytes::Bytes`, not `axum::extract::ws::Utf8Bytes`.** DESIGN §15.3 names
  the axum type, which would make `aulos-queue` depend on `axum` and invert the §3 dependency
  direction (`tests/arch.rs` would fail). `bytes` is in §18.6 for exactly this job ("pre-serialised
  WS frames shared across clients without copies"). **WP-14 converts once per frame** with
  `Utf8Bytes::try_from(frame.text.clone())` (or `Message::Text`), which is a refcount bump plus a
  UTF-8 scan; `WireFrame::as_str()` is there if a `&str` is easier.
- **`EventHub` is `Clone` over an `Arc<Inner>`.** DESIGN §15.3 writes `ring: Mutex<Ring>` inline in
  the struct, but the aggregator task owns one hub and every HTTP handler needs another. Nothing
  else about the shape changed. `publish_frame` holds the ring lock across
  allocate → serialise → record → broadcast on purpose: a frame's `seq`, its ring position and its
  bus position are one fact, and two publishers that allocated first and locked second could
  interleave them. The lock is never held across an `await`.
- **`EventHub::publish_frame(FrameBody)` is the aggregator's entry point, additional to DESIGN's
  `publish(kind, body: impl Serialize)`.** `publish` is kept verbatim and is what the passthrough
  kinds (`subscription`, `subscription_removed`, `ytdl_options`, `providers`, `notice`, `health`)
  use; the four item kinds must go through `publish_frame`, because a `serde_json::Value` cannot be
  folded by the resume merge. `publish` refuses a non-replayable kind (`snapshot`, `resume`,
  `pong`, `error`) with an ERROR log and no frame: those are per-connection and **WP-14 builds them
  itself** from `StateView::load()` and `EventHub::resume()`, using `published.seq` for a snapshot
  and `hub.head()` for a `pong`/`error`.
- **`RingEntry` carries a `FrameBody`, not DESIGN §15.3's `batch: Option<Arc<DeltaBatch>>`.** The
  §15.3 merge table has a rule for `added`, `completed` and `removed` too, and folding those out of
  the serialised text would mean re-parsing JSON the server had just produced. `FrameBody::Delta`
  is the design's `batch`; the other four variants are what make the rest of the table expressible.
- **`Resume::Merged` carries a fourth field, `merged: MergeCounts`.** PROTOCOL §6.3's `resume`
  frame has to state `{added, completed, removed, delta_items}`, and WP-14 cannot recover those
  from a `Vec<Arc<WireFrame>>` without parsing them back. `merged.removed` is a **total id count**
  across the reason groups, as PROTOCOL says.
- **Every merged frame carries `seq = fold.to`.** A fold replays a window rather than issuing new
  frames, and PROTOCOL §6.3 tells the client its cursor is `to` once it has applied them all;
  minting fresh sequences per resuming client would consume `seq` and desynchronise `to`. So the
  "strictly increasing `seq`" rule holds for the live bus but a resume burst is flat — WP-14 should
  emit the `resume` envelope with `seq = to` as well.
- **The merge is "last word wins", which is stronger than the DESIGN §15.3 table and is what makes
  it equal to a replay.** The table says a `removed` drops an earlier `added`; the proptest in
  `tests/realtime.rs` found two more cases the fixed emission order needs: a **later**
  `added`/`completed` must drop an **earlier** `removed` (a delete racing a terminal write would
  otherwise fold into "create it, then delete it" and lose a row), and an `added` and a `completed`
  for the same id must keep only whichever came last (`completed` is emitted after `added`, so
  keeping both would always let the terminal object win). Both are implemented and tested; DESIGN
  §15.3's table could be amended with the symmetry.
- **`Aggregator::with_done_total(u64)` is additive and WP-17 must call it** with
  `RecoveryReport::terminal_total`. DESIGN §15.2 annotates `Published::done_total` "from SQLite",
  but the aggregator holds no `Store` — that is what makes "500 items with zero database round
  trips" structural rather than a discipline — and `Engine::recover` publishes only the bounded done
  *window*. Without the seed a restart reports `done_total` as the window length and every client
  believes its history was truncated to 500 rows. After the seed the counter moves on its own: `+1`
  when a known non-terminal row becomes terminal, `-1` when a terminal row is removed or retried,
  and unchanged when one is evicted from the window or first appears already terminal.
- **`RemoveReason`'s wire strings differ from PROTOCOL §5.7.** **DECIDED AND APPLIED (wave-2
  integration): the documents win.** DESIGN §15.1 and PROTOCOL §5.7 both spell the four reasons
  `deleted`, `cleared`, `auto_cleared`, `group_cascade`, and BRIEF is silent, so the design docs
  decide (BRIEF's own precedence rule). `aulos_core::RemoveReason` keeps the variant names
  `Expired`/`Replaced` — they say what happened to the row — and gains
  `#[serde(rename = "auto_cleared")]` / `#[serde(rename = "group_cascade")]`, an `as_str` and a
  `Display`. One test now walks all four in PROTOCOL's order and asserts serde, `as_str`,
  `Display` and the round trip agree, so they cannot drift. Three assertions in `aulos-queue`
  updated. **Decided before the iOS client shipped, which is what the note asked for.** PROTOCOL and DESIGN §15.1 spell the
  four reasons `deleted`, `cleared`, `auto_cleared`, `group_cascade`; `aulos_core::RemoveReason`
  (WP-02) names the last two `Expired` and `Replaced`, so the frames say `"expired"` and
  `"replaced"`. The **order** is the same, and `aulos_queue::REASON_ORDER` is the single source of
  it for both the live flush and the resume fold. Fixing the wire strings is a two-line
  `#[serde(rename = …)]` plus `as_str` change in `aulos-core::event`, which is not this package's
  file; either that or PROTOCOL §5.7 should be amended. **Decide before the iOS client ships.**
- **The aggregator keeps its own `GroupAcc` per group, and it is the one that reaches the wire.**
  The WP-12 note assigns `downloaded`, `speed` and `active_percent` to WP-13, but the engine owns
  the `GroupAcc` instances and there is no aggregator → engine group-progress message, so the
  aggregator mirrors the whole accumulator from the child views it already sees (folded in O(1) per
  child change, with a five-minute recompute for float drift) and writes its
  `percent`/`speed`/`eta` onto the group row. The engine stays authoritative for
  `children_total`/`_done`/`_error`/`_active`, which the aggregator never touches. The one input
  this loses is a **queued** child's `filesize_approx`, which lives in the entry blob and not on the
  wire: before any child starts, both accumulators say 0 %, and once every child has finished (real
  `size`) or started (`total_bytes` from its first progress frame) the mirror is byte-weighted and
  strictly better than the engine's, because it includes the in-flight bytes the engine
  structurally cannot see. In between, a group with some still-queued children falls back to the
  documented count-weighted percent. Closing the gap properly means either an additive
  `EngineCmd::GroupProgress { group, downloaded, speed, active_percent }` (a WP-12 file change) or
  moving `GroupAcc` ownership into the aggregator; both are out of this package's lane.
- **The aggregator forwards `Stage`/`File` with `EngineHandle::stage`/`file`, which `await` an
  `EngineCmd` send.** There is a theoretical cycle — engine → `DomainEvent` (4 096) → router →
  aggregator inbox (1 024, `Block`) → aggregator → `EngineCmd` (1 024) → engine — that could
  deadlock if all three channels filled at once. DESIGN §15.1 asks for exactly this forward and the
  harness pump WP-12's tests use does the same. If it ever bites, the fix is a one-task forwarder
  owning the `EngineHandle` and fed by its own bounded channel, so the aggregator never awaits the
  engine.
- **A flush emits up to four `delta` frames, then leaves the rest dirty.** DESIGN §15.1's worked
  example ("1 000 dirty items produce 5 frames of 200 … every item within 4 ticks") only closes if
  a flush may emit several consecutive frames, so `MAX_DELTA_FRAMES_PER_FLUSH = 4`: 1 000 dirty ids
  become four frames on one tick and one on the next. The `dirty` set is an `IndexSet` drained from
  the front and re-dirtied at the back, which **is** the "persistent round-robin cursor" — there is
  no separate index. `added`/`completed` are chunked at `AULOS_WS_MAX_DELTAS_PER_FRAME` too, so
  `Engine::recover`'s single 1 000-view `Added` event cannot produce one enormous frame.
- **A dirty id with no `last_sent` baseline is promoted to an `added` upsert, not dropped.** A
  `delta` may never introduce a record (PROTOCOL §5.4), so a `StatusChanged` that arrives without a
  preceding `Added` — which should not happen, but would be an invisible lost row if it did —
  becomes a full object in the `added` position instead.
- **The urgency classifier is generated from the same field list as the diff**, with the nine
  numeric progress fields marked `num` and everything else urgent. DESIGN §15.1 states it as "text
  is urgent" (`msg`, `title`, `phase`); implementing it as "everything except the numbers" also
  covers `status`, `error`, `filename`, `size` and the group counters, which a client wants
  promptly for the same reason and which the design's own table lists as urgent under `Stage` /
  `Added` / `Completed`. `aulos_queue::text_changed` exposes the narrower rule PROTOCOL §5.4 states
  to client authors. The generated `DIFF_FIELDS` is declared with length `ItemView::FIELDS.len()`,
  so adding a field to `ItemView` without classifying it here is a **compile error**.
- **`aulos_queue::protocol_block(&Config)` is the `snapshot`'s `protocol` object** (PROTOCOL §5.3),
  ready for WP-14 to inline into both the WS `snapshot` and `GET api/v2/state`.
- **`Published.truncated.groups` is always empty and `by_id` indexes the concatenation.** The BRIEF
  CUTs group collapsing, so every non-terminal child ships inline; `by_id[id]` is a position in
  `items`, or `items.len() + i` for the `i`-th entry of `done`, and `Published::get` / `all()` hide
  that. `by_id` is pointer-equal across a tick with no membership change, which is the DESIGN §15.2
  reuse optimisation.
- **`ViewExtras.download_url` is still `None` in the published snapshot.** **DECIDED (wave-2
  integration): the patching stays; the engine-side formatter is not built.** `aulos-api` fills the
  field on every surface and `stress_consistency` proves the paths agree, so the wire is correct
  today. The proposed formatter is a pure optimisation — one parse per frame per socket, for three
  of fourteen frame kinds — with a *silent* failure mode: it is wiring a build can forget, and
  forgetting it turns every `download_url` on every surface `null`. The one part that looked like a
  real correctness gap is not one: a `delta` is only emitted for an id whose full object has
  already gone out, and that publish marks the state dirty, so `Published` always holds it by then
  — the `DownloadType::Video` fallback is unreachable defensive code, in the same sense as
  `SkipReason::NotCancelable`, and `aulos_api::view`'s module docs now say so. Revisit if the WS
  fan-out ever shows up in a profile. The aggregator only
  merges progress over what the engine gave it, so WP-14 must apply
  `PUBLIC_HOST_URL`/`PUBLIC_HOST_AUDIO_URL` plus percent-encoding when it serves an `ItemView` —
  including views taken straight out of `StateView::load()` and out of the `added`/`completed`
  frames' JSON. Note that this means a frame's `download_url` is currently `null` on the wire:
  either WP-14 fills it before the view reaches the aggregator (the engine's `ViewExtras` is the
  natural place, via a wiring-time formatter) or the field has to be filled by the client from
  `filename`. **This needs a decision at integration time**; the cleanest fix is an additive
  formatter passed to `Engine::new`, which is a WP-12 file change.
- **The `ack` frame is CUT (BRIEF), so nothing takes a client cursor.** The PLAN's `ack` acceptance
  bullet is covered by its substance instead: `hub::tests::one_client_catching_up_never_shortens_
  another_clients_window` proves `floor` moves only on the frame and byte bounds, and
  `aggregator::tests::an_item_added_and_removed_in_one_window_leaves_no_row` plus
  `a_rest_readers_cursor_is_never_newer_than_the_socket` cover the other two halves of that bullet.

## WP-16 — `aulos-subscriptions` and `aulos-telegram`

### Things the integrator must act on

- **`SubCmd::Add` carries only `url`, `selection` and `folder`.** **APPLIED (wave-2
  integration).** It now carries `request: Box<DownloadRequest>` and
  `check_interval_minutes: Option<u32>` — PLAN WP-16's `create(req: DownloadRequest, interval:
  u32)`, which is what the note asked for. `custom_name_prefix`, `auto_start`,
  `split_by_chapters`, `playlist_item_limit`, both subtitle fields and both `ytdl_options_*` reach
  the record instead of being replaced by config defaults, so **the v1 parity gap on
  `POST <p>subscribe` is closed**; an empty `chapter_template` still means "use the configured
  default" and a `None` interval still means `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL`. Both API routes
  dropped their follow-up `SubCmd::Update` workaround (v1 and v2). `NewSubscription` shrank to the
  two resolved fields plus the template.

  One consequence worth knowing: the command now carries a **typed `Url`**, so
  `SubError::MissingUrl` is unreachable by construction. It was already unreachable over HTTP —
  `tests/v1_golden/MANIFEST.json` records that `parse_download_options` rejects a falsy `url`
  first — and the string is still pinned in `aulos_api::v1::legacy`. The test that drove that
  branch was retargeted onto the `_normalize_url` trim/uniqueness parity it shared a block with,
  which *is* still reachable, rather than deleted. `aulos-core::subscription`
  (WP-02) declares it that way, but legacy's `POST <p>subscribe` accepted the whole download
  template — `check_interval_minutes`, `custom_name_prefix`, `auto_start`, `playlist_item_limit`,
  `split_by_chapters`, `chapter_template`, `subtitle_language`, `subtitle_mode`,
  `ytdl_options_presets`, `ytdl_options_overrides` — and PLAN WP-16's interface block shows
  `SubscriptionsHandle::create(req: DownloadRequest, interval: u32)`. The manager therefore fills
  the missing fields from the effective config today:
  `check_interval_minutes = SUBSCRIPTION_DEFAULT_CHECK_INTERVAL`,
  `chapter_template = OUTPUT_TEMPLATE_CHAPTER`,
  `playlist_item_limit = DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT`, and
  `SubscriptionRecord::new`'s defaults for the rest. **A caller cannot yet set a subscription's
  interval, and that is a v1 parity gap on `POST subscribe`.** The fix is additive and small:
  add the fields to `SubCmd::Add` (the enum is `#[non_exhaustive]`, and nothing outside WP-16
  matches on it yet), then extend
  `aulos-subscriptions::manager`'s `NewSubscription { … }` construction in `on_cmd` — the struct
  already has the fields, so it is a one-block change with no new plumbing. WP-14 should wire
  `PATCH`/`POST` bodies through once that lands. Not done here because changing an existing
  `aulos-core` signature is outside WP-16's ownership.

- **`Notifier` is declared in `aulos-telegram::watch`, not in `aulos-core::event`.**
  **APPLIED (wave-2 integration):** moved to `aulos-core::event`, next to `DomainEvent` and
  `EventRouter` where DESIGN §12.6 puts it, with `pub use aulos_core::event::Notifier;` left in
  `aulos-telegram::watch`. `aulos-core` already had `async-trait`, so no dependency and no `§3`
  row changed, and no call site moved. A future APNs crate can now implement it without depending
  on `aulos-telegram`. DESIGN §12.6
  wants it next to `DomainEvent` and the `EventRouter` so a future APNs crate can implement it
  without depending on `aulos-telegram`; `aulos-core` does not declare it yet, and adding a trait
  there is another crate's file. The shape is the design's verbatim
  (`id`, `interested(&ItemView)`, `async on_event(&DomainEvent)`). Moving it is a cut-and-paste
  plus a `pub use aulos_core::event::Notifier;` re-export here, and no call site changes:
  `TelegramActor` consumes its `EventInbox` directly (DESIGN §12.1), so nothing dispatches
  *through* the trait yet.

- **`aulos-api` (WP-14/WP-15) needs its own copy of the two subscription projections.**
  **VERIFIED, no change.** `aulos-api` has them and `v1_golden` plus
  `v1_routes::the_subscription_routes_answer_the_legacy_thirteen_keys` pin the shapes. The
  suggested lift of `parse_enabled` into `aulos-core::subscription` is **not** done: `aulos-api`'s
  own `parse_bool` already implements the legacy `_coerce_bool` token set and is exercised by the
  golden corpus, so the lift would move code without removing any. It cannot
  depend on `aulos-subscriptions` (DESIGN §3), so `aulos-subscriptions::public` is the *reference*
  implementation and its tests are the normative shapes: `to_v1_dict` (exactly
  `SubscriptionView::V1_KEYS`, with `last_checked` divided by 1000 into a **float**), `v2_frame`
  (`{"t":"subscription","seq":…,"subscription":{…16 keys…}}`) and `v2_removed_frame`
  (`{"t":"subscription_removed","seq":…,"ids":[…]}` — an **array**, even for one deletion).
  `parse_enabled` (the legacy `_coerce_bool` port, `true|1|on` / `false|0|off`, and
  `enabled must be a boolean` on anything else) lives there too and should be lifted to
  `aulos-core::subscription` if `aulos-api` wants to share it rather than re-implement it.

- **`DomainEvent::SubscriptionRemoved` carries one `SubId`, the frame carries an array.** The
  manager publishes one event per deleted id (a `DELETE` of three ids publishes three), so
  `aulos-api` must either batch them into one `subscription_removed` frame or emit one frame per
  id with a single-element array. Either is protocol-legal; PROTOCOL §5.9's example is one frame
  with an array.

### Deliberate deviations, and why

- **`aulos-subscriptions` does not take `arc-swap`.** DESIGN §3's row does not budget it and
  `tests/arch.rs` enforces the row, so the live `YTDL_OPTIONS` snapshot reaches the checker through
  a one-method trait, `aulos_subscriptions::check::OptionsSource`. `aulos-server` should implement
  it over the `Arc<ArcSwap<YtdlOptions>>` it already owns (four lines); `StaticOptions` is the
  no-reload implementation for `check-config` and the tests.

- **`aulos-telegram` does not take `regex`.** Same reason. The legacy URL pattern
  `https?://[^\s<>()\[\]{}"']+` is a literal prefix plus a negated character class, so
  `urls::extract` scans it by hand; `URL_PATTERN` is kept as a documented constant and the port is
  covered by the legacy accept/reject table. One parity consequence is pinned by a test: a
  **bracketed IPv6 URL is not extracted from a message at all**, because `[` and `]` are in the
  pattern's negated class — legacy behaved the same way. `urls::validate` still rejects `[::1]`,
  and that is the entry point every other caller uses.

- **`aulos-telegram` has a `transport` module DESIGN §3 does not list.** DESIGN §12.1 types the
  actor's field as `teloxide::Bot`, which can only be exercised against `api.telegram.org`; PLAN
  WP-16 requires every command text, every callback text and the whole rate-limit ladder to be
  asserted "against a mocked bot transport, never a real token". `transport::Transport` is the
  three-call seam (`sendMessage`, `editMessageText`, `answerCallbackQuery`),
  `TeloxideTransport` is the shipping implementation, `MockTransport` is the test one, and
  `TelegramActor::new` still builds the real one from the token exactly as the design says.
  `transport::poll_updates` is the long-polling loop and `transport::to_incoming` the update
  mapping; **WP-17 should spawn `poll_updates(transport.bot().clone(), actor.incoming(), shutdown)`
  next to `actor.spawn(inbox)`**, and take `actor.incoming()` *before* `spawn` consumes the actor.

- **The global `governor` limiter is driven by the injected `Clock`, not by `quanta`.** A private
  `governor::clock::Clock` adapter over `aulos_core::Clock` is what makes the per-chat rules
  testable: with the real clock, a test that means to exercise the 3 s per-chat interval trips the
  20/s global burst instead and passes for the wrong reason. The per-chat half is hand-rolled
  because a GCRA `Quota` is immutable and the `429` rule changes a chat's interval at runtime.

- **The board's change detection compares the body, not the message.** `render::render_board` is
  `render_body(lines)` plus an `updated HH:MM:SS` footer, and only the body is compared against
  `last_rendered`. Comparing the whole message would make every 1 Hz tick a change and there would
  be no `message is not modified` guard at all — which is the one rule DESIGN §12.4 says breaks
  naive implementations.

- **The subscription backoff is DESIGN §14.2's formula, not PLAN's prose.** DESIGN says
  `min(interval * 2^min(failures, 8), AULOS_SUB_BACKOFF_MAX_SECS)`; the PLAN acceptance bullet
  describes the curve as "1, 2, 4 …", which is the same doubling shape counted from a different
  starting point. The implemented curve after 0, 1, 2 … failures is
  `interval, 2×, 4×, 8× …` capped, and `model::tests` pins every value.

- **`Manager::load` writes the computed first-check time back to `next_due`.** DESIGN §14.2 only
  says the first check is at `now + AULOS_SUB_FIRST_CHECK_DELAY_SECS + jitter(0..30 s)`; persisting
  it means `healthz.components.subscriptions.next_due_in_s` and the `subscription` frame report the
  schedule the timer is actually on, instead of a stale value or `null`. One batched transaction at
  boot.

- **`EngineCmd::Add` is all-or-nothing, so a subscription check retries the batch minus the
  rejected entry.** DESIGN §14.3 step 6 wants one `Add`, and parity wants a failing entry left
  unseen with its message collected into `error`. The happy path is one round trip; each rejection
  costs exactly one more, bounded by the batch length.

### BRIEF scope trims applied here

- `AULOS_TELEGRAM_WATCH_ALL` defaults to **`true`**, per the BRIEF table — the legacy blind spot
  (web and subscription downloads invisible to the bot) is treated as a bug. `false` reproduces it
  exactly, and `tests/actor.rs` asserts both.
- No Prometheus metrics: `edits_throttled_total` is exposed on `TelegramHealth` for `healthz`
  (DESIGN §12.4 asks for it there) and nowhere else.

## WP-14 — `aulos-api`: v2 REST, WebSocket, files, health, auth

### Things the integrator must act on

- **`aulos-api` now declares `aulos-provider`, and `tests/arch.rs`'s table was amended by one
  line.** **APPLIED (wave-2 integration):** DESIGN §3's `aulos-api` row now lists the crate, and
  `arch.rs`'s amendment comment became a plain explanation, since the document and the table agree
  again. No §3 rule changed. DESIGN §3's `aulos-api` row omits the crate, while PLAN WP-14's `ApiState` types
  `registry: Arc<RwLock<Registry>>` and `GET api/v2/{catalog,providers,resolve-preview}` and the
  add path's catalog defaults are all projections of it. No §3 *rule* forbids the edge (A1 is about
  provider crates depending on the store or the queue; A2–A5 are untouched), so the row gained
  `"aulos-provider"` with an `// Amendment (WP-14)` comment next to WP-01's and WP-08's. **DESIGN §3
  should gain the same word.** The alternative — a port trait in `aulos-api` implemented by
  `aulos-server` over the registry — would have mirrored `Match`, `MatchReason`, `ProviderState`
  and `FormatCatalog` for no architectural gain.
- **`CancelScope::Generation` cannot isolate one add today, and one line in `aulos-queue` fixes
  it.** **APPLIED (wave-2 integration), but it took more than one line, and the one-line version
  would have been a bug.** `add_generation` was doing two jobs: stamping an add *and* answering
  "has a blanket cancel happened since this work was spawned?" (the `self.add_generation >
  meta.generation` checks). Bumping it per add would make a *later add* condemn an earlier one's
  in-flight resolution. So the two jobs are now two counters: `Engine::add_generation` is the
  per-add stamp (`handle_add` increments it, so the first add is generation 1 and 0 means "no
  add"), and `Engine::cancel_epoch` is bumped only by `CancelScope::All` and is what the `>` checks
  compare against. `ResolveMeta` and `Expansion` carry the epoch they were spawned under. A retry
  mints a fresh generation rather than borrowing the last add's, so only `All` can condemn it.

  Three tests changed: `add::a_single_add_…` now expects generation 1;
  `resolve::cancel_resolve_by_generation_leaves_a_concurrent_add_running` finally asserts what its
  name says (and that `All` still gets the survivor); and `rest_queue`'s `todo(WP-12)` assertion
  flipped to `assert_ne!` plus a check that the concurrent add is untouched. `Engine::handle_add` reads `self.add_generation` without incrementing it, and only
  `CancelScope::All` bumps it, so two adds that race share a generation and
  `POST api/v2/downloads/cancel-resolve {"generation": n}` cancels both. The wire side is complete
  — the `202` carries `generation`, the route maps `{"generation": n}` → `Generation(n)` and `{}` →
  `All`, `capabilities.features` advertises `cancel_resolve` — so the fix is `self.add_generation
  += 1` (or a separate per-add counter) in `crates/aulos-queue/src/add.rs`, which is a WP-12 file.
  `rest_queue::cancel_resolve_scopes_by_generation_and_falls_back_to_everything` asserts today's
  behaviour and carries a `todo(WP-12)` on the one assertion that will flip.
- **`ServerInfo.yt_dlp` is `None` until the binary fills it in.** **STILL OPEN — owner is
  WP-17.** `capabilities.yt_dlp`, `GET <p>version`'s `yt-dlp` and `healthz.yt_dlp` all say `null`
  until `aulos-server` calls `ApiState::with_info(...)` with the shim's answer. `capabilities.yt_dlp`,
  `GET <p>version`'s `yt-dlp` and `healthz.yt_dlp` all read it, and `aulos-api` may not depend on
  `aulos-provider-ytdlp` (DESIGN §3). WP-17 should call
  `ApiState::with_info(ServerInfo::new(&cfg, clock).with_yt_dlp(runner.identity().yt_dlp))` once
  the shim has answered; until then the three surfaces say `null`, which is honest.
- **`healthz` synthesises `store` and `queue` when the registry has none, and nothing else.**
  **VERIFIED, no change.** The other thirteen components remain WP-17's probes.
  The other thirteen components of DESIGN §16.3 are WP-17's probes; `rest_meta::
  the_healthz_payload_is_stable_for_the_stock_component_set` seeds the full stock set by hand and
  snapshot-tests the payload, so the document and the wire are pinned to each other. The 503 rule
  is implemented here: `HealthView::is_fatal()` (a `down` store) **or** a WAL over 256 MB.
- **`GET api/v2/items?q=` filters the page, not the query.** **APPLIED (wave-2 integration).**
  `ItemFilter` gained `title_like: Option<Box<str>>` and `where_clause` a
  `title LIKE ? ESCAPE '\\'` term, so `q` is part of the query: `total` counts the matching set,
  every page is full and the cursor continues the *filtered* set. `%`, `_` and `\\` in the needle
  are escaped, so a title containing a wildcard is matched literally; an empty or whitespace needle
  is no predicate at all. `title` is `TEXT`, so SQLite's `LIKE` is already ASCII-case-insensitive
  and no `lower()` is needed on either side. The page-side `retain` is gone. `aulos_store::ItemFilter` has no title
  predicate, so `q` is applied to the rows the keyset query returned and `total` stays the
  unfiltered count. An additive `ItemFilter.title_like: Option<Box<str>>` plus a `LIKE` clause in
  `aulos-store` (a WP-04 file) is what makes a filtered set pageable honestly.
- **`POST api/v2/items/clear` is an addition to PROTOCOL §4.7.** **APPLIED (wave-2
  integration): PROTOCOL §4.7 adopted the row**, with a paragraph on why a v2-only deployment
  needs it and on the `reason: "cleared"` frames it produces. DESIGN §8.10 defines
  `EngineCmd::Clear` and the v1 shim's `POST <p>delete {"where":"done"}` needs it, but the §4.7
  table has no v2 clear route, which would leave a v2-only deployment deleting history one id at a
  time. It answers `{"removed": [ids], "seq": n}`. **PROTOCOL §4.7 should adopt the row.**
- **`GET api/v2/catalog` (no `?url=`) reports `provider: "merged"`.** **APPLIED (wave-2
  integration): PROTOCOL §4.6 now says so**, on the `provider` row of the field table, together
  with a paragraph stating that a `?url=` matching nothing answers the merged catalog with
  `match: null` rather than an error. §4.6 defines `provider` as
  "the provider whose catalog this is" and says nothing about the union. `"merged"` is not a legal
  `ProviderId`, and `match` is `null` on the same payload, so the two facts together are
  unambiguous — but PROTOCOL should say so.
- **`GET api/v2/providers` fills `version`, `capabilities` and `argv` with `null`/`[]`.**
  **Kept as it is.** `Provider::describe()` is a real improvement but it is new surface on a trait
  five crates implement, not an integration fix; `null` remains more honest than an invention. The
  `Provider` trait exposes none of the three (DESIGN §6.1), and inventing them would be worse than
  saying nothing. An additive `Provider::describe() -> ProviderDescription` would fill them for
  every provider at once; `limits.slots` already comes from `Provider::own_slots()` and
  `fallback` is derived by asking each provider whether it answers `Match::Weak` to a URL in the
  reserved `.invalid` TLD.
- **The `subscription`/`subscription_removed` frames are published by the aggregator, not here.**
  `aulos-api` only sends `SubCmd`s; the manager publishes `DomainEvent::SubscriptionChanged` and
  WP-13's aggregator turns it into the frame (one frame per deleted id, with a single-element
  `ids` array — PROTOCOL §5.9's example batches them, and either is legal).
- **`POST api/v2/subscriptions` applies `check_interval_minutes` with a follow-up
  `SubCmd::Update`.** `SubCmd::Add` carries only `url`, `selection` and `folder` (WP-16's note), so
  an explicit interval would otherwise be silently ignored. One extra message on a rare route; it
  can be deleted the day `SubCmd::Add` grows the field.

### Deliberate deviations, and why

- **The WS session is one task, not two.** DESIGN §15.4 step 3 asks for a reader and a writer
  sharing a `CancellationToken`; splitting an `axum::extract::ws::WebSocket` needs `futures-util`,
  which is not in `aulos-api`'s §3 row. The session is one `tokio::select!` over the socket, the
  bus and the keepalive tick, and the property the two tasks existed for — a wedged writer can
  neither hold memory nor block progress — is the **send timeout**
  (`AULOS_WS_SEND_TIMEOUT_MS`, then close `1013`), which this shape enforces directly. There is no
  `ConnClosed` command and no `Drop` guard that sends one, because the watch registry is CUT; the
  `Drop` guard that remains keeps `healthz.ws.clients` from leaking on any close path.
- **The 100 ms `hello` grace before the snapshot is gone.** DESIGN §15.4 step 1 waits so that a
  `hello`'s `topics` list can narrow the snapshot — and **topic narrowing is CUT** (BRIEF), so the
  wait buys nothing and costs 100 ms on exactly the path this package exists to make fast. The
  snapshot goes out immediately and `hello` is handled in the loop like any other client frame.
  Subscribe-before-snapshot is unchanged, so no update can be lost.
- **The snapshot cursor is an `Option<Seq>`.** `seq` is allocated from **zero**
  (`HiLoAllocator::next()` returns 0 for the first frame) and `Published::empty()` also reports
  `seq: 0`, so a `frame.seq > snapshot.seq` filter silently ate the very first `added` frame of a
  fresh boot — a lost update, and the first bug this package's tests caught. `None` now means
  "nothing is reflected in this snapshot yet". A cheaper alternative is for the allocator to hand
  out `1` first, which would make `Seq(0)` mean "no frame" everywhere; that is an `aulos-store`
  change and it is worth considering, because `Published::empty().seq == Seq(0)` is a trap for any
  future reader of `Published`.
- **`download_url` is filled by patching, on every surface.** The engine leaves it `None`
  (WP-12/WP-13 notes), so `aulos_api::view` fills it: `project()` for the snapshot, `items` and
  `items/{id}`, and `patch_frame()` for the `added`/`completed`/`delta` frames — one parse and
  re-serialise per frame per socket, and only for the three kinds that can carry a file name.
  `stress_consistency` is what proves the two paths agree; if they had disagreed the reconstructed
  client state would differ from the snapshot on every finished item. A `delta` carries no
  `selection`, so the download type is looked up in `Published` and falls back to the video root.
  **The cheaper fix is still the one WP-13 proposed**: an additive formatter passed to
  `Engine::new` so `ViewExtras.download_url` is filled once, at the source; then `view::patch_frame`
  can be deleted.
- **A patched frame's JSON keys are alphabetical.** `serde_json` without `preserve_order` uses a
  `BTreeMap`, so a re-serialised frame reads `{"items":…,"reason":…,"seq":…,"t":…}`. Key order is
  not part of the protocol and no client can depend on it, but it is worth knowing before staring
  at a packet capture.
- **`ApiError` is a struct, not PLAN WP-14's three-field tuple.** `field` has to hold a *dynamic*
  name, because the engine reports validation failures as `WireError`s built from the catalog and
  `SubError::Invalid` does the same. The tuple's three positions survive as `ApiError::new`'s three
  arguments. `request_id` is stamped on the way out by one middleware (`trace::headers`) rather
  than threaded through forty handlers, keyed on an `EnvelopeStamp` response extension.
- **A request id is minted with `ItemId::new()`.** PROTOCOL §1.3 says a ULID and `ulid` is not in
  `aulos-api`'s §3 row, so the mint goes through the core newtype that wraps it. No item is
  created. A one-line `aulos_core::id::new_ulid()` would read better.
- **`Last-Modified` is formatted by hand.** `time` is not in the §3 row either, so `files.rs`
  carries Howard Hinnant's `civil_from_days` and an IMF-fixdate formatter with exact tests
  (including the RFC 9110 example and a leap day). Twenty lines, no dependency.
- **`GET api/v2/state`'s delta is folded out of the hub's serialised frames.** The hub hands back
  `Vec<Arc<WireFrame>>` (already encoded once for every reader), so the REST delta re-parses them
  into the four §4.3 buckets rather than asking the ring for a second representation. The
  passthrough kinds a window can also contain (`subscription`, `notice`, `providers`,
  `ytdl_options`, `health`) have no place in the §4.3 shape and are dropped — a polling client
  re-reads them from its next snapshot.
- **`api/v2/items`' `next_cursor` is `"<ord>.<id>"`.** PROTOCOL §4.4 says only that it is opaque;
  `base64` is not in the §3 row, and a keyset cursor that is readable in a log is easier to support
  than one that is not.
- **`SkipReason::NotCancelable` is unreachable.** The engine's cancel is idempotent from every
  state (DESIGN §8.7), so no request can produce it; it stays in the closed wire enum because
  PROTOCOL §4.2 documents it, and `rest_queue::the_reachable_skip_reasons_are_all_produced` says so
  in a comment next to the five that are.
- **`?probe=deep` re-probes providers and folds the result into the registry.** DESIGN §16.3 says
  it "re-runs the tool probes live"; the tool probes belong to WP-17, and the one live probe
  `aulos-api` can reach is `Provider::probe()`. The route is rate-limited to one real run per 10 s
  and reports `"probe": "shallow" | "deep" | "throttled"` so an operator can tell which they got.

### BRIEF scope trims applied here

- `GET <p>metrics` answers `404 not_found` from the same envelope as everything else, and neither
  `metrics` nor `metrics-exporter-prometheus` is linked.
- The WS client frames `hello` (topic narrowing), `ack`, `watch` and `unwatch` are accepted and
  produce **no** error, so a client written from PROTOCOL §5.11 is never disconnected for sending
  one. `ping`/`pong`, the `Lagged` resync, the lag budget (`1013`), the send timeout (`1013`), the
  client cap (an `error` frame then `1013`) and the 1 MiB frame cap (`1009`) are all implemented and
  tested.
- `truncated.groups` is always `[]` and a group's `children_inline` is always `true`, because the
  snapshot carries every non-terminal child.

### What the v1 shim (WP-15) needs from here

- Mount the shim in **one** place: `pub mod v1;` in `lib.rs` plus
  `router = router.merge(v1::router(state.clone()))` when `state.cfg.v1_enabled`, next to the
  documented seam in `router`'s doc comment.
- `<p>version`, `<p>robots.txt`, `<p>`, `<p>socket.io/*`, `<p>healthz`, `<p>livez`,
  `<p>download/*` and `<p>audio_download/*` are already served for **both** protocol versions;
  registering any of them again will panic at router build time. PROTOCOL §10.1 lists them under
  v1 because a v1 client uses them, not because the shim re-implements them. The `GET /` → `<p>`
  redirect (prefix ≠ `/`) is v1's and is not registered here.
- Reusable pieces: `v2::downloads::parse_one` (the add-body parser), `v2::cookies`' four legacy
  strings and its `MAX_COOKIE_BYTES`, `cors::v1` (the legacy method set), `error::ApiError` and
  `error::Json`, `view::project`/`view::public_url` for `download_url`, and
  `v2::query::lookup`/`items` for id resolution. `aulos_core::Status::v1()` is the status mapping.

---

## WP-15 — `aulos-api::v1`, the v1 compatibility shim

Three changes outside `crates/aulos-api/src/v1/`, all additive, plus the deviations from PLAN
WP-15's interface block.

### `aulos-store`: one new module

- **`crates/aulos-store/src/v1.rs` — `live_media_ids(&Store)`**, plus the one `pub mod v1;` line in
  `lib.rs`. No existing signature changed.

  DESIGN §11.4 projects v1's `id` as "the provider's `media_id` when present, else the ULID". For
  `done[]` that is free — `Store::v1_done` returns whole `Item` rows, which carry `media_id`. For
  `queue[]`/`pending[]` it is not: DESIGN §11.4 sources those from the **published snapshot**,
  because that is the only place transient progress (`percent`, `speed`, `eta`) exists, and
  `ItemView` deliberately carries no `media_id` (PROTOCOL §0 rule 3 — v2 has exactly one
  identifier). Without a side lookup an in-flight row's v1 `id` would be its ULID, which is a
  visible change to a field the legacy client displays and keys its list by.

  The lookup is two columns, no parameters, served off the `(status, ord)` index, over the
  non-terminal set — bounded by the queue's working size, not by the table. `aulos-store`'s own
  test asserts the excluded statuses are exactly `Status::is_terminal`, so this and `v1_done`
  partition the table with no row in both and none in neither.

  **If WP-02 ever adds `media_id` to `ItemView`, delete this module and the extra
  `project_history` argument below.**

### `aulos-api` (WP-14's files)

- **`lib.rs`**: `pub mod v1;` and the two-line mount at the documented seam, merged **after** the
  v2 CORS layer — `Router::layer` wraps only the routes registered so far, so this is what gives the
  shim legacy's own two-header CORS (DESIGN §11.6) instead of v2's method/`Vary`/`Max-Age` set.
- **`v2/meta.rs::default_robots()`** returned `"User-agent: *\nDisallow: /\n"`. DESIGN §11.7 pins
  the body to three `\n`-terminated lines (`Disallow: /download/`, `Disallow: /audio_download/`),
  and WP-00's `robots_txt` case captures exactly that. Now one line: it returns
  `crate::v1::legacy::ROBOTS_TXT`, which is the single definition the golden replay compares
  against.

### Deviations from PLAN WP-15's interface block

- **`project_history(active, done, media, cfg)`** takes a fourth argument — the `MediaIds` map from
  the store lookup above. `project_item(view, media_id, cfg)` likewise. Keeping the projection pure
  was the point of the PLAN's signature, and threading the lookup through the caller is what
  preserves that.
- **`migrate_legacy_request` and the request parser live in `v1/request.rs`**, and
  `parse_download_options(cfg, presets, body) -> DownloadRequest` is the shape the shim actually
  needs (the PLAN lists only `migrate_legacy_request`).
- **The `', '` joiner is unit-tested, not driven through the route.** A v1 `POST add` carries
  exactly one `url`, as legacy's `dqueue.add(url, …)` did, so `WaitResolved` is handed one id and
  the multi-message join is unreachable over HTTP. `v1::add::failures` implements it (including
  legacy's de-duplication of a playlist's shared child error) and is tested directly.
- **`aulos_v1_add_resolve_total{outcome}` is a set of process atomics**, not a Prometheus counter:
  the metrics endpoint is CUT (BRIEF scope trims). `v1::add::add_resolve_counters()` exposes
  `(ok, error, timeout, skipped)` and is what the timeout test asserts against.
- **The `print-schema`-generated JSON-Schema check is CUT** with `print-schema` itself. The three
  claims it was to encode — all three history arrays always present, `status` only ever one of the
  five legacy strings, `percent` decodes as a number — are asserted directly against live responses
  in `tests/v1_routes.rs::the_shipped_client_models_decode_every_route` and by the golden harness's
  `assert_item_shape`.
- **The golden harness lives at `crates/aulos-api/tests/v1_golden/harness.rs`** with its test target
  at `crates/aulos-api/tests/v1_golden.rs`. `tests/v1_golden/harness.rs` at the repository root
  (where WP-00's README expects it) is not a cargo target and would never run; the corpus itself is
  read in place from `<workspace>/tests/v1_golden` rather than copied.
- **`legacy_configuration()` is exposed but unrouted.** DESIGN §11.4 says the v1 shim emits
  `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` as **strings**; the
  only legacy carrier for them was the Socket.IO `configuration` event, which BRIEF §8 does not
  provide. The function and its test pin the string-typed contract in one place, ready for a
  `configuration` route if one is ever wanted.
- **`percent` is always a number in v1.** DESIGN §11.4's field table says so ("always a number;
  legacy was sometimes `null`, which the client already clamps"); PROTOCOL §10.5's "`percent` may
  be `null` in v1" is the losing side of that conflict, and PROTOCOL §0 rule 4 agrees with DESIGN.
- **`filename` and `size` are always present** (legacy omitted the keys until a file existed) and
  **`folder` is `""` rather than `null`** when there is none, which is what legacy's subscription
  path emitted and what the fixture records.

---

## Wave-2 integration pass (integrator, 2026-09-04)

### The tree

**Nothing was uncommitted and no code was discarded.** `git status` was clean after WP-12/13/14/15
(in progress)/16 landed; the only ignored path in the tree was `target/`, which `.gitignore`
already covers. No build artefacts, no tool caches, no strays. The `sh`/`py` fixture stand-ins
still carry mode 100755.

### Gates, all green on `rustc 1.95.0 (59807616e 2026-04-14)`

| Gate | Result |
|---|---|
| `cargo fmt --all` | no diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` (the CI form) | clean |
| `cargo test --workspace` / `--locked` | **1 624 passed, 0 failed**, 76 test binaries |
| `cargo test -p aulos-workspace-tests` | 12 arch + 10 packaging + 3 Δ C9 + 1 SC-equivalence |
| `cargo test -p aulos-provider-sc --no-default-features` (the `plain` client) | 165 passed |
| `python3 crates/aulos-provider-ytdlp/tests/shim_contract.py` | every check passed |
| `python3 tools/capture/verify.py` | `OK — … 131 v1 case(s) verified` |

**No test was deleted, weakened or `#[ignore]`d.** The single "ignored" line is the same
```` ```ignore ```` documentation block in `aulos-store`'s `import::canonical` module doc that
wave 1 recorded; it now *could* compile, since WP-12 created `aulos_queue::canonical_key`, but
turning a doc example into a cross-crate dev-dependency of `aulos-store` is not worth it.

Three tests were **retargeted** and two **strengthened**; each is justified where it appears
above and again under "Tests changed" below. Nothing was retargeted to dodge a failure that was
really a product bug.

### What was applied

Ten requests were real changes; five were verified as already delivered by the package they were
addressed to; three were decided *not* to do, with the reasoning recorded inline; and eight are
carried forward, all of them to WP-17/WP-18, which have not landed.

| Request (owner) | Change |
|---|---|
| `CancelScope::Generation` cannot isolate one add (WP-14 → WP-12's file) | `add_generation` split into a per-add stamp plus a `cancel_epoch`; the "one line" the note asked for would have made a later add condemn an earlier one's resolve |
| `SubCmd::Add` carries only three fields, a v1 parity gap (WP-16 → `aulos-core`) | it now carries `DownloadRequest` + `check_interval_minutes`; ten legacy `POST subscribe` fields stop being silently dropped, and both API routes lost their follow-up `Update` |
| `OutTmplJob::merge_info` with the compacted entry blob (WP-12/WP-06 → the ytdlp provider) | `aulos_provider::outtmpl_info` (the shared §7.5 filter) + `aulos_provider_ytdlp::outtmpl_job` + one call in `YtdlpProvider::download`; **and** `compact_entry` stopped persisting whole raw info dicts |
| `#[derive(Hash)]` on `Selection` (WP-12 → `aulos-core`) | derived; `DedupeKey`'s hand-written `Hash` deleted |
| `Notifier` belongs in `aulos-core::event` (WP-16 → `aulos-core`, DESIGN §12.6) | moved, re-exported from `aulos-telegram::watch`; no dependency and no call site changed |
| `RemoveReason`'s wire strings differ from PROTOCOL §5.7 (WP-13 → `aulos-core`) | serde renames to `auto_cleared`/`group_cascade` plus `as_str`/`Display`, all four pinned by one test |
| `PluginManifest.warnings` are visible nowhere (WP-14 → `aulos-provider`/`aulos-core`) | `ReloadReport.warnings`, `CommandLoadResult.warnings`, `Registry::command_warnings()`, `GET api/v2/providers`'s `warnings` and `healthz.plugin_warnings` |
| `GET api/v2/items?q=` filters the page, not the query (WP-14 → `aulos-store`) | `ItemFilter.title_like` + a `LIKE ? ESCAPE '\'` term, so `total` and the cursor describe the matching set |
| A legacy add for an advisory catalog is rejected (WP-15 → the v1 shim) | `v1::request::snap_to_advisory_catalog`, called from `POST <p>add` — a **confirmed** v1 parity break, reproduced by a test before it was fixed |
| DESIGN §3 omits `aulos-api → aulos-provider` (WP-14 → the docs) | the §3 row gained the crate; `arch.rs`'s amendment comment became a plain explanation |

### Decisions taken, where the note asked for one

- **`ViewExtras.download_url` stays patched in `aulos-api`; the engine-side formatter is not
  built.** Both WP-13 and WP-14 proposed it and both called it "the cheaper fix". It is cheaper on
  CPU — one parse per frame per socket, three of fourteen frame kinds — but it is wiring a build
  can forget, and forgetting it silently nulls the field on every surface, whereas `aulos-api`
  always holds the `Config`. The correctness gap the notes worried about does not exist: the
  aggregator emits a `delta` only for an id whose full object has already gone out, and that
  publish marks the state dirty, so `Published` always holds it. The `DownloadType::Video`
  fallbacks are unreachable defensive code and now say so.
- **`RemoveReason`: the documents win, not the implementation.** DESIGN §15.1 and PROTOCOL §5.7
  agree with each other and BRIEF is silent, so per BRIEF's own precedence rule the wire strings
  changed rather than the two documents. Decided before the iOS client shipped, which is the
  deadline the WP-13 note set.
- **A `command` plugin's warnings are keyed by directory, not by provider id**, because a
  hook-only manifest can warn without registering a provider, and they are **outside**
  `ReloadReport::is_empty` so a no-op rescan cannot publish a `providers` frame.
- **The advisory-catalog snap is confined to a download type the catalog declares.** `audio` on a
  StreamingCommunity URL still answers `400`: SC serves no audio-only rendition, and a refusal is
  more honest than silently handing back a video file. `v2` is untouched — a v2 client reads
  `GET api/v2/catalog?url=` first and deserves the honest error if it ignores it.

### Requests deliberately not applied

Each is a genuine improvement rather than an integration fix, and each is recorded inline at its
bullet: the additive per-item beat on `ProgressSink` (the factory-wide counter already gives
DESIGN §4.7's discrimination, proven by a test); `Provider::describe()` and
`Provider::partials_resumable()`/`keeps_entry_after_success()` (new surface on a trait five crates
implement); lifting `parse_enabled` into `aulos-core` (would move code without removing any); and
having the `seq` allocator hand out `1` first so `Seq(0)` means "no frame" (an `aulos-store` change
whose only current beneficiary is a comment).

### Tests changed, and why

| Test | Change |
|---|---|
| `aulos-queue add::a_single_add_inserts_resolving_and_acks_before_resolution` | expected generation `0` → `1`; it pinned the behaviour the WP-14 request asked to change |
| `aulos-queue resolve::cancel_resolve_by_generation_leaves_a_concurrent_add_running` | asserted the *opposite* of its own name ("both adds are in that generation"); now asserts the name, plus that `All` still condemns the survivor |
| `aulos-api rest_queue::cancel_resolve_scopes_by_generation_…` | the `todo(WP-12)` `assert_eq!` flipped to `assert_ne!`, plus a check that the concurrent add is untouched |
| `aulos-subscriptions check_parity::an_empty_url_is_missing_url` | **retargeted**, not deleted: `SubCmd::Add` now carries a typed `Url`, so the branch is unreachable by construction, and it was already unreachable over HTTP (the golden corpus says so). It now covers the `_normalize_url` trim/uniqueness parity it shared a block with, and still pins the legacy string |
| `aulos-core reload::an_empty_report_serialises_four_arrays` | five arrays now; a companion test pins that a warning alone is not a change |
| `aulos-core event::notice_and_reason_enums_serialise_snake_case` | **strengthened** to walk all four `RemoveReason`s and cross-check serde, `as_str`, `Display` and the round trip |
| `aulos-provider plugin_example::the_example_plugin_resolves_and_downloads_end_to_end` | see below |
| `aulos-api v1_routes` history assertions (5) | see below |

### Two flaky tests fixed, both load-sensitive, both pre-existing

Neither was caused by this pass; both surfaced because a full `cargo test --workspace` run puts 76
test binaries on the machine at once, and both passed in isolation every time.

1. **`plugin_example::the_example_plugin_resolves_and_downloads_end_to_end`** asserted
   `stages.contains(&Stage::Postprocessing)` — which the plugin's own
   `[progress] last_match_wins = true` explicitly does not guarantee, as the comment *two lines
   below the assertion* already said: `stage=mux` and `stage=done` can arrive in one read, and then
   only `done` survives. The racy assertion is gone; the `mux → postprocessing` mapping is now
   pinned deterministically off the shipped manifest's `status_map`, and each leg is already driven
   per line by `command::progress::tests::status_map_translates_mux_to_postprocessing` against the
   same spec. The runtime assertion is the one the contract guarantees: the item leaves `preparing`
   and only ever into a stage the `status_map` declares.
2. **Five `v1_routes` assertions read `GET history` immediately after `rig.settle()`**, which is six
   fixed 20 ms sleeps. `GET history` sources `queue`/`pending` from the **published snapshot**
   (DESIGN §11.4), which the aggregator refreshes on its own tick, so a row that
   `GET api/v2/items/{id}` already reports terminal can still sit in the last published generation
   — that is the documented ordering ("a REST reader's cursor is never newer than the socket"), not
   a bug. All five now poll for the condition through one `until_history_lacks` helper.

### DESIGN.md / PROTOCOL.md edits made here

- **DESIGN §3**: the `aulos-api` row lists `aulos-provider` (WP-14's request).
- **DESIGN §16.3**: the `healthz` payload gained `plugin_warnings`, with a paragraph on how it
  relates to `ReloadReport.failed` and `GET api/v2/providers`.
- **PROTOCOL §4.6**: `provider` may be the literal `"merged"`, and a `?url=` matching nothing
  answers the merged catalog with `match: null` rather than an error (WP-14's two requests).
- **PROTOCOL §4.7**: adopted the `POST api/v2/items/clear` row (WP-14's request), and documented
  the `warnings` key on `api/v2/providers` and `plugins/reload`.

**Still documentation debt**, unchanged from wave 1: the five wave-0 deviations (`ProviderId`/
`FileSlot` hoisted into `aulos-core`, `Registry::pick`/`catalog_for` returning `Option`, `OutTmpl`
in `aulos-provider`, `FormatSpec.flags.slow` on `mp4`, `ChatConfig`'s twelve keys) and the wave-1
ones recorded in prose here. This pass paid for its own changes and did not grow the debt.

### Carried forward — every request in this file that is still open

All eight are addressed to `aulos-server` (WP-17) or the e2e scripts (WP-18), neither of which has
landed. **None of them blocks WP-17 starting; two of them are on its critical path**, marked ⚠.

| Request | Owner |
|---|---|
| ⚠ Define the `HookFinalizer` newtype over the engine handle and pass it to `HookDispatcher::with_finalizer`, and wire `Engine::with_pre_terminal(...)` over `HookDispatcher`'s hook list — **without both, a `best_remux` item never finalises** | WP-17 |
| ⚠ Call `Aggregator::with_done_total(RecoveryReport::terminal_total)`, or a restart reports `done_total` as the window length and every client believes its history was truncated | WP-17 |
| Take `telegram_actor.incoming()` and `health_handle()` **before** `spawn` consumes the actor; spawn `transport::poll_updates(...)` next to `actor.spawn(inbox)` | WP-17 |
| `ApiState::with_info(ServerInfo::new(&cfg, clock).with_yt_dlp(runner.identity().yt_dlp))` once the shim has answered, or `capabilities.yt_dlp`, `GET <p>version` and `healthz.yt_dlp` stay `null` | WP-17 |
| Implement `aulos_subscriptions::check::OptionsSource` over the `Arc<ArcSwap<YtdlOptions>>` (four lines) | WP-17 |
| Call `config::load_with_warnings()` **and** `YtdlOptions::load(...)`, merging both reports before the single exit-2 step | WP-17 |
| `Registry::set_command_loader(CommandPluginLoader::with_env(PluginEnv { state_dir }))` at boot, or `{cookies_file}` renders empty | WP-17 |
| Schedule the six-hourly checkpoint and `await store.close()` on shutdown (ask for `pub async fn checkpoint(&self)` rather than reaching into `schema`); run the importer only when the DB file did not exist, handling `ImportFatal` as WP-05's note describes | WP-17 |
| Add `crates/aulos-provider-ytdlp/tests/smoke_extract.sh` and `tests/e2e/run.sh` — each turns on a CI step that no-ops today | WP-18 |

One request is carried forward **conditionally**: a children channel and a `ProgressSink` on
`ResolveCtx`. Nothing in v1.0 publishes children before a resolve returns or routes resolution logs
to an item's event stream, so it stays a design note rather than a gap.

---

## WP-17 — `aulos-server`: wiring, POT supervisor, config watcher, CLI, e2e

### The carried-forward requests, all nine of them

Every open request addressed to WP-17 in the wave-2 pass is done. Both ⚠ items are on the
critical path and both are now covered end-to-end:

| Request | Where |
|---|---|
| ⚠ `HookFinalizer` newtype + `Engine::with_pre_terminal` over the dispatcher's hook list | `adapters::EngineFinalizer`, `adapters::DispatcherPreTerminal`, wired in `wiring::run_with`. **The same `Vec<Arc<dyn Hook>>` reaches both**, so the engine's "will a pre-terminal hook run?" answer and the dispatcher's "which hooks run?" answer cannot disagree. Asserted end-to-end by `tests/server.rs::a_pre_terminal_hook_runs_and_the_item_still_finalises`, which injects a recording pre-terminal hook and checks both that it ran and that the item reached `finished` |
| ⚠ `Aggregator::with_done_total(RecoveryReport::terminal_total)` | `wiring::run_with`, step 12 |
| `telegram_actor.incoming()` / `health()` before `spawn`, plus `transport::poll_updates` | `wiring::spawn_telegram` |
| `ApiState::with_info(... .with_yt_dlp(...))` | `wiring::run_with`, step 15 — `healthz.yt_dlp` reads `2026.08.30.232658` against a real shim |
| `aulos_subscriptions::check::OptionsSource` over the `ArcSwap` | `adapters::SwapOptions` |
| `config::load_with_warnings()` **and** `YtdlOptions::load(...)` merged before one exit-2 | `bootstrap::load_config` |
| `Registry::set_command_loader(CommandPluginLoader::with_env(PluginEnv { state_dir }))` | `bootstrap::build_registry` |
| the importer only on a first start, `ImportFatal` handled as WP-05's note describes | `bootstrap::open_store` |
| `tests/e2e/run.sh` | added, with `tests/e2e/ws_watch.py` |

### Two bugs this package found in its own wiring, both fixed here

Recorded because each is the kind of thing an integration pass would otherwise re-discover:

1. **The listener bound before the recovered queue reached the published snapshot.** DESIGN §16.1
   promises "the first request already sees a consistent snapshot", but `Engine::recover` publishes
   its working set as a `DomainEvent`, which reaches `Published` only after the router and the
   aggregator have both run a tick. So the first `GET api/v2/state` (or the first WS `snapshot`)
   answered an **empty queue** for up to one `AULOS_WS_BATCH_MS` — which a client reads as "the
   server lost everything I had". `wiring::await_first_publish` now waits for the recovered row
   count before binding, with a 10 s ceiling and a WARN. `tests/server.rs::
   the_first_request_already_sees_a_recovered_queue` reproduced it before the fix.
2. **A shutdown mid-download stranded the item as `canceled`.** DESIGN §16.4 step 6 says the
   still-active rows are marked `queued`; the engine's own reaction to a cancelled job is to write
   `canceled`, which is terminal, so the next boot never picked it up and the "asserted by a
   restart that resumes them" acceptance failed. See the request below.

### Things the integrator (or the owning crate) should act on

- **`aulos-provider-ytdlp`: the shim logs at ERROR when the parent kills it.** **APPLIED (final integration).** `ytdlp_runner.py`'s `__main__` grew a `BrokenPipeError` arm that writes a `DEBUG:` line and exits `EXIT_CANCELED`, ahead of the `OSError` arm that used to catch it. Confirmed in the
  container: 0.1 s after `wiring` reports `the shutdown grace expired with downloads still running;
  killing them active=1`, the Python shim writes `ERROR: ytdlp_runner protocol channel failed:
  [Errno 32] Broken pipe` on the way out. The parent closing the pipe it is killing the child
  through is not the child's error, and the pgid kill is DESIGN §16.4 step 6 doing exactly its job,
  so a **`DEBUG`** (or a `BrokenPipeError` arm that exits quietly) is the honest level. Harmless as
  it stands — `aulos-server` forwards it at WARN, and the e2e's ERROR sweep now anchors on the
  `tracing` level field rather than the message text — but it reads as a failure in a log a user
  is looking at after a restart, which is the one moment they *are* looking.
- **`aulos-queue`: `Engine::run` cannot terminate.** **APPLIED (final integration)**, by the `EngineCmd::Shutdown` route below rather than by `drop(self.tx)`: an ack the caller can wait on is what lets step 6 stay inside the engine. `wiring::shutdown_tasks`'s aborts are gone. `Engine` keeps a clone of its **own**
  `EngineCmd` sender (`Engine::tx`, handed to every job task), so `rx.recv()` never returns `None`
  however many `EngineHandle`s the process drops. Combined with the fact that the aggregator, the
  hook dispatcher and the Telegram actor each hold an `EngineHandle` *and* wait on an `EventInbox`
  that closes only when the engine drops its `EventSender`, DESIGN §16.4's "dropping the senders
  closes the chain" **does not close**: before this was handled, every shutdown cost the full
  `TaskTracker` ceiling (measured: 20 s) and then closed the store anyway.
  `wiring::shutdown_tasks` breaks the cycle explicitly — a 500 ms flush window, then abort the
  three consumers, then a bounded await of the engine, then abort — and documents why nothing
  durable is lost (`Store::write` hands its ops to the store's *writer thread* and awaits only the
  commit ack, and `Store::close` drains that queue). Shutdown is now ~2.7 s.
  **The clean fix is one line in `aulos-queue`**: `Engine::run` should `drop(self.tx)` after taking
  `rx` (job tasks hold their own clones), or `EngineCmd` should grow a `Shutdown` variant. Either
  would let this module delete the aborts.
- **`aulos-queue`: no shutdown command, so DESIGN §16.4 step 6 is written from outside.** **APPLIED (final integration).** `EngineCmd::Shutdown { ack }` + `EngineHandle::shutdown() -> ShutdownReport` + `Engine::handle_shutdown`, which captures the ids, cancels, writes `queued`/`SHUTDOWN_MSG` and ends the loop as one uninterrupted sequence. `crates/aulos-queue/tests/shutdown.rs` pins it; `wiring` now calls it instead of writing the rows itself. The
  interrupted ids are captured from the published snapshot *before* the jobs are killed, and after
  the engine settles they are rewritten `queued` with `msg = "Interrupted by shutdown"` and
  `auto_start = true` through `aulos_store::WriteOp::SetStatus`. The store has exactly one writer,
  so "last write wins" is deterministic and the loser is the engine's `canceled`. An additive
  `EngineCmd::Shutdown { ack }` would move this back inside the engine, where it belongs.
  Verified against a real yt-dlp download: `handing interrupted downloads back to the next boot
  count=1`, then `boot recovery … scheduled=1`, then the item reads `downloading | Interrupted by
  shutdown`.
- **`aulos-core`: `EventInbox::dropped()` is unreadable after the inbox is moved.** **APPLIED (final integration).** `EventInbox::dropped_handle() -> Arc<AtomicU64>`, taken in `wiring` before `spawn` consumes the inbox, so `healthz.components.events.dropped.telegram` is measured rather than reported as `0`. The counter is
  an `Arc<AtomicU64>` the router and the inbox share, but the inbox is consumed by
  `TelegramActor::spawn`, so `healthz.components.events.dropped.telegram` is reported as `0` rather
  than measured. `HooksHealth::events_dropped` gives the hooks half honestly. An additive
  `EventInbox::dropped_handle() -> Arc<AtomicU64>` (or `EventRouter::dropped_counter(name)`) closes
  it in four lines. Every drop is still WARN-logged by the router itself.
- **`aulos-telegram`: no `health_handle()`.** **APPLIED (final integration).** `TelegramActor::health_handle() -> TelegramHealthHandle`, taken before `spawn`; `edits_throttled_total` now moves after boot. `TelegramActor::health()` needs `&self` and `spawn`
  consumes the actor, so `components.telegram` is published once, after `load()`, and
  `edits_throttled_total` stops at its boot value. `HookDispatcher::health_handle()` is the shape
  to copy.
- **`aulos-telegram`: `TelegramActor::new` hides its `teloxide::Bot`.** **APPLIED (final integration).** `TelegramActor::bot() -> Option<teloxide::Bot>` (`None` after `with_transport`, which has no token), so `wiring::spawn_telegram` polls the bot `new` built instead of rebuilding one. `telegram_will_run` stays, for the reason the next bullet gives. `transport::poll_updates`
  needs one, so the wiring builds `TeloxideTransport` itself and uses `with_transport` — which
  means the three startup gates of `new` are reproduced in `wiring::telegram_will_run`. A
  `TelegramActor::bot()` accessor, or a `new` that returns the bot alongside the actor, would
  remove the duplication. (`telegram_will_run` is needed anyway: see the next bullet.)
- **`aulos-provider`: `HookSpec` is not `Clone`.** **APPLIED (final integration).** `#[derive(Clone)]` on `HookSpec` and `HookAction`; `bootstrap::build_registry` reads the loader's cached specs and no longer walks the plugin directory a second time. `CommandPluginLoader::hooks()` caches
  `Arc<HookSpec>` for a later re-scan, but `HookDispatcher::new` and `ManifestHook::new` both take
  an **owned** `HookSpec`, and the loader's `Arc` cannot be unwrapped because the loader keeps a
  reference. `bootstrap::build_registry` therefore walks the plugin directory a second time with
  `command::scan_with` to get owned specs. Either `#[derive(Clone)]` on `HookSpec` or a
  `ManifestHook::from_arc(Arc<HookSpec>)` would drop the second walk.
- **`aulos-store`: no `checkpoint()`.** **APPLIED (final integration).** `Store::checkpoint()` over a `WriteMsg::Checkpoint` the writer runs after the in-flight batch commits; `wiring` schedules it six-hourly. DESIGN §7.1 asks for `wal_checkpoint(TRUNCATE)` "on
  graceful shutdown **and every 6 h**". The shutdown half is `Store::close()`; the six-hourly half
  is **not implemented**, because the only route to a `PRAGMA` from here is `Store::read`, whose
  `&Connection` parameter cannot be named without a `rusqlite` dependency that `tests/arch.rs`
  rule A2 forbids. `pragma wal_autocheckpoint = 512` already bounds the WAL and `healthz` reports
  `wal_bytes`, so nothing is unbounded — but an additive `pub async fn checkpoint(&self)` would let
  the wiring schedule it in three lines.
- **`.github/workflows/docker.yml`'s PR smoke step will fail.** **APPLIED (final integration).** The line is now `if docker run --rm "$img" healthcheck; then …exit 1; fi` — the non-zero exit is asserted rather than tolerated, so an "always exit 0" regression fails the build. It runs
  `docker run --rm "$img" healthcheck` under `set -eu`, and `healthcheck` against a *stopped*
  server exits **1** by design (DESIGN §3.1, and PLAN WP-17 asks for exactly that). The line was
  written when the subcommand printed "not implemented" and exited 0. `docker run --rm "$img"
  healthcheck || true` — or better, dropping the line, since the `URL_PREFIX=metube` container
  check two lines below already proves the subcommand works — fixes it. `.github/` is not this
  package's to edit.
- **`aulos-core`: `HealthRegistry`'s roll-up cannot produce the DESIGN §16.3 payload.** **APPLIED (final integration)** in `aulos-core`, as the bullet's own "proper fix" asked: `HealthView::roll_up()` caps the worst component at `degraded` unless `is_fatal()` holds, and `HealthRegistry::set` uses it. The two local workarounds stay — they are independently right — but they are no longer load-bearing. `set()`
  recomputes `HealthView.status` as the **worst** component (`fold(Ok, worse)`), so a single `down`
  component makes the whole view `down` — while `aulos-api`'s `healthz` still answers `200`,
  because §16.3 reserves `503` for an unusable store or a runaway WAL. §16.3's own example payload
  is `"status": "degraded"` with `"pot": {"status":"down"}` inside it, which that roll-up cannot
  produce. **Found by running the binary**: on a machine with no `deno` and no `N_m3u8DL-RE`,
  `healthz` answered `200 {"status":"down"}`, and a body-based `healthcheck` would then have
  restarted a perfectly working container every two minutes.

  Two things were changed here rather than in `aulos-core`: an optional tool's component is
  `degraded`, which is DESIGN §16.1 step 9's own word for those four ("ffmpeg, ffprobe,
  N_m3u8DL-RE, deno are WARN and **mark the component degraded**"); and `healthcheck` decides on
  the **HTTP status** with the body's word as the reason, so no future `down` component can make
  Docker kill a healthy server. `tests/e2e/run.sh` asserts the same way. The proper fix is one
  line in `aulos-core`: cap the roll-up at `Degraded` unless the fatal condition holds, i.e.
  `if worst == Down && name != "store" { Degraded }` — or let `HealthView` carry the roll-up and
  the fatal flag separately.
- **`aulos-core`: an engine-task panic cannot be discriminated in a panic hook.** **NOT APPLIED — no fix exists on stable.** Both routes the bullet names are still unstable (`tokio::task::Builder::name` needs `tokio_unstable`), so this stands as a known limitation rather than an open task.
  `signals::install_panic_hook` aborts on a panic on the store's writer **thread** (matched by
  name, cross-checked against `aulos-store`'s source by a test), which is DESIGN §16.4's rule for
  the store. The engine is a tokio *task*, not a named thread, and tokio exposes no "current task
  name" in a panic hook, so an engine panic is logged and the task ends — after which every
  `EngineHandle` send fails and the API answers `state_unavailable`. Making it abort needs either
  a `tokio::task::Builder::name` + an unstable `tokio_taskdump`-style hook, or the engine catching
  its own panics.

### Deliberate deviations from PLAN WP-17's interface block

- **`PotSupervisor::spawn(cfg: &Arc<Config>, health: Arc<HealthRegistry>)`** takes the config by
  reference: clippy's `needless_pass_by_value` is denied workspace-wide and the body only reads it.
  `PotSupervisor::builder(PotSettings)` is additive and is what the wiring and the tests use — it
  injects the probe, the clock, the shutdown token and the event sender, and it is what makes "a
  sidecar that hangs while its probe fails three times" testable at all.
- **`ConfigWatcher::new(...).with_shutdown(...).start()`** alongside the PLAN's
  `ConfigWatcher::spawn(targets, cfg, ytdl, events)`. Two additive arguments: a `HealthRegistry`
  (DESIGN §17.2 step 6 requires a deleted file to *degrade the component*, and the registry is the
  only place that can be said) and a `CancellationToken` (so shutdown stops the task rather than
  aborting it mid-reload). `with_force_poll(true)` is how "with inotify disabled, the poll fallback
  still reloads" is tested without touching `fs.inotify` limits.
- **`HealthRegistry` is not defined here.** It is an `aulos-core` type (WP-02), as DESIGN §3
  requires; `health::Probes` / `health::run` are this package's *writers* for the components that
  move (store latency and WAL, queue composition and slots, each hook's counters, the event drop
  counters, the subscription schedule). The boot probes write the rest once.
- **`slots.<pool>.used` in `components.queue` is derived from the published snapshot**, not read
  out of the engine's semaphores: the engine owns them and exposes no accessor, which is the
  property that makes it a single task with no `Mutex`. A StreamingCommunity job holds its own
  pool's permit *instead of* a global one (DESIGN §8.7), so the two counts partition the running
  set.
- **`store` is published by this package rather than left to `aulos-api`'s synthesis.** The
  synthesised component cannot report `latency_ms`, and DESIGN §16.3's one 503 condition is "the
  store is unusable" — which has to be *measured*. `health::store_component` times a real
  `kv_get` through the read pool; `health::tests::a_closed_store_is_down_which_is_the_only_503_
  condition` proves the 503 follows. `queue` is published for the same reason (slots and
  `progress_dropped_total` have no other source). Every other synthesis in `aulos-api` is left
  alone.
- **The DESIGN §16.4 step 2 WebSocket `1001` is one hop later than written.** A session closes with
  `1001 "server shutting down"` when its frame bus closes, i.e. when the last `EventHub` drops —
  and one lives inside the axum router's state, i.e. inside the serve future. So the sequence is:
  signal axum's graceful shutdown, give the sockets `WS_CLOSE_GRACE` (2 s) to read the close frame,
  then drop the listener task. A client that has not read it by then sees a TCP close and
  reconnects — the same observable behaviour, one hop later. An additive `EventHub::close()` in
  `aulos-queue` would make step 2 exact.
- **`ClearScheduler` is not a task.** DESIGN §16.1 step 13 lists one, but `Engine::sweep_clears`
  runs on the engine's own 1 Hz tick plus its `sleep_until(min(clear_after))` fast path
  (`aulos-queue::clear`), so a separate task would have nothing to do.
- **`SIGQUIT` dumps the runtime's own metrics, not a task list.** DESIGN §16.4 says "log every
  task's state at ERROR"; tokio has no supported way to enumerate task states, so
  `signals::dump_state` reports `num_workers` / `num_alive_tasks` / `global_queue_depth`, which is
  what an operator staring at a wedged container can actually act on. The process continues, as
  the design requires.

### BRIEF scope trims applied here

- The Prometheus `metrics` endpoint and the whole DESIGN §16.7 inventory are **CUT**.
  `AULOS_METRICS_ENABLED` is still parsed (an existing compose file must boot), and every counter
  §16.7 maps onto a `healthz` path is still reported there — `components.queue.progress_dropped_
  total`, `components.events.dropped.<subscriber>`, `components.pot.restarts`,
  `components.<hook>.runs_total`, `components.store.wal_bytes`, `components.subscriptions.failing`.
- `print-schema` and `repair-ids` are **CUT**. The allocator boot check logs a `WARN` and continues
  (`bootstrap::open_store` walks `Store::id_warnings()`), so the documented recovery command has
  nothing to recover.
- `tests/load/` and the criterion benchmarks are **CUT**.

### The e2e harness, and what could and could not be run here

`tests/e2e/run.sh` (gated on `AULOS_E2E=1`) plus `tests/e2e/ws_watch.py`, a dependency-free RFC
6455 reader — `websocat` is not installed in CI and the image ships no Python WebSocket library, so
the third option was sixty lines of framing. The script opens the socket **before** the add
(`--match` latches the item id off the first `added` frame), because a socket that connects
afterwards is correctly handed a `snapshot` and the acceptance list asks for a literal `added`.

**The image now builds and the script now passes.** `docker build -f docker/Dockerfile -t
aulos-server:dev .` produced a 796 MB `linux/arm64` image (native on this Mac; CI builds
`linux/amd64` per BRIEF §16), and `AULOS_E2E=1 tests/e2e/run.sh` ends in `END-TO-END: PASS` —
every assertion in both profiles, against the real container and the real CC-BY video.

The earlier pass could not get there: the OrbStack VM's docker data root had been filled to
read-only (`write /var/lib/docker/buildkit/metadata_v2.db: read-only file system`) by three
exhausted release builds. Restarting OrbStack (`orbctl stop && orbctl start`) remounted it
read-write with 53.6 GB free, which is all it needed — no further pruning, and the user's stopped
containers, tagged images and volumes were left alone. **If a future pass hits the same
read-only failure, restart OrbStack rather than pruning.**

What the previous pass verified against the host binary is kept below, because it is still the
finer-grained record — the **real** `aulos-server` binary, with the pinned nightly
`yt-dlp==2026.8.30.232658.dev0` in a throwaway `uv` venv, against the **real** CC-BY video
(`https://www.youtube.com/watch?v=aqz-KE-bpKQ`) over the real network:

| Assertion | Result |
|---|---|
| `healthz` carries all 15 DESIGN §16.3 components | ✅ (`pot` `disabled`, `deno`/`nm3u8dl` `down` — neither is installed on a Mac) |
| `healthz.yt_dlp` | ✅ `2026.08.30.232658`, from the shim handshake |
| `components.store.latency_ms` / `wal_bytes` / `db_bytes` | ✅ measured |
| `components.queue.slots` | ✅ `global 0/3`, `streamingcommunity 0/1` |
| `POST api/v2/downloads` → `202 {id}` before extraction | ✅ |
| the WS `added → delta → completed` sequence for that id | ✅ via `ws_watch.py`, socket opened first |
| the file lands in `DOWNLOAD_DIR` | ✅ a real 690 MB 4K mp4, and a 10 MB m4a |
| `GET download/<name>` → 200, and `Range: bytes=0-99` → `206` + `content-range: bytes 0-99/722944168` + exactly 100 bytes | ✅ |
| v1 `POST add` → `200 {"status":"ok"}`; `GET history` has `queue`/`pending`/`done` | ✅ |
| `socket.io/` → `501` | ✅ |
| restart mid-download resumes | ✅ `handing interrupted downloads back … count=1` → `boot recovery … scheduled=1` → `downloading \| Interrupted by shutdown` |
| `SIGTERM` exits 0, in ~2.7 s | ✅ |
| the legacy importer on a first start | ✅ (`tests/cli.rs`, `bootstrap::tests`, over WP-04's fixtures) |
| profile B in full, against WP-04's real `state/v2` corpus | ✅ `import-report.errors = []`, one warning, `.aulos-imported` written, and `dQw4w9WgXcQ` + `aBcDeF12345` in `GET history` with their legacy ids preserved |
| `HTTPS=true` with a self-signed certificate | ✅ `axum-server` + `rustls` served `https://…/healthz`, and the `healthcheck` subcommand reached it over TLS |

Writing profile B by hand is what caught the second real bug in this harness: the legacy format is
`{schema_version, kind, items: [{key, info}]}`, not `{schema_version, data: {url: info}}`, so a
plausible-looking seed would have tested the importer's *error* path and passed for the wrong
reason. It now copies `crates/aulos-store/tests/fixtures/state/v2/` verbatim.

#### The container-only assertions, now run

These are the ones the previous pass had to leave unverified, with what the real container said:

| Assertion | Result |
|---|---|
| the image builds | ✅ 796 MB, `linux/arm64`, cargo-chef dependency layer cached |
| `doctor` inside the image | ✅ every required *and* optional tool present: python3 3.13.5, `yt-dlp` 2026.08.30.232658, all three `getpot_bgutil` plugins, ffmpeg/ffprobe 7.1.5, `N_m3u8DL-RE` 0.5.1, deno 2.9.6, `bgutil-pot` 0.8.1 |
| the `HEALTHCHECK` wiring | ✅ `docker inspect` reports `healthy`, i.e. the `healthcheck` subcommand ran under the image's own `CMD [… "healthcheck"]` |
| `bgutil-pot` supervised for real | ✅ `components.pot` → `{"status":"ok","pid":43,"endpoint":"http://127.0.0.1:4416","last_probe_ok":true,"restarts":0}`, and `bgutil-pot stopped for shutdown` on `SIGTERM` |
| the healthz roll-up in a fully-provisioned container | ✅ `ok`, with only `jellyfin` and `telegram` `disabled`; `degraded` for the first ~15 s while `pot`'s first probe is outstanding, which is why the script accepts either |
| entrypoint `PUID`/`PGID` | ✅ the downloaded file is `1000:1000` |
| entrypoint `UMASK` | ✅ **both directions**: profile A (`UMASK=022`) → the download is `644`; profile B (`UMASK=077`) → the `aulos.db` SQLite creates is `600` |
| entrypoint `CHOWN_DIRS=false` | ✅ profile B runs as the host uid over a bind mount and does not chown it |
| `docker restart` mid-download | ✅ resumed with `msg = "Interrupted by shutdown"`, never `canceled` |
| the `docker logs` ERROR sweep | ✅ zero server-level `ERROR` lines across both profiles; the only WARNs are the two shutdown ones |
| profile B's re-import guard, in the image | ✅ `imported_at` unchanged across a `docker restart` |

Two things the container taught the script itself, both now fixed in `run.sh`:

- **The ERROR sweep was matching a child's message text.** `aulos-server` forwards the yt-dlp
  shim's stderr into `tracing` at WARN, keeping each line verbatim, and the kill in DESIGN §16.4
  step 6 makes the shim log `ERROR: ytdlp_runner protocol channel failed: [Errno 32] Broken pipe` —
  0.1 s after `the shutdown grace expired … killing them`, i.e. the shutdown working. `grep -E '(^|
  )ERROR( |:)'` flagged that `… WARN ytdlp.child: ERROR: …` line and failed the run. The pattern
  now anchors on the `tracing` **level field** (`^<ts>Z +ERROR ` or `"level":"ERROR"`), which also
  let the blanket `grep -v bgutil_pot` exclusion go — so `bgutil-pot`'s terminal `failed` state
  (logged at ERROR from `pot::enter_failed`) now fails the e2e as it should, where the word match
  had been hiding it.
- **`UMASK` is not readable with `docker exec … umask`**: `exec` does not go through the
  entrypoint, so it reports the daemon's own `0022` whatever `UMASK` is set to. The assertions go
  through a file the server created instead, which is the only place the inherited umask shows.

#### `tests/cli.rs`: one flake, and why it was undiagnosable

`explicit_serve_runs_serve` failed once, during a run that shared the machine with a 16-core
`docker build`, with `unexpected announce line: ""` — an EOF on the child's stdout, i.e. the boot
had failed and exited. It did not reproduce in twelve subsequent runs, three of them under a
deliberate `yes`-per-core load, so the trigger is not pinned down. What *is* fixed is that the next
occurrence will say why:

- **stderr is now drained on a thread and quoted in the panic message.** It had been
  `Stdio::piped()` with no reader, which threw the entire boot log away — the one thing that would
  have named the failure — and was a hang waiting to happen besides: the boot log is well over a
  pipe buffer, and a full pipe blocks the child *before* it announces itself, at which point the
  `read_line` on stdout waits forever instead of failing.
- **`PYTHONDONTWRITEBYTECODE=1`**, as the image sets, so two concurrent boots do not both compile
  the `pystub` package into a shared `__pycache__` (CPython's write is atomic, so this was wasted
  work rather than the race it looked like — but the test runs the shim once and has no use for a
  cache it leaves behind in a checked-in fixture directory).

---

## Final integration — workspace gates, the still-open bullets, README, docker + e2e

The pass that finishes the build. It owns no new feature work: it re-runs every gate over the
whole workspace, closes the requests the per-package notes left addressed to "the integrator",
rebuilds the image, re-runs the end-to-end suite against it, and writes the README.

### The gates

All four are green on this commit, from a cold `cargo` state:

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | 80 test binaries, 0 failed |
| `cargo test -p aulos-workspace-tests` | 10 passed (arch A1–A5 + the packaging gates) |

And the three checks CI runs that the four gates do not, so the first Actions run is not the place
they are discovered:

| CI-only check | Result |
|---|---|
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | clean |
| `cargo test -p aulos-provider-sc --no-default-features --locked` (the plain `reqwest` path) | 5 passed |
| `shellcheck --severity=warning` over the three shipped scripts | clean |
| `ruff` + `py_compile` + `shim_contract.py` + `tools/capture/verify.py` | clean, `131 v1 case(s) verified` |

**Nothing was deleted, weakened or `#[ignore]`d to get there** — the WIP commit's edits were
already correct, they had simply never been compiled together. The one "ignored" line in the whole
run is still the same ```` ```ignore ```` doc block in `aulos-store`'s `import::canonical`, for the
reason wave 1 recorded.

### The carried-forward bullets

Every request in this file addressed to the integrator or to an owning crate is now marked
**APPLIED** at its own bullet, with the shape it landed in. Ten of the eleven were closed; the
eleventh — discriminating an engine-task panic in the panic hook — is marked **NOT APPLIED**,
because both routes it names still require `tokio_unstable`. It is a known limitation, not an open
task.

Two of them were closed in the crate that owned the problem rather than worked around downstream,
which is worth recording because the notes had proposed both fixes and both had a cheaper local
alternative:

- **`HealthRegistry::roll_up` caps at `degraded`** in `aulos-core`, so DESIGN §16.3's own example
  payload — `"status": "degraded"` wrapped around a `down` `pot` — is representable. `aulos-server`
  and `tests/e2e/run.sh` keep their own defences (an optional tool's component is `degraded`, and
  `healthcheck` decides on the HTTP status rather than the body's word), but they are no longer
  the only thing standing between a missing `deno` and Docker restarting a healthy container.
- **`EngineCmd::Shutdown { ack }`** puts DESIGN §16.4 step 6 back inside the engine. It is what
  let `wiring::shutdown_tasks` delete the abort dance it had documented as a workaround: the engine
  now ends its own loop, so the realtime chain closes by dropping senders, the way §16.4 describes.

### Two defects this pass found on its own

1. **Neither `ci.yml` nor `docker.yml` would ever have run.** Both were written with
   `on: push: branches: [master]` and `docker.yml` gated its GHCR push on
   `github.ref == 'refs/heads/master'`, but the repository's default branch is **`main`**. Every
   push to `main` would have triggered nothing at all — no fmt, no clippy, no tests, no image —
   and the failure mode is silence, which is the worst kind: a green-looking repository with no
   evidence behind it. Both files now say `main`. (`release.yml` and `update-yt-dlp.yml` are tag-
   and schedule-triggered and were never affected.)
2. **`docker/compose.example.yml` pointed at an image that is never published.** It read
   `ghcr.io/tatoalo/aulos-server:latest`, which is the name DESIGN §18.3 was written with, before
   the repository was named `aulos`. `docker.yml` publishes `ghcr.io/${GITHUB_REPOSITORY}`, i.e.
   `ghcr.io/tatoalo/aulos`. A user copying the example would have got a pull failure on the first
   command of the quickstart. The compose file now carries the real name and a comment saying why
   it differs from the design text. At the time of this note DESIGN.md §18.3/§19 still said
   `aulos-server`, and were left alone because the fix belonged with whoever decides the published
   package name. **Superseded**: the round-1 integration pass made that decision — the image is
   `ghcr.io/tatoalo/aulos` — and rewrote DESIGN §18.3/§19 to match. See "Review round 1 —
   integration pass" at the end of this file.

### README.md

Rewritten from the WP-01 skeleton (which still said "Status: **skeleton**… `serve` binds nothing")
into a quickstart: what Aulos is, a compose snippet adapted from `docker/compose.example.yml`, the
first `curl`s, the pointer to the env var table (DESIGN §17.3) and to `PROTOCOL.md`, how to add a
community plugin (DESIGN §6.5/§13.4 and `plugins/examples/`), how to import legacy MeTube state,
and the dev commands — the four gates and the e2e. 204 lines, under the 250-line ceiling the task set. Every command and every JSON body in
it was checked against the code rather than against the design: the `POST api/v2/downloads` body is
the one `tests/e2e/run.sh` actually sends, and the `plugin.toml` fragment uses the placeholder
vocabulary of the shipped `plugins/examples/bandcamp/plugin.toml`.

## Review fixes — `aulos-api`

Recorded because three of them changed the wire contract or reached outside the crate.

- **`ErrorCode::MethodNotAllowed` (405) was added to `aulos-core`.** PROTOCOL §1.5 says "every
  non-2xx response, without exception" is the envelope, but the router had no `fallback` and no
  `method_not_allowed_fallback`, so an unrouted path and a wrong method were axum's bare
  zero-length bodies. Both now answer the envelope, and 405 needed a code the closed §1.6 list did
  not have. The list is `#[non_exhaustive]` and PROTOCOL §1.6 says to treat it as an enum with an
  `unknown` fallback, so this is an additive change; §1.6 gained the row.
- **`aulos_core::urls` is a new module** carrying the SSRF classifier (`validate`, `check`,
  `is_blocked*`, `Reject`), lifted so the v1/v2 add paths can run the guard DESIGN §16.6/§17.3
  promised without `aulos-api` depending on the bot crate — `AULOS_ALLOW_PRIVATE_TARGETS` was a
  parsed-but-never-read config key. **`aulos_telegram::urls` still holds its own copy** of the same
  table; it was left untouched to keep this pass inside its crate, and the follow-up is to make
  that module a re-export of `aulos_core::urls` (its `extract` port stays where it is). Until then
  the two tables must be changed together.
- **`POST api/v2/downloads` no longer answers `"id": null`.** When every URL deduped it falls back
  to the first `duplicates[].existing_id`, so `id` is always a string as PROTOCOL §4.1 types it.
  §4.1 was amended to say so; `id` is therefore no longer *strictly* `ids[0]`.
- **`plugins/reload`, `ytdl-options/reload` and `subscriptions/{id}/check` now read their (optional)
  body** purely to run the `Content-Type` gate. They took no body argument at all before, so
  DESIGN §16.6's "every mutating v2 route requires `Content-Type: application/json`" was not true
  of them. Their success shapes are unchanged.

## Review round 1 — integration pass (integrator, 2026-09-05)

Two things reached outside a single crate and are recorded here.

- **`.github/workflows/pat-check.yml` is a deliberate fifth workflow.** BRIEF's scope trims say CI
  is SIMPLIFIED *to* `ci.yml`, `docker.yml`, `update-yt-dlp.yml` and `release.yml`, and
  `packaging::only_the_workflows_the_brief_keeps_are_present` enforced that as an exact set — so
  `16bb579`, which added `pat-check.yml`, turned `cargo test --workspace` red on `main` and every
  CI run with it. The file is kept rather than deleted: it is `workflow_dispatch`-only, compiles
  and pushes nothing, gates nothing, and its whole job is to tell an operator whether the
  `AULOS_REPO_PAT` secret that `update-yt-dlp.yml` consumes still reads the repo and can open PRs.
  The trim table pins the *automatic* CI surface — what runs on a push, a PR, a tag or a schedule —
  and a manual diagnostic is not on it. **The gate was strengthened, not relaxed**: it now keeps a
  `SHIPPED_CI` list and a separate, explicit `MANUAL_DIAGNOSTICS` list; every workflow the trims
  name as CUT is asserted absent by name plus an `upstream-sync*` prefix rule; and anything on the
  diagnostics list must declare `workflow_dispatch:` and must not declare `push:`,
  `pull_request:` or `schedule:`, so a diagnostic cannot quietly grow into a CI job. Adding a
  sixth workflow is therefore still a deliberate edit to that test.
- **The published image is `ghcr.io/tatoalo/aulos`, decided once.** `docker.yml` pushes
  `ghcr.io/${GITHUB_REPOSITORY}`; `aulos-server` is the binary inside the image and the name of the
  bin crate, never the image. DESIGN §18.3/§19 (the compose snippet, the cutover runbook's
  `docker pull`, and all four rehearsal `docker run`s) named `ghcr.io/tatoalo/aulos-server` and
  would have failed to pull on the first step of a real cutover; they and the seven mentions in the
  superseded `docs/design-candidates/migration.md` are corrected, and §18.3 now states the rule.
  New gate `packaging::the_operator_docs_name_the_image_the_workflow_actually_publishes` covers
  DESIGN.md, `docker/compose.example.yml` and README.md, and also asserts that `docker.yml` still
  derives the name from the repository — the assumption the pinned name rests on. The bullet above
  in this file (§"Final integration") and the two mentions in STATUS.md keep the old name on
  purpose: they are the record of the defect, and rewriting them would make them false.

## Production bug 1 — the stale `msg` on a terminal row

- **The schema gained its first post-`0001` migration, and it is data-only.**
  `crates/aulos-store/migrations/0002_clear_finished_msg.sql` is
  `UPDATE items SET msg = NULL WHERE status = 'finished'`, and `SCHEMA_VERSION` is now `2`. It
  changes no DDL, so `crates/aulos-store/schema.sql` and its `insta` snapshot are unchanged and
  `migrations_from_empty_match_the_checked_in_schema` still passes untouched. It exists because the
  code fix (clearing `msg` at the terminal write) is forward-only: `set_status` is the only writer
  of that column and it is never re-run for a settled row, so every download the first production
  build completed would have kept `"MoveFiles…"` across the upgrade, and boot recovery would have
  reloaded it into the engine's cache. A `0002` that changed DDL would have needed the snapshot
  regenerated; a future one will.
- **The invariant is `finished ⇒ msg IS NULL`, defended on four writers**, not on one: the engine's
  terminal write, the group roll-up (`sync_group_status`, which does not go through `terminate`),
  the legacy importer (which bypasses the engine entirely and was importing `completed.json`'s own
  `msg` verbatim onto finished rows), and the migration above. DESIGN §8.10 lists them.
- **`error` and `canceled` clear the live line too, which the bug report did not ask for.** The
  report's premise that "the `error` status already carries its text in `msg` correctly" is true
  only of `GET /history`, where the v1 shim substitutes `error.message`
  (`aulos_api::v1::history::project_item`). On v2 and on the WebSocket an `error` row carried
  whatever stage it happened to die in — `status: "error", msg: "MoveFiles…"` — which is the same
  cosmetic defect on the other status. The importer's terminal *note* ("Imported with unknown
  legacy status: …") is not a live line and is deliberately kept, which is also why the migration
  is scoped to `finished`: SQL cannot tell a note from a stale progress line.

## Production bug 2b — the sidecars and the per-job scratch directory

- **`aulos-hooks`' NFO hook and a carried-over `jellyfin_nfo_generator.py` `Exec` entry now
  actively conflict, and only documentation separates them.** Before the fix the legacy script
  found no `.info.json` and returned early, so the built-in hook always had the sidecar to itself.
  Now the script works as designed: it writes `<base>.nfo` and **deletes** the `.info.json` it
  consumed, in the scratch directory, before the move. The shim carries the `.nfo` out and drops
  the deleted sidecar from the move set, so the operator gets the NFO they always wanted — but
  `NfoHook::run` then runs, finds nothing at `info_json_path(file)`, renders from the queue row
  alone (no plot, no tags, no upload date) and **overwrites** the good file. README now tells
  migrating users to drop the `Exec` entry, which is the documented fix. If `aulos-hooks` wants a
  belt-and-braces one, the cheap version is to skip when `nfo_path(file)` already exists and the
  sidecar does not — i.e. "somebody else already wrote this NFO from data I no longer have". That
  is a one-condition change in `NfoHook::run` and it is outside this WP's paths.
- **`aulos-queue` owns the two cleanup facts this change depends on**, and one of them is a
  behaviour change: (a) on the **success** path nothing removes `/downloads/<ULID>/` — neither
  `release_job` (`cleanup_partials` is reached only when `slot.settled` is `Some`, i.e.
  cancel/pause) nor `recovery`'s orphan sweep, which only matches `.part`/`.ytdl`. That is why the
  shim sweeps the directory rather than trusting it to be cleaned: a file left there is left there
  for good. (b) On **cancel**, `cleanup_partials` removes the whole directory, and the
  `.info.json`/`.description` now live in it, so a cancelled job destroys them where it used to
  leave them orphaned in `DOWNLOAD_DIR`. README and DESIGN §9.2 both say so; nothing in
  `aulos-queue` needs to change for it.
