# Aulos — project status and recovery notes

Last updated: 2026-09-06 (Jellyfin discovery regression fixed). Update this file at every checkpoint.

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
- **The e2e is a developer-machine gate, by owner's decision (2026-09-05), and CI does not run
  it.** GitHub's runners get "Sign in to confirm you're not a bot" from YouTube, so on a runner the
  suite measures the runner's IP reputation rather than this repository — it failed there for
  exactly that reason before being removed. `docker.yml` now builds the image and smokes it
  **offline** (`doctor`, `healthcheck` against no server, and a `URL_PREFIX=metube` container that
  must reach `healthy`). The one deliberate network check in CI is the three-daily `mode=extract`
  in `update-yt-dlp.yml`. Run the suite locally with `AULOS_E2E=1 tests/e2e/run.sh`, or
  `AULOS_E2E=1 AULOS_E2E_PLATFORM=linux/amd64 tests/e2e/run.sh` for the release architecture.
- **Carried-forward bullets**: every request `docs/INTEGRATION-NOTES.md` left addressed to the
  integrator is marked **APPLIED** at its own bullet. The one exception is marked **NOT APPLIED**
  and says why: discriminating an engine-task panic in the panic hook needs `tokio_unstable`.
- **Two defects the final pass found**: `ci.yml` and `docker.yml` triggered on `master` while the
  default branch is `main`, so **no workflow would ever have run**; and
  `docker/compose.example.yml` named `ghcr.io/tatoalo/aulos-server`, which `docker.yml` never
  publishes (it pushes `ghcr.io/${GITHUB_REPOSITORY}`, i.e. `ghcr.io/tatoalo/aulos`). Both fixed.

Continue with "Next steps" below (review workflow → OrbStack smoke → iOS migration → cutover).

## Review round 1 (2026-09-05)

Six parallel reviewers (engine/store, protocol, providers, security, perf, ops) produced 43 findings
(1 blocker, 22 major, 20 minor). Each blocker/major was handed to a skeptic verifier: 22 of 23
confirmed, 1 refuted (`perf-2`, the hub does not block on SQLite). Twelve per-crate fix commits
landed (`63f3d36`..`e69fef0`), each with regression tests; highlights: start/retry during the kill
grace stranded items; graceful shutdown ignored `AULOS_RESTART_POLICY=pause`; group roll-ups did not
account for deleted children; `state` ETag could pin a stale snapshot; a WS client connecting
mid-flush could lose a frame; `POST downloads` returned `id: null` on full dedupe; the SSRF knob had
no reader; `STATE_DIR` was reachable through the download route; `YTDL_OPTIONS` secrets were logged;
CSRF on empty-body POSTs; batched store writes waited the full flush window; audio dir not chowned;
signals unhandled during boot; no `stop_grace_period` in the compose example; Telegram stall alerts
for merely-queued items.

Workspace gates re-run by the orchestrator on `e69fef0` after all fixes: fmt clean, clippy
`-D warnings` clean, `cargo test --workspace` all green.

### What each fix agent landed

Every finding handed out was fixed; **nothing was declined**. Each agent staged only its own crate's
paths and ran its crate's gates before committing.

| Crate | Commit | n | Findings |
|---|---|---|---|
| `aulos-queue` | `8634623` | 14 | engine-01…06, engine-08, engine-09, protocol-01, perf-3, perf-4, perf-5, perf-7, perf-8 |
| `aulos-api` | `e69fef0` | 12 | protocol-02…08, security-{ssrf-api-guard-missing, state-dir-served-over-download-route, csrf-optional-json-body, cookie-tmp-permissions-race, file-serve-no-nosniff} |
| `aulos-provider` | `95b14c3` | 2 | providers-1, perf-6 |
| `aulos-provider-sc` | `1116450` | 2 | providers-2, providers-5 |
| `aulos-provider-ytdlp` | `91ecdc1` | 2 | providers-3, providers-4 |
| `aulos-core` | `d9c7e7f` | 2 | security-ytdl-options-secrets-not-redacted, ops-6 |
| `aulos-store` | `12ea8d5` | 2 | perf-1, engine-07 |
| `docker/` | `bd4525b` | 2 | ops-1, ops-3 |
| `aulos-server` | `eac917c` | 1 | ops-2 |
| `aulos-hooks` | `449bab5` | 1 | ops-4 |
| `aulos-telegram` | `029984f` | 1 | ops-5 |
| `aulos-subscriptions` | `63f3d36` | 1 | ops-7 |

