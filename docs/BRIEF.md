# Aulos Server — Architecture Brief (orchestrator decisions)

This is the ground truth the design and implementation agents must work within. Where this
brief is silent, the design docs decide. Where the design docs conflict with this brief, this
brief wins.

## Goal

Re-implement the MeTube-POT fork's backend (queue, subscriptions, Telegram bot, Jellyfin sync,
StreamingCommunity support, BgUtils POT support) as a native Rust service that is a drop-in
replacement on the existing VPS docker-compose (same env var names, same volumes), while
feeling dramatically snappier for the iOS client (`~/Development/metube_ios`, app "Aulos").

Reference material (read these first):
- `docs/reference/legacy-backend-spec.md` — exhaustive spec of the Python backend being replaced.
- `docs/reference/ios-client-reference.md` — exhaustive spec of the iOS client, its protocol use,
  and the list of backend changes that would make it snappy (section 7).
- The legacy source lives at `/Users/apogliaghi/Development/metube_pot` (read-only reference).

## Non-negotiable decisions

1. **Language/runtime**: Rust (edition 2024, stable toolchain 1.95), `tokio`, `axum`, `serde`,
   `tracing`. Single binary `aulos-server`. Cargo workspace with multiple crates.
2. **Storage**: SQLite (WAL) via `rusqlite` (bundled) behind a single async-friendly store
   actor/handle. No whole-file JSON rewrites. Transient progress fields live only in memory.
   Provide a one-shot importer for the legacy `queue.json` / `pending.json` / `completed.json`
   / `subscriptions.json` / `telegram_bot_config.json` (schema_version 2) files found in
   `STATE_DIR`, run automatically on first start when the SQLite DB does not exist yet.
3. **Realtime protocol v2**: native WebSocket (axum `ws`) at `<URL_PREFIX>ws`. JSON envelope
   `{ "t": "<type>", "seq": <u64>, ... }`. On connect: one `snapshot` (same item shape as REST).
   Then `delta` frames batched at a fixed cadence (default 250 ms, configurable) carrying only
   changed fields per item id; `added`/`completed`/`removed` are delivered promptly (not batched
   with progress). A monotonically increasing `seq` on every frame; `GET <prefix>api/v2/state?since=<seq>`
   returns either a delta list or a full snapshot. Snapshot and REST item shapes are identical.
   No Socket.IO in the Rust server.
4. **Item identity**: one server-assigned immutable `id` (ULID string) per queue item, used for
   every mutation and every event. `url` is data, not a key.
5. **Async add**: `POST <prefix>api/v2/downloads` validates and returns `202 {"id": ...}` (or
   `202 {"ids": [...]}` for batch) *before* any metadata extraction. Resolution happens in the
   background; the item is visible immediately in state `resolving`; a playlist resolves into
   child items (the parent becomes a `group` record or is replaced — design decides, but the
   client must be able to show progress for the whole group cheaply).
6. **Status vocabulary v2** (closed): `queued`, `resolving`, `preparing`, `downloading`,
   `postprocessing`, `finished`, `error`, `canceled`. `percent` is f64 0..=100, `eta` is
   integer seconds or null, `speed` bytes/s f64 or null. All numeric fields are always numbers.
7. **Honest HTTP**: JSON error envelope `{"error": {"code": "...", "message": "..."}}`, 4xx for
   client errors, 401 for auth failures (never a redirect), 202/200/204 for success.
   `Content-Type: application/json` everywhere.
8. **v1 compatibility shim** (so the existing iOS build, the README bookmarklet and the iOS
   Shortcut keep working during cutover): `POST <prefix>add`, `GET <prefix>history`,
   `POST <prefix>delete`, `POST <prefix>start`, `GET <prefix>version`, `POST <prefix>subscribe`,
   `GET <prefix>subscriptions`, `POST <prefix>subscriptions/{update,delete,check}` with the legacy
   request/response shapes and the legacy status names (`pending`/`downloading`/`finished`/`error`).
   The v1 shim is a thin translation layer over the v2 core. Socket.IO is *not* provided; the
   Angular UI is not a target.
