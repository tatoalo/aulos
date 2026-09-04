
# MeTube-POT Backend — Architecture Reference / Rust Re-implementation Spec

Source of truth: `/Users/apogliaghi/Development/metube_pot` @ `fd35a66` (master). Files: `app/main.py` (1061 L), `app/ytdl.py` (1736 L), `app/subscriptions.py` (721 L), `app/telegram_bot.py` (604 L), `app/extractors/streamingcommunity.py` (503 L), `app/state_store.py`, `app/dl_formats.py`, `app/jellyfin_sync.py`, `app/jellyfin_nfo_generator.py`, `app/audio_sync_fix.py`, `Dockerfile`, `docker-entrypoint.sh`, `.github/workflows/*`.

Runtime: Python 3.13, aiohttp 3.13.2 + python-socketio 5.16.0 (AsyncServer), yt-dlp nightly pinned in Dockerfile (`2026.8.30.232658.dev0`), `watchfiles`, `python-telegram-bot 21.11.1`, `curl_cffi`, `beautifulsoup4`.

---

## 1. Config

Implemented by `class Config` in `/Users/apogliaghi/Development/metube_pot/app/main.py:42`. Every key in `_DEFAULTS` is read from `os.environ.get(KEY, default)` — **all values start as strings**. Then:

1. Any value starting with `%%` is replaced by `getattr(self, value[2:])` (indirection to another config key).
2. Keys in `_BOOLEAN` must be one of `true|false|True|False|on|off|1|0`; anything else → `log.error` + `sys.exit(1)`. Truthy set = `{'true','True','on','1'}`.
3. `URL_PREFIX` gets a trailing `/` appended if missing (so default `''` becomes `'/'`).
4. `PUBLIC_HOST_URL`, `PUBLIC_HOST_AUDIO_URL`: trailing `/` appended **only if non-empty**.
5. `YTDL_OPTIONS_FILE` / `YTDL_OPTIONS_PRESETS_FILE` starting with `.` are resolved to absolute paths (`Path(...).resolve()`).
6. `self._runtime_overrides = {}` then `load_ytdl_options()` and `load_ytdl_option_presets()`; failure of either → `sys.exit(1)`.

### 1.1 Full `_DEFAULTS` table

| Env var | Default | Effective type | Controls |
|---|---|---|---|
| `DOWNLOAD_DIR` | `.` (Docker: `/downloads`) | path str | Base dir for video/other downloads; served at `<prefix>download/`. |
| `AUDIO_DOWNLOAD_DIR` | `%%DOWNLOAD_DIR` | path str | Base dir when `download_type == 'audio'`; served at `<prefix>audio_download/`. |
| `TEMP_DIR` | `%%DOWNLOAD_DIR` | path str | yt-dlp `paths.temp`; N_m3u8DL-RE `--tmp-dir`. |
| `DOWNLOAD_DIRS_INDEXABLE` | `false` | bool | `show_index` on both static download routes (directory listing). |
| `CUSTOM_DIRS` | `true` | bool | Allow `folder` in add/subscribe; gates the `custom_dirs` socket event. |
| `CREATE_CUSTOM_DIRS` | `true` | bool | `os.makedirs` a non-existent `folder` instead of erroring. |
| `CUSTOM_DIRS_EXCLUDE_REGEX` | `(^\|/)[.@].*$` | regex str | Dirs matched by `re.search` are omitted from the custom-dirs listing. Empty string ⇒ no exclusion. |
| `DELETE_FILE_ON_TRASHCAN` | `false` | bool | On `/delete where=done`, also `os.remove(dldirectory/filename)`. |
| `STATE_DIR` | `.` (Docker: `/downloads/.metube`) | path str | Holds `queue.json`, `pending.json`, `completed.json`, `subscriptions.json`, `cookies.txt`, `telegram_bot_config.json`. |
| `URL_PREFIX` | `''` → normalized `'/'` | str | Prefix for **every** HTTP route and the Socket.IO path. |
| `PUBLIC_HOST_URL` | `download/` | str | Sent to browser; UI prepends it to `filename` to build download links. |
| `PUBLIC_HOST_AUDIO_URL` | `audio_download/` | str | Same, for audio downloads / `.mp3`. |
| `OUTPUT_TEMPLATE` | `%(title)s.%(ext)s` | yt-dlp outtmpl | Default `outtmpl.default`. |
| `OUTPUT_TEMPLATE_CHAPTER` | `%(title)s - %(section_number)02d - %(section_title)s.%(ext)s` | yt-dlp outtmpl | `outtmpl.chapter`; also the default `chapter_template` in add requests; exposed to frontend. |
| `OUTPUT_TEMPLATE_PLAYLIST` | `%(playlist_title)s/%(title)s.%(ext)s` | yt-dlp outtmpl | Used instead of `OUTPUT_TEMPLATE` when the entry has `playlist_index`. Empty string ⇒ keep `OUTPUT_TEMPLATE`. |
| `OUTPUT_TEMPLATE_CHANNEL` | `%(channel)s/%(title)s.%(ext)s` | yt-dlp outtmpl | Same for entries with `channel_index`. Empty ⇒ keep previous. |
| `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` | `0` | int-as-str | Default `playlist_item_limit`; `0` = unlimited. Exposed to frontend. |
| `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` | `60` | int-as-str (minutes) | Default `check_interval_minutes` on `/subscribe`. Exposed to frontend. |
| `SUBSCRIPTION_SCAN_PLAYLIST_END` | `50` | int-as-str | `playlistend` for subscription flat extraction (`max(…,1)` on add). |
| `SUBSCRIPTION_MAX_SEEN_IDS` | `50000` | int-as-str | Cap on `seen_ids` per subscription (list truncated to first N, newest-first). |
| `CLEAR_COMPLETED_AFTER` | `0` | int-as-str (seconds) | >0 ⇒ schedule auto-`clear()` of the completed item after N s. Invalid ⇒ logged error, treated as 0. |
| `YTDL_OPTIONS` | `{}` | JSON object | Global yt-dlp opts. Invalid JSON / non-dict ⇒ exit 1. |
| `YTDL_OPTIONS_FILE` | `''` | path str | JSON file merged **over** `YTDL_OPTIONS`; hot-reloaded (see 1.3). |
| `YTDL_OPTIONS_PRESETS` | `{}` | JSON object of name→object | Named opt bundles. Must be `dict[str, dict]`. |
| `YTDL_OPTIONS_PRESETS_FILE` | `''` | path str | Merged over `YTDL_OPTIONS_PRESETS`. **Not** watched despite README claim. |
| `ALLOW_YTDL_OPTIONS_OVERRIDES` | `false` | bool | If false, a non-empty `ytdl_options_overrides` in a request ⇒ 400. Exposed to frontend. |
| `CORS_ALLOWED_ORIGINS` | `''` | comma list | Split/stripped into `_cors_origins`; used for Socket.IO CORS and the `on_response_prepare` hook. `*` = allow all. |
| `ROBOTS_TXT` | `''` | path str | If set, `<prefix>robots.txt` serves `BASE_DIR/ROBOTS_TXT` as a file. |
| `HOST` | `0.0.0.0` | str | aiohttp bind host. |
| `PORT` | `8081` | int-as-str | aiohttp bind port. |
| `HTTPS` | `false` | bool | Wrap in TLS using `CERTFILE`/`KEYFILE`. |
| `CERTFILE` | `''` | path str | TLS cert (`load_cert_chain`). |
| `KEYFILE` | `''` | path str | TLS key. |
| `BASE_DIR` | `''` | path str | Prefix for `ui/dist/metube/browser` and `ROBOTS_TXT`. |
| `DEFAULT_THEME` | `auto` | `light\|dark\|auto` | Value of the `metube_theme` cookie set by `GET <prefix>` if absent. |
| `MAX_CONCURRENT_DOWNLOADS` | `3` | int-as-str | Size of the global `asyncio.Semaphore` around running downloads. |
| `LOGLEVEL` | `INFO` | str | `getattr(logging, X.upper())`; unknown ⇒ INFO. |
| `ENABLE_ACCESSLOG` | `false` | bool | Pass aiohttp `access_logger` or `None`. |
| `SC_THREAD_COUNT` | `16` | int-as-str | N_m3u8DL-RE `--thread-count`. **Read from `os.environ` inside the child process, not from Config.** |
| `SC_USE_FFMPEG` | `false` | bool (Config) / env-string (child) | Force ffmpeg instead of N_m3u8DL-RE for StreamingCommunity. Child re-reads `os.environ["SC_USE_FFMPEG"]` and accepts `true|1|on`. |
| `SC_MAX_CONCURRENT_DOWNLOADS` | `1` | int-as-str | Size of the dedicated StreamingCommunity semaphore (`max(1, …)`). |
| `JELLYFIN_SYNC_ENABLED` | `false` | bool | Trigger `POST /Library/Refresh` after each finished download. |
| `JELLYFIN_URL` | `''` | str | Jellyfin base URL (trailing `/` stripped). |
| `JELLYFIN_API_KEY` | `''` | str | `Authorization: MediaBrowser Token="…"`. |
| `JELLYFIN_SYNC_TIMEOUT_SECONDS` | `20` | float-as-str | urllib timeout; invalid ⇒ warn + 20. |
| `TELEGRAM_BOT_ENABLED` | `false` | bool | Start the Telegram bot on aiohttp startup. |
| `TELEGRAM_STALL_TIMEOUT_SECONDS` | `180` | int-as-str | Seconds without progress before "stalled" message. |
| `TELEGRAM_HARD_TIMEOUT_SECONDS` | `7200` | int-as-str | Total elapsed seconds before "taking longer than expected". |
| `TELEGRAM_MAX_URLS_PER_MESSAGE` | `10` | int-as-str | Cap on URLs parsed per Telegram message. |

`_BOOLEAN` = `('DOWNLOAD_DIRS_INDEXABLE','CUSTOM_DIRS','CREATE_CUSTOM_DIRS','DELETE_FILE_ON_TRASHCAN','HTTPS','ENABLE_ACCESSLOG','ALLOW_YTDL_OPTIONS_OVERRIDES','SC_USE_FFMPEG','JELLYFIN_SYNC_ENABLED','TELEGRAM_BOT_ENABLED')`.

### 1.2 Env vars NOT in `_DEFAULTS` but read elsewhere

| Var | Read at | Meaning |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | `telegram_bot.py:65` | Bot token; missing ⇒ bot logs error and does not start. |
| `TELEGRAM_ALLOWED_CHAT_IDS` | `telegram_bot.py:67` | Comma-separated ints; empty ⇒ bot refuses to start. |
| `METUBE_VERSION` | `main.py:985` | Reported by `/version` as `version`; default `"dev"`. |
| `LOGLEVEL` | `main.py:40` | Also read pre-Config for `logging.basicConfig`. |
| `PUID`/`PGID`/`UID`/`GID`/`UMASK`/`CHOWN_DIRS` | `docker-entrypoint.sh` | Process identity, see §12. |
| `DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1` | Dockerfile ENV | Needed by N_m3u8DL-RE (.NET). |

`vps_setup.md` (untracked) references `JELLYFIN_LIBRARY_ID`, `JELLYFIN_METADATA_REFRESH_MODE`, `JELLYFIN_IMAGE_REFRESH_MODE` — **these are not implemented anywhere in the code** and are silently ignored.

### 1.3 `YTDL_OPTIONS` / presets loading + hot reload

`load_ytdl_options()` → `(bool success, str msg)`:
1. `YTDL_OPTIONS = json.loads(os.environ.get('YTDL_OPTIONS','{}'))` (re-read from env each call), must be a dict, else `(False, 'Environment variable YTDL_OPTIONS is invalid')`.
2. If `YTDL_OPTIONS_FILE` empty → `_apply_runtime_overrides()`; return `(True,'')`.
3. File missing → `(False, 'File "<path>" not found')`. Invalid JSON/non-dict → `(False, 'YTDL_OPTIONS_FILE contents is invalid')`.
4. `YTDL_OPTIONS.update(file_opts)` — **file wins over env** — then `_apply_runtime_overrides()`.

`load_ytdl_option_presets()` is analogous with the extra invariant `all(isinstance(name,str) and isinstance(options,dict))`; messages: `'Environment variable YTDL_OPTIONS_PRESETS is invalid'`, `'File "<path>" not found'`, `'YTDL_OPTIONS_PRESETS_FILE contents is invalid'`.

Runtime overrides: `set_runtime_override(k,v)` stores in `_runtime_overrides` **and** writes into `YTDL_OPTIONS`; `remove_runtime_override(k)` pops from both; `_apply_runtime_overrides()` re-applies after any reload. Only used for `cookiefile` (cookie upload / startup autodetect).

Hot reload (`watchfiles`), only registered if `YTDL_OPTIONS_FILE` is non-empty:
- `FileOpsFilter(DefaultFilter)` accepts a change iff `path == config.YTDL_OPTIONS_FILE` (string compare) **and**, if the file exists, `os.path.samefile(path, YTDL_OPTIONS_FILE)` (falling back to string compare on OSError), **and** `change_type ∈ {modified, added, deleted}`.
- `awatch(config.YTDL_OPTIONS_FILE, watch_filter=FileOpsFilter())`; on each batch: `config.load_ytdl_options()` then emit Socket.IO `ytdl_options_changed` with `{"success": bool, "msg": str, "update_time": float|null}` (`update_time` = `os.path.getmtime`, or `null` if file unset/missing/stat error).
- Presets files are **not** watched (README is wrong).

