# Aulos — project status and recovery notes

Last updated: 2026-09-04 22:45 (paused after WP-17; final integration interrupted). Update this file at every checkpoint.

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
| — | final integration (workspace gates, README quickstart, PLAN status table, docker build + e2e rerun) | **interrupted** — partial edits committed as WIP, gates not re-verified | (wip commit) |


## ▶ RESUME HERE — the very last step of the build

The build was paused at 22:45 on 2026-09-04 while the *final integration* agent was mid-way. Its
partial edits are committed as `wip: final integration pass (interrupted…)`. To finish (≈30–60 min
for one agent):

1. `cd ~/Development/aulos_server && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo test -p aulos-workspace-tests` — fix whatever the WIP commit broke (the integrator was touching CI workflows, telegram/store/queue glue, `crates/aulos-queue/tests/shutdown.rs`, `crates/aulos-provider-ytdlp/tests/smoke_extract.sh`).
2. Apply any still-open bullets in `docs/INTEGRATION-NOTES.md`.
3. `docker build -f docker/Dockerfile -t aulos-server:dev .` then `AULOS_E2E=1 tests/e2e/run.sh` (needs ≥ 20 GB free on the host; the shared `target/` dir grows fast — `rm -rf target/debug/incremental` is safe; if Docker reports a read-only filesystem, `orbctl stop && orbctl start`).
4. Write the README quickstart (compose snippet from DESIGN §18.3, env var pointer to DESIGN §17.3, plugin how-to pointer to DESIGN §6.5/§13.4) and refresh the table above.
5. Commit (no AI attribution), push `main`, confirm the first GitHub Actions run (`ci.yml`, `docker.yml`) is green and the image is on GHCR.

Then continue with "Next steps" below (review workflow → OrbStack smoke → iOS migration → cutover).

## How the work is being done

Claude Code orchestrates; Opus 5 agents implement one WP each in a shared checkout, commit only
their own paths, and an integrator agent makes workspace-wide gates green after each wave. The
workflow scripts live in the orchestrator's session, not in this repo; the PLAN.md sections are
self-contained enough to hand any WP to a fresh engineer/agent.

## Next steps (in order)

1. Finish the final integration (see RESUME HERE above).
2. **Review-and-fix workflow**: parallel reviewers (DESIGN conformance, PROTOCOL conformance, legacy
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

- **Two of WP-17's wiring steps are on its critical path**, and both fail silently rather than
  loudly: the `HookFinalizer` + `Engine::with_pre_terminal` pair (without them a `best_remux` item
  never finalises) and `Aggregator::with_done_total` (without it a restart reports `done_total` as
  the done-window length). The wave-2 integration section lists all eight carried-forward items.
- Five wave-0 deviations from DESIGN are recorded only in INTEGRATION-NOTES (types hoisted into
  `aulos-core`, `Registry::pick`/`catalog_for` returning `Option`, `OutTmpl` in `aulos-provider`,
  `FormatSpec.flags.slow` placement, `ChatConfig` key count). DESIGN.md should be updated to match.
- `wreq` 6.0.0-rc.31 / `wreq-util` 3.0.0-rc.14 are prerelease pins; plain `reqwest` fallback exists.
- Runtime image base is `debian:trixie-slim` (POT binary needs glibc 2.38); builder is bookworm.
- `BGUTIL_TAG` pinned to `v0.8.1` (the tag in DESIGN was wrong).
- `aulos-workspace-tests` now dev-depends on several crates, so `cargo test -p aulos-workspace-tests`
  is a heavier build than intended.