9. **Provider (plugin) system**: a `Provider` trait in a `aulos-provider` crate:
   `id()`, `matches(&Url) -> Match` (score), `resolve(url, opts) -> Vec<MediaEntry>`,
   `download(entry, request, ProgressSink, CancellationToken) -> Result<Outcome>`.
   Built-ins implemented in Rust:
   - `ytdlp` (catch-all fallback). Runs one process per job using a **thin Python shim**
     (`ytdlp_runner.py`, shipped in the image) that receives a JSON job (mode `extract` or
     `download`, the fully-merged yt-dlp options dict, url) on stdin and streams JSON lines
     (progress hooks, postprocessor hooks, final result, errors) on stdout. This preserves 100%
     compatibility with `YTDL_OPTIONS`, `YTDL_OPTIONS_FILE`, presets and overrides (they are
     Python API option dicts, not CLI flags) and keeps the BgUtils POT plugin + nightly yt-dlp
     pin working unchanged. Rust owns arg/option construction (port of `dl_formats.py`
     `get_format`/`get_opts`), process lifecycle, process-group kill (SIGTERM, then SIGKILL after
     a grace period), timeouts, and progress normalisation (port of `_calculate_progress_percent`).
   - `streamingcommunity`: native Rust scraping (Inertia version, watch page, embed iframe,
     `window.streams`/`masterPlaylist` token/expires), just-in-time m3u8 re-extraction at download
     time, N_m3u8DL-RE with ffmpeg fallback, natural-order gapless segment mux fallback, progress
     parsing of ANSI frames (last match wins). Evaluate a Chrome-TLS-impersonating HTTP client
     (`wreq`/`rquest`) vs plain `reqwest`; the design must state the choice and a fallback.
   - `command` (the community plugin format): discovered from `PLUGINS_DIR` (default
     `/config/plugins`), each `<name>/plugin.toml` declaring `match` (host regexes), an optional
     `resolve` command that prints JSON entries, a `download` command template
     (`{url}`, `{out_dir}`, `{out_name}`, `{tmp_dir}`, headers/env), and a `progress` parser
     (`json_lines` or `regex` with named groups `percent|downloaded|total|speed|eta|status`). This
     lets people add platforms in any language without recompiling.
   Provider selection: highest score wins; ties broken by declaration order; `ytdlp` is the
   fallback with the lowest score.
10. **Concurrency**: global `MAX_CONCURRENT_DOWNLOADS` slots plus per-provider limits (SC keeps
    `SC_MAX_CONCURRENT_DOWNLOADS`, acquired without holding a global slot, as today). Resolution
    has its own bounded pool so a 500-item playlist never starves downloads. Cancel is
    cooperative via `CancellationToken` and kills the whole process group.
11. **Telegram**: `teloxide`. Same commands/config as legacy (`/start`, `/config` with inline
    keyboards, allowed chat ids, per-chat defaults persisted, URL extraction with the SSRF
    guard, max URLs per message) plus: a single live progress message per chat/job edited at
    most every 3 s (rate-limit aware), completion/failure messages, stall + hard-timeout
    warnings. Attribution by explicit `source` on the job (not a contextvar).
12. **Subscriptions**: same data model and public projection as legacy; scheduler is per
    subscription with jitter, bounded concurrency for checks, exponential backoff on failure
    (fix the legacy 60 s hot-retry), first check shortly after boot (not +60 s), and
    `POST .../check` returns immediately with a job handle while checks run in the background.
13. **Post-completion hooks are plugins too.** A `CompletionHook` trait in `aulos-hooks`
    (`on_event(&JobEvent)` for `finished`/`error`/`canceled`, with debounce support). Built-ins:
    `jellyfin` (`POST /Library/Refresh`, or the targeted refresh when `JELLYFIN_LIBRARY_ID` is
    set; debounced so N completions in 30 s trigger one refresh), `nfo` (NFO generation for
    StreamingCommunity items), `audio_sync` (the `best_remux` ffmpeg re-encode, reimplemented in
    Rust by spawning ffmpeg/ffprobe). Community hooks use the SAME `plugin.toml` manifest format
    as providers: a `[[hook]]` table with `on = [...]`, and either `http = { method, url, headers,
    body }` templates (enough for Plex `GET /library/sections/{id}/refresh?X-Plex-Token=…`, Emby,
    ntfy, generic webhooks) or `command = [...]` templates, with `{title}`, `{filename}`,
    `{folder}`, `{status}`, `{url}`, `{provider}` placeholders and `debounce_ms`. Legacy
    `JELLYFIN_*` env vars keep working by materialising the built-in jellyfin hook config.
