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
    # Must exceed the serial shutdown chain -- AULOS_SHUTDOWN_GRACE_SECS (20) + engine shutdown (5s)
    # + WS close (2s) + engine drain (2s) + consumer drain (2s) + tracker (10s) = 41s -- which runs
    # *before* the WAL checkpoint. Docker's 10 s default would SIGKILL mid-shutdown.
    stop_grace_period: 60s
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
      # JELLYFIN_SYNC_ENABLED / JELLYFIN_URL / JELLYFIN_API_KEY / JELLYFIN_PATH_MAP
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

### Jellyfin

`JELLYFIN_SYNC_ENABLED=true` with a `JELLYFIN_URL` and a `JELLYFIN_API_KEY` asks Jellyfin to scan
its libraries when a download finishes. Completions inside a 30 s window
(`AULOS_JELLYFIN_DEBOUNCE_SECS`) are coalesced into one request, capped at 5 minutes
(`AULOS_JELLYFIN_MAX_WAIT_SECS`), so a 50-item playlist costs one scan rather than fifty.

By default that request is `POST /Library/Refresh` — a **global** scan. It is the only endpoint on
any Jellyfin version that discovers a file the server has never seen; a new file has no item yet,
so nothing scoped to an item or a library can find it. `docs/reference/jellyfin-refresh-experiment.md`
has the measurements.

- **`JELLYFIN_LIBRARY_ID` does nothing.** It is still accepted so no deployment fails to boot over
  it, but Jellyfin exposes no per-library scan, so an id cannot narrow the work. Setting it logs
  one WARN at boot and shows `library_id_ignored: true` in `/healthz`.
- **`JELLYFIN_PATH_MAP` is how you get a targeted scan.** Set it to comma-separated
  `aulos_prefix=jellyfin_prefix` pairs — e.g. `JELLYFIN_PATH_MAP=/downloads=/data/videos` when the
  same media is mounted at `/downloads` in this container and `/data/videos` in Jellyfin's. Aulos
  then sends `POST /Library/Media/Updated` naming the finished files in Jellyfin's own spelling,
  and only the containing folder is rescanned. The longest matching source prefix wins. Anything
  the map does not cover, and any request Jellyfin does not accept, falls back to the global scan,
  so a wrong map costs latency and never silence. Note the trade-off: the targeted call is handled
  by Jellyfin's library monitor, which waits out its own `LibraryMonitorDelay` (60 s by default)
  before it scans, whereas a global scan starts within a second or two. Leave the map unset unless
  full scans are actually costing you something.

`GET /healthz` reports `components.jellyfin` with `mode` (`global_scan` or `media_updated`),
`last_request_at`, `last_status` (the HTTP code Jellyfin returned) and the usual
`runs_total` / `failures_total`. `mode` and `last_status` together are how you tell a working
sync from one that is being politely accepted and ignored.

## The web UI

Opening the server in a browser gives you the built-in UI: the queue, the add form, per-item
progress and the completed history. It is compiled into the binary — no static root to mount, no
Node, no CDN — and it is served at the same address as the API, under `URL_PREFIX`.

`GET <prefix>` is content-negotiated, so nothing that already talks to the server changes: a
request whose `Accept` list contains `text/html` (every browser, on a document navigation) gets
the page, and everything else — `Accept: application/json`, the bare `*/*` that `curl` sends, no
`Accept` header at all — gets the same JSON identity document it always did.

- `AULOS_WEB_UI=false` turns the UI off entirely: `GET <prefix>` is then the identity document for
  every `Accept`, and the assets and the manifest answer `404`.
- `DEFAULT_THEME` (`auto` | `light` | `dark`) is the theme the page *starts* in; a viewer's own
  choice is remembered in their browser, never in a cookie the server sets.
- The page, its assets and its manifest are served **without** authentication even when
  `AULOS_API_TOKEN` or `AULOS_TRUSTED_PROXY_AUTH_HEADER` is configured — they contain nothing
  secret, and a browser cannot attach a bearer token to a document navigation. Every API route
  keeps its auth: the page asks for the token when the API answers `401`, and stores it locally.
  Put the whole thing behind your reverse proxy's auth if you want the UI itself gated.
- It installs as a PWA (`<prefix>manifest.webmanifest`), which is what makes it usable full-screen
  from an iPhone home screen.

Under 640 px it becomes a phone layout — a bottom add sheet, 44 px targets, safe-area padding —
from the same stylesheet, so there is no second URL and no app to install. The design source is
`docs/design/web-ui/` (see [its README](docs/design/web-ui/README.md)); the full specification,
including the routes, the CSP and the front-end architecture, is
**[`docs/DESIGN.md` §24](docs/DESIGN.md)**.

Screenshots are **not** checked in — they are generated, not authored. Every run of the browser
smoke below regenerates five of them into `tools/web/screenshots/` (desktop light and dark, phone
light and dark, the phone add sheet), and CI's `web` job uploads that directory as the
`web-screenshots` artifact on every run, so the current look is always one download away from a
build page.

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

### Coming from MeTube-POT's Jellyfin sync

MeTube called `POST /Library/Refresh` unconditionally and never read `JELLYFIN_LIBRARY_ID`, so
carrying your env file across changes nothing about which endpoint gets called: aulos calls the
same one. Two notes for anyone who read the earlier cutover note or the earlier docs:

- **Unsetting `JELLYFIN_LIBRARY_ID` is no longer necessary.** Between the cutover and 2026-09-06 a
  set id put aulos on `POST /Items/{id}/Refresh`, which cannot discover a new file and answers
  `204` while doing nothing — downloads landed on disk and never appeared. That path is gone; the
  id is inert and merely warns. If you unset it as a workaround, you can leave it unset or put it
  back, it makes no difference now.
