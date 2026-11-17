# Configuration

Aulos reads environment variables at startup. Environment changes require a restart.
The tables below list binary defaults; image overrides are stated explicitly.

## Loading and validation

- Boolean values accept `true`, `false`, `True`, `False`, `on`, `off`, `1` and `0`.
- An empty value is not the same as leaving a variable unset.
- `%%NAME` references another known setting. A suffix such as `%%DOWNLOAD_DIR/audio` appends a path.
  Cycles and unknown references are errors.
- Unknown `AULOS_*` names are errors, except reserved test/build names accepted by the parser.
  Other unknown environment variables are ignored.
- `URL_PREFIX` is normalized to a leading and trailing slash.
- Invalid settings are reported by `aulos-server check-config`. Use it before recreating a service.

The parser and full default list are in
[config.rs](../crates/aulos-core/src/config.rs). `RUST_LOG` can override the tracing filter.

## yt-dlp options

Use yt-dlp Python API option names in a JSON object. `YTDL_OPTIONS_FILE` is merged over
`YTDL_OPTIONS`. Preset files similarly override inline preset definitions.

For each job, the option layers are the base options, runtime overrides such as the cookie file,
selected presets in request order, and request overrides. Request overrides require
`ALLOW_YTDL_OPTIONS_OVERRIDES=true`. The provider then applies download-specific options.

Both options files are watched. A failed reload keeps the last valid settings and reports the
error in health and WebSocket updates. `SIGHUP` and `POST api/v2/ytdl-options/reload` also reload
them. Changes affect new work.

Example `ytdl-options.json`:

```json
{
  "writeinfojson": true
}
```

Mount the file and set `YTDL_OPTIONS_FILE=/config/ytdl-options.json`.

A configured `format` overrides the format picker, except for video/mp4/`best_remux`.
For video/mp4 jobs, Aulos sets `merge_output_format` to `mp4`. Other jobs retain the caller's
value. Each yt-dlp item forces `noplaylist`; submit a playlist URL to resolve its entries into
separate queue items.

`GET api/v2/debug/options?item_id=<id>` shows the layered user options with source labels and
redacted secrets. It does not include every option later derived by the provider or Python shim.

## Container

These settings are handled by the entrypoint or image, outside the Rust configuration table:

| Setting | Default | Meaning |
|---|---|---|
| `PUID`, `PGID` | `1000` | User and group for the server process. |
| `UID`, `GID` | unset | Override `PUID` and `PGID` when provided. |
| `UMASK` | `022` | File creation mask. |
| `CHOWN_DIRS` | `true` | `true`: change root directories and state contents; `false`: leave existing ownership alone; `recursive`: change all download, audio, state and temporary contents. Newly created directories are assigned to the configured user in every mode. |
| `DOTNET_SYSTEM_GLOBALIZATION_INVARIANT` | `1` | Runtime setting for N_m3u8DL-RE. |

When Docker starts the container as a non-root user, the entrypoint keeps that identity and skips
ownership changes. The mounted directories must already be writable.

## Paths and file serving

| Setting | Default | Meaning |
|---|---|---|
| `DOWNLOAD_DIR` | `.` | Root for video, captions and thumbnails. The image default is `/downloads`. |
| `AUDIO_DOWNLOAD_DIR` | `%%DOWNLOAD_DIR` | Root for audio downloads. |
| `TEMP_DIR` | `%%DOWNLOAD_DIR/.aulos-tmp` | Per-job scratch files. Keep on the download filesystem when possible. |
| `DOWNLOAD_DIRS_INDEXABLE` | `false` | Enable JSON directory listings on file routes. |
| `CUSTOM_DIRS` | `true` | Allow the request `folder` field and directory discovery. |
| `CREATE_CUSTOM_DIRS` | `true` | Create missing custom directories. |
| `CUSTOM_DIRS_EXCLUDE_REGEX` | `(^\|/)[.@].*$` | Exclude matching directories from discovery. Empty disables the filter. |
| `DELETE_FILE_ON_TRASHCAN` | `false` | Default for API delete/clear file removal. An explicit `delete_file` overrides it. |
| `STATE_DIR` | `.` | Database and cookies directory. The image default is `/downloads/.metube`. |
| `URL_PREFIX` | empty | Base path for UI, API, files and WebSocket. Empty becomes `/`. |
| `PUBLIC_HOST_URL` | `download/` | Public file URL prefix for non-audio items; relative or absolute. |
| `PUBLIC_HOST_AUDIO_URL` | `audio_download/` | Public file URL prefix for audio items; relative or absolute. |

## Naming