14. **POT sidecar**: the server supervises `bgutil-pot server` as a child process (restart with
    backoff, log to tracing) and exposes its health in `GET <prefix>healthz`.
15. **Config**: every legacy env var name keeps its meaning and default (see spec §1). New vars
    are prefixed `AULOS_` (e.g. `AULOS_WS_BATCH_MS`, `AULOS_PLUGINS_DIR`, `AULOS_DB_PATH`).
    Boolean parsing accepts the same token set. Invalid config exits non-zero with a clear error.
    Hot reload of `YTDL_OPTIONS_FILE` via `notify`.
16. **Packaging**: multi-stage Dockerfile (Rust builder with cargo-chef, `debian:bookworm-slim`
    runtime with python3 + pip-pinned nightly yt-dlp + yt-dlp plugins dir + deno + ffmpeg +
    N_m3u8DL-RE + bgutil-pot + tini + gosu). CI builds **linux/amd64 only** on `ubuntu-latest` for now
    (no QEMU, no arm64; keep the Dockerfile arch-parametrised via `TARGETARCH` so arm64 can be added later). `PUID/PGID/UMASK` entrypoint
    semantics preserved (`CHOWN_DIRS` honoured). GitHub Actions: fmt/clippy/test, docker build &
    push to GHCR, yt-dlp nightly bump PR automation ported. Healthcheck hits `healthz`.
17. **Testing**: unit tests per crate; integration tests using a `fake` provider that emits a
    scripted progress timeline (no network); a `tests/e2e` script that builds the image and runs
    it via docker (OrbStack locally) against a public URL when `AULOS_E2E=1`.
18. **Code quality bar**: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
    `cargo test` all green. No `unwrap()` outside tests. Errors via `thiserror` in libs,
    `anyhow` in the binary. Structured `tracing` with request ids.

## Out of scope (for now)

- ~~A web UI. (A minimal status page may be added later.)~~ — **amended 2026-09-05, see below.**
- Socket.IO compatibility.
- Pickle/shelve legacy state import (only schema_version 2 JSON).
- APNs push (design should leave a hook: a `notifier` trait with Telegram as the first impl).

### Amendment (owner, 2026-09-05): a web UI is IN scope

The first line above is superseded, and so is DESIGN's resolved decision **#27** ("No HTML"). The
historical text is kept struck through rather than deleted, because the whole shape of the v1.0
build — the identity document at `GET <p>`, the absence of a static root, the "no HTML anywhere"
posture of the API — was decided under it, and a reader of the git history needs to see what the
rules were when those decisions were made.

What is now in scope, and the boundaries it must respect:

- **One page, embedded in the binary.** The UI is `include_str!`/`include_bytes!`d from
  `crates/aulos-api/web/` and served by `aulos-api`; there is still no static root to mount and
  still exactly one artifact to ship. Vanilla HTML/CSS/ES2022 modules — no framework, no bundler,
  no npm dependency, no external origin, no icon font. The two shipped text assets stay under a
  **70 KB unminified budget**, enforced in CI and by a unit test.
- **It may not change what the API does.** `GET <p>` is content-negotiated (`Accept: text/html` →
  the page; everything else, including the bare `*/*` `curl` sends, → the same JSON identity
  document as before), and that is the **only** content-negotiated route in the server. No other
  route, no other body, no other header changes for an existing client.
- **`AULOS_WEB_UI` (bool, default `true`)** turns it off, restoring the pre-UI surface exactly.
- The page, its assets and its manifest are served **without** auth even when `AULOS_API_TOKEN` or
  the trusted-proxy header is configured; every API route keeps its auth unchanged (rationale in
  DESIGN §24.4).
- Socket.IO is still not emulated, the Angular UI is still not a target, and the iOS client remains
  the primary client: the page is a second consumer of the same v2 protocol, never a reason to
  extend it.

DESIGN **§24** is the specification. The design artboards it was built from are
`docs/design/web-ui/*.dc.html` (`docs/design/web-ui/README.md` explains which are shipped and which
are rejected alternates).

## Testing against the user's VPS

`/Users/apogliaghi/Development/metube_pot/vps_setup.md` (untracked) is the user's private VPS compose and may be used as a *reference* for env values and for reaching Jellyfin during manual tests. NEVER start a Telegram bot with that token from a dev machine (long polling would conflict with the live bot, HTTP 409). Never commit any of those values.

## Repository layout target

