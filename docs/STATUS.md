# Aulos — project status and recovery notes

Last updated: 2026-09-05 (build complete: all 18 work packages plus the final integration pass). Update this file at every checkpoint.

## What this is

A native Rust rewrite of the MeTube-POT Python backend (`github.com/tatoalo/metube_pot`) that is a
drop-in replacement on the VPS compose (same env vars, same volumes) and a much snappier backend
for the iOS app **Aulos** (`~/Development/metube_ios`). Repo: `github.com/tatoalo/aulos` (private).

Read in this order when picking the project back up:

1. `docs/BRIEF.md` — binding decisions, **including the "Scope trims for v1.0" section** that
   overrides DESIGN/PLAN where they conflict.
2. `docs/PLAN.md` — the 18 work packages (WP-00…WP-17), interfaces, acceptance tests.
3. `docs/DESIGN.md` (architecture, ~5k lines) and `docs/PROTOCOL.md` (wire contract for clients).
4. `docs/INTEGRATION-NOTES.md` — deviations from DESIGN made during implementation, per WP.
5. `docs/reference/legacy-backend-spec.md` and `docs/reference/ios-client-reference.md` — exhaustive
   specs of the systems being replaced/served.

## Ground rules

- Gates: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo test -p aulos-workspace-tests` (architecture + packaging rules).
- **No AI attribution in commit messages or PRs** (no `Co-Authored-By: Claude`, no "Generated with"
  lines). History was rewritten on 2026-09-04 to remove the trailers the first build agents added.
- CI builds `linux/amd64` only for now (`ubuntu-latest`); the Dockerfile stays `TARGETARCH`-aware.
- Never run a Telegram bot with the production token from a dev machine (polling conflict, 409).

## Work package status

| WP | Title | Status | Commit |
|---|---|---|---|
| 00 | Legacy golden corpora (`tests/golden`, `tests/v1_golden`, `tools/capture`) | done | 1bdfc47 |
| 01 | Workspace, CI, Dockerfile skeleton | done | 9948cbd |
| 02 | `aulos-core`: domain, config, catalog, EventRouter | done | 3b384e4 |
| 03 | `aulos-provider`: trait, registry, sink, proc helpers, fake provider, arch test | done | 6b51f68 |
| — | integrate wave 0 | green | 0524740 |
| 04 | `aulos-store`: schema, writer actor, allocators, reads | done | 70539d1 |
| 05 | `aulos-store`: legacy JSON importer + `import`/`check-config` | done | ca9c442 |
| 06 | `aulos-provider-ytdlp`: formats, options, outtmpl | done | 42baf9c |
| 07 | `aulos-provider-ytdlp`: Python shim `ytdlp_runner.py` + Rust client | done | dd7ba25 |
| 08 | `aulos-provider-sc`: HTTP client (wreq rc + reqwest fallback), scrape pipeline | done | 537acc7 |
| 09 | `aulos-provider-sc`: N_m3u8DL-RE/ffmpeg engines, gapless mux, progress parser | done | 7b02009 |
| 10 | `aulos-provider`: command plugins + `[[hook]]` manifest, example plugin | done | 377e112 |
| 11 | `aulos-hooks`: dispatcher, jellyfin, nfo, audio-sync, community hooks | done | 81dfc47 |
| — | integrate wave 1 | green | cefc9a3 |
| 12 | `aulos-queue`: engine (add, resolve, groups, slots, cancel, pause, recovery) | done | 3d793ae |
| 13 | `aulos-queue`: aggregator, event hub, replay ring, published snapshot | done | 78da7bd |
| 16 | `aulos-subscriptions` + `aulos-telegram` | done | 917ed7e |
| 14 | `aulos-api`: v2 REST, WebSocket, files, health, auth | done | 29396b5 |
| 15 | `aulos-api`: v1 compatibility shim (golden corpus replay) | done | 82c31d2 |
| — | integrate wave 2 | green | (this commit) |
| 17 | `aulos-server`: wiring, POT supervisor, config watcher, CLI, docker e2e | done — image built, `AULOS_E2E=1 tests/e2e/run.sh` PASS (real YouTube download via POT, restart-resume, legacy import) | 652396b |
| — | final integration (workspace gates, carried-forward bullets, README quickstart, docker build + e2e rerun) | **done** — four gates green, `AULOS_E2E=1 tests/e2e/run.sh` ends `END-TO-END: PASS` (35 assertions, 0 failures), README written | (this commit) |


## ✅ Build complete

Every work package (WP-00…WP-17) and the final integration pass are done, on `main`.

- **Gates**: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` (80 test binaries, 0 failed) and `cargo test -p aulos-workspace-tests`
  (10 passed) are all green, as are the three checks CI runs that the four gates do not
  (`clippy --all-features --locked`, `cargo test -p aulos-provider-sc --no-default-features`,
  `shellcheck` over the three shipped scripts) and the `python` job.