| Setting | Default | Meaning |
|---|---|---|
| `OUTPUT_TEMPLATE` | `%(title)s.%(ext)s` | Default yt-dlp filename template. |
| `OUTPUT_TEMPLATE_CHAPTER` | `%(title)s - %(section_number)02d - %(section_title)s.%(ext)s` | Default chapter filename template. |
| `OUTPUT_TEMPLATE_PLAYLIST` | `%(playlist_title)s/%(title)s.%(ext)s` | Template for playlist entries. Empty uses the default template. |
| `OUTPUT_TEMPLATE_CHANNEL` | `%(channel)s/%(title)s.%(ext)s` | Template for channel entries. Empty uses the default template. |

## Request and subscription defaults

| Setting | Default | Meaning |
|---|---|---|
| `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` | `0` | Default request limit. `0` means unlimited. |
| `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` | `60` | Minutes between subscription checks. |
| `SUBSCRIPTION_SCAN_PLAYLIST_END` | `50` | Entry limit for subscription scans. |
| `SUBSCRIPTION_MAX_SEEN_IDS` | `50000` | Maximum stored seen IDs per subscription. |
| `CLEAR_COMPLETED_AFTER` | `0` | Seconds before terminal records are cleared. `0` disables expiry; deadlines persist across restarts. |

## yt-dlp option settings

| Setting | Default | Meaning |
|---|---|---|
| `YTDL_OPTIONS` | `{}` | JSON object of yt-dlp Python API options. |
| `YTDL_OPTIONS_FILE` | empty | JSON file merged over `YTDL_OPTIONS`. |
| `YTDL_OPTIONS_PRESETS` | `{}` | JSON object mapping preset names to option objects. |
| `YTDL_OPTIONS_PRESETS_FILE` | empty | JSON file merged over inline presets. |
| `ALLOW_YTDL_OPTIONS_OVERRIDES` | `false` | Allow non-empty per-request option overrides. |

## Server

| Setting | Default | Meaning |
|---|---|---|
| `CORS_ALLOWED_ORIGINS` | empty | Comma-separated allowed origins. `*` allows all origins. |
| `ROBOTS_TXT` | empty | Custom robots file. Relative paths resolve under `BASE_DIR`. |
| `HOST` | `0.0.0.0` | Listen address. |
| `PORT` | `8081` | Listen port; must fit an unsigned 16-bit integer. |
| `HTTPS` | `false` | Enable TLS using the PEM files below. |
| `CERTFILE` | empty | TLS certificate file. |
| `KEYFILE` | empty | TLS private key file. |
| `BASE_DIR` | empty | Base directory for `ROBOTS_TXT`. Does not set a web asset directory. |
| `DEFAULT_THEME` | `auto` | Initial UI theme: `auto`, `light` or `dark`. Browser preferences can override it. |
| `MAX_CONCURRENT_DOWNLOADS` | `3` | Global download slots; at least 1. |
| `LOGLEVEL` | `INFO` | Base log level. Invalid values fall back to `INFO` with a warning. |
| `ENABLE_ACCESSLOG` | `false` | Log requests at INFO instead of DEBUG. |

## StreamingCommunity

| Setting | Default | Meaning |
|---|---|---|
| `SC_THREAD_COUNT` | `16` | N_m3u8DL-RE thread count. |
| `SC_USE_FFMPEG` | `false` | Use ffmpeg directly for StreamingCommunity downloads. |
| `SC_MAX_CONCURRENT_DOWNLOADS` | `1` | StreamingCommunity slots, independent of global slots; at least 1. |
| `AULOS_SC_HTTP` | `auto` | `auto`, `impersonate` or `plain`. Impersonation requires the default `sc-impersonate` build feature. |
| `AULOS_SC_META_CONCURRENCY` | `4` | Concurrent season metadata fetches. |
| `AULOS_SC_EXTRA_HOSTS` | empty | Comma-separated additional StreamingCommunity host match strings. |
| `AULOS_SC_USE_OUTPUT_TEMPLATE` | `false` | Use configured output templates for StreamingCommunity naming. |

## Jellyfin

| Setting | Default | Meaning |
|---|---|---|
| `JELLYFIN_SYNC_ENABLED` | `false` | Enable library refresh after downloads. |
| `JELLYFIN_URL` | empty | Jellyfin base URL; trailing slash is removed. |
| `JELLYFIN_API_KEY` | empty | API key; redacted in configuration output. |
| `JELLYFIN_SYNC_TIMEOUT_SECONDS` | `20` | Request timeout in seconds. Invalid values fall back to 20. |
| `JELLYFIN_LIBRARY_ID` | empty | Accepted but unused. A non-empty value logs a warning. |
| `JELLYFIN_PATH_MAP` | empty | Comma-separated `aulos_prefix=jellyfin_prefix` mappings for targeted updates. |
| `JELLYFIN_METADATA_REFRESH_MODE` | `Default` | Accepted but unused. Valid values: `None`, `ValidationOnly`, `Default`, `FullRefresh`. |
| `JELLYFIN_IMAGE_REFRESH_MODE` | `Default` | Accepted but unused. Same valid values as metadata refresh mode. |