### 1.4 `frontend_safe()` — the `configuration` payload

`_FRONTEND_KEYS` = exactly `('CUSTOM_DIRS','CREATE_CUSTOM_DIRS','OUTPUT_TEMPLATE_CHAPTER','PUBLIC_HOST_URL','PUBLIC_HOST_AUDIO_URL','DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT','SUBSCRIPTION_DEFAULT_CHECK_INTERVAL','ALLOW_YTDL_OPTIONS_OVERRIDES')`. Note `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` are emitted **as strings** (never int-coerced in Config).

### 1.5 Logging

- Pre-Config `logging.basicConfig(level=LOGLEVEL or INFO)` only if root has no handlers.
- After Config: root level re-applied; `dampenThirdPartyLoggers()` sets `httpx, httpcore, telegram, watchfiles, asyncio` to WARNING.
- `DEBUG` root level flips yt-dlp `quiet=False, verbose=True` for both extraction and download.

---

## 2. Wire protocol — REST

All routes are registered under `config.URL_PREFIX` (default `/`). Serialization uses `ObjectSerializer` (a `json.JSONEncoder` subclass): objects with `__dict__` → their `__dict__`; other non-str/bytes iterables → `list(...)`.

**Content-type gotcha:** most handlers use `web.Response(text=serializer.encode(x))` which yields **`text/plain; charset=utf-8`**, not `application/json`. Only `/presets`, `/cancel-add` set `application/json` explicitly, and `/version` uses `web.json_response`. A Rust rewrite should keep this or knowingly change it (the Angular client uses `HttpClient.post<T>` which parses regardless).

### 2.1 Route table

| Method | Path | Body / query | Success response | Errors |
|---|---|---|---|---|
| POST | `<p>add` | JSON object, see §2.2 | `serializer.encode(dqueue.add(...))` → `{"status":"ok"}` or `{"status":"error","msg":…}`; HTTP 200 either way | 400 `Invalid JSON request body`, 400 `JSON request body must be an object`, plus all validation reasons in §2.2 |
| GET | `<p>presets` | — | `{"presets": ["A","B"]}` (sorted names), `application/json` | — |
| POST | `<p>cancel-add` | ignored | `{"status":"ok"}`, `application/json` | — |
| POST | `<p>subscribe` | add body + `check_interval_minutes` | `{"status":"ok","subscription":{…public dict…}}` or `{"status":"error","msg":…}` | 400 `check_interval_minutes must be an integer`, 400 `check_interval_minutes must be at least 1`, + §2.2 |
| GET | `<p>subscriptions` | — | JSON array of subscription public dicts | — |
| POST | `<p>subscriptions/update` | `{"id":str, …changes}` | `{"status":"ok","subscription":{…}}` or `{"status":"error","msg":"Subscription not found"}` | 400 `missing subscription id`, 400 `no valid fields to update`; uncaught `ValueError("enabled must be a boolean")` ⇒ **HTTP 500** |
| POST | `<p>subscriptions/delete` | `{"ids":[str,…]}` | `{"status":"ok"}` | 400 `missing ids list` (also when `ids` is `[]`) |
| POST | `<p>subscriptions/check` | `{"ids":[str,…]}` or `{}` | `{"status":"ok"}` (awaits all checks — can block for a long time) | 400 `ids must be a list` |
| POST | `<p>delete` | `{"ids":[str,…], "where":"queue"\|"done"}` | `{"status":"ok"}` | 400 (no reason) if `ids` falsy or `where` not in `['queue','done']` |
| POST | `<p>start` | `{"ids":[str,…]}` | `{"status":"ok"}` | none (missing ids just logged as warning); `ids=None` ⇒ TypeError ⇒ 500 |
| POST | `<p>upload-cookies` | `multipart/form-data`, field name **`cookies`** | `{"status":"ok","msg":"Cookies uploaded (N bytes)"}` | HTTP 400 body `{"status":"error","msg":"No cookies file provided"}`; 400 `Cookie file too large (max 1MB)` (limit 1_000_000 bytes) |
| POST | `<p>delete-cookies` | ignored | `{"status":"ok"}` | 400 `No uploaded cookies to delete`; 400 long message when `cookiefile` is configured manually via YTDL_OPTIONS; 500 if reload of YTDL_OPTIONS fails after deletion |
| GET | `<p>cookie-status` | — | `{"status":"ok","has_cookies":bool}` | — |
| GET | `<p>history` | — | `{"done":[info…],"queue":[info…],"pending":[info…]}` (arrays of full download-info objects, **not** `[key, info]` pairs) | — |
| GET | `<p>` (index) | — | `ui/dist/metube/browser/index.html` (via `BASE_DIR`); sets cookie `metube_theme=<DEFAULT_THEME>` if not present | RuntimeError at startup if the dist dir is missing |
| GET | `<p>robots.txt` | — | `ROBOTS_TXT` file, else text `User-agent: *\nDisallow: /download/\nDisallow: /audio_download/\n` | — |
| GET | `<p>version` | — | `{"yt-dlp":"<yt_dlp.version.__version__>","version":"<METUBE_VERSION or dev>"}`, `application/json` | — |
| GET | `/` and `<p>` minus trailing slash | only when `URL_PREFIX != '/'` | 302 `HTTPFound(URL_PREFIX)` | — |
| GET | `<p>download/*` | — | static from `DOWNLOAD_DIR`, `show_index=DOWNLOAD_DIRS_INDEXABLE` | 404 |
| GET | `<p>audio_download/*` | — | static from `AUDIO_DOWNLOAD_DIR` | 404 |
| GET | `<p>*` | — | static from `BASE_DIR/ui/dist/metube/browser` (frontend assets) | 404 |
| OPTIONS | `<p>add`, `<p>cancel-add`, `<p>subscribe`, `<p>subscriptions`, `<p>subscriptions/update`, `<p>subscriptions/delete`, `<p>subscriptions/check`, `<p>upload-cookies`, `<p>delete-cookies` | — | `{"status":"ok"}` | — |

CORS: `on_response_prepare` adds `Access-Control-Allow-Origin: <Origin>` and `Access-Control-Allow-Headers: Content-Type` iff `Origin` header present and (`'*' in _cors_origins` or `Origin ∈ _cors_origins`). No `Access-Control-Allow-Methods`, no credentials.

Server startup: `web.run_app(app, host=HOST, port=int(PORT), reuse_port=supports_reuse_port(), [ssl_context], access_log=…)`. `supports_reuse_port()` probes `SO_REUSEPORT`.

aiohttp lifecycle hooks (order matters for the rewrite):
- `on_startup`: `dqueue.initialize()`, `start_telegram_bot`, `_subscription_loop_startup`, and (if `YTDL_OPTIONS_FILE`) `watch_files`.
- `on_cleanup`: `Download.shutdown_manager()`, `stop_telegram_bot`, `submgr.close()` (a no-op).
- Startup also: if `STATE_DIR/cookies.txt` exists, `config.set_runtime_override('cookiefile', …)` (only inside the `__main__` block).

### 2.2 `/add` and `/subscribe` request schema and validation

`parse_download_options(post)` (`main.py:547`) — after `_migrate_legacy_request` (§2.3):

| Key | Type | Default | Validation |
|---|---|---|---|
| `url` | str | required | falsy ⇒ 400 `missing 'url', 'download_type', or 'quality'`; `.strip()`ed |
| `download_type` | str | required | ∈ `{video, audio, captions, thumbnail}` else 400 `download_type must be one of [...]` |
| `codec` | str | `auto` | ∈ `{auto,h264,h265,av1,vp9}`; forced to `auto` for audio/captions/thumbnail |
| `format` | str | `''` | per-type list (below) |
| `quality` | str | required | per-type list (below) |
| `folder` | str\|null | `None` | not validated here; path containment checked in `__calc_download_path` |
| `custom_name_prefix` | str | `''` | must not contain `..` nor start with `/` or `\` ⇒ 400 |
| `playlist_item_limit` | int-ish | `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` | `int()`; non-int ⇒ 400 `playlist_item_limit must be an integer` |
| `auto_start` | bool | `True` | not type-checked (truthiness used; `__add_download` compares `auto_start is True`, so a string `"false"` is truthy but `is True` fails ⇒ pending) |
| `split_by_chapters` | bool | `False` | — |
| `chapter_template` | str | `OUTPUT_TEMPLATE_CHAPTER` | same `..` / leading-separator check ⇒ 400 |
| `subtitle_language` | str | `en` | must fullmatch `^[A-Za-z0-9][A-Za-z0-9-]{0,34}$` ⇒ 400 |
| `subtitle_mode` | str | `prefer_manual` | ∈ `{auto_only, manual_only, prefer_manual, prefer_auto}` ⇒ 400 |
| `ytdl_options_presets` | list[str] | `[]` | accepts a list, or a single string; legacy singular key `ytdl_options_preset` accepted. Other types ⇒ 400. Every name must exist in `config.YTDL_OPTIONS_PRESETS` ⇒ 400 |
| `ytdl_options_overrides` | JSON object **or** JSON string | `{}` | invalid JSON ⇒ 400 `ytdl_options_overrides must be valid JSON`; non-object ⇒ 400 `... must be a JSON object`; non-empty while `ALLOW_YTDL_OPTIONS_OVERRIDES=false` ⇒ 400 `ytdl_options_overrides are disabled` |
| `check_interval_minutes` (subscribe only) | int | `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` | `int()`; `<1` ⇒ 400 |

Allowed `format`/`quality` per `download_type`:

| `download_type` | `format` ∈ | `quality` ∈ | Notes |
|---|---|---|---|
| `video` | `any`, `mp4`, `ios` | `best`, `worst`, `2160`, `1440`, `1080`, `720`, `480`, `360`, `240` (+ `best_remux` **only when format=mp4**) | |
| `audio` | `m4a`, `mp3`, `opus`, `wav`, `flac` | `best`; `mp3` also `320\|192\|128`; `m4a` also `192\|128` | `codec` forced to `auto` |
| `captions` | `srt`, `txt`, `vtt`, `ttml`, `sbv`, `scc`, `dfxp` | forced `best` | `codec` forced `auto` |
| `thumbnail` | `jpg` | forced `best` | `codec` forced `auto` |

Return dict keys (positional order matters — `dqueue.add` takes them positionally): `url, download_type, codec, format, quality, folder, custom_name_prefix, playlist_item_limit, auto_start, split_by_chapters, chapter_template, subtitle_language, subtitle_mode, ytdl_options_presets, ytdl_options_overrides`.

### 2.3 Legacy request migration (`_migrate_legacy_request`)

Applied only when `download_type` is absent. Reads legacy `format`, `quality`, `video_codec`, `subtitle_format`.

| Legacy `format` | Legacy `quality` | → `download_type` | → `codec` | → `format` | → `quality` |
|---|---|---|---|---|---|
| `m4a\|mp3\|opus\|wav\|flac` | any | `audio` | `auto` | same | unchanged |
| `thumbnail` | any | `thumbnail` | `auto` | `jpg` | `best` |
| `captions` | any | `captions` | `auto` | `subtitle_format` or `srt` | `best` |
| other (`any`/`mp4`/…) | `best_ios` | `video` | `video_codec` | `ios` | `best` |
| other | `audio` | `audio` | `auto` | `m4a` | `best` |
| other | else | `video` | `video_codec` | legacy `format` | legacy `quality` |

---

## 3. Wire protocol — Socket.IO

- Server: `socketio.AsyncServer(cors_allowed_origins=_cors_origins or [])`, attached with `sio.attach(app, socketio_path=config.URL_PREFIX + 'socket.io')`. Default path `/socket.io`. **Default namespace `/` only** — no custom namespaces, no rooms except `to=sid` on connect.
- The client (`ui/src/app/services/metube-socket.service.ts`) computes `path = location.pathname.replace(/share-target/,'') + 'socket.io'`.
- **Every payload is a JSON *string*** produced by `serializer.encode(...)`, emitted as the single event argument. The client does `JSON.parse(strdata)`. A Rust rewrite must preserve this double-encoding or the client breaks.
- The server never handles inbound events other than `connect` (no `disconnect` handler).

### 3.1 Event table

| Event | Fires when | Payload (after `JSON.parse`) |
|---|---|---|
| `all` | On `connect`, `to=sid` | `[[[key, info], …], [[key, info], …]]` — element 0 = queue **+** pending (concatenated, in that order), element 1 = done. `key` is always the download's `url`. |
| `added` | `Notifier.added(dl)` from `DownloadQueue.__add_download` (both auto-start and pending paths, and on state re-import at startup) | one download-info object (§4) — note it is the **`DownloadInfo`**, whose `status` is still `"pending"` |
| `updated` | `Notifier.updated(dl)` — once when status flips to `"preparing"`, then for **every** status message drained from the child process | one download-info object |
| `completed` | `_post_download_cleanup` when the download was in `queue` and not canceled | one download-info object with terminal `status` (`finished` or `error`) |
| `canceled` | `DownloadQueue.cancel()` for pending/never-started items, and `_post_download_cleanup` when `download.canceled` | a bare JSON **string**: the download `url` |
| `cleared` | `DownloadQueue.clear()` per id | a bare JSON string: the download `url` |
| `configuration` | On `connect`, `to=sid` | `frontend_safe()` object (§1.4) |
| `custom_dirs` | On `connect`, `to=sid`, only if `CUSTOM_DIRS` | `{"download_dir":[str,…],"audio_download_dir":[str,…]}` |
| `ytdl_options_changed` | On `connect` (if `YTDL_OPTIONS_FILE`) and on every accepted file change | `{"success":bool,"msg":str,"update_time":float\|null}` |
| `subscriptions_all` | On `connect`, `to=sid`; also `SubscriptionManager.emit_all()` (currently never called) | array of subscription public dicts (§7.1) |
| `subscription_added` | after `add_subscription` succeeds | one subscription public dict |
| `subscription_updated` | after `update_subscription`, and after **every** subscription check (success, extraction failure, or "no longer subscribable") | one subscription public dict |
| `subscription_removed` | per deleted id in `delete_subscriptions` | bare JSON string: subscription id |

Broadcast scope: `added/updated/completed/canceled/cleared/subscription_*/ytdl_options_changed` are broadcast to **all** connected clients (no `to=`). There is **no** per-download throttling or coalescing — see §13.

---

## 4. Download model

`class DownloadInfo` (`app/ytdl.py:298`). Field-by-field (this is what is JSON-encoded to clients):

| Field | Type | Set where | Notes |
|---|---|---|---|
| `id` | str | ctor | `entry['id']`, prefixed as `f'{custom_name_prefix}.{id}'` when a prefix is set |
| `title` | str | ctor | `entry['title'] or entry['id']`, same prefixing |
| `url` | str | ctor | `entry.get('webpage_url') or entry['url']` — **this is the primary key everywhere** (queue key, cancel id, delete id) |
| `quality` | str | ctor | validated quality |
| `download_type` | str | ctor | `video\|audio\|captions\|thumbnail` |
| `codec` | str | ctor | `auto\|h264\|h265\|av1\|vp9` |
| `format` | str | ctor | per-type format |
| `folder` | str\|null | ctor | relative custom dir |
| `custom_name_prefix` | str | ctor | |
| `msg` | str\|null | ctor `None`; `update_status` | last status message (error text, SC phase text) |
| `percent` | float\|null | `_calculate_progress_percent` | 0.0–99.9 while active, exactly `100.0` on `finished` |
| `speed` | float\|null | `status.get('speed')` | bytes/s (yt-dlp) or derived (SC paths) |
| `eta` | int\|null | `status.get('eta')` | seconds |
| `downloaded_bytes` | int\|null | update_status | mirrored from yt-dlp |
| `total_bytes` | int\|null | update_status | |
| `total_bytes_estimate` | int\|null | update_status | |
| `fragment_index` | int\|null | update_status | used for HLS progress bounding |
| `fragment_count` | int\|null | update_status | |
| `status` | str | ctor `"pending"` | see §4.1 |
| `size` | int\|null | update_status | `os.path.getsize(final file)` |
| `timestamp` | int | ctor | `time.time_ns()` — nanoseconds, used for ordering restored items |
| `error` | str\|null | ctor | pre-download problem (e.g. upcoming livestream schedule, entry `msg`) |
| `entry` | dict\|null | ctor | full yt-dlp info dict, `_sanitize_entry_for_pickle`d (generators/iterators/sets → lists, unpicklable → `None`, depth cap 64) |
| `playlist_item_limit` | int | ctor | |
| `split_by_chapters` | bool | ctor | |
| `chapter_template` | str | ctor | |
| `subtitle_language` | str | ctor | default `"en"` |
| `subtitle_mode` | str | ctor | default `"prefer_manual"` |
| `ytdl_options_presets` | list[str] | ctor | |
| `ytdl_options_overrides` | dict | ctor | |
| `subtitle_files` | list[{filename,size}] | ctor `[]`; update_status | paths relative to the download dir |
| `filename` | str | **not** in ctor — created lazily in `update_status` | `os.path.relpath(final_path, download_dir)`; `.webm`→`.jpg` rewrite for thumbnails |
| `chapter_files` | list[{filename,size}] | lazily created | from `SplitChapters` postprocessor hook, de-duplicated by filename |

Because `filename`/`chapter_files` are lazily created, a download-info object emitted before the first `filename` status has **no** `filename` key at all (the Angular `Download` interface declares it non-optional — clients must tolerate `undefined`).

### 4.1 Status values and transitions

Only these strings ever appear in `status`:

| Value | Produced by |
|---|---|
| `"pending"` | `DownloadInfo.__init__`; also default when restoring a record lacking `status` |
| `"preparing"` | `Download.start()` right after the child process is spawned, before any yt-dlp output |
| `"downloading"` | yt-dlp `progress_hooks`; SC ffmpeg/N_m3u8DL-RE progress frames and phase messages |
| `"finished"` | yt-dlp progress hook, `MoveFiles` postprocessor hook, SC success, or `_download()`'s final `{'status':'finished'}` when `ret == 0` |
| `"error"` | yt-dlp non-zero return, `YoutubeDLError`, unexpected exception, SC failures, or forced by `_post_download_cleanup` when the terminal status isn't `finished` |

Transitions:
```
pending ──(auto_start=True or /start)──► preparing ──► downloading* ──► finished
                                                   └────────────────► error