- **Image + e2e**: `docker build -f docker/Dockerfile -t aulos-server:dev .` produces a 796 MB
  `linux/arm64` image (CI builds `linux/amd64`), and `AULOS_E2E=1 tests/e2e/run.sh` ends in
  `END-TO-END: PASS` — 35 assertions, 0 failures, across both profiles, including a real 690 MB
  CC-BY download through the supervised POT sidecar, a restart mid-download that resumes rather
  than stranding the item, and a legacy `STATE_DIR` import with `errors: []`.
- **Carried-forward bullets**: every request `docs/INTEGRATION-NOTES.md` left addressed to the
  integrator is marked **APPLIED** at its own bullet. The one exception is marked **NOT APPLIED**
  and says why: discriminating an engine-task panic in the panic hook needs `tokio_unstable`.
- **Two defects the final pass found**: `ci.yml` and `docker.yml` triggered on `master` while the
  default branch is `main`, so **no workflow would ever have run**; and
  `docker/compose.example.yml` named `ghcr.io/tatoalo/aulos-server`, which `docker.yml` never
  publishes (it pushes `ghcr.io/${GITHUB_REPOSITORY}`, i.e. `ghcr.io/tatoalo/aulos`). Both fixed.

Continue with "Next steps" below (review workflow → OrbStack smoke → iOS migration → cutover).

## How the work is being done

Claude Code orchestrates; Opus 5 agents implement one WP each in a shared checkout, commit only
their own paths, and an integrator agent makes workspace-wide gates green after each wave. The
workflow scripts live in the orchestrator's session, not in this repo; the PLAN.md sections are
self-contained enough to hand any WP to a fresh engineer/agent.

## Next steps (in order)

1. **Review-and-fix workflow**: parallel reviewers (DESIGN conformance, PROTOCOL conformance, legacy
   parity vs `docs/reference/legacy-backend-spec.md`, security: path containment / SSRF / process
   kill, hot-path performance), adversarial verification of each finding, fix agents, repeat until
   dry. Then run the docker e2e again.
3. **Manual smoke on OrbStack**: `docker compose -f docker/compose.example.yml up` with a scratch
   `/downloads`, add a YouTube URL via `POST /api/v2/downloads`, watch `ws`, confirm
   `POST /add` + `GET /history` (v1) satisfy the *currently shipped* iOS build.
4. **iOS client migration** (`~/Development/metube_ios`): replace Socket.IO with a native
   `URLSessionWebSocketTask` client for PROTOCOL.md v2 (snapshot + deltas + seq resume), adopt
   stable ids, drop the background-upload add machinery (async `/add` returns 202 instantly), add
   retry/pause/cancel, an in-app add form driven by `GET /api/v2/catalog?url=`, file open/share via
   `download_url`, remove debug cruft, fix the README. Keep AulosCore models Codable-only.
5. **Cutover kit**: compose snippet for the VPS (image `ghcr.io/tatoalo/aulos`), `aulos-server
   import` of the legacy `STATE_DIR` JSON, rollback = switch the image tag back. Runbook in
   DESIGN §19.
6. Later: arm64 image, APNs notifier, minimal web status page, community plugin docs.

## Known open items (from INTEGRATION-NOTES.md)

- **No code request is open any more.** Every carried-forward bullet in INTEGRATION-NOTES.md is
  marked APPLIED (WP-17 closed the nine addressed to it, including both ⚠ critical-path ones; the
  final integration pass closed the eleven addressed to the integrator). The single **NOT APPLIED**
  one is a stable-Rust limitation, not a task: an engine-task panic cannot be discriminated in a
  panic hook without `tokio_unstable`, so such a panic is logged and the API then answers
  `state_unavailable` rather than aborting the process the way a store-thread panic does.
- **Documentation debt**: DESIGN.md §18.3/§19 still name the published image
  `ghcr.io/tatoalo/aulos-server`, but `docker.yml` publishes `ghcr.io/tatoalo/aulos`
  (`ghcr.io/${GITHUB_REPOSITORY}`). `docker/compose.example.yml` and README.md use the real name;
  DESIGN was deliberately left alone, because which name is *correct* is the repository owner's
  call — either rename the design text or add a `images:` override to `docker.yml`.
- Five wave-0 deviations from DESIGN are recorded only in INTEGRATION-NOTES (types hoisted into
  `aulos-core`, `Registry::pick`/`catalog_for` returning `Option`, `OutTmpl` in `aulos-provider`,
  `FormatSpec.flags.slow` placement, `ChatConfig` key count). DESIGN.md should be updated to match.
- `wreq` 6.0.0-rc.31 / `wreq-util` 3.0.0-rc.14 are prerelease pins; plain `reqwest` fallback exists.
- Runtime image base is `debian:trixie-slim` (POT binary needs glibc 2.38); builder is bookworm.
- `BGUTIL_TAG` pinned to `v0.8.1` (the tag in DESIGN was wrong).
- `aulos-workspace-tests` now dev-depends on several crates, so `cargo test -p aulos-workspace-tests`
  is a heavier build than intended.