**42 fixed, 1 refuted (`perf-2`), 0 declined** — 43 findings accounted for. Doc edits were made only
where the doc was the thing that was wrong; among them PROTOCOL §1.6, §4.1, §4.3, §4.7 and DESIGN
§3, §7.1, §7.6, §10.1, §10.2, §12.5, §13.4, §16.1, §16.4, §16.6, §18.2, §18.3, §19, §21.4. Where the
code disagreed with a doc that was already right — PROTOCOL §3.3, DESIGN §6.5.1, §8.9, §9.2, §14.3,
§16.4, §16.5 — the code was changed and the doc left alone.

### Integration pass (2026-09-05)

- **One red gate, found and fixed here.** `cargo test --workspace` failed on
  `aulos-workspace-tests::packaging::only_the_workflows_the_brief_keeps_are_present`: `16bb579`
  added `.github/workflows/pat-check.yml`, and that test pins the workflow set to the four BRIEF's
  scope trims keep. This was **already red on `main`** (`16bb579` is an ancestor of `e69fef0`) and
  had been failing every CI run since; the orchestrator's re-run of the gates missed it.
  Resolution: `pat-check.yml` is kept — it is `workflow_dispatch`-only, builds nothing and gates
  nothing, and exists to tell an operator whether the `AULOS_REPO_PAT` that `update-yt-dlp.yml`
  consumes is still valid, so it is not part of the automatic CI surface the trim table pins. The
  test now separates `SHIPPED_CI` from `MANUAL_DIAGNOSTICS` and gained teeth rather than losing
  them: every workflow the trims name as CUT is asserted absent by name (plus an `upstream-sync*`
  prefix rule), and a listed diagnostic must declare `workflow_dispatch:` and must **not** declare
  `push:`/`pull_request:`/`schedule:` — so it cannot quietly become a CI job.
- **Image name decided and aligned.** The published image is **`ghcr.io/tatoalo/aulos`**
  (`docker.yml` pushes `ghcr.io/${GITHUB_REPOSITORY}`); `aulos-server` is the binary inside it, not
  the image. The seven `ghcr.io/tatoalo/aulos-server` references in DESIGN §18.3/§19 — the cutover
  runbook's own `docker pull`, all four rehearsal `docker run`s and both compose snippets — are
  corrected, as are the seven in the superseded `docs/design-candidates/migration.md`. §18.3 now
  states the rule and why the earlier draft was wrong. New gate
  `packaging::the_operator_docs_name_the_image_the_workflow_actually_publishes` fails if DESIGN.md,
  `docker/compose.example.yml` or README.md ever names the wrong image again, and pins the
  assumption it rests on (that `docker.yml` still derives the name from the repository).
  The two surviving mentions of the old name, in this file above and in INTEGRATION-NOTES.md, are
  deliberately left: they are the *record of the defect*, and rewriting them would make them false.
- **Working tree was clean** — no leftovers to commit, no junk to remove; all twelve fix commits
  were already pushed.