## Telegram

| Setting | Default | Meaning |
|---|---|---|
| `TELEGRAM_BOT_ENABLED` | `false` | Enable the Telegram bot. |
| `TELEGRAM_BOT_TOKEN` | empty | Bot token; redacted in configuration output. |
| `TELEGRAM_ALLOWED_CHAT_IDS` | empty | Comma-separated integer chat IDs. An empty list prevents bot startup. |
| `TELEGRAM_STALL_TIMEOUT_SECONDS` | `180` | Warn after this many seconds without observed progress. |
| `TELEGRAM_HARD_TIMEOUT_SECONDS` | `7200` | Warn when a download runs this long. Does not cancel it. |
| `TELEGRAM_MAX_URLS_PER_MESSAGE` | `10` | Maximum accepted URLs in one message. |
| `AULOS_TELEGRAM_BOARD` | `board` | Progress message mode: `board` or `per_job`. |
| `AULOS_TELEGRAM_EDIT_INTERVAL_MS` | `3000` | Minimum interval between progress message edits. |
| `AULOS_TELEGRAM_WATCH_ALL` | `false` | Report downloads from every origin in Telegram. |

## APNs push

| Setting | Default | Meaning |
|---|---|---|
| `APNS_ENABLED` | `false` | Enable push for the iOS app (private alpha). |
| `APNS_KEY_FILE` | empty | Path to the APNs `.p8` signing key. |
| `APNS_KEY_ID` | empty | Apple key identifier. |
| `APNS_TEAM_ID` | empty | Apple developer team identifier. |
| `APNS_TOPIC` | `com.tatoalo.aulos` | Default and allowed root bundle identifier. |
| `APNS_PUSH_ALL` | `false` | Send alerts and activity starts for all origins and installations. |
| `APNS_BASE_URL_OVERRIDE` | empty | Test-only gateway override. Leave empty in deployments. |

## Identity and plugins

| Setting | Default | Meaning |
|---|---|---|
| `METUBE_VERSION` | `dev` | Reported server version. The image supplies its build version. |
| `PLUGINS_DIR` | `/config/plugins` | Fallback for `AULOS_PLUGINS_DIR` when that variable is not explicitly set. |

## Storage

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_DB_PATH` | empty | Empty selects `<STATE_DIR>/aulos.db`. |
| `AULOS_DB_READERS` | `4` | SQLite reader pool size; clamped to at least 1. |
| `AULOS_DB_FLUSH_MS` | `200` | SQLite write batching window in milliseconds. |
| `AULOS_DB_SYNCHRONOUS` | `NORMAL` | SQLite durability setting: `NORMAL` or `FULL`. |

## Realtime

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_WS_BATCH_MS` | `250` | Numeric progress cadence in milliseconds; 50 to 5000. |
| `AULOS_WS_URGENT_MS` | `25` | Urgent update cadence in milliseconds; 0 to 1000. |
| `AULOS_WS_MAX_DELTAS_PER_FRAME` | `200` | Maximum item patches per delta frame. |
| `AULOS_WS_REPLAY_FRAMES` | `512` | Replay ring frame limit. |
| `AULOS_WS_REPLAY_BYTES` | `4194304` | Replay ring byte limit. |
| `AULOS_WS_MAX_CLIENTS` | `64` | Concurrent WebSocket client limit. |
| `AULOS_WS_SEND_TIMEOUT_MS` | `5000` | Timeout for a blocked WebSocket send. |
| `AULOS_MEM_DONE_ITEMS` | `500` | Number of recent terminal records kept in the published snapshot. |
| `AULOS_SNAPSHOT_GROUP_INLINE` | `50` | Accepted but unused. Active group children are included in snapshots. |