pending ──(/delete where=queue)──► [removed, 'canceled' event]
preparing|downloading ──(/delete where=queue)──► proc.kill() ──► _post_download_cleanup
        └─ if download.canceled: 'canceled' event, item dropped (NOT moved to done)
finished|error ──(/delete where=done)──► [removed, 'cleared' event]
finished|error ──(CLEAR_COMPLETED_AFTER > 0, after N s)──► clear() ──► 'cleared'
```
Notable: there is **no** `postprocessing` status — long ffmpeg/postprocessor phases appear to the UI as a frozen `finished`-percent `downloading` state or as `finished` with the process still running.

---

## 5. Queue mechanics

### 5.1 Collections

`DownloadQueue.__init__` (`ytdl.py:1196`) creates three `PersistentQueue`s over `STATE_DIR`:

| Attribute | `PersistentQueue` name | Legacy shelf path | JSON path |
|---|---|---|---|
| `self.queue` | `"queue"` | `<STATE_DIR>/queue` | `<STATE_DIR>/queue.json` |
| `self.done` | `"completed"` | `<STATE_DIR>/completed` | `<STATE_DIR>/completed.json` |
| `self.pending` | `"pending"` | `<STATE_DIR>/pending` | `<STATE_DIR>/pending.json` |

Also: `self.active_downloads = set()` (**dead code — never mutated**), `self.semaphore = asyncio.Semaphore(int(MAX_CONCURRENT_DOWNLOADS))`, `self.sc_semaphore = asyncio.Semaphore(max(1, int(SC_MAX_CONCURRENT_DOWNLOADS)))`, `self._add_generation = 0`, `self._canceled_urls: set[str]`.

`self.done.load()` runs synchronously in the constructor. `initialize()` (called from `on_startup`) fires two fire-and-forget tasks: `__import_queue()` (re-`__add_download(v, True)` — i.e. **auto-restarts every previously-queued download**) and `__import_pending()` (`__add_download(v, False)`).

`get()` returns a 2-tuple: `(queue.items() + pending.items() as [key, info] pairs, done.items() as pairs)`.

### 5.2 `PersistentQueue` on disk

`PersistentQueue.__init__(name, path)`:
- `os.mkdir(os.path.dirname(path))` if the parent isn't a dir (**non-recursive** — fails if grandparent missing).
- `self.legacy_path = path`; `self.path = path + ".json"`; `self.store = AtomicJsonStore(self.path, kind=f"persistent_queue:{name}")`; `self.dict = OrderedDict()` (insertion-ordered; `next()` returns the first item).

File format (via `AtomicJsonStore`):
```json
{"schema_version":2,"kind":"persistent_queue:queue",
 "items":[{"key":"<url>","info":{ …persisted fields… }}, …]}
```
`_PERSISTED_DOWNLOAD_FIELDS` (only non-`None` values are written): `id, title, url, quality, download_type, codec, format, folder, custom_name_prefix, playlist_item_limit, split_by_chapters, chapter_template, subtitle_language, subtitle_mode, ytdl_options_presets, ytdl_options_overrides, status, timestamp, error, msg, filename, size, chapter_files`. Deliberately **not** persisted: `percent, speed, eta, downloaded_bytes, total_bytes, total_bytes_estimate, fragment_index, fragment_count, subtitle_files`.

`entry` is persisted only for `queue`/`pending` (`_should_persist_entry()` returns `identifier != "completed"`), and then only in compacted form (`_compact_persisted_entry`): keys starting with `playlist` or `channel`, plus `n_entries` and `__last_playlist_index`; returns `None` if nothing matches. **Exception:** if `str(entry.get('extractor')).lower()` contains `streamingcommunity`, the *entire* entry is kept (needed for `_sc_base_url` / `_sc_needs_m3u8_extraction` and NFO metadata).

`AtomicJsonStore` (`app/state_store.py`): `STATE_SCHEMA_VERSION = 2`. `save()` → `tempfile.mkstemp(prefix=".<basename>.", suffix=".tmp", dir=parent)`, `json.dump(..., ensure_ascii=False, separators=(",",":"))` + `"\n"`, `flush`, `fsync`, `os.replace`, then `fsync` the directory. `load()` returns `None` if missing; on any error, or if `payload["kind"] != self.kind`, the file is **quarantined** to `<path>.invalid.<YYYYmmddHHMMSS>` and `None` is returned. JSON-hostile values are wrapped: `bytes` → `{"__metube_bytes__": "<base64>"}`, `datetime` → `{"__metube_datetime__": "<isoformat>"}` (round-tripped by `from_json_compatible`).

Legacy migration: if `queue.json` is absent/quarantined, `read_legacy_shelf(<STATE_DIR>/queue)` opens a Python **`shelve`** (pickle) DB, sorts entries by `info.timestamp`, converts and immediately writes the JSON file. `DownloadInfo.__setstate__` performs the old→new schema migration (`format=m4a…`→`download_type=audio`, `thumbnail`→`jpg`, `captions`→`subtitle_format`, `quality=best_ios`→`format=ios`, `quality=audio`→m4a audio, `ytdl_options_preset` str→list, plus defaults for every field added since). A Rust rewrite can skip pickle support if it accepts that legacy shelves are unreadable, but it **must** handle `schema_version: 1` JSON (loading rewrites it to v2 compacted form).

Mutation API: `put(value)` keys by `value.info.url`, writes the whole file, and **rolls back the in-memory dict on write failure**; `delete(key)` likewise. `exists/get/items/next/empty` are pure in-memory. **Every put/delete rewrites the entire JSON file.**

### 5.3 Concurrency and the subprocess model

`__start_download(download)`:
```
if _is_streamingcommunity(download):   # entry['extractor'] contains 'streamingcommunity'
    async with self.sc_semaphore:      # acquired OUTSIDE the global one, by design
        await self.__run_download(download)
else:
    await self.__run_download(download)

__run_download: async with self.semaphore:
    if download.canceled: return
    await download.start(self.notifier)
    self._post_download_cleanup(download)