- **Set `JELLYFIN_PATH_MAP` if you want a targeted scan**, per the Jellyfin section above. That is
  the only supported way to narrow the work, and it needs the paths as Jellyfin sees them — which,
  in the usual compose, are not the paths aulos writes to.

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

### Exec postprocessors carried over from MeTube

Aulos downloads into a per-job scratch directory (`/downloads/<ULID>/`) and moves the finished
file to its destination at the end of the job — MeTube downloaded straight into `DOWNLOAD_DIR`.
That matters for a `YTDL_OPTIONS` entry many MeTube setups carry:

```json
{ "key": "Exec", "exec_cmd": "python3 /config/jellyfin_nfo_generator.py %(filepath)q" }
```

A postprocessor's default `when` is `post_process`, which yt-dlp runs **before** the move, so
`%(filepath)q` is a path inside the scratch directory. Aulos keeps every sidecar it writes — the
`.info.json`, the `.description`, thumbnails, subtitles — next to the media in that directory,
and moves the whole set together with anything a postprocessor of yours *wrote* there. So a hook
that reads a sidecar relative to `%(filepath)q`, or writes its output next to it, works: it sees
the file set it expects, and what it produces reaches `DOWNLOAD_DIR`.

Four things to know:

- The path an `Exec` hook is handed is the **pre-move** one. If your script needs the final path
  (to hand it to another service, say), add `"when": "after_move"` to the postprocessor entry —
  yt-dlp then runs it with `%(filepath)q`, and Aulos with `%(infojson_filename)q`, already
  pointing at `DOWNLOAD_DIR`.
- **Remove the `jellyfin_nfo_generator.py` entry** rather than carrying it over.
  `AULOS_NFO_ENABLED=true` writes the same `.nfo` in-process for **every** provider — not just
  yt-dlp — with no grandchild process per download, and honours `AULOS_NFO_DELETE_INFO_JSON`.
  Set `AULOS_NFO_PROVIDERS` (a comma-separated list of provider ids, empty by default, meaning
  all) to narrow it. Running **both** is worse than running either: the script deletes the
  `.info.json` as soon as it has read it, so the built-in hook — which runs after the download —
  finds no metadata at all and, exactly as the script itself does in that situation, writes
  nothing. Whichever `.nfo` you end up with is then the script's, wherever its `when` left it. If
  you would rather keep the script, leave `AULOS_NFO_ENABLED` off.
- The built-in hook renders from the `.info.json`, so yt-dlp has to be writing one: keep
  `"writeinfojson": true` in your `YTDL_OPTIONS` (a MeTube setup that ran the NFO script already
  has it). Without a sidecar — and without one of the providers that store their metadata in the
  queue row, such as StreamingCommunity — there is nothing to render from, and the hook writes no
  file rather than a title-and-empty-plot stub that Jellyfin would adopt as that video's metadata.
  `GET /healthz` counts those runs as `components.nfo.wrote_nothing_total`.
- A **cancelled** download takes its sidecars with it. The scratch directory is removed on cancel
  (it is removed on delete either way), and since the `.info.json` now lives there until the
  move, a cancelled job no longer leaves one orphaned in `DOWNLOAD_DIR` with no video beside it.

## Development

Requires Rust 1.95 (edition 2024). The four gates, which are what CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p aulos-workspace-tests      # DESIGN §3 dependency rules + packaging gates
```

The web UI has its own suite, in Node rather than Rust, and it is fully offline — it drives a mock
server that speaks the same protocol, so it needs neither a built binary nor the network:

```sh
cd tools/web
npm ci
npx playwright install chromium

npx playwright test        # the smoke: ~38 tests, chromium, a few seconds
npm run serve              # a mock on http://127.0.0.1:8099/ to hand-drive the page
```

This is what CI's `web` job runs, along with the 70 KB budget check on `app.js` + `app.css`. To
point the *serving* half of the same suite at a real binary instead — the manual check that the two
sides of the contract meet — start a server and set `AULOS_WEB_BASE` to its base URL **including
`URL_PREFIX`**; everything that needs the mock's scripted queue skips itself:

```sh
cargo build -p aulos-server
DOWNLOAD_DIR=/tmp/dl STATE_DIR=/tmp/state PORT=8091 HOST=127.0.0.1 ./target/debug/aulos-server &
cd tools/web && AULOS_WEB_BASE=http://127.0.0.1:8091/ npx playwright test
```

[`tools/web/README.md`](tools/web/README.md) has the mock's flags and what each mode covers.

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
| `crates/aulos-api` | axum: v2 REST + WebSocket, v1 shim, health, file serving, the embedded web UI |
| `crates/aulos-api/web` | The shipped page itself — HTML, CSS, one ES module, two icons, the manifest |
| `crates/aulos-telegram` | teloxide bot |
| `crates/aulos-subscriptions` | Subscription manager and scheduler |
| `crates/aulos-hooks` | Jellyfin, NFO, audio-sync and community hooks |
| `crates/aulos-server` | The binary: wiring, POT sidecar supervisor, config watcher, signals |
| `docker/` | `Dockerfile`, `entrypoint.sh`, `compose.example.yml` |
| `plugins/examples/` | Worked example plugins |
| `tools/web/` | Dev-only: the protocol mock and the Playwright smoke for the page (nothing here ships) |
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
- [`docs/design/web-ui/`](docs/design/web-ui/README.md) — the web UI's design artboards, and how
  their tokens map onto the shipped CSS.
- [`docs/INTEGRATION-NOTES.md`](docs/INTEGRATION-NOTES.md) — where the implementation deviates
  from DESIGN, and why.