```
aulos_server/
  Cargo.toml                 # workspace
  crates/
    aulos-core/              # domain types, status enum, ids, events, config, errors
    aulos-store/             # SQLite store + legacy JSON importer
    aulos-provider/          # Provider trait, MediaEntry, ProgressSink, registry, command plugin
    aulos-provider-ytdlp/    # yt-dlp provider (+ python/ytdlp_runner.py)
    aulos-provider-sc/       # StreamingCommunity provider
    aulos-queue/             # scheduler, slots, resolution pool, cancellation, hooks dispatch
    aulos-api/               # axum: v2 REST + WS, v1 shim, healthz, static file serving of downloads
    aulos-telegram/          # teloxide bot
    aulos-subscriptions/     # subscription manager + scheduler
    aulos-hooks/             # jellyfin, nfo, audio-sync
    aulos-server/            # binary: wiring, supervisor for bgutil-pot, signal handling
  docker/                    # Dockerfile, entrypoint.sh, compose examples
  plugins/examples/          # example command plugin(s)
  docs/                      # DESIGN.md, PROTOCOL.md, PLAN.md, reference/
  .github/workflows/
```

## Scope trims for v1.0 (orchestrator, 2026-09-04) — these override DESIGN.md / PLAN.md

The design documents are the reference for *how*; this list decides *what ships in v1.0*. Items
listed as CUT must not be implemented (leave a one-line `// v1.0: not implemented, see BRIEF` where
the design references them in a public surface). Items listed as SIMPLIFIED replace the design's
version.

| Area | Decision |
|---|---|
| Prometheus `metrics` endpoint, `AULOS_METRICS_ENABLED`, DESIGN §16.7 inventory | **CUT.** `healthz`/`livez` stay. |
| CLI subcommands | **SIMPLIFIED** to `serve` (default), `check-config`, `import`, `doctor`, `healthcheck`. `print-schema` and `repair-ids` are CUT (the allocator boot check logs a WARN and continues instead of refusing to start). |
| WS client → server frames `hello` topic narrowing, `ack`, `watch`/`unwatch`, engine watch registry, `ConnId`, `AULOS_SNAPSHOT_GROUP_INLINE` | **CUT.** The snapshot carries every non-terminal item (children included) plus the done window; groups carry aggregates. Keep: `ping`/`pong` keepalive, `Lagged` → resync, lag budget close, frame-size cap, client cap. |
| CI | **SIMPLIFIED** to `ci.yml` (fmt, clippy, test, arch, python shim contract test), `docker.yml` (amd64, push to GHCR on master), `update-yt-dlp.yml` (ported with the smoke build), `release.yml` (tag → image tag + notes). `deny`, `coverage`, `schema`, `gitleaks`, `dev-build.yml`, `update-sidecars.yml`, `upstream-sync-*.yml`, trivy, syft are CUT. |
| WP-00 golden corpora | **SIMPLIFIED.** Keep the pure-Python dumps (`formats.json`, `opts.json`, `percent.json`) and a v1 corpus captured against the legacy server run locally with `uv run python app/main.py` from `/Users/apogliaghi/Development/metube_pot` for the **network-free** routes only (`history`, `delete`, `start`, `version`, `presets`, `robots.txt`, `cancel-add`, `cookie-status`, cookie upload/delete, `subscriptions/*` validation errors, and every `POST add` *validation* 400). Skip cases that need yt-dlp to reach the network. No docker image needed. |
| `AULOS_TELEGRAM_WATCH_ALL` | keep. Default was `true` here; **now `false`** — superseded by DESIGN decisions 26/43 once APNs became a second notifier. Half the legacy blind spot is fixed unconditionally (a subscription reports whatever the knob says); the other half is fixed by the phone, not by fanning out. |
| Criterion benchmarks, `tests/load/` | **CUT** (the WS batching tests in WP-13 are sufficient). |
| `axum-server` TLS (`HTTPS=true`) | keep, but it may be implemented last and is not on the e2e path. |
| `wreq` Chrome impersonation for StreamingCommunity | keep as the primary path behind feature `sc-impersonate` (default on); `reqwest` plain fallback must exist. If `wreq` does not compile on the pinned toolchain, ship plain `reqwest` and record it in `docs/DESIGN.md §10.1`. |
| Coverage floors, "engineer-day" sizing | not enforced. |

Everything else in DESIGN.md, PROTOCOL.md and PLAN.md stands.