```
Both entry points re-check `download.canceled` (regression-tested in `test_cancel_before_start_marks_download_canceled`).

`Download.start(notifier)`:
1. Lazily create a process-wide `multiprocessing.Manager()` (`Download.manager`, torn down by `Download.shutdown_manager()` on cleanup).
2. `self.status_queue = Download.manager.Queue()` — a **manager (proxy) queue**, i.e. every `put`/`get` is a pickled round-trip over a socket to the manager process.
3. `self.proc = multiprocessing.Process(target=self._download)`; `start()` (fork on Linux — the child inherits the whole interpreter state, including the loaded config and yt-dlp).
4. `info.status = 'preparing'`; `await notifier.updated(info)`.
5. `self.status_task = asyncio.create_task(self.update_status())`.
6. `await loop.run_in_executor(None, self.proc.join)` — one **thread-pool thread blocked per running download**.
7. Push sentinel `None` onto the status queue and `await self.status_task` so all statuses (including the final `MoveFiles` size) are applied before cleanup.

Child side `_download()` builds:
```python
ytdl_params = {
  'quiet': not debug, 'verbose': debug, 'no_color': True,
  'paths': {'home': download_dir, 'temp': temp_dir},
  'outtmpl': {'default': output_template, 'chapter': output_template_chapter},
  'format': self.format,           # from get_format()
  'socket_timeout': 30,
  'ignore_no_formats_error': True,
  'progress_hooks': [put_status],
  'postprocessor_hooks': [put_status_postprocessor],
  **self.ytdl_opts,                # from get_opts(); user opts can override the above
}
```
If `info.split_by_chapters`: `outtmpl['chapter'] = info.chapter_template` and a `{'key':'FFmpegSplitChapters','force_keyframes':False}` postprocessor is appended.

`put_status` forwards **only** these keys from yt-dlp's progress dict: `tmpfilename, filename, status, msg, total_bytes, total_bytes_estimate, downloaded_bytes, fragment_index, fragment_count, speed, eta`.

`put_status_postprocessor(d)`:
- `postprocessor == 'MoveFiles' and status == 'finished'` → `{'status':'finished','filename': join(info_dict['__finaldir'], basename(filepath)) if '__finaldir' else filepath}`; additionally, for `download_type == 'captions'`, one `{'subtitle_file': path}` per `info_dict['requested_subtitles'][*]['filepath']`.
- `postprocessor == 'SplitChapters' and status == 'finished'` → one `{'chapter_file': path}` per `info_dict['chapters'][*]['filepath']`.

Then: `ret = self._download_streamingcommunity()`; if it returns `None`, run `yt_dlp.YoutubeDL(params).download([url])` and push `{'status': 'finished' if ret==0 else 'error'}`; if it returned `0`, push `{'status':'finished'}`. `YoutubeDLError` → `{'status':'error','msg':str(exc)}`; any other exception → same with `log.exception`.

Parent side `update_status()` loop:
- Blocking `self.status_queue.get` is offloaded via `run_in_executor` — **a second thread-pool thread per running download**, and one executor task per message.
- `None` sentinel ⇒ return. `self.canceled` ⇒ return immediately (remaining statuses dropped).
- `self.tmpfilename = status.get('tmpfilename')` (unconditional — overwritten with `None` by messages lacking it).
- `filename` handling: relpath against `download_dir`; for `captions`, filenames not ending in an allowed caption extension (`.txt` when `format == 'txt'`, else `.vtt/.srt/.sbv/.scc/.ttml/.dfxp`) are **skipped entirely** (`continue`); `size = os.path.getsize(...)`; for `thumbnail`, `\.webm$` → `.jpg`.
- `chapter_file` / `subtitle_file` messages append to `chapter_files` / `subtitle_files` (de-duped by relpath) and `continue` — they never update status/percent. For `captions` + `format == 'txt'`, `_convert_srt_to_txt_file()` strips cue numbers/timestamps/tags into a sibling `.txt` and deletes the `.srt`; `filename`/`size` then point at the txt.
- Otherwise: `status`, `msg`, byte/fragment counters, `percent` via `_calculate_progress_percent(status, previous_percent)`, `speed`, `eta`, then `await notifier.updated(info)` — **one Socket.IO broadcast per status message**.
- Progress-source tracking: `progress_source = status['filename'] or status['tmpfilename']`; if the source changed (e.g. video → audio stream of a merge), `previous_percent` is reset to `None` so the monotonic clamp restarts.

`_calculate_progress_percent(status, previous)` semantics (unit-tested):
1. `status == 'finished'` ⇒ `100.0`.
2. Exact `total_bytes` ⇒ `downloaded/total*100`.
3. Else: estimate percent from `total_bytes_estimate`; if fragment counts exist, floor = `idx/count*100`, ceiling = `min((idx+1)/count*100, 99.9)`, result = `min(max(estimate, floor), ceiling)`; with no fragment info, the estimate is ignored when `total_bytes_estimate <= downloaded_bytes` (the bogus 1 KiB/1 KiB HLS frame).
4. `None` ⇒ keep `previous`. Then clamp to `[0.0, 99.9]` and never decrease below `previous`.

`cancel()`: if `running()` → `proc.kill()` (SIGKILL, no graceful stop, ffmpeg children may be orphaned); set `canceled = True`; push `None`. `close()` → `proc.close()` if started. `running()` swallows `ValueError` (closed process) → `False`.

`_post_download_cleanup(download)` (synchronous, called inside the semaphore):
1. If terminal status ≠ `finished`: delete `tmpfilename` if it is a file; force `status = 'error'`.
2. `download.close()`.
3. If `queue.exists(url)`: `queue.delete(url)`; then either `notifier.canceled(url)` (when `canceled`) or: `done.put(download)`, `notifier.completed(info)`, and — if `status == 'finished'` — `__sync_jellyfin_library(info)`; finally, if `int(CLEAR_COMPLETED_AFTER) > 0`, schedule `__auto_clear_after_delay(url, n)` (which sleeps then `clear([url])` if still present). All of these are `asyncio.create_task` fire-and-forget.
   - Consequence: a download started from **pending** and never present in `queue` (impossible in practice, since `start_pending` moves it) or one whose queue entry vanished emits **no** `completed` event.

### 5.4 Add flow, playlist expansion, cancellation generation

`add(url, …, already=None, _add_gen=None)`:
- On the top-level call (`already is None`): `_add_gen = self._add_generation`; `self._canceled_urls.clear()`.
- Recursion guard: `already` is a set of URLs; a repeat ⇒ `{'status':'ok'}` with `log.info('recursion detected, skipping')`.
- `__extract_info` is run via `run_in_executor` (thread) — `YoutubeDLError` ⇒ `{'status':'error','msg':str}`; any other exception ⇒ logged + `{'status':'error','msg':str}` (never a 500).

`__extract_info(url, presets, overrides)`:
1. If `StreamingCommunityExtractor.can_extract(url)` and it returns a truthy entry, use it (yt-dlp is never invoked).
2. Otherwise build `params = {**merged_user_opts, 'quiet': not debug, 'verbose': debug, 'no_color': True, 'extract_flat': True, 'ignore_no_formats_error': True, 'noplaylist': True, 'paths': {...}}` — **MeTube's keys are applied after user opts, so presets cannot override `extract_flat`/`noplaylist`** (tested). `impersonate` is converted via `ImpersonateTarget.from_str`.
3. `extract_info(url, download=False)`.
4. `__needs_strict_extract_retry(entry)`: entry is a dict, `_type` is `video`, `formats == []` (empty list, not `None`), and it has `id|url|webpage_url` ⇒ retry with `extract_flat=False, ignore_no_formats_error=False` so real errors (geo-block, auth) surface instead of a phantom entry.

`__add_entry(entry, …, already, _add_gen)`:
- Empty entry ⇒ `{'status':'error','msg':'Invalid/empty data was given.'}`.
- Pre-download `error` string: for `live_status == 'is_upcoming'` with `release_timestamp` ⇒ `f"Live stream is scheduled to start at {ts:%Y-%m-%d %H:%M:%S %z}"`; else `entry['msg']` if present.
- `etype = entry.get('_type') or 'video'`:
  - `etype.startswith('url')` ⇒ recursive `self.add(entry['url'], …)`.
  - `etype in ('playlist','channel')` ⇒ materialize `entries` (generators → list), `total_entries = len(entries)`, `index_digits = len(str(total_entries))`. If `playlist_item_limit > 0`, slice `entries[:limit]`. For each entry (1-based `index`):
    - fill `id` via `_entry_id(etr)` if missing; force `_type = 'video'`;
    - `etr[etype] = entry['id'] or entry['channel_id'] or entry['channel']`;
    - `etr[f'{etype}_index'] = zero-padded index` (width `index_digits`), `etr[f'{etype}_count'] = total_entries`, `etr[f'{etype}_autonumber'] = index`;
    - `etr['n_entries'] = total_entries`, `etr['__last_playlist_index'] = total_entries` (yt-dlp internals for auto-padding);
    - copy `id/title/uploader/uploader_id` from the parent as `{etype}_{prop}`;
    - recurse. **Before each entry**, if `_add_gen is not None and self._add_generation != _add_gen`, abort with `{'status':'ok','msg': f'Canceled - added {len(already)} items before cancel'}`.
    - Aggregate: any child error ⇒ `{'status':'error','msg': ', '.join(child msgs)}`, else `{'status':'ok'}`.
  - `etype == 'video'` (or url-with-id-and-title) ⇒ `key = entry.get('webpage_url') or entry['url']`; skip if `key in self._canceled_urls`; skip if `self.queue.exists(key)` (**note: `pending` and `done` are NOT checked — a completed URL can be re-added**); else build `DownloadInfo` and `__add_download`.
  - anything else ⇒ `{'status':'error','msg': f'Unsupported resource "{etype}"'}`.

There is **no `playlist_strict_mode`** in this fork (upstream MeTube has it; it was dropped/never merged here). `playlist_item_limit` is applied twice: as a slice in `__add_entry`, and as `ytdl_options['playlistend']` in `__add_download`.

`add_entry(entry, …)` (public, used by subscriptions) deep-copies the entry and calls `__add_entry` with a fresh `already` set and `_add_gen=None` (never cancellable).

`cancel_add()` just increments `_add_generation` — cooperative cancellation checked only between playlist entries.

### 5.5 Output template resolution and download path

`__add_download(dl, auto_start)`:
1. `dldirectory, err = __calc_download_path(dl.download_type, dl.folder)`; on error return `{'status':'error','msg': …}` — messages:
   - `'A folder for the download was specified but CUSTOM_DIRS is not true in the configuration.'`
   - `'Folder "X" must resolve inside the base download directory "Y"'` (realpath prefix check — note it's a `startswith`, so a sibling dir with a matching prefix passes)
   - `'Folder "X" for download does not exist inside base directory "Y", and CREATE_CUSTOM_DIRS is not true in the configuration.'`
2. `output = OUTPUT_TEMPLATE` or `f'{custom_name_prefix}.{OUTPUT_TEMPLATE}'`.
3. If `entry['playlist_index'] is not None`: swap in `OUTPUT_TEMPLATE_PLAYLIST` (unless empty) and pre-resolve every `playlist*` field via `_resolve_outtmpl_fields(output, sanitized_entry, ('playlist',))`.
4. Same for `channel_index` with `OUTPUT_TEMPLATE_CHANNEL` / prefix `('channel',)`.
   - `_sanitize_path_component` replaces `[\\:*?"<>|]` with `_` in **string** values only (NTFS safety); numbers pass through.
   - `_resolve_outtmpl_fields` walks matches of yt-dlp's own `STR_FORMAT_RE_TMPL` (extended type chars `ljhqBUDS`), and for each match whose root key starts with the given prefix, substitutes `YoutubeDL({'quiet':True}).evaluate_outtmpl(match, info)`. Full yt-dlp template syntax works (defaults `%(x|Unknown)s`, math `%(playlist_index+100)d`, conditionals `%(playlist_index&{} - |)s`). Non-matching fields are left verbatim for the child to resolve.
5. `ytdl_options = _build_ytdl_options(presets, overrides)`; `if playlist_item_limit > 0: ytdl_options['playlistend'] = limit`.
6. `Download(dldirectory, TEMP_DIR, output, OUTPUT_TEMPLATE_CHAPTER, quality, format, ytdl_options, dl)` — note the **chapter template passed here is always the global config value**; the per-download `chapter_template` is applied inside `_download()` only when `split_by_chapters` is true.
7. `auto_start is True` ⇒ `queue.put(download)` + `create_task(__start_download(...))`; else `pending.put(download)`. Then `await notifier.added(dl)`.

Option layering (`_build_ytdl_options`): `dict(config.YTDL_OPTIONS)` → `update(preset)` for each preset **in request order** → `update(overrides)`. `null` values are preserved (so a preset can clear a global `download_archive`). This dict is then fed through `get_opts()` (§6) which deep-copies it.

`Download.__init__` also converts `ytdl_opts['impersonate']` (a string) into `ImpersonateTarget`.

### 5.6 `clear` / `cancel` / `start_pending` semantics

- `start_pending(ids)`: for each id present in `pending`: `queue.put(dl)`, `pending.delete(id)`, `create_task(__start_download(dl))`. Missing ids are only logged. Always `{'status':'ok'}`.
- `cancel(ids)`: for each id — add to `_canceled_urls` (so an in-flight playlist add skips it); if in `pending`, delete + `notifier.canceled(id)`; else if not in `queue`, warn; else if `dl.started()`, `dl.cancel()` (kill, cleanup path emits `canceled`); else set `dl.canceled = True`, `queue.delete(id)`, `notifier.canceled(id)`.
- `clear(ids)`: only touches `done`. If `DELETE_FILE_ON_TRASHCAN`, `os.remove(join(__calc_download_path(...), info.filename))` inside a broad try/except (warn on failure; `chapter_files`/`subtitle_files` are **not** deleted). Then `done.delete(id)` + `notifier.cleared(id)`.

### 5.7 Custom dirs listing

`get_custom_dirs()` (`main.py:902`) — 5-second memoization keyed on `(DOWNLOAD_DIR, AUDIO_DOWNLOAD_DIR, CUSTOM_DIRS_EXCLUDE_REGEX)`, stored as function attributes, timestamped with `asyncio.get_running_loop().time()`. Walk: `pathlib.Path(base).glob('**/')`, strip the `base` prefix and any leading `/`, drop entries where `re.search(EXCLUDE_REGEX, d)` matches, and always ensure `''` (the base itself) is present at index 0. If `DOWNLOAD_DIR == AUDIO_DOWNLOAD_DIR`, `audio_download_dir` is the same list object. This is a full recursive directory walk — O(number of directories) on every cache miss.

### 5.8 Jellyfin post-completion hook

`__sync_jellyfin_library(info)`: no-op unless `JELLYFIN_SYNC_ENABLED is True`; warns and returns if URL or key is blank; parses timeout (invalid ⇒ 20); runs `refresh_jellyfin_library` in the default executor; catches `JellyfinSyncError` (warn) and everything else (exception log). Logs `'Jellyfin library refresh requested for %s (HTTP %s)'`.

### 5.9 ffmpeg postprocessing timeout scaling

Lives in `app/audio_sync_fix.py` (invoked as an `Exec` postprocessor, §6/§10): `duration = ffprobe format=duration` (30 s probe timeout) ⇒ `timeout = max(600, ceil(duration/2))` seconds; if duration is unknown ⇒ `1800`. Commit `0b350d6` raised this from a flat `300`.

---

## 6. `dl_formats` — complete mapping

`AUDIO_FORMATS = ("m4a","mp3","opus","wav","flac")`, `CAPTION_MODES = ("auto_only","manual_only","prefer_manual","prefer_auto")`.

`CODEC_FILTER_MAP`:

| codec | filter fragment |
|---|---|
| `h264` | `[vcodec~='^(h264\|avc)']` |
| `h265` | `[vcodec~='^(h265\|hevc)']` |
| `av1` | `[vcodec~='^av0?1']` |
| `vp9` | `[vcodec~='^vp0?9']` |
| `auto` / unknown | `""` |

### 6.1 `get_format(download_type, codec, format, quality) -> str`

Inputs are lower-cased/stripped with defaults `video`/`any`/`auto`/`best`.

| Condition | Returned selector |
|---|---|
| `format.startswith("custom:")` | `format[7:]` verbatim (escape hatch, checked **first**) |
| `download_type == "thumbnail"` | `bestaudio/best` |
| `download_type == "captions"` | `bestaudio/best` |
| `download_type == "audio"` | `bestaudio[ext={format}]/bestaudio/best`; unknown format ⇒ `ValueError(f"Unknown audio format {format}")` |
| video, format ∉ {any,mp4,ios} | `ValueError(f"Unknown video format {format}")` |
| video, `format == "ios"` | `bestvideo[vcodec~='^((he\|a)vc\|h26[45])']{vres}+bestaudio[acodec=aac]/bestvideo[vcodec~='^((he\|a)vc\|h26[45])']{vres}+bestaudio[ext=m4a]/bestvideo{vcombo}+bestaudio[ext=m4a]/best{vcombo}` |
| video, `format == "mp4"` and `quality == "best_remux"` | `bestvideo+bestaudio/best` |
| video with a codec filter | `bestvideo{codec_filter}{vcombo}+bestaudio{afmt}/bestvideo{vcombo}+bestaudio{afmt}/best{vcombo}` |
| video, codec auto | `bestvideo{vcombo}+bestaudio{afmt}/best{vcombo}` |
| unknown `download_type` | `ValueError(f"Unknown download_type {download_type}")` |

where `vfmt/afmt = ("[ext=mp4]","[ext=m4a]")` for `mp4`/`ios` else `("","")`; `vres = "[height<={quality}]"` unless quality ∈ {`best`,`best_remux`,`worst`}; `vcombo = vres + vfmt`. Note `quality == "worst"` produces no `worst*` selector at all — it degenerates to the `best…` selector.

### 6.2 `get_opts(download_type, _codec, format, quality, ytdl_opts, subtitle_language="en", subtitle_mode="prefer_manual") -> dict`

Starts from `copy.deepcopy(ytdl_opts)`; builds `postprocessors` (prepended) and `late_postprocessors` (appended after the user's own).

| Branch | Option/postprocessor effects |
|---|---|
| `audio` | prepend `{"key":"FFmpegExtractAudio","preferredcodec":format,"preferredquality": 0 if quality=="best" else quality}` (note: the quality stays a **string** like `"192"`). Then if `format != "wav"` **and** `"writethumbnail"` not already in opts: `opts["writethumbnail"]=True` + `{"key":"FFmpegThumbnailsConvertor","format":"jpg","when":"before_dl"}`, `{"key":"FFmpegMetadata"}`, `{"key":"EmbedThumbnail"}` |
| `thumbnail` | `opts["skip_download"]=True`, `opts["writethumbnail"]=True`, `{"key":"FFmpegThumbnailsConvertor","format":"jpg","when":"before_dl"}` |
| `video` + `mp4` + `best_remux` | `opts.pop("format")` (so a user `format` cannot beat `get_format`), `opts["merge_output_format"]="mp4"`, `{"key":"FFmpegVideoConvertor","preferedformat":"mp4"}`, and a **late** `{"key":"Exec","exec_cmd":"python3 /app/app/audio_sync_fix.py %(filepath)q"}` |
| `captions` | `opts["skip_download"]=True`; `subtitlesformat = format` with `txt` mapped to `srt`; then per mode: `manual_only` ⇒ `writesubtitles=True, writeautomaticsub=False, subtitleslangs=[lang]`; `auto_only` ⇒ `writesubtitles=False, writeautomaticsub=True, subtitleslangs=[f"{lang}-orig", lang]`; `prefer_auto` ⇒ both True, `[f"{lang}-orig", lang]`; default/`prefer_manual` ⇒ both True, `[lang, f"{lang}-orig"]` |

Final: `opts["postprocessors"] = prepended + (existing user postprocessors) + late`. Mode/language are normalized (`_normalize_caption_mode` falls back to `prefer_manual`; `_normalize_subtitle_language` falls back to `en`). `_codec` is accepted but unused.

---

## 7. Subscriptions

Module `app/subscriptions.py`. One `SubscriptionManager(config, dqueue, notifier)` instance, an `asyncio.Lock` guarding all mutations, and a single background loop.

### 7.1 Data model

`@dataclass SubscriptionInfo`:

| Field | Type | Default | Notes |
|---|---|---|---|
| `id` | str | required | `str(uuid.uuid4())` |
| `name` | str | required | from `info.title \| channel \| playlist_title \| uploader \| url` |
| `url` | str | required | normalized = `.strip()`; also the uniqueness key (`_url_index`) |
| `enabled` | bool | `True` | |
| `check_interval_minutes` | int | `60` | `max(1, int(...))` on write; floor of 60 s applied at check time |
| `download_type` | str | `"video"` | |
| `codec` | str | `"auto"` | |
| `format` | str | `"any"` | |
| `quality` | str | `"best"` | |
| `folder` | str | `""` | passed as `folder or None` to `add_entry` |
| `custom_name_prefix` | str | `""` | |
| `auto_start` | bool | `True` | |
| `playlist_item_limit` | int | `0` | |
| `split_by_chapters` | bool | `False` | |
| `chapter_template` | str | `""` | passed as `chapter_template or None` |
| `subtitle_language` | str | `"en"` | |
| `subtitle_mode` | str | `"prefer_manual"` | |
| `ytdl_options_presets` | list[str] | `[]` | legacy singular `ytdl_options_preset` migrated |
| `ytdl_options_overrides` | dict | `{}` | |
| `last_checked` | float\|None | `None` | epoch seconds; `None` ⇒ due immediately |
| `seen_ids` | list[str] | `[]` | newest-first; capped at `SUBSCRIPTION_MAX_SEEN_IDS` |
| `error` | str\|None | `None` | last error, or `"; ".join(first 3 queue errors)` |
| `timestamp` | float | `time.time()` | in-memory only — **not** persisted (`_subscription_to_record` omits it; tests assert this) |

`to_public_dict()` — exactly the wire shape for REST + socket events:
`{"id","name","url","enabled","check_interval_minutes","download_type","codec","format","quality","folder","last_checked","seen_count","error"}` where `seen_count = len(seen_ids)`. Secrets/knobs like `custom_name_prefix`, `ytdl_options_*`, `seen_ids` are **not** exposed.

### 7.2 Persistence

`<STATE_DIR>/subscriptions.json` via `AtomicJsonStore(kind="subscriptions")`, shape `{"schema_version":2,"kind":"subscriptions","items":[<record>,…]}`. Legacy shelf at `<STATE_DIR>/subscriptions` is imported once and rewritten. On load: unknown keys are dropped (`SubscriptionInfo(**{k:v for k in field_names})`), `ytdl_options_preset` → `ytdl_options_presets`, `seen_ids` deduped via `dict.fromkeys` and truncated; the file is rewritten if it came from legacy, if `schema_version` differs, or if the normalized records differ from what was read. Every mutation (`_save_locked`) rewrites the whole file, with in-memory rollback on failure.

### 7.3 Scheduling

- `start_background_loop()` (from `on_startup`) spawns `_periodic_loop`: **`await asyncio.sleep(60)` then `run_due_checks()`**, forever. So the tick granularity is 60 s and the first check happens 60 s after startup.
- `run_due_checks()`: under the lock, collect enabled subs where `last_checked is None` or `now - last_checked >= max(60, check_interval_minutes*60)`; then check each **sequentially** outside the lock.
- `check_now(ids=None)`: targets are the named ids that exist, or all enabled subs; each checked sequentially, awaited by the HTTP handler.

### 7.4 Extraction and new-item detection

`extract_flat_playlist(config, url, playlistend, _depth=0)`:
- params = `{quiet, verbose, no_color, extract_flat:True, ignore_no_formats_error:True, lazy_playlist:True, paths:{...}, **config.YTDL_OPTIONS}` — note user options are applied **last** here (unlike `__extract_info`), so `YTDL_OPTIONS` *can* override `extract_flat`. `impersonate` converted. `playlistend` added when `> 0`.
- `_type == 'video'` ⇒ `(info, [])`. `playlist|channel` ⇒ materialize entries, drop falsy, keep `_is_media_entry` ones; if none and `_depth < 1`, try the first up to 5 child URLs recursively (handles YouTube "channel of tabs/collections" pages). `url*` types and anything else ⇒ `(info, [])`.
- `_is_media_entry(e)`: not `playlist|multi_video|channel`, no `entries`, has `webpage_url|url`, and — if `ie_key`/`extractor_key` contains `playlist|channel|tab` — requires at least one of `duration, timestamp, release_timestamp, upload_date, view_count, live_status, availability` to be non-None.
- `_entry_id(e)` = `str(e['id'])` if present else its URL. `_entry_video_url(e)` = `webpage_url or url`.

`add_subscription(...)`:
1. Normalize URL; empty ⇒ `{"status":"error","msg":"Missing URL"}`.
2. Under the lock, reject duplicates against `_url_index` **and** an in-flight `_pending_urls` set ⇒ `{"status":"error","msg":"This URL is already subscribed"}`; otherwise mark pending (cleared in `finally`).
3. `extract_flat_playlist(url, max(SUBSCRIPTION_SCAN_PLAYLIST_END,1))`; `YoutubeDLError` ⇒ `{"status":"error","msg":str(exc)}`; falsy info ⇒ `{"status":"error","msg":"Could not resolve URL"}`; `_type ∉ {playlist,channel}` ⇒ `{"status":"error","msg": VIDEO_ONLY_MSG}` where `VIDEO_ONLY_MSG = "This URL points to a single video, not a channel or playlist. Use Download instead."`.
4. **Backfill suppression:** every currently visible media entry id is written straight into `seen_ids` without queueing, *except* entries with `live_status == 'is_upcoming'` (deliberately left unseen so they download when they go live). `last_checked = time.time()`.
5. Re-check duplicate under the lock, insert, save (rollback on failure), emit `subscription_added`, return `{"status":"ok","subscription": public_dict}`.

`_check_one_unlocked(sub)`:
1. `extract_flat_playlist(sub.url, SUBSCRIPTION_SCAN_PLAYLIST_END)`. On `YoutubeDLError`: set `error = str(exc)`, save, emit `subscription_updated`, return (**`last_checked` is not updated, so the sub retries on the very next tick — a hot-retry loop for permanently broken feeds**).
2. Filter to media entries; if `_type == 'video'` or no entries: set `error = VIDEO_ONLY_MSG`, save, emit, return (again without touching `last_checked`).
3. Snapshot all per-download settings under the lock.
4. New items: every entry whose `_entry_id` is **not** in `seen_ids`, **plus** any entry already seen whose `live_status == 'is_live'` (so a live stream is re-queued once it starts). Note: `queue.exists(url)` in `__add_entry` prevents a duplicate while it is still downloading, but a *finished* live URL can be re-added on the next check.
5. Queue each via `dqueue.add_entry(entry, …)` with `_type` forced to `video`, `webpage_url` set, `id` filled. Entries that fail (`status == 'error'`) are **not** marked seen (so they retry) and their messages are collected.
6. `seen_ids = dict.fromkeys(queued_ids + previous_seen_ids)` truncated to `SUBSCRIPTION_MAX_SEEN_IDS` (newest-first), `last_checked = now`, `error = "; ".join(queue_errors[:3]) or None`, save, emit `subscription_updated`.

`update_subscription(id, changes)`: only `enabled` (via `_coerce_bool`, accepting bools and `true/1/on`/`false/0/off` strings, else `ValueError("enabled must be a boolean")` → HTTP 500), `check_interval_minutes` (`max(1,int(...))`, `int()` failure ⇒ 500), and `name` (non-empty). Rollback via deep copy on save failure. Emits `subscription_updated`, returns `{"status":"ok","subscription":…}` or `{"status":"error","msg":"Subscription not found"}`.

`delete_subscriptions(ids)`: removes ids present, drops url index entries, saves (whole-map rollback on failure), emits `subscription_removed` per id, always `{"status":"ok"}`.

Edge cases worth preserving: subscription checks are strictly sequential (a slow channel blocks all others and `POST /subscriptions/check`); the loop never runs on startup (first tick at +60 s); `error` is cleared only by a fully successful check; `folder=""` becomes `None` so downloads land in the base dir; subscriptions bypass all of `parse_download_options`' per-request validation on the *stored* values (they were validated at subscribe time).

---

## 8. Telegram bot

`app/telegram_bot.py`. Constructed on `on_startup` with `dqueue`, the legacy `get_available_formats()` list from `main.py:390`, `STATE_DIR`, and the four `TELEGRAM_*` ints/bools.

Startup gating (all silent no-ops other than a log line): `enabled` false ⇒ "Telegram bot disabled"; `TELEGRAM_BOT_TOKEN` empty ⇒ error; `TELEGRAM_ALLOWED_CHAT_IDS` empty after parsing ⇒ error. Otherwise: load chat config, build a PTB `Application`, register handlers, `initialize()`, `start()`, `updater.start_polling(drop_pending_updates=True)`, spawn `_monitor_downloads()`.

Handlers:

| Handler | Trigger | Behavior |
|---|---|---|
| `CommandHandler("start")` | `/start` | Replies "Hi! Send one or more links and I will queue them for download.\nUse /config to set default format/quality for this chat." |
| `CommandHandler("config")` | `/config` | Sends the config text + inline keyboard |
| `CallbackQueryHandler(pattern=r"^cfg:")` | button presses | see callback grammar below |
| `MessageHandler(filters.TEXT & ~filters.COMMAND)` | any text message | URL extraction + queueing |

Authorization: `_get_authorized_chat_id(update)` returns `None` (silently ignoring the update, with a warning log) unless `update.effective_chat.id ∈ allowed_chat_ids`. `TELEGRAM_ALLOWED_CHAT_IDS` is a comma list of ints; unparseable entries are warned and skipped.

Callback data grammar: `cfg:menu:{main|format|quality|limit}`, `cfg:toggle:split`, `cfg:set:format:{fmt}`, `cfg:set:quality:{q}`, `cfg:set:limit:{int}`. All replies use `query.edit_message_text(...)` after `query.answer()`. Setting a format resets quality to the first available if the current one is invalid; setting quality is ignored if it isn't in the format's list; limit keyboard offers `[0,1,5,10,20]`.

Per-chat config: `<STATE_DIR>/telegram_bot_config.json`, a JSON object keyed by `str(chat_id)`. Written atomically-ish via `config_path.with_suffix(".tmp")` + `replace` (note: `with_suffix` turns `telegram_bot_config.json` into `telegram_bot_config.tmp`). Defaults created on first access: `{"format":"mp4","quality":"best","download_type":"video","codec":"auto","subtitle_language":"en","subtitle_mode":"prefer_manual","folder":"","custom_name_prefix":"","playlist_item_limit":<DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT>,"auto_start":true,"split_by_chapters":false,"chapter_template":<OUTPUT_TEMPLATE_CHAPTER>}`.

URL handling:
- `URL_RE = re.compile(r"https?://[^\s<>()\[\]{}\"']+")`; each match is `rstrip`ed of `.,;:!?)]}>'"`; duplicates dropped preserving order.
- If more than `max_urls_per_message`, a message is sent (`Too many links in one message (N). Maximum allowed: M.`) and the list is truncated.
- `_validate_url`: scheme must be `http|https`; netloc/hostname required; rejects `localhost` and `*.local`; if the host parses as an IP address, rejects private/loopback/link-local/multicast/reserved/unspecified (SSRF guard). Rejected ones are reported as `Ignored invalid links:\n- <url> (<reason>)`.
- Selection normalization `_normalize_download_selection(config)` maps the legacy `format`/`quality` chat config into the new 4-tuple: audio formats ⇒ `audio`; `thumbnail` ⇒ `thumbnail/jpg/best`; `captions` ⇒ `captions/srt/best`; `quality == "audio"` ⇒ `audio/m4a/best`; `quality == "best_ios"` ⇒ `video/ios/best`; else pass through.
- For each valid URL: set the `contextvars.ContextVar` `telegram_current_chat_id`, `await dqueue.add(url, download_type, codec, format, quality, folder, custom_name_prefix, playlist_item_limit, auto_start, split_by_chapters, chapter_template, subtitle_language, subtitle_mode, [], {})` (exact positional order asserted by `test_telegram_bot.py`), then reset the var.
- Reports: `Queued N link(s) with current chat config.` and/or `Some links failed:\n- <url>: <msg>`.

Queue integration (via `Notifier` in `main.py` calling into the bot):

| Callback | Effect |
|---|---|
| `on_added(dl)` | Reads the `contextvars` chat id (set only while `dqueue.add` is awaited — so **playlist children added during that call are attributed to the chat, but nothing added by the web UI or subscriptions is watched**). Creates/updates `_watched_downloads[dl.url] = WatchedDownload(chats={chat_id}, started_at=now, last_progress_at=now)`. |
| `on_updated(dl)` | If watched and `dl.status in ("downloading","preparing")`, refresh `last_progress_at = time.monotonic()`. |
| `on_completed(dl)` | Pops the watch entry; if `status == "finished"` sends `✅ Download complete: {title}` + `\nFile: {filename}` when a filename exists; else `❌ Download failed: {title}\n{msg or error or "Download failed"}`. One message per watching chat. |
| `on_canceled(url)` | Drops the watch entry silently. |

`_monitor_downloads()` loop: every **15 s**, for each watched download and each watching chat — if `now - last_progress_at > stall_timeout_seconds` and not yet notified: `⚠️ Download seems stalled for {int(secs)}s:\n{url}` (once per chat, tracked in `stall_notified`); if `now - started_at > hard_timeout_seconds` and not yet notified: `⏱️ Download is taking longer than expected ({int(secs)}s):\n{url}` (once per chat, `timeout_notified`). Messages are collected under the lock and sent after release. **There is no progress-message editing or percentage reporting** — despite the task brief, the bot only sends discrete notifications; `_send_message` always sends a *new* message and swallows `TelegramError` with an error log. Neither timeout cancels the download.

`stop()`: cancels the monitor task, stops updater/app, `shutdown()`.

---

## 9. StreamingCommunity extractor

`app/extractors/streamingcommunity.py`. HTTP via `curl_cffi.requests.Session(impersonate="chrome")` (TLS fingerprint spoofing) + BeautifulSoup HTML parsing. Hard-coded `USER_AGENT` = Chrome 131 on Win64.

### 9.1 Detection and dispatch

- `can_extract(url)`: `urlparse(url).hostname` lower-cased **contains** the substring `streamingcommunity` (so any `*streamingcommunity*.tld` mirror matches). Errors ⇒ `False`.
- `extract_info(url)` (static): `base_url = f"{scheme}://{netloc}"`, then `extract(url)`:
  - `"/season-" in url` ⇒ `extract_season`
  - `"/watch/" in url` ⇒ `extract_watch`
  - `"/titles/" in url` ⇒ `extract_title`
  - else log error, `None`. Any exception ⇒ log + `None` (falls through to yt-dlp in `__extract_info`).

### 9.2 Scraping flow

1. `get_version()`: `GET {base}/it`, parse `div#app[data-page]` as JSON, take `data["version"]` — the Inertia.js asset version; cached on the instance.
2. `_inertia_get(path)`: `GET {base}{path}` with headers `x-inertia: true`, `x-inertia-version: <version>`; `raise_for_status()`; return `response.json()`. Missing version ⇒ `Exception("Could not get site version")`.
3. For a watch page: `props["embedUrl"]` → `GET embedUrl` → find the first `<iframe>` → `iframe["src"]` (vixcloud domain).
4. `get_m3u8_from_embed(embed_url)`: `GET` it, scan every `<script>` whose text contains `masterPlaylist`; then
   - `'token': '...'` via regex, `'expires': '<digits>'` via regex;
   - `window.streams = [...]` parsed as JSON; pick the entry with `active: true`, else the first; take `url` (un-escaping `\/`). Fallback: `url: '...'` inside `masterPlaylist`.
   - Existing query params of that stream URL are preserved (server params like `ub`, `ab`, `b`); `h=1` is added **only** if `window.canPlayFHD = true`; then `token` and `expires` are appended. Re-assembled with `urlunparse`/`urlencode`.
   - Returns `{"m3u8_url": ..., "referer": embed_url}` or `None`.

### 9.3 Entry shapes

`extract_watch(url)` (regex `/watch/(\d+)(?:\?e=(\d+))?`) and `extract_episode(...)` return a **video** entry:
```json
{"id":"sc_<title_id>[_<episode_id>]","title":"<Name>[ S01E02[ - <ep name>]]",
 "url":"<watch url>","webpage_url":"<same>","ext":"mp4","_type":"video",
 "extractor":"streamingcommunity","extractor_key":"StreamingCommunity",
 "season_number":int|null,"episode_number":int|null,"episode":"<ep name>",
 "series":"<title name>"|null,
 "_sc_needs_m3u8_extraction":true,"_sc_base_url":"<base>"}
```
Note the m3u8 URL resolved during extraction is **discarded** — only the watch URL is stored, because tokens expire quickly (just-in-time re-extraction at download time). `series` is `null` for movies (`title_type != "tv"`).

`extract_season(url)` (regex `/titles/(\d+)-([^/]+)/season-(\d+)`): fetches `/it/titles/{id}-{slug}` for the name, then `/it/titles/{id}-{slug}/season-{n}` for `props.loadedSeason.episodes`, calls `extract_episode` per episode (**each episode does 3+ HTTP round-trips** — a 20-episode season is ~60 requests), and returns a playlist:
```json
{"id":"sc_<id>_s<n>","title":"<Name> Season <n>","original_url":"<url>",
 "_type":"playlist","entries":[…],"extractor":"streamingcommunity","extractor_key":"StreamingCommunity"}
```
`extract_title(url)` (regex `/titles/(\d+)-([^/]+)$`): movies ⇒ delegate to `extract_watch(f"{base}/it/watch/{id}")`; TV ⇒ iterate `title.seasons` (or `props.loadedSeason`), call `extract_season` per season and flatten into one playlist with `id = sc_<title_id>`.

`get_fresh_m3u8(base_url, watch_url)` (static, called in the **child process** at download time): re-does steps 2–4 with a brand-new session, derives `origin` from the iframe src, serializes session cookies as `"k=v; k=v"`, performs a diagnostic `GET` of the m3u8 (logging status/length/preview — pure debug traffic), and returns `{"m3u8_url", "http_headers": {"Referer": <embed url>, "Origin": <iframe origin>, "User-Agent": USER_AGENT}, "cookies": "<cookie string>"}` or `None`.

### 9.4 Download paths

`Download._download_streamingcommunity()` (returns `None` ⇒ "not mine, use yt-dlp"; `0` ⇒ success; non-zero ⇒ failure):
1. Gate: `info.entry` truthy, `entry['extractor']` contains `streamingcommunity`, and `entry['_sc_needs_m3u8_extraction']` truthy.
2. `get_fresh_m3u8(entry['_sc_base_url'], info.url)`; failure ⇒ `{"status":"error","msg":"Failed to extract video URL"}` and return 1.
3. `safe_title = re.sub(r'[<>:"/\\|?*]', "_", info.title).strip(". ")`; `output_path = <download_dir>/<safe_title>.mp4`; also writes `<safe_title>.info.json` containing the whole entry (this is what the NFO generator would consume).
4. If `os.environ["SC_USE_FFMPEG"]` ∈ `{true,1,on}` ⇒ push `{"status":"downloading","msg":"Starting ffmpeg download..."}` and run the ffmpeg path. Otherwise push `{"status":"downloading","msg":"Starting N_m3u8DL-RE download..."}` and run N_m3u8DL-RE with `report_error=False`; on non-zero: log warning, push `{"status":"downloading","msg":"N_m3u8DL-RE failed, retrying with ffmpeg..."}`, `_cleanup_streamingcommunity_partial()`, then run the ffmpeg path (which does report errors).

`_download_streamingcommunity_nm3u8(...)`:
```
N_m3u8DL-RE <m3u8> --save-dir <download_dir> --save-name <safe_title>
  --tmp-dir <temp_dir or download_dir/.tmp> --thread-count <SC_THREAD_COUNT>
  --auto-select --del-after-done --no-log
  --mux-after-done format=mp4:muxer=ffmpeg --log-level INFO
  -H "User-Agent: …" -H "Referer: …" -H "Origin: …" [-H "Cookie: …"]
```
stdout+stderr merged; lines ANSI-stripped and the last 30 kept for error reporting; progress parsed at most every 0.5 s. On exit ≠ 0 ⇒ `{"status":"error","msg": f"N_m3u8DL-RE failed with code {rc}: <last 500 chars of last 20 lines>"}` (unless `report_error=False`). On success: use `output_path`, else the newest `glob(<safe_title>*.mp4)` by mtime; if found ⇒ `{"status":"finished","filename": path}`.

**Gapless fallback mux** (commit `4a9181c`): if the expected mp4 doesn't exist but a segment directory `<download_dir>/<safe_title>` does, collect all `.m4s/.ts/.mp4/.m4a/.aac` files under it, sort by **natural (numeric-aware) filename order** — explicitly *not* mtime, because parallel downloads scramble mtimes — binary-concatenate them into `<seg_dir>/_merged.ts` (1 MiB chunks), then `ffmpeg -y -i _merged.ts -map 0 -c copy -bsf:a aac_adtstoasc -movflags +faststart <output>` with a 600 s timeout. The code comments emphasize that ffmpeg's `concat` demuxer must **not** be used because it pads each segment to its container duration (~64 ms A/V gap and a dropped frame per join). Success ⇒ `finished` + `rmtree(seg_dir)`; otherwise `{"status":"error","msg":"Download finished but muxing failed"}` (or `"…but output file not found"` / `"…but no segments to mux"`).

`_download_streamingcommunity_ffmpeg(...)`: builds a CRLF-joined header blob (`User-Agent`, `Referer`, `Origin`, optional `Cookie`), probes duration with `ffprobe -show_entries format=duration` (30 s timeout, failures tolerated), then
```
ffmpeg -y -headers <hdrs> -i <m3u8> -c copy -bsf:a aac_adtstoasc -progress pipe:1 <output>
```
stderr drained on a daemon thread (last 20 lines kept). Progress: parse `out_time_ms`, `total_size`, `speed=<x>x` lines; on each `progress` line and at most every 0.5 s, push `{"status":"downloading","downloaded_bytes": total_size, "total_bytes_estimate": int(size/(time/duration)), "eta": int((duration-time)/speed), "speed": speed*(size/time)}` (estimate/eta/speed only when duration and speed are known). On rc 0 + file exists ⇒ `{"status":"finished","filename": output}`; else `{"status":"error","msg": f"FFmpeg failed with code {rc}: <last 500 chars>"}`.

`_cleanup_streamingcommunity_partial`: removes the partial mp4 and rmtree's `<temp>/<safe_title>`, `<temp>/<safe_title>.tmp`, `<download_dir>/<safe_title>`.

N_m3u8DL-RE progress parsing (`_parse_nm3u8_progress`, unit-tested): strips ANSI/OSC sequences (`_NM3U8_ANSI_RE`), converts `\r`→`\n`, then takes the **last** match of each pattern (Spectre.Console repaints multiple frames per read, and the first is usually `0/100 0.00%`):

| Pattern | Fields |
|---|---|
| `(\d+)/(\d+)\s+([\d.]+)%` | `downloaded_bytes` = segment index, `total_bytes` = segment count (abused as counters) |
| `([\d.]+)\s*(KB\|MB\|GB)\s*/\s*([\d.]+)\s*(KB\|MB\|GB)` | overrides both with real byte sizes (KB=1024, MB=1024², GB=1024³) |
| `([\d.]+)\s*(KB\|MB\|GB)ps` | `speed` in bytes/s |
| `(\d{2}):(\d{2}):(\d{2})(?=\s\|$)` | `eta` in seconds |

If neither size pattern matches, `{}` is returned and nothing is emitted.

Output naming: SC downloads bypass `OUTPUT_TEMPLATE` entirely — the file is always `<download_dir>/<sanitized info.title>.mp4` (plus `.info.json`). SC playlists therefore ignore `OUTPUT_TEMPLATE_PLAYLIST` too.

---

## 10. Jellyfin sync, NFO generator, audio_sync_fix

**`app/jellyfin_sync.py`** — `refresh_jellyfin_library(*, base_url, api_key, timeout) -> int`. `base_url.rstrip('/')`; empty base ⇒ `JellyfinSyncError("JELLYFIN_URL is required")`; empty key ⇒ `JellyfinSyncError("JELLYFIN_API_KEY is required")`. Issues `POST {base}/Library/Refresh` with headers `Accept: application/json` and `Authorization: MediaBrowser Token="<key>"` and **no body**, via `urllib.request.urlopen(timeout=timeout)`. Returns `response.status` (typically 204). `HTTPError` ⇒ `JellyfinSyncError(f"Jellyfin refresh failed with HTTP {code}: {details}")` where `details` prefers the JSON `message`/`Message` field; `OSError` ⇒ `JellyfinSyncError(f"Jellyfin refresh request failed: {exc}")`. It refreshes **all** libraries — there is no per-library or per-item targeting (hence `JELLYFIN_LIBRARY_ID` in the sample compose being inert). Called once per finished download from `DownloadQueue._post_download_cleanup` → `__sync_jellyfin_library` (executor thread).

**`app/jellyfin_nfo_generator.py`** — a standalone CLI (`python3 jellyfin_nfo_generator.py <video_filepath>`), **not referenced by any code path in this repo** (only `audio_sync_fix.py` is wired via the `Exec` postprocessor). It is intended to be used from a user-supplied `YTDL_OPTIONS` `Exec` postprocessor. Behavior: derive `<base>.info.json`; if absent, warn and exit 0 (treated as success); else read it and write `<base>.nfo`, then **delete the info.json**. XML root is `episodedetails` when `series|season_number|episode_number` is set, else `movie`. Elements: `title`, `originaltitle`, (`showtitle`, `season`, `episode`, `subtitle` for episodes), `plot`, `year`+`premiered` (from `upload_date` `YYYYMMDD`), `dateadded` (UTC now, `%Y-%m-%d %H:%M:%S`), `studio`+`director` (from `uploader` or `channel`), `uniqueid type="streamingcommunity"|"youtube"` (from `id`, based on whether `extractor` contains `streamingcommunity`), `website` (`original_url` or `webpage_url`), up to 20 `tag`s, `runtime` in whole minutes. Output is pretty-printed with blank lines removed. Exit code 1 on JSON/IO/unexpected errors.

**`app/audio_sync_fix.py`** — wired automatically as the **late** `Exec` postprocessor `python3 /app/app/audio_sync_fix.py %(filepath)q` for `download_type=video, format=mp4, quality=best_remux` only (see §6.2). Purpose: after SponsorBlock's `ModifyChapters` stream-copy cuts, video cuts land on keyframes while audio cuts are exact, so drift accumulates; re-encoding audio regenerates timestamps. Flow: file must exist and end in `.mp4` (else skip, exit 0); must have a video stream per `ffprobe -select_streams v` (else skip); compute the duration-scaled timeout (§5.9); `mkstemp(suffix=".mp4", dir=<same dir>)`; run
```
ffmpeg -y -loglevel warning -i <file> -map 0 -dn -ignore_unknown -c copy -c:a aac -b:a 256k -movflags +faststart <tmp>
```
then `os.replace(tmp, file)`. Non-zero rc / timeout / exception ⇒ log + exit 1 (which makes yt-dlp's `Exec` postprocessor report a failure); the temp file is removed in `finally`. Note the hard-coded container path `/app/app/audio_sync_fix.py` — a Rust port should reimplement this in-process or keep an equivalent binary at a known path.

---

## 11. BgUtils POT provider

Fully out-of-process; **no Python code in this repo references it**. Wiring is entirely Docker-level:

1. **Sidecar binary**: the Dockerfile resolves the latest tag of `jim60105/bgutil-ytdlp-pot-provider-rs`, downloads `bgutil-pot-linux-{x86_64|aarch64}` (per `TARGETARCH`) to `/usr/local/bin/bgutil-pot`, `chmod +x`.
2. **yt-dlp plugin**: `bgutil-ytdlp-pot-provider-rs.zip` from the same release is unzipped into `python3 -c 'import site; print(site.getsitepackages()[0])'`, i.e. the site-packages dir, where yt-dlp auto-discovers `yt_dlp_plugins`. No `extractor_args` are needed for the default configuration.
3. **Process**: `docker-entrypoint.sh` starts `bgutil-pot server >/tmp/bgutil-pot.log 2>&1 &` (as the target user via `gosu` when running as root) **before** exec'ing MeTube. It is a background child of the entrypoint shell with **no supervision or restart** — if it dies, POT silently stops working and YouTube downloads start failing with bot checks. Default listen address is the provider's own default (`127.0.0.1:4416`); nothing in this repo overrides it.
4. **Deno**: installed via `deno.land/install.sh` into `/usr/local` because `yt-dlp[deno]` + `yt-dlp-ejs` use it to run YouTube's JS challenge solver.
5. Users can add `extractor_args` (e.g. player clients) through `YTDL_OPTIONS`/`YTDL_OPTIONS_FILE`; the tests exercise `{"extractor_args": {"youtube": {"player_client": ["web"]}}}`.

A Rust rewrite needs to: keep spawning and (ideally) supervising `bgutil-pot server`, and keep the plugin installed wherever the chosen yt-dlp invocation strategy can find it (a Rust port that shells out to a `yt-dlp` binary must ensure the plugin dir is on yt-dlp's plugin search path).

---

## 12. Process / deploy

### 12.1 Dockerfile stages

**Stage 1 — `node:lts-alpine AS builder`**: `COPY ui ./`, `corepack enable`, `CI=true pnpm install && pnpm run build` → `/metube/dist/metube`.

**Stage 2 — `python:3.13-slim`**, `WORKDIR /app`:
- `COPY pyproject.toml uv.lock docker-entrypoint.sh ./`; strip CRs from the entrypoint; `chmod +x`.
- apt packages: `ca-certificates ffmpeg unzip aria2 coreutils gosu curl file gdbmtool sqlite3 libssl3t64 tini libstdc++6 build-essential`.
- `uv` installed to `/usr/local/bin`, then `UV_PROJECT_ENVIRONMENT=/usr/local uv sync --frozen --no-dev --compile-bytecode`, cache cleaned, and the `uv`/`uvx`/`uvw` binaries **deleted**.
- Deno installed to `/usr/local`; `build-essential` purged; apt lists removed; `/.cache` created `777`.
- POT provider binary + yt-dlp plugin (see §11).
- `N_m3u8DL-RE v0.5.1-beta` (`linux-x64` or `linux-arm64`, build `20251029`) untarred into `/usr/local/bin`.
- `pip install --break-system-packages --no-deps yt-dlp==2026.8.30.232658.dev0` — the nightly pin, deliberately overriding whatever `uv.lock` resolved.
- `COPY app ./app`; `COPY --from=builder /metube/dist/metube ./ui/dist/metube`.
- ENV: `PUID=1000 PGID=1000 UMASK=022 DOWNLOAD_DIR=/downloads STATE_DIR=/downloads/.metube TEMP_DIR=/downloads PORT=8081 SC_USE_FFMPEG=false DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1`; `VOLUME /downloads`; `EXPOSE 8081`.
- `HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 CMD curl -fsS "http://localhost:${PORT}/" || exit 1` (hits the SPA index, so it does not verify the queue or the POT sidecar; also ignores `URL_PREFIX`).
- `ARG VERSION=dev` → `ENV METUBE_VERSION=$VERSION`.
- `ENTRYPOINT ["/usr/bin/tini","-g","--","./docker-entrypoint.sh"]` (tini as PID 1 with process-group signalling).

External binaries required at runtime: `ffmpeg`, `ffprobe`, `aria2c` (only if a user configures it), `deno`, `N_m3u8DL-RE`, `bgutil-pot`, `gosu`, `tini`, `curl` (healthcheck), plus `gdbmtool`/`sqlite3` (legacy shelve inspection).

### 12.2 Entrypoint (PUID/PGID/umask)

```sh
PUID="${UID:-$PUID}"; PGID="${GID:-$PGID}"     # legacy UID/GID win
umask ${UMASK}
mkdir -p "$DOWNLOAD_DIR" "$STATE_DIR" "$TEMP_DIR"
if running as root:root:
    warn if PUID==0
    unless CHOWN_DIRS=false:  chown -R PUID:PGID /app "$DOWNLOAD_DIR" "$STATE_DIR" "$TEMP_DIR"
    gosu PUID:PGID bgutil-pot server >/tmp/bgutil-pot.log 2>&1 &
    exec gosu PUID:PGID python3 app/main.py
else:
    bgutil-pot server >/tmp/bgutil-pot.log 2>&1 &
    exec python3 app/main.py
```
Note the recursive `chown` of `/app` **and the entire downloads volume** on every container start — a multi-TB library makes startup very slow (hence `CHOWN_DIRS=false`).

### 12.3 CI workflows

| Workflow | Trigger | What it does |
|---|---|---|
| `main.yml` (`build`) | push to `master` (ignoring `**.md`), or manual `workflow_dispatch{publish}` | Job `quality-checks`: Node LTS + pnpm (`install --frozen-lockfile`, `lint`, `build`, `ng test --watch=false`), then uv (`sync --frozen --group dev`), `python -m compileall app`, `uv run pytest app/tests/`, and a Trivy fs scan (CRITICAL/HIGH, table output, non-blocking). Job `dockerhub-build-push`: QEMU + Buildx, multi-arch `linux/amd64,linux/arm64`, `VERSION=$(date +%Y.%m.%d)`, tags `<dockerhub>:latest`, `<dockerhub>:<date>`, `ghcr.io/<repo>:latest`, `ghcr.io/<repo>:<date>`. Jobs `dockerhub-sync-readme` (lsiodev/readme-sync — hence the 25 000-char README limit in AGENTS.md) and `create-release` (deletes any existing tag/release for today's date, then publishes a release whose body lists the images plus `git log <last date tag>..HEAD`). |
| `dev-build.yml` | PR labeled/synchronize/opened/reopened/closed | If the PR carries the `dev` label: build `linux/amd64` with `VERSION=dev-pr<N>`, push `ghcr.io/<repo>:dev`, and comment on the PR. On close: delete the `dev` package version from GHCR and comment. |
| `update-yt-dlp.yml` | cron `0 0 */3 * *` + manual | **yt-dlp nightly bump automation**: greps `yt-dlp==<ver>` out of the Dockerfile, runs `pip install --dry-run --pre yt-dlp` and parses "Would install yt-dlp-<ver>"; if different, `sed`s the Dockerfile, pushes branch `auto/update-yt-dlp-nightly-<ver>` (reusing it if it exists), opens (or reuses) a PR titled `Upgrade yt-dlp nightly to <ver>` with the `automated` label if present, and enables `gh pr merge --auto --squash`. Uses `secrets.TATOALO_REPO_PAT`. Writes a step summary. This is why ~30 of the last 40 commits are yt-dlp bumps. |
| `upstream-sync-check.yml` | cron `0 3 * * 6` + manual | **Upstream sync**: reads `alexta69/metube`'s latest release, determines the last synced version from closed issues labeled `upstream-sync` + `synced:<ver>`, skips if an issue for the tag already exists, ensures labels, adds the `upstream` remote and fetches tags, diffs `last_synced...<tag>` (stat, name-only, and full diff truncated to 50 KB) plus a "fork gap" `HEAD...<tag>` stat, sends it to a configurable LLM (`LLM_PROVIDER_URL/MODEL/API_KEY`, with gpt-5 reasoning-model support per commit `8e0de23`), and files an issue `Upstream Sync Analysis: <tag>`. |
| `upstream-sync-label.yml` | issue closed | If the issue has the `upstream-sync` label, parse `Upstream Sync Analysis: <VERSION>` from the title, create the label `synced:<VERSION>` (green `0e8a16`) if needed, and apply it — this is the state store the check workflow reads. |

Local dev commands (AGENTS.md): `uv sync --frozen --group dev`, `python -m compileall app`, `uv run pytest app/tests/` (pytest config: `asyncio_mode = auto`, `testpaths = ["app/tests"]`, `pythonpath = [".","app"]`, `addopts = "-v"`).

---

## 13. Known pain points / performance issues (rewrite targets)

**Broadcast / socket flooding**
1. Every single yt-dlp progress-hook message produces one `sio.emit('updated', …)` **broadcast to all clients**, with no throttling, coalescing, or diffing. yt-dlp's default progress interval plus fragmented HLS downloads means dozens of events/second per download, multiplied by `MAX_CONCURRENT_DOWNLOADS` and by the number of open browser tabs. This is the single biggest snappiness killer. A rewrite should tick at a fixed cadence (e.g. 4 Hz), send only changed fields, and ideally per-download deltas.
2. Each payload is the **entire** download-info object, JSON-encoded and then embedded as a *string* inside the Socket.IO frame (double encoding, double parse on the client).
3. `Notifier.updated` also calls into the Telegram bot on every update, taking an `asyncio.Lock` per event.
4. `log.debug(f"...{status}")` is executed with eager f-string formatting on every status message even when DEBUG is off (two such lines in `update_status`).

**State persistence**
5. `PersistentQueue.put/delete` rewrite the **whole** JSON file with `fsync` on the file *and* the directory, on every enqueue/dequeue. Adding a 500-item playlist ⇒ 500 full-file rewrites with O(n²) total bytes written and 1 000 fsyncs. Same for `SubscriptionManager._save_locked` on every check.
6. Mitigation already present (transient progress fields are not persisted, so progress updates don't hit disk) — keep this property.
7. `_load_state_items` normalizes and, on any difference, immediately rewrites the file at startup.

**Process / concurrency model**
8. One `multiprocessing.Process` **fork per download** plus a `multiprocessing.Manager()` proxy queue: every status message is pickled, sent over a socket to the manager process, and re-pickled to the parent. Two thread-pool threads are consumed per active download (`proc.join` blocked forever, and a fresh `run_in_executor(queue.get)` per message). With `MAX_CONCURRENT_DOWNLOADS` large this saturates the default executor and starves `__extract_info`, which also runs in the same pool.
9. Metadata extraction (`__extract_info`) is a **blocking, un-throttled** executor call: adding a playlist of N items performs N sequential extractions on the event loop's thread pool; the UI shows nothing until each entry is added.
10. `Download.cancel()` uses `proc.kill()` (SIGKILL) — no cleanup inside the child, orphaned ffmpeg/N_m3u8DL-RE grandchildren, partial `.part` files left except for the single `tmpfilename`.
11. `_post_download_cleanup` runs **inside** the semaphore, so the (synchronous, disk-touching) `done.put` + file rewrite blocks the next download from starting.
12. Startup `__import_queue` **auto-restarts everything that was in the queue**, all at once, before the UI connects.
13. `self.active_downloads` is dead code; nothing tracks actually-running downloads.

**Blocking the event loop**
14. `get_custom_dirs()` does a full recursive `glob('**/')` synchronously on the event loop (5 s memo helps, but a large library still stalls every cache miss, and it runs on every client `connect`).
15. `POST /subscriptions/check` awaits all checks sequentially — a request can hang for minutes; a single slow feed blocks the periodic loop for all subscriptions.
16. `os.path.getsize` calls in `update_status` are synchronous (cheap, but on network storage they aren't).
17. Subscription failures don't update `last_checked`, so a permanently broken feed is re-extracted every 60 s forever.

**Correctness / robustness smells worth fixing in the port**
18. `queue.exists(key)` is the only dedupe check in `__add_entry` — `pending` and `done` are ignored, so re-adding a URL that's pending creates a *second* pending entry keyed identically (the second `put` silently replaces the first).
19. `DownloadInfo.filename` / `chapter_files` are created lazily, so emitted objects sometimes lack keys the TypeScript interface declares as required.
20. `subtitle_files` is never persisted, so it disappears from completed items after a restart; `DELETE_FILE_ON_TRASHCAN` only deletes the primary `filename`, leaving chapter/subtitle files orphaned.
21. `__calc_download_path` uses `realpath(...).startswith(realpath(base))` — a sibling directory sharing a prefix (`/downloads-evil` vs `/downloads`) passes the containment check.
22. `auto_start` is compared with `is True`, so a JSON string `"true"` silently routes the download to *pending*.
23. `tmpfilename` is unconditionally overwritten by every status message (including ones with no `tmpfilename`), so partial-file cleanup on failure often has nothing to delete.
24. `sio` responses use `text/plain` for JSON bodies (§2).
25. `update_subscription` can raise `ValueError`/`TypeError` out of the handler ⇒ HTTP 500 instead of 400.
26. `PersistentQueue.__init__` uses non-recursive `os.mkdir` for the state dir's parent.
27. `get_fresh_m3u8` performs an extra full `GET` of the m3u8 purely for debug logging on every SC download.
28. `bgutil-pot` is an unsupervised background shell child; the healthcheck can't detect its death.
29. `vps_setup.md` (untracked in the working tree) contains what look like **live secrets**: a Telegram bot token, a Jellyfin API key, and a WireGuard private key. Worth rotating and keeping out of the repo/spec.

---

# ~30-line summary of the most important facts and surprises

1. Two processes-worth of state: an aiohttp + python-socketio server, and one **forked `multiprocessing.Process` per download** that runs yt-dlp and streams status dicts back through a `multiprocessing.Manager().Queue()` proxy.
2. The parent burns **two thread-pool threads per active download** (blocking `proc.join`, plus a `run_in_executor` per status message) in the same default executor used for blocking metadata extraction.
3. `DownloadInfo.url` is the universal primary key — queue key, cancel id, delete id, socket `canceled`/`cleared` payload. Not the `id`.
4. Socket.IO payloads are **JSON strings inside the event argument** (double-encoded); the Angular client `JSON.parse`s them. Socket.IO path is `URL_PREFIX + 'socket.io'`, default namespace only.
5. `all` is a 2-element array: `[queue+pending as [key,info] pairs, done as pairs]`; `history` (REST) is instead `{done,queue,pending}` arrays of bare info objects — two different shapes for the same data.
6. Status enum is exactly `pending → preparing → downloading* → finished|error`. There is **no** postprocessing status, so long ffmpeg phases look frozen to the UI.
7. `percent` is deliberately clamped to `[0, 99.9]` while active and forced monotonic per progress source; `100.0` only on `finished`. Fragment counts bound the bogus early-HLS `1 KiB/1 KiB` estimate.
8. Progress is broadcast **once per yt-dlp progress hook, to every client, with the full object, unthrottled** — the primary performance problem to fix.
9. Persistence is three whole-file, fsync'd JSON rewrites (`queue.json`, `pending.json`, `completed.json`) plus `subscriptions.json`, rewritten on **every** put/delete; transient progress fields are excluded (good), and `entry` is compacted to playlist/channel keys — except StreamingCommunity entries, which are stored whole.
10. Legacy Python `shelve`/pickle state files are still imported once and migrated to `schema_version: 2` JSON; invalid JSON is quarantined to `<path>.invalid.<ts>`.
11. Concurrency has **two** semaphores: the global `MAX_CONCURRENT_DOWNLOADS` (default 3), and a dedicated `SC_MAX_CONCURRENT_DOWNLOADS` (default 1) acquired *outside* the global one so a queued SC download never holds a global slot.
12. On startup, everything previously in `queue.json` is **auto-restarted** in parallel; `pending.json` is re-registered as pending.
13. Option layering is global `YTDL_OPTIONS` (env, then file overriding env) → presets in request order → per-download overrides; `null` clears a key. MeTube's flat-extract keys (`extract_flat`, `noplaylist`, …) are applied *after* user options during extraction so presets can't break it — but `subscriptions.extract_flat_playlist` applies user options *last*, so subscriptions can be broken by `YTDL_OPTIONS`. That asymmetry looks like a bug.
14. `/add` accepts both the new schema (`download_type/codec/format/quality`) and a full legacy schema (`format=m4a|thumbnail|captions|...`, `quality=best_ios|audio`, `video_codec`, `subtitle_format`) via `_migrate_legacy_request`; `DownloadInfo.__setstate__` performs the same migration for on-disk records.
15. Most REST responses are JSON with `Content-Type: text/plain`; only `/presets`, `/cancel-add`, `/version` are `application/json`.
16. Validation errors are aiohttp `HTTPBadRequest` with a human `reason` string (no JSON body); business errors are HTTP 200 with `{"status":"error","msg":…}`. `subscriptions/update` can leak a `ValueError` as a 500.
17. `cancel-add` is cooperative generation-counter cancellation, only checked *between* playlist entries; canceled URLs are tracked in `_canceled_urls` so an in-flight expansion skips them.
18. There is **no `playlist_strict_mode`** in this fork (upstream has it) — playlist limiting is `playlist_item_limit`, applied both as a slice and as `playlistend`.
19. Playlist/channel expansion injects yt-dlp-compatible fields (`playlist_index` zero-padded to the digit width, `playlist_count`, `playlist_autonumber`, `n_entries`, `__last_playlist_index`, `playlist_title/uploader/...`) and pre-resolves only `playlist*`/`channel*` template fields via yt-dlp's own `evaluate_outtmpl`, sanitizing Windows-invalid chars first.
20. Subscriptions **backfill-suppress**: on subscribe, all currently visible video ids are marked seen without downloading, except `live_status == "is_upcoming"`. Already-seen `is_live` entries are re-queued.
21. The subscription loop sleeps 60 s *before* its first run, checks sequentially, and on extraction failure does **not** update `last_checked` — permanently broken feeds re-extract every minute forever.
22. `to_public_dict()` is a deliberately narrow projection (13 keys incl. `seen_count`, excluding `seen_ids`, `custom_name_prefix`, `ytdl_options_*`); `timestamp` is intentionally not persisted.
23. The Telegram bot only watches downloads whose `dqueue.add()` was invoked from a Telegram handler — it uses a `contextvars.ContextVar` chat id read inside `on_added`. Web-UI and subscription downloads are invisible to it.
24. Surprise: the bot does **no** progress reporting or message editing. It sends discrete messages only: queued count, per-URL failures, a 15 s-poll "stalled" warning after `TELEGRAM_STALL_TIMEOUT_SECONDS`, a "taking longer" warning after `TELEGRAM_HARD_TIMEOUT_SECONDS` (each once per chat), and a final ✅/❌ on completion. Neither timeout cancels anything.
25. The bot has a real SSRF guard (rejects `localhost`, `*.local`, and private/loopback/link-local/multicast/reserved IPs) and caps URLs per message.
26. StreamingCommunity bypasses yt-dlp entirely: detection is `"streamingcommunity" in hostname`; extraction scrapes the Inertia.js `x-inertia-version`, the watch page's `embedUrl`, its `<iframe>`, then `window.streams` / `masterPlaylist` for the stream URL + `token`/`expires` (+ `h=1` only when `canPlayFHD`). Tokens expire fast, so only the watch URL is stored and the m3u8 is re-extracted **just in time inside the child process**.
27. SC downloads default to `N_m3u8DL-RE` (`--auto-select --del-after-done --mux-after-done format=mp4:muxer=ffmpeg`, `SC_THREAD_COUNT` threads) with an automatic ffmpeg retry on failure; progress is scraped from ANSI-laden Spectre.Console repaint frames, always taking the **last** match so stale `0/100 0.00%` frames don't pin the UI at zero.
28. The "gapless fallback mux" is the notable trick: when N_m3u8DL-RE leaves only segments, the code **binary-concatenates the raw TS segments in natural filename order** (never mtime) and lets ffmpeg read one continuous stream — explicitly avoiding `-f concat`, which pads each segment to its container duration and injects a ~64 ms A/V gap plus a dropped frame per join.
29. SC output ignores `OUTPUT_TEMPLATE` entirely: always `<download_dir>/<sanitized title>.mp4` plus a hand-written `.info.json`.
30. `audio_sync_fix.py` is auto-wired as a late `Exec` postprocessor **only** for `video/mp4/best_remux` (re-encodes audio to 256 kbps AAC to undo SponsorBlock stream-copy drift), with an ffprobe-derived timeout of `max(600, ceil(duration/2))`s (`1800` if unknown). `jellyfin_nfo_generator.py` exists but is **not wired anywhere** — it's a manual/`Exec`-postprocessor CLI, and it deletes the `.info.json` it consumes.
31. Jellyfin sync is a single unauthenticated-body `POST {JELLYFIN_URL}/Library/Refresh` with `Authorization: MediaBrowser Token="…"`, fired per finished download; it refreshes **all** libraries (`JELLYFIN_LIBRARY_ID` in the sample compose is inert, as are the metadata/image refresh-mode vars).
32. The POT provider is 100% Docker-level: a `bgutil-pot` binary started as an **unsupervised background shell child** by the entrypoint, plus a yt-dlp plugin zip unpacked into site-packages. No Python code references it, and the healthcheck (`curl http://localhost:$PORT/`) can't detect its death.
33. yt-dlp is pinned to a *nightly* in the Dockerfile (`pip install --break-system-packages --no-deps`), overriding `uv.lock`, and bumped automatically every 3 days by `update-yt-dlp.yml` with auto-merge — which is why ~75% of recent commits are version bumps. Upstream drift is tracked via an LLM-written GitHub issue plus `synced:<version>` labels as the state store.
34. Security note for the parent: `vps_setup.md` in the working tree (untracked) contains what appear to be **live** credentials — a Telegram bot token, a Jellyfin API key, and a WireGuard private key. These should be rotated and excluded before any of this is shared.
