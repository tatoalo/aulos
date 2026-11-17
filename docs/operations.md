# Operations

## Subscription releases

After subscribing, choose existing releases to download or select **Future releases only**.
Use **Browse releases** to return to the picker later. **Select available** excludes downloaded
and queued items. Turn on **Show downloaded**, select an item and use **Download again** to
repeat a successful download.

Completed items show their subscription name. The name stays attached if the subscription is
renamed or deleted. Successful download history also stays in the database when you clear the
queue or delete files from Jellyfin. Downloads cleared before this upgrade cannot be recovered.


## Deployment

Start with the [README quickstart](../README.md#quickstart) or
[Compose example](../docker/compose.example.yml). The image is `ghcr.io/tatoalo/aulos` and the
binary inside it is `aulos-server`. Published images target `linux/amd64`.

Persist `DOWNLOAD_DIR` and `STATE_DIR`. The image defaults to `/downloads` and
`/downloads/.metube`. Keep `TEMP_DIR` on the same filesystem as downloads when possible.
Its default is `/downloads/.aulos-tmp`.

The container entrypoint creates required directories, applies `UMASK`, and drops to `PUID` and
`PGID`. Existing `UID` and `GID` environment values override those settings. `CHOWN_DIRS` controls
ownership changes to existing directories. See [configuration](configuration.md#container).

Set `stop_grace_period: 60s`. Shutdown waits for active work, stops child processes, drains
consumers, and checkpoints SQLite. Increase the timeout if you increase
`AULOS_SHUTDOWN_GRACE_SECS`.

## Authentication and network access

The quickstart exposes port 8081 on localhost. For remote access, use a reverse proxy with TLS
and configure authentication before changing the published address.

Aulos supports these settings:

| Configuration | Access rule |
|---|---|
| Neither auth setting | API, WebSocket and files are open to anyone who can reach them. |
| `AULOS_API_TOKEN` | Send `Authorization: Bearer <token>`. |
| `AULOS_TRUSTED_PROXY_AUTH_HEADER` | The named header must be present and non-empty. |
| Both | Either a valid token or the trusted header grants access. |

When using a trusted header, the proxy must remove client-supplied values and set its own after
login. Keep the backend reachable only through that proxy. Aulos does not validate proxy cookies.

The root page, web assets, manifest, `robots.txt`, `healthz` and `livez` are public at the server
layer. Put the whole origin behind proxy authentication if those routes must also be restricted.
The web UI prompts for a token when its API calls receive `401`.

Forward WebSocket upgrades for `/ws`. With `URL_PREFIX=/aulos/`, forward `/aulos/ws` and serve the
API and UI under `/aulos/`. Aulos normalizes the prefix to a leading and trailing slash.
See [protocol authentication](PROTOCOL.md#14-authentication) for WebSocket token handling.

`HTTPS=true` enables TLS in the binary with PEM files in `CERTFILE` and `KEYFILE`.

## CLI

`aulos-server` defaults to `serve` when no subcommand is given.

| Command | Purpose |
|---|---|
| `serve` | Start the server. |
| `check-config` | Validate and print effective configuration. |
| `doctor` | Probe runtime tools. |
| `healthcheck` | Check a running server. |
| `import` | Import legacy JSON with explicit source and destination paths. |

## Configuration checks

Run these before starting a changed deployment:

```sh
docker compose run --rm aulos check-config
docker compose run --rm aulos doctor
```

`check-config` validates configuration and prints effective settings with secrets redacted.
`doctor` probes Python, yt-dlp, ffmpeg, ffprobe, deno, N_m3u8DL-RE and bgutil-pot.
It exits non-zero when a required tool is missing.

Environment changes require recreating the container. yt-dlp option files and provider manifests
can reload while the server runs. Community hook changes require a restart.

## Upgrades and backups

Use a published version tag when you need a fixed deployment. `latest` follows builds from `main`.

For a consistent filesystem backup, stop Aulos before copying its state:

```sh
docker compose stop aulos
cp -a ./downloads/.metube ./aulos-state-backup
docker compose start aulos
```

Adjust the paths to match your volume mounts. Back up the configuration directory and downloaded
media separately. If `AULOS_DB_PATH` points outside `STATE_DIR`, include that database directory.
Do not copy only `aulos.db` while the server is writing to it; SQLite may also have live WAL files.

To update:

```sh
docker compose pull aulos
docker compose up -d aulos
docker compose ps
curl --fail http://localhost:8081/healthz
```

Database migrations run at startup. Keep the previous image tag and a matching state backup for
rollback. Do not assume an older binary can read a database migrated by a newer release.

The server version is exposed by `/api/v2/capabilities`. Avoid setting `AULOS_VERSION` or
`METUBE_VERSION` in Compose. A container updater that copies old environment values can preserve
an old version string after an image update. Recreate from Compose to inherit the image defaults.

## Health and troubleshooting

| Check | Use |
|---|---|
| `GET /livez` | Process liveness; returns `{"ok":true}`. |
| `GET /healthz` | Component status, queue counts, tool checks and integration diagnostics. |
| `GET /healthz?probe=deep` | Requests a tool recheck, subject to the server's rate limit. |
| `aulos-server healthcheck` | The container health check. Uses the configured prefix and TLS settings. |
| `docker compose logs aulos` | Startup, request and download errors. |

Health is `ok`, `degraded` or `down`. An unusable store produces HTTP `503`. Optional components
can be degraded while the rest of the server remains available. Components may also be `starting`
or `disabled`. `/metrics` returns `404`; Prometheus export is not implemented.

At startup, optional tool checks allow 30 seconds. `SLOW` timeouts and transient `FAILED` checks
retry after 5, 15, 30 and 60 seconds, then every five minutes. `MISSING` means the executable could
not be found or executed and requires a restart after correction. `doctor` performs one pass.

For download failures, collect the item error, server version, provider status and relevant logs.
Remove tokens, cookies and proxy credentials before sharing them.

- **Wrong format or quality:** check `format` and `merge_output_format` in your
  [yt-dlp options](configuration.md#yt-dlp-options).
- **Extractor redirect limit:** `AULOS_RESOLVE_MAX_DEPTH` defaults to 3. Increase it only for a
  legitimate longer chain. Repeated URLs are a loop and fail regardless of the limit.
- **StreamingCommunity failures:** check `/api/v2/providers`, `doctor`, and the item error.
  Provider readiness checks local setup, not mirror availability or site compatibility.
- **Missing Jellyfin media:** check path mappings and `healthz.components.jellyfin`.
  See [Jellyfin](integrations.md#jellyfin).
- **No NFO file:** enable `writeinfojson` in yt-dlp options or use a provider that stores usable
  metadata. See [NFO files](integrations.md#nfo-files).

## Importing MeTube state

Only schema-version-2 JSON is supported. Pickle and shelve state are not supported.
Existing Aulos SQLite databases do not need this procedure.

1. Stop the old server and back up its state and configuration.
2. Mount the legacy state directory into the Aulos container.
3. Run a dry run, inspect the report, then import.

```sh
docker compose run --rm aulos import \
  --state-dir /downloads/.metube \
  --db /downloads/.metube/aulos.db --dry-run

docker compose run --rm aulos import \
  --state-dir /downloads/.metube \
  --db /downloads/.metube/aulos.db

docker compose up -d aulos
```

The importer reads `queue.json`, `pending.json`, `completed.json`, `subscriptions.json` and
`telegram_bot_config.json`. It preserves the source files, stores its report in SQLite and writes
`.aulos-imported`. The dry run uses a temporary database and does not change the destination.

`--force` allows import into a database that already holds items. `--skip-corrupt` skips
unparseable records. Review every warning and error before using either option.

Startup never imports JSON automatically. If it detects legacy JSON beside an empty database,
it exits with the command needed to migrate. After import, inspect `/api/v2/import-report`.

Review old yt-dlp `Exec` postprocessors before starting. Remove scripts replaced by Aulos's NFO or
audio-sync hooks to avoid running the same work twice. Scripts that need the final media path
should run at yt-dlp's `after_move` stage.
