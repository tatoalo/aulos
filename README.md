# Aulos Server

Aulos is a native Rust download server: you hand it a URL, it works out what is behind it, queues
it, downloads it with `yt-dlp` (or StreamingCommunity, or a plugin you wrote), and tells every
connected client what is happening while it happens. It is a **drop-in replacement** for the
[MeTube-POT](https://github.com/tatoalo/metube_pot) Python backend — the same env var names, the
same volumes, the same v1 HTTP routes, and a one-shot importer for the legacy JSON state — so an
existing compose deployment can switch images and keep its history. On top of that it serves a
versioned v2 REST + WebSocket protocol (stable ids, a snapshot plus sequenced deltas, resumable
after a reconnect) for the Aulos iOS client, supervises the BgUtils POT sidecar, and runs
post-completion hooks (Jellyfin refresh, NFO writing, audio sync) plus any community hook you drop
in a directory.

## Quickstart

```yaml
# docker-compose.yml — see docker/compose.example.yml for the full annotated version.
services:
  aulos:
    image: ghcr.io/tatoalo/aulos:latest
    container_name: aulos
    restart: unless-stopped
    ports: ["8081:8081"]
    # Must exceed AULOS_SHUTDOWN_GRACE_SECS (20) + kill ladder (5s) + WS close (2s) + tracker (10s);
    # Docker's 10 s default would SIGKILL mid-shutdown and skip the WAL checkpoint.
    stop_grace_period: 40s
    environment:
      PUID: "1000"
      PGID: "1000"
      UMASK: "022"
      DOWNLOAD_DIR: /downloads
      AUDIO_DOWNLOAD_DIR: /downloads/audio
      STATE_DIR: /downloads/.metube          # unchanged: the importer reads it on first start
      TEMP_DIR: /downloads/.tmp
      MAX_CONCURRENT_DOWNLOADS: "3"
      YTDL_OPTIONS_FILE: /config/ytdl-options.json
      # Optional integrations, all off unless enabled:
      # JELLYFIN_SYNC_ENABLED / JELLYFIN_URL / JELLYFIN_API_KEY / JELLYFIN_LIBRARY_ID
      # TELEGRAM_BOT_ENABLED / TELEGRAM_BOT_TOKEN / TELEGRAM_ALLOWED_CHAT_IDS
    volumes:
      - /srv/media:/downloads
      - /srv/aulos/config:/config
```

```sh
docker compose up -d
curl -s localhost:8081/healthz | python3 -m json.tool
curl -s -X POST localhost:8081/api/v2/downloads \
  -H 'content-type: application/json' \
  -d '{"url":"https://www.youtube.com/watch?v=aqz-KE-bpKQ","download_type":"video"}'
```

`POST api/v2/downloads` answers `202 {"id": …}` immediately — extraction happens on the queue —
and the WebSocket at `/ws` then carries `added` → `delta` → `completed` for that id.

Before the first real run, two subcommands are worth a minute:

```sh
docker compose run --rm aulos check-config   # the effective config, secrets redacted; exit 1 if invalid
docker compose run --rm aulos doctor         # ffmpeg, ffprobe, N_m3u8DL-RE, deno, python3, yt-dlp, bgutil-pot
```

## Configuration

Every setting is an environment variable, and every legacy MeTube-POT variable keeps its name,
meaning and default. The **complete table** — roughly ninety variables, each marked as legacy
(`L`), legacy-with-a-behaviour-note (`L*`) or new (`AULOS_*`) — is
**[`docs/DESIGN.md` §17.3](docs/DESIGN.md)**. `yt-dlp` options themselves come from
`YTDL_OPTIONS` / `YTDL_OPTIONS_FILE` (hot-reloaded on change), not from env vars.

## Talking to it

- **[`docs/PROTOCOL.md`](docs/PROTOCOL.md)** is the wire contract: the v2 REST surface (§4), the
  WebSocket snapshot/delta/resume model (§5), the item and group shapes, the error envelope, and
  the compatibility rules a client can rely on across versions.
- The **v1 shim** reproduces the legacy routes (`POST add`, `GET history`, `POST delete`,
  `POST start`, `GET version`, the subscription routes …) byte-for-byte against a captured golden
  corpus, so an unmodified legacy client keeps working. Socket.IO is *not* emulated: `/socket.io/`
  answers `501` rather than pretending.
- `GET api/v2/catalog?url=…` tells you which provider would take a URL and what it can do with it,
  before you queue anything.

## Adding a community plugin

A plugin is **one directory with one `plugin.toml`** inside `AULOS_PLUGINS_DIR` (default
`/config/plugins`, i.e. `/srv/aulos/config/plugins` in the compose above). It is picked up at boot,
on `SIGHUP`, on `POST api/v2/plugins/reload`, and about a second after you save the file — no
recompile, no restart, and the executables can be in any language.

A directory can declare a **provider** (`[match]` + `[download]` — support for a new site) or one
or more **hooks** (`[[hook]]` tables — something that happens after a download), or both.

```toml
manifest_version = 1
name    = "Bandcamp"
version = "0.3.1"

[match]
hosts      = ["bandcamp.com"]
path_regex = '^/(album|track)/'

[capabilities]
resolve = true

[download]
command = ["python3", "download.py",
           "--url", "{url}", "--quality", "{quality}",
           "--out", "{out_path}", "--tmp", "{tmp_dir}"]
expect_output = "result_frame"
```

Start from the two worked examples in **[`plugins/examples/`](plugins/examples/)** —
`bandcamp/` (a provider, with a real `resolve.py` and `download.py`) and `media-server-hooks/`
(hook-only). The reference is **[`docs/DESIGN.md` §6.5](docs/DESIGN.md)** for the provider manifest
(the complete key table, the placeholder vocabulary, the progress-parsing spec) and
**§13.4** for the `[[hook]]` manifest. A manifest that fails to parse degrades that one plugin with
a reason surfaced in `GET api/v2/providers` and `healthz.plugin_warnings`; it never fails the
startup and never fails silently.

## Importing legacy MeTube state

On a **first** start, if the SQLite database does not exist yet, Aulos imports the legacy JSON
from `STATE_DIR` automatically (`queue.json`, `pending.json`, `completed.json`,
`subscriptions.json`, `telegram_bot_config.json`), preserving the legacy ids so existing clients
keep resolving their history, and writes a `.aulos-imported` marker so the next boot does not
re-import.

To run it by hand — to rehearse the migration, or to import into a database that already exists:

```sh
# Rehearse: import into an in-memory DB and print the report, writing nothing.
docker compose run --rm aulos import \
  --state-dir /downloads/.metube --db /downloads/.metube/aulos.db --dry-run

# For real.
docker compose run --rm aulos import \
  --state-dir /downloads/.metube --db /downloads/.metube/aulos.db
```

`--force` imports even when the destination already holds items; `--skip-corrupt` skips
unparseable records instead of aborting. Only `schema_version: 2` JSON is supported — the legacy
pickle/shelve state is out of scope. The report lists every error and warning; a clean run reports
`errors: []`.

## Development

Requires Rust 1.95 (edition 2024). The four gates, which are what CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p aulos-workspace-tests      # DESIGN §3 dependency rules + packaging gates
```

The end-to-end suite runs the **real image** against a real Creative-Commons video, so it is gated
behind an env var and never runs under `cargo test`:

```sh
docker build -f docker/Dockerfile -t aulos-server:dev .
AULOS_E2E=1 tests/e2e/run.sh
```

The release architecture is `linux/amd64`. To run the suite against *that* image from an arm64 Mac
— OrbStack runs amd64 under Rosetta — set the platform:

```sh
AULOS_E2E=1 AULOS_E2E_PLATFORM=linux/amd64 AULOS_E2E_BUILD=1 tests/e2e/run.sh
```

**This suite is a developer-machine gate and is deliberately not run in CI**: it downloads a real
video, and YouTube answers GitHub's datacenter ranges with "Sign in to confirm you're not a bot".
CI builds the image and smokes it offline instead; the one network check there is the three-daily
`mode=extract` in `update-yt-dlp.yml`.

The suite asserts the things only a container can be asked: the `healthz` roll-up with the POT sidecar
supervised, `202`-before-extraction, the WebSocket sequence, `PUID`/`PGID`/`UMASK` on the produced
file, `Range` requests on the file routes, the v1 shim, a restart mid-download resuming rather than
stranding the item, a clean `docker logs` ERROR sweep, and a second profile that imports a real
legacy `STATE_DIR`. Knobs: `AULOS_IMAGE`, `AULOS_E2E_BUILD`, `AULOS_E2E_URL`, `AULOS_E2E_PORT`, `AULOS_E2E_PLATFORM`,
`AULOS_E2E_KEEP`.

### Layout

| Path | What lives there |
|---|---|
| `crates/aulos-core` | Domain types, status vocabulary, ids, events, config, health registry |
| `crates/aulos-store` | SQLite (WAL) store actor + legacy JSON importer |
| `crates/aulos-provider` | `Provider` trait, registry, progress sink, `command` plugins |
| `crates/aulos-provider-ytdlp` | yt-dlp provider and its Python shim |
| `crates/aulos-provider-sc` | StreamingCommunity provider |
| `crates/aulos-queue` | Scheduler, slots, resolution pool, cancellation, realtime aggregator |
| `crates/aulos-api` | axum: v2 REST + WebSocket, v1 shim, health, file serving |
| `crates/aulos-telegram` | teloxide bot |
| `crates/aulos-subscriptions` | Subscription manager and scheduler |
| `crates/aulos-hooks` | Jellyfin, NFO, audio-sync and community hooks |
| `crates/aulos-server` | The binary: wiring, POT sidecar supervisor, config watcher, signals |
| `docker/` | `Dockerfile`, `entrypoint.sh`, `compose.example.yml` |
| `plugins/examples/` | Worked example plugins |
| `tests/e2e/` | The container end-to-end suite |

### CLI

```
aulos-server [serve | check-config | import | doctor | healthcheck]
```

The subcommand is optional and defaults to `serve`, because the entrypoint ends in
`exec … aulos-server "$@"`. `healthcheck` is what the image's `HEALTHCHECK` runs; it loads the same
config as `serve`, so `URL_PREFIX` normalisation applies, and it exits non-zero against a server
that is not running — which is the point.

## Documentation

- [`docs/DESIGN.md`](docs/DESIGN.md) — the architecture, and the env var table (§17.3).
- [`docs/PROTOCOL.md`](docs/PROTOCOL.md) — the v2 wire contract for clients.
- [`docs/BRIEF.md`](docs/BRIEF.md) — binding decisions and the v1.0 scope trims.
- [`docs/PLAN.md`](docs/PLAN.md) / [`docs/STATUS.md`](docs/STATUS.md) — work packages and state.
- [`docs/INTEGRATION-NOTES.md`](docs/INTEGRATION-NOTES.md) — where the implementation deviates
  from DESIGN, and why.