## Queue

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_RESOLVE_CONCURRENCY` | `4` | Maximum concurrent resolution tasks. |
| `AULOS_RESOLVE_TIMEOUT_SECS` | `120` | Deadline for one resolution task. |
| `AULOS_RESOLVE_MAX_DEPTH` | `3` | Extractor redirect depth. Does not change HTTP redirect handling inside tools. |
| `AULOS_RESOLVE_FALLTHROUGH` | `true` | Try one ready runner-up provider after an unsupported result. |
| `AULOS_SCHED_LOOKAHEAD` | `32` | Queued items examined when looking for an available provider slot. |
| `AULOS_MAX_BATCH_URLS` | `500` | Maximum URLs in a batch add. |
| `AULOS_DEDUPE_MODE` | `active` | `off`, `active` or `strict`. |
| `AULOS_JOB_STALL_SECS` | `900` | No-progress warning threshold. `0` disables it. |
| `AULOS_JOB_TIMEOUT_SECS` | `0` | Hard job deadline. `0` disables it. |
| `AULOS_KILL_GRACE_MS` | `5000` | Time between SIGTERM and SIGKILL for a child process group. |
| `AULOS_AUTO_RETRY_MAX` | `2` | Automatic retry limit for retryable failures. `0` disables retries. |
| `AULOS_RESTART_POLICY` | `resume` | `resume` requeues interrupted work; `pause` leaves it waiting for a user. |
| `AULOS_CLEAN_ORPHAN_TEMP` | `false` | Remove orphan temporary files and unused job scratch directories at boot. |
| `AULOS_ENTRY_MAX_BYTES` | `262144` | Provider metadata size limit. `0` disables the limit. |
| `AULOS_CUSTOM_DIRS_MAX_DEPTH` | `8` | Maximum depth for directory discovery. |

## Plugins and hooks

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_PLUGINS_DIR` | `/config/plugins` | Provider and hook manifest directory. |
| `AULOS_PLUGINS_ENABLED` | `true` | Enable plugin discovery. |
| `AULOS_PLUGIN_TIMEOUT_RESOLVE` | `60` | Accepted but unused. Set `limits.resolve_timeout_secs` in the plugin manifest. |
| `AULOS_HOOKS_ENABLED` | `true` | Enable the hook dispatcher, including built-in hooks. |

## POT sidecar

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_POT_ENABLED` | `true` | Supervise the packaged BgUtils POT sidecar. |
| `AULOS_POT_CMD` | `bgutil-pot server` | Sidecar command, split on whitespace. Shell quoting is not interpreted. |
| `AULOS_POT_URL` | `http://127.0.0.1:4416` | Sidecar URL. |
| `AULOS_POT_MAX_RESTARTS` | `10` | Sidecar restart limit within its rolling window. |

## Hooks

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_JELLYFIN_DEBOUNCE_SECS` | `30` | Trailing refresh grouping window in seconds. |
| `AULOS_JELLYFIN_MAX_WAIT_SECS` | `300` | Maximum time to defer a grouped refresh. |
| `AULOS_NFO_ENABLED` | `true` | Enable NFO generation. |
| `AULOS_NFO_PROVIDERS` | empty | Comma-separated provider allowlist. Empty allows all providers. |

## Subscriptions

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_SUB_CHECK_CONCURRENCY` | `2` | Concurrent subscription checks. |
| `AULOS_SUB_CHECK_TIMEOUT_SECS` | `180` | Deadline for a subscription check. |
| `AULOS_SUB_BACKOFF_MAX_SECS` | `21600` | Maximum delay after repeated check failures. |
| `AULOS_SUB_FIRST_CHECK_DELAY_SECS` | `10` | Initial scheduler delay after boot. |

## Config watching and logging

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_CONFIG_DEBOUNCE_MS` | `250` | File-watch event grouping window. |
| `AULOS_CONFIG_POLL_SECS` | `30` | Polling fallback interval when filesystem watches are unavailable. |
| `AULOS_LOG_FORMAT` | `text` | `text` or `json`. |

## API and lifecycle

| Setting | Default | Meaning |
|---|---|---|
| `AULOS_WEB_UI` | `true` | Serve the embedded UI and its assets. |
| `AULOS_API_TOKEN` | empty | Bearer token for API, WebSocket and file access. |
| `AULOS_TRUSTED_PROXY_AUTH_HEADER` | empty | Accept a non-empty value in this proxy-controlled header. |
| `AULOS_ALLOW_PRIVATE_TARGETS` | `true` | Allow private-network download targets. |
| `AULOS_METRICS_ENABLED` | `false` | Accepted but unused. `/metrics` always returns `404`. |
| `AULOS_SHUTDOWN_GRACE_SECS` | `20` | Grace period for active downloads before cancellation. |
| `AULOS_VERSION` | `dev` | Server version alias. Overrides `METUBE_VERSION` when both are set. |
| `AULOS_IMPORT_ON_ERROR` | `fail` | Explicit importer policy: `fail` or `skip`. Does not enable automatic import. |