- **Gates on the integrated tree**: `cargo fmt --all --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean; `cargo test --workspace` **1779 passed, 0 failed** across 80
  test binaries; `cargo test -p aulos-workspace-tests` green (15 in `packaging`).

**Round 2** (regression review, blind end-to-end trace, shipped-iOS simulation against the v1 shim)
has not run — it was interrupted by the org's API spend limit. Resume from there.

## Review round 2 (2026-09-05)

Three reviewers (regression of the round-1 fixes, a blind end-to-end code trace, and a simulation
of the currently shipped iOS build against the v1 shim) raised 13 findings; the skeptics confirmed
all 13. Five fix commits (`76f0339`..`326f0a7`): DESIGN §11.6/R2 now state that the shipped iOS
build goes blank against Aulos (it only loads history inside its Socket.IO connect handler), making
"ship the v2 app first" a cutover gate; the compose `stop_grace_period` covers the whole shutdown
chain; boot deletes only a database it created itself; the download-tree exclusion, the v1 delete
and the batch envelope were scoped correctly; group byte sums, mid-resolve recovery, start-as-retry
(PROTOCOL §4.2), cancel-resolve scope, the `done_total` seed and deferred partial cleanup in the
engine. One cross-crate fallout fixed by the orchestrator: the API skip-reasons test still expected
`start` on a canceled item to be refused.

Integration by the orchestrator: fmt/clippy clean, `cargo test --workspace` green, image rebuilt,
`AULOS_E2E=1 tests/e2e/run.sh` → `END-TO-END: PASS` (43 checks).

## Production bug round 4 — Jellyfin never indexed anything (2026-09-06)

Downloads reached `finished` with the file on disk and never appeared in Jellyfin, while
`/healthz` showed `jellyfin: { failures_total: 0, status: "ok" }` and the log claimed success.

**Cause**: a set `JELLYFIN_LIBRARY_ID` selected `POST /Items/{id}/Refresh`, which refreshes
metadata for an item Jellyfin already has and cannot discover a new file — and it answers `204`
for that no-op, so the fallback (gated on the call being *rejected*) never fired. MeTube ignored
the variable and always called `/Library/Refresh`, which is why MeTube worked; "the id is now
actually used" was the regression, not an improvement.

**Established empirically first** — `docs/reference/jellyfin-refresh-experiment.md`: throwaway
Jellyfin 10.10.7 and 12.0.0 containers, one endpoint per trial against a fresh mp4.
`/Items/{id}/Refresh` (± `&recursive=true`) never indexes; `/Library/Refresh` indexes in ~1 s;
`/Library/Media/Updated` indexes after the server's `LibraryMonitorDelay` (60 s default). Neither
version has a per-library scan endpoint at all.

**Fix** (`0378b0d` experiment, `35a4805` code, docs commit below):
- The default is `POST /Library/Refresh` on every completion again, debounce unchanged.
- `JELLYFIN_LIBRARY_ID` is accepted but inert: one boot WARN, `library_id_ignored` in `/healthz`,
  global scan regardless. `JELLYFIN_*_REFRESH_MODE` go inert with it.
- New `JELLYFIN_PATH_MAP` (`/downloads=/data/videos`, comma-separated, longest prefix wins) is the
  opt-in targeted mode over `/Library/Media/Updated`. Falls back to the global scan on any non-2xx
  and on any path it does not cover — never on a status class, since `204` is the no-op's answer.
- Log lines say what was *requested*; `components.jellyfin` carries `mode`, `last_request_at` and
  `last_status`. No verification poll: in targeted mode nothing can appear for a whole
  `LibraryMonitorDelay`, so a short poll would warn on every healthy scan.

Jellyfin is deliberately **not** in `tests/e2e/run.sh` (no container, and it would put an 870 MB
image behind a network-free profile). The manual check is: finish one download against a real
Jellyfin, then read `components.jellyfin.mode` and `last_status` from `/healthz`.

## Production bug round 3 + the web UI (2026-09-05, after the VPS cutover)

The owner cut the VPS over to `ghcr.io/tatoalo/aulos:latest` on 2026-09-05 (legacy import clean:
0 errors, counts matched) and filed a bug report after the first real download. Three fix agents,
each adversarially verified and sent back once:

- **Stale `msg` on finished rows** (`6656603`, `1223e96`). `Engine::terminate` kept `msg` on every
  terminal write, so yt-dlp's last postprocessor frame ("MoveFiles…") survived as the row's
  subtitle. The terminal write now clears the live line, `handle_stage` also refuses frames while a
  row awaits its hooks, and migration `0002_clear_finished_msg.sql` backfills rows a pre-fix build
  settled. PROTOCOL §2.3/§2.4/§3.1 state the rule; `error` rows keep their text via `error`.
- **NFO hook never ran for yt-dlp** (`fd05830`, `2e8ceb1`, `e9311d5`). Its gate required
  `provider == "streamingcommunity"`. It now applies to every provider (`AULOS_NFO_PROVIDERS`
  optional allow-list), renders from the stored entry or the on-disk `.info.json` with the legacy
  generator's field mapping (fixture-tested against the Python script), writes nothing when it has
  no metadata, and every hook now reports a skip reason: DEBUG `hook skipped` lines plus
  `skipped_total`/`last_skip_reason` in `healthz` once anything was declined.
- **Sidecars split from the media** (`6cab9bc`, `041fb94`). yt-dlp writes `.info.json`/`.description`
  straight to `paths.home` while the media sits in the per-job scratch dir. Verified against real
  yt-dlp source; the shim now keeps the whole file set in the scratch dir and sweeps it out with the
  media, so `%(filepath)q` `Exec` hooks keep working (README: "Exec postprocessors carried over from
  MeTube"). The legacy `jellyfin_nfo_generator.py` hook is now redundant and can be removed.

Orchestrator additions: `tests/e2e/run.sh` asserts the sidecar, the `.nfo`, the cleared terminal
`msg` (v1 and v2) and `components.nfo.runs_total >= 1` after the real download; the merged catalog
no longer lets StreamingCommunity's advisory "Source" `mp4` shadow the yt-dlp quality ladder
(`9e076d5` — the web UI's default picker exposed it).

**Web UI** — in scope as of 2026-09-05 (BRIEF amendment, DESIGN §24). Designed on a Claude Design
canvas (artboards in `docs/design/web-ui/`, matching the iOS app's tokens), then built by a
front-end + server pair, an integration agent, two reviewers (15 findings, all addressed) and a docs
agent. Framework-free HTML/CSS/ES module embedded in `aulos-api` (`src/web.rs`, `web/`), served at
`GET <p>` to browsers only (`Accept: text/html`; the JSON identity document is byte-identical for
everything else, with `Vary: Accept`), assets under `<p>assets/`, PWA manifest, strict CSP, ETag +
`no-cache`, `AULOS_WEB_UI` flag, `DEFAULT_THEME` finally honoured. Live queue over the v2 WebSocket
(snapshot + deltas, `since`/`boot` resume, rAF-coalesced row reconciliation by id), add bar driven
by the catalog, phone layout with a bottom add sheet, token sheet on 401. Tests: 13 Rust route
tests (`crates/aulos-api/tests/web.rs`) and a 37-test offline Playwright smoke against a PROTOCOL
v2 mock (`tools/web`, CI job `web`). Not in the UI yet: subscriptions, the generic §4.6 option
controls, a client `ping`.

Integration by the orchestrator: `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` clean; `cargo test --workspace` **1856 passed, 0 failed** across 83
test binaries; `cargo test -p aulos-workspace-tests` green (one call site followed `nfo::render`'s
new `Source` argument); image rebuilt; `AULOS_E2E=1 tests/e2e/run.sh` → `END-TO-END: PASS`
(47 checks; the `healthz` nfo-count assertion polls, because `healthz` is rate-limited and serves a
cached component set when asked again too soon).

## Where things stand

- **Server**: in production on the VPS since 2026-09-05; round-3 fixes and the web UI are on `main`
  and published as `ghcr.io/tatoalo/aulos:latest` by `docker.yml` on push.
- **iOS**: `v2-protocol` branch reviewed (24 fixes), builds, 137 tests incl. live suite green.
- **Owner tasks**: pull the new image on the VPS (the `0002` migration runs at boot and clears the
  stale "MoveFiles…" rows); remove the `jellyfin_nfo_generator.py` `Exec` hook from
  `ytdl_options.json` (the built-in NFO hook covers it); open the web UI at the server address;
  StreamingCommunity check against the live site through the VPN; decide whether to merge
  `v2-protocol` into the iOS default branch; rotate the Telegram token and Jellyfin key.
- **Follow-ups recorded in INTEGRATION-NOTES**: the legacy importer parses `items.url` with a bare
  `url::Url` (a `javascript:` URL in a legacy `queue.json` would be stored; the UI refuses to open
  it); invert `Hook::applies`/`skip_reason` so the reason is the required method; a subscriptions
  view and the §4.6 option controls in the web UI.

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
- ~~**Documentation debt**: DESIGN.md §18.3/§19 name the published image
  `ghcr.io/tatoalo/aulos-server`, but `docker.yml` publishes `ghcr.io/tatoalo/aulos`.~~
  **RESOLVED** in the round-1 integration pass: the name is `ghcr.io/tatoalo/aulos`, DESIGN and
  `docs/design-candidates/migration.md` were rewritten to match, and a packaging test now enforces
  it. See "Integration pass" above.
- Five wave-0 deviations from DESIGN are recorded only in INTEGRATION-NOTES (types hoisted into
  `aulos-core`, `Registry::pick`/`catalog_for` returning `Option`, `OutTmpl` in `aulos-provider`,
  `FormatSpec.flags.slow` placement, `ChatConfig` key count). DESIGN.md should be updated to match.
- `wreq` 6.0.0-rc.31 / `wreq-util` 3.0.0-rc.14 are prerelease pins; plain `reqwest` fallback exists.
- Runtime image base is `debian:trixie-slim` (POT binary needs glibc 2.38); builder is bookworm.
- `BGUTIL_TAG` pinned to `v0.8.1` (the tag in DESIGN was wrong).
- `aulos-workspace-tests` now dev-depends on several crates, so `cargo test -p aulos-workspace-tests`
  is a heavier build than intended.
