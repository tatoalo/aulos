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
- **`deny.toml` is not shipped.** The BRIEF cuts the `deny` CI job for v1.0, and a config no
  workflow reads is dead weight. Restoring the job means restoring the file.
- **`clippy::doc_markdown` is deliberately absent** from the workspace pedantic subset: with
  `-D warnings` it fails the build on every acronym in a prose module doc (`SQLite`, `WAL`, `HLS`,
  `NFO`, `ANSI`). `unwrap_used = "deny"`, `expect_used = "warn"` and `disallowed_methods = "deny"`
  are set as DESIGN §3 requires.
- **`crates/aulos-provider-ytdlp/python/` exists with a `.gitkeep`** so the image's
  `COPY crates/aulos-provider-ytdlp/python/ /app/python/` resolves before WP-07 lands
  `ytdlp_runner.py`. WP-07 should just add the file; no Dockerfile change is needed.
- **CI has conditional no-op steps that WP-00 and WP-07 turn on by adding a file**, with no
  workflow edit: `ci.yml`'s `python` job runs `ruff`/`py_compile` once
  `crates/aulos-provider-ytdlp/python/ytdlp_runner.py` exists, the shim contract test once
  `crates/aulos-provider-ytdlp/tests/shim_contract.py` exists, and `tools/capture/verify.py` once
  that exists. `docker.yml` runs `tests/e2e/run.sh` (with `AULOS_IMAGE` and `AULOS_E2E=1`) once it
  is executable, and `update-yt-dlp.yml` runs
  `crates/aulos-provider-ytdlp/tests/smoke_extract.sh` for the real `mode=extract` smoke of
  DESIGN §18.5. If a package prefers a different path, update the workflow in that package.
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
