# Aulos Protocol v2 — Client Author's Reference (PROTOCOL.md)

Status: **normative**. This document is the contract. If the server disagrees with this document,
the server is wrong.

Audience: whoever writes the client. It is written so that Swift `Codable` types can be
hand-written from it with no guessing: every field has a type, a nullability, and a statement of
whether the key can ever be absent.

Companion documents: `docs/DESIGN.md` (why and how), `docs/PLAN.md` (implementation order).

---

## 0. The five rules that make this protocol easy

1. **One object shape.** `GET api/v2/items`, `GET api/v2/items/{id}`, the WebSocket `snapshot`,
   and the WebSocket `added`/`completed` frames all carry the **same** `Item` object. Playlists are
   `Item`s with `kind: "group"` in the **same array**. One Swift struct, one array, one decoder.
2. **No key is ever absent from a full `Item`.** Optional means the value is JSON `null`, not that
   the key is missing. Declare your Swift properties as non-optional where the type says
   non-optional, and as `T?` where it says `T | null` — and never write a fallback for a missing
   key.
3. **`id` is the only key.** A 26-character ULID string, server-assigned, immutable for the
   record's life — including when a provisional item becomes a group. `url` is data.
4. **`ord` is the only sort key.** A server-assigned, monotonically increasing integer, stable
   across server restarts. Sort by `ord` ascending, then `id` ascending. Never sort by title, never
   prepend, never insert at index 0.
5. **In a `delta`, an absent key means "unchanged"; an explicitly `null` value means "changed to
   null".** This is stated on the wire as `protocol.delta_semantics: "absent-key-means-unchanged"`.
   Merge by iterating the keys that are present.

---

## 1. Conventions

### 1.1 Base URL and prefix

Every path in this document is relative to `URL_PREFIX`, written `<p>`. `<p>` always begins and
ends with `/`; the default is `/`. So with the default prefix the add endpoint is
`/api/v2/downloads`, and with `URL_PREFIX=/metube/` it is `/metube/api/v2/downloads`.

Do not guess the prefix. `GET <p>api/v2/capabilities` echoes it as `url_prefix`, and so does
`GET <p>version` and `GET <p>healthz`. Construct your paths by appending to the configured base
URL, then verify once against `capabilities.url_prefix`.

**`GET <p>` is content-negotiated, and it is the only route in the server that is.** When the
request's `Accept` list contains `text/html` (or `text/*`) the server answers the embedded web UI's
`index.html`; for every other `Accept` — `application/json`, the bare `*/*` that `curl` and most
HTTP clients send by default, or no `Accept` header at all — it answers the same small JSON
identity document it always has, unchanged:

```json
{ "name": "aulos-server", "version": "1.0.0", "url_prefix": "/", "protocol": "v2" }
```

`*/*` alone does **not** count as a vote for HTML, precisely so that a client that never thought
about the header keeps getting JSON. `HEAD` behaves the same. Both representations carry
`Vary: Accept`. If your HTTP library sets `Accept: text/html` for you — some do — send
`Accept: application/json` explicitly on this one route, or read `<p>version` instead, which is
never negotiated. When the operator sets `AULOS_WEB_UI=false` the negotiation disappears and every
`Accept` gets the identity document.

Nothing else changes for an API client: no other route has an HTML representation, no response body
or header on any `api/v2/*` route is affected, and the UI adds no endpoint you need to know about.

### 1.2 Content type and encoding

- Every response body is `application/json; charset=utf-8`, including every error. The only
  exceptions are the file routes (`<p>download/*`, `<p>audio_download/*`) and `<p>robots.txt`.
- Every request body is `application/json`. Mutating endpoints **require** that header; a request
  without it is rejected with `400 bad_request`. (The one exception is the multipart cookie
  upload.)
- All timestamps are **integer milliseconds** since the Unix epoch, UTC. There are no date strings
  anywhere except `ytdl_options.update_time`, which is a float of epoch **seconds** for legacy
  compatibility and is documented as such at its one use site.
- All durations are **integer seconds** unless the field name ends in `_ms`.
- All sizes and byte counts are **integers**. All rates are `bytes/s` as JSON numbers.
- Numbers are always JSON numbers. A numeric field is never sent as a string. You do not need a
  flexible numeric decoder.

### 1.3 Response headers

| Header | On | Meaning |
|---|---|---|
| `X-Request-Id` | every response | a ULID; echoed from your request's `X-Request-Id` if you send one. Log it; it is the key to the server logs. |
| `X-Aulos-Seq` | every response | the frame sequence number at the moment the response was produced. After a mutation, this is the `seq` at or before which the corresponding WebSocket frame will arrive — which is what makes optimistic UI deterministic (§7.4). |
| `ETag` | `capabilities`, `catalog`, `state`, file routes | see the endpoint |
| `Retry-After` | `503` | seconds; always `1` |

### 1.4 Auth

The server has no user model. Two mechanisms, both optional, and they compose:

| Mechanism | How |
|---|---|
| **Cookie passthrough (primary)** — the deployment runs Authelia or similar in front | Send your session cookies on every request, including the WebSocket upgrade. The server forwards nothing and validates nothing; the proxy decides. **The server never redirects on an auth failure**: you get `401` with the JSON error envelope, never a `303` to a login page, and never a 200 with an HTML body. |
| **Bearer token (for non-browser clients)** | When the operator sets `AULOS_API_TOKEN`, `Authorization: Bearer <token>` is accepted on any v1 or v2 route as an alternative to proxy auth. Intended for Shortcuts, bookmarklets and `curl`. |

For the WebSocket, cookies on the upgrade request are the normal path. If the reverse proxy cannot
forward them to `<p>ws`, the same bearer token is accepted two other ways:
`Sec-WebSocket-Protocol: aulos.v2, bearer.<token>`, or `?token=<token>` in the query string.

A `401` body is always:

```json
{ "error": { "code": "unauthorized", "message": "authentication required",
             "field": null, "provider": null, "provider_code": null,
             "request_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA" } }
```

Treat `401` as "show the login sheet". Treat `403` the same way. Treat a non-2xx with an
unparseable body as a server or proxy problem, not as an expired session — that heuristic is no
longer needed.

#### The web UI routes are unauthenticated

Three routes are served **without** auth even when `AULOS_API_TOKEN` or the trusted-proxy header is
configured, because they carry the embedded web UI and nothing else:

| Route | What it is |
|---|---|
| `GET <p>` with `Accept: text/html` | `index.html` (the JSON branch of the same route, §1.1, is equally open — it always was) |
| `GET <p>assets/*` | `app.css`, `app.js`, `icon.svg`, `icon-180.png` |
| `GET <p>manifest.webmanifest` | the PWA manifest |

They are identical bytes in every deployment and contain no queue data, no configuration and no
secret; and a browser cannot attach `Authorization: Bearer …` to a document navigation, so gating
them would answer the first request with a `401` the user has no way to act on — and this server
never answers auth with a redirect to a login page (above). **Every other route keeps its auth
exactly as documented here**, which is what the page itself relies on: it treats a `401` from any
API call or from the WS upgrade as "ask for the token". An operator who wants the page itself gated
puts the origin behind their proxy's auth, or sets `AULOS_WEB_UI=false`, in which case the assets
and the manifest answer `404 not_found` like any other unknown route.

### 1.5 Error envelope

Every non-2xx response, without exception, has this exact shape:

```json
{ "error": { "code": "validation_failed",
             "message": "quality \"1081\" is not valid for format \"mp4\"",
             "field": "quality",
             "provider": null,
             "provider_code": null,
             "request_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA" } }
```

| Field | Type | Notes |
|---|---|---|
| `code` | string | from the closed list in §1.6. Branch on this, never on `message`. |
| `message` | string | human-readable, already cleaned: no `"ERROR: "` prefix, no ANSI, no `\r`, at most 512 characters. Safe to show to the user. |
| `field` | string \| null | the offending request field, when there is one |
| `provider` | string \| null | e.g. `"ytdlp"`, when a provider produced the error |
| `provider_code` | string \| null | the provider's own code, e.g. `"ExtractorError"`, for diagnostics only |
| `request_id` | string | matches the `X-Request-Id` header |

The **same object** appears as an `Item`'s `error` field, minus `request_id`, so you need only one
Swift type for both. See `WireError` in §2.3. All five of `code`, `message`, `field`, `provider`
and `provider_code` are present on both surfaces; which of them are non-null differs by surface
(`field` is set on a validation 400 and `null` on an item error; `provider`/`provider_code` are
usually the other way round), but the key set does not. If you want one struct for both, declare
`request_id` as `String?` and it decodes an item error too.

### 1.6 Error codes (closed list)

Treat this as a closed enum with an `unknown` fallback case for forward compatibility — the list
is `non_exhaustive` on the server and may gain members.

HTTP errors:

| `code` | HTTP | Meaning |
|---|---|---|
| `bad_request` | 400 | malformed body, missing `Content-Type`, unparseable field |
| `validation_failed` | 400 | field-level validation; `field` is set |
| `unsupported_url` | 400 | no provider matched and the scheme is unusable |
| `overrides_disabled` | 400 | the server forbids `ytdl_options_overrides` |
| `unknown_preset` | 400 | a named preset does not exist |
| `folder_invalid` | 400 | `folder` escapes the base dir, does not exist, or custom dirs are off |
| `unauthorized` | 401 | auth failure |
| `not_found` | 404 | unknown item / group / subscription id, or no route matched the path |
| `method_not_allowed` | 405 | the path exists but not under this method; the response also carries `Allow` |
| `conflict` | 409 | duplicate subscription URL, or a strict-mode duplicate add |
| `payload_too_large` | 413 | cookie upload over **1 000 000 bytes** (decimal, not 1 MiB — the legacy cap, preserved byte-for-byte along with its message `Cookie file too large (max 1MB)`), or a batch add over the server's cap |
| `internal` | 500 | a bug. `message` is a request id; details are in the server logs. |
| `socketio_removed` | 501 | you hit `<p>socket.io/*`. Socket.IO is not provided; use `<p>ws` or `GET api/v2/state`. This is the only 501 the server emits. |
| `state_unavailable` | 503 | the database is busy; retry after `Retry-After` seconds |

Item-terminal errors (these appear in `Item.error`, never as an HTTP status):

| `code` | Meaning | Retry likely to help? |
|---|---|---|
| `auth_required` | the site needs credentials or cookies (login, members-only, private) | no — upload cookies |
| `bot_check` | YouTube bot check; the POT sidecar is probably unhealthy | no — check `healthz` |
| `geo_restricted` | not available in this region | no |
| `unavailable` | removed, deleted, or the account was terminated | no |
| `not_yet_live` | an upcoming stream. Usually you meet this code on a **`queued`** item rather than a failed one (§2.3) — the server keeps such an item parked and unscheduled, and its subscription re-queues it when the stream starts | later, and the server may do it for you |
| `no_format` | the requested format is not available for this item | change the selection |
| `network` | transport error, HTTP 5xx, or a timeout | **yes**, and the server already retried |
| `throttled` | HTTP 429 | **yes**, later |
| `postprocessing_failed` | ffmpeg or a postprocessor failed | maybe |
| `disk_full` | out of disk space | no |
| `tool_missing` | a required binary is absent from the image | no |
| `provider_degraded` | the provider that handles this URL is misconfigured | no — check `healthz` |
| `timeout` | a resolve, job or stall deadline expired | yes |
| `canceled` | cancelled by a user | n/a |
| `contract` | a provider or plugin violated its protocol | no |
| `internal` | a server bug | no |
| `unsupported_url` | no provider could handle this URL | no |

A sensible client offers a **Retry** button for `network`, `throttled`, `timeout`,
`postprocessing_failed` and `not_yet_live`, and offers **Delete** for the rest.

---

## 2. The `Item` object

This is the one type you must get right. Everything else is small.

### 2.1 Example — a downloading item

```json
{
  "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
  "kind": "item",
  "ord": 981,
  "group_id": null,
  "group_index": null,
  "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "title": "Rick Astley - Never Gonna Give You Up",
  "status": "downloading",
  "auto_start": true,
  "provider": "ytdlp",

  "percent": 42.7,
  "speed": 3145728.0,
  "eta": 63,
  "downloaded_bytes": 44040192,
  "total_bytes": null,
  "total_bytes_estimate": 103809024,
  "fragment_index": null,
  "fragment_count": null,
  "phase": "video",
  "phase_percent": null,

  "msg": null,
  "error": null,

  "filename": null,
  "size": null,
  "download_url": null,
  "chapter_files": [],
  "subtitle_files": [],

  "selection": { "download_type": "video", "codec": "auto", "format": "mp4", "quality": "1080" },
  "folder": null,
  "request": {
    "custom_name_prefix": "",
    "playlist_item_limit": 0,
    "auto_start": true,
    "split_by_chapters": false,
    "chapter_template": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
    "subtitle_language": "en",
    "subtitle_mode": "prefer_manual",
    "ytdl_options_presets": [],
    "ytdl_options_overrides": {}
  },

  "created_at": 1756999900000,
  "started_at": 1756999901000,
  "finished_at": null,
  "attempt": 0,
  "source": { "kind": "telegram", "ref": "12345" },

  "children_total": null,
  "children_done": null,
  "children_error": null,
  "children_active": null,
  "children_inline": null
}
```

### 2.2 Example — a finished item and a group

```json
{
  "id": "01JBQ9CCFFFF0000000000000B",
  "kind": "item",
  "ord": 977,
  "group_id": "01JBQ8AA0000000000000000GG",
  "group_index": 12,
  "url": "https://www.youtube.com/watch?v=abc",
  "title": "Episode 12",
  "status": "finished",
  "auto_start": true,
  "provider": "ytdlp",
  "percent": 100.0,
  "speed": null,
  "eta": null,
  "downloaded_bytes": 288314112,
  "total_bytes": 288314112,
  "total_bytes_estimate": null,
  "fragment_index": null,
  "fragment_count": null,
  "phase": null,
  "phase_percent": null,
  "msg": null,
  "error": null,
  "filename": "Lo-fi beats/Episode 12.mp4",
  "size": 288314112,
  "download_url": "download/Lo-fi%20beats/Episode%2012.mp4",
  "chapter_files": [],
  "subtitle_files": [
    { "filename": "Lo-fi beats/Episode 12.en.srt", "size": 4821,
      "download_url": "download/Lo-fi%20beats/Episode%2012.en.srt", "lang": "en" }
  ],
  "selection": { "download_type": "video", "codec": "auto", "format": "mp4", "quality": "best" },
  "folder": null,
  "request": { "custom_name_prefix": "", "playlist_item_limit": 0, "auto_start": true,
               "split_by_chapters": false, "chapter_template": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
               "subtitle_language": "en", "subtitle_mode": "prefer_manual",
               "ytdl_options_presets": [], "ytdl_options_overrides": {} },
  "created_at": 1756999950000,
  "started_at": 1757000010000,
  "finished_at": 1757000598000,
  "attempt": 0,
  "source": { "kind": "api_v2", "ref": null },
  "children_total": null, "children_done": null, "children_error": null,
  "children_active": null, "children_inline": null
}
```

```json
{
  "id": "01JBQ8AA0000000000000000GG",
  "kind": "group",
  "ord": 982,
  "group_id": null,
  "group_index": null,
  "url": "https://www.youtube.com/playlist?list=PL123",
  "title": "Lo-fi beats",
  "status": "downloading",
  "auto_start": true,
  "provider": "ytdlp",
  "percent": 12.4,
  "speed": 4194304.0,
  "eta": 4210,
  "downloaded_bytes": 123456789,
  "total_bytes": null,
  "total_bytes_estimate": 5100000000,
  "fragment_index": null,
  "fragment_count": null,
  "phase": null,
  "phase_percent": null,
  "msg": null,
  "error": null,
  "filename": null, "size": null, "download_url": null,
  "chapter_files": [], "subtitle_files": [],
  "selection": { "download_type": "video", "codec": "auto", "format": "mp4", "quality": "best" },
  "folder": null,
  "request": { "custom_name_prefix": "", "playlist_item_limit": 0, "auto_start": true,
               "split_by_chapters": false, "chapter_template": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
               "subtitle_language": "en", "subtitle_mode": "prefer_manual",
               "ytdl_options_presets": [], "ytdl_options_overrides": {} },
  "created_at": 1756999950000, "started_at": 1756999951000, "finished_at": null,
  "attempt": 0, "source": { "kind": "api_v2", "ref": null },
  "children_total": 500,
  "children_done": 62,
  "children_error": 1,
  "children_active": 3,
  "children_inline": false
}
```

### 2.3 Complete field table

`T` means the key is always present with a value of type `T`. `T | null` means the key is always
present and may be `null`. **No key is ever absent.**

| Key | Type | Notes |
|---|---|---|
| `id` | `string` | 26-char ULID. The only identifier. Immutable, including across a group promotion. |
| `kind` | `"item"` \| `"group"` | A `"group"` is a playlist/channel/season container. It never downloads a file of its own. Both live in the same array. |
| `ord` | `integer` | **The sort key.** Monotonic, server-assigned, stable across restarts. Sort ascending, tie-break on `id`. |
| `group_id` | `string \| null` | the `id` of this item's group, or `null` for a top-level record |
| `group_index` | `integer \| null` | 1-based position within the group |
| `url` | `string` | the source page URL. Data, not a key. |
| `title` | `string` | never `null`; before resolution it is the URL |
| `status` | `Status` | the closed 8-value enum, §3. A group's `status` is a roll-up over the same 8 values — never a different vocabulary, and never `resolving`, `preparing` or `postprocessing`. The exact rule is §3.3. |
| `auto_start` | `boolean` | with `status == "queued"`: `true` = waiting for a slot, `false` = waiting for the user (the legacy *pending* bucket) |
| `provider` | `string \| null` | e.g. `"ytdlp"`, `"streamingcommunity"`, `"command:bandcamp"`. `null` until resolution picks one. |
| `percent` | `number` | **never null.** `0.0` before any progress; `0.0…99.9` while active; exactly `100.0` when `finished`. `error`/`canceled` keep the last value. |
| `speed` | `number \| null` | bytes per second |
| `eta` | `integer \| null` | whole seconds remaining |
| `downloaded_bytes` | `integer \| null` | |
| `total_bytes` | `integer \| null` | exact size, when the server knows it |
| `total_bytes_estimate` | `integer \| null` | estimate; use it only if `total_bytes` is `null` |
| `fragment_index` | `integer \| null` | HLS/DASH fragment or segment progress |
| `fragment_count` | `integer \| null` | |
| `phase` | `string \| null` | a finer-grained, purely cosmetic label: `"video"`, `"audio"`, `"fragment"`, `"remux"`, `"audio_sync"`, or a provider-specific string. Do not switch on it. |
| `phase_percent` | `number \| null` | progress **of the current postprocessing phase**, `0.0…100.0`. Independent of `percent`. When `status == "postprocessing"` and this is non-null, render it as a secondary bar. |
| `msg` | `string \| null` | a short **live** status line, e.g. `"Merging formats"`, `"N_m3u8DL-RE failed, retrying with ffmpeg..."`, `"Paused"`, `"Retrying in 30s"`. Already cleaned. It describes what is happening *now*, so **no terminal status ever carries a live line**: the server clears it at the terminal write. It is **always `null` when `status == "finished"`**, on every writer — the engine, a group's roll-up, the legacy importer and the schema migration that backfilled the rows an older build wrote. On `error` and `canceled` it is `null` too, unless something set an explicit terminal *note* that was never a progress line (the legacy importer writes one for a record whose status it could not map); the reason itself is in `error`, and the v1 shim projects `error.message` back into v1's overloaded `msg`. The clear arrives as an explicit `null` in the `delta`/`completed` frame, per §5.4. |
| `error` | `WireError \| null` | `{ code, message, field, provider, provider_code }` — the §1.5 object without `request_id`. Non-null when `status == "error"`, optionally when `canceled`, **and also on a `queued` item that has a pre-download problem** — an upcoming livestream, where `error.code == "not_yet_live"` and `error.message` is the scheduled-start text. That last case is not a failure: the item exists, is not scheduled, and starts when the user presses start (or when its subscription notices the stream went live). Render it as a queued row with an explanatory subtitle, not as a failure. |
| `filename` | `string \| null` | the produced file, **relative** to its download root. `null` until known. The key always exists. |
| `size` | `integer \| null` | bytes on disk |
| `download_url` | `string \| null` | a ready-to-open, percent-encoded URL. `null` until the file exists. Audio items use the audio root automatically. **It is usually relative to `<p>` (e.g. `"download/My%20Video.mp4"`) but may be absolute** (e.g. `"https://cdn.example/My%20Video.mp4"`), because the operator can point the server's public file prefix at a CDN. The resolution rule is one line and covers both: **if the value parses as an absolute URL — it has a scheme — open it as-is; otherwise resolve it against your base URL plus `<p>`.** Never concatenate unconditionally. |
| `chapter_files` | `[FileRef]` | always an array, possibly empty. Never `null`. |
| `subtitle_files` | `[FileRef]` | same |
| `selection` | `Selection` | `{ download_type, codec, format, quality }`, all four always strings |
| `folder` | `string \| null` | the relative custom directory, or `null` for the base dir |
| `request` | `RequestView` | the rest of the add request, echoed back: `{ custom_name_prefix, playlist_item_limit, auto_start, split_by_chapters, chapter_template, subtitle_language, subtitle_mode, ytdl_options_presets, ytdl_options_overrides }`. This is what makes "download again with the same options" and a request inspector possible. See below. |
| `created_at` | `integer` | unix ms |
| `started_at` | `integer \| null` | unix ms of the first `preparing` |
| `finished_at` | `integer \| null` | unix ms of the terminal transition |
| `attempt` | `integer` | 0 on the first try; incremented by a retry (manual or automatic) |
| `source` | `SourceRef` | `{ kind, ref }` — see below |
| `children_total` | `integer \| null` | groups only; the declared child count |
| `children_done` | `integer \| null` | groups only; children with `status == "finished"` |
| `children_error` | `integer \| null` | groups only |
| `children_active` | `integer \| null` | groups only; children in `preparing`/`downloading`/`postprocessing` |
| `children_inline` | `boolean \| null` | groups only. `true` = this group's children are present in the same payload. `false` = they were omitted because the group is large; fetch them with `GET api/v2/items?group_id=<id>` or a WebSocket `watch` frame. `null` on non-groups. Note that this bounds the **snapshot**, not the delta stream: you may still receive `delta` patches for children of a group you never received, and your apply step must ignore a patch for an unknown id (§5.4, §7). |

**`selection`, `folder` and `request` are immutable for the record's life**, so they are guaranteed
never to appear in a `delta` frame. Read them once from `added`/`snapshot` and never re-read them.

`FileRef`:

| Key | Type |
|---|---|
| `filename` | `string` |
| `size` | `integer \| null` |
| `download_url` | `string \| null` — same absolute-or-relative rule as `Item.download_url` |
| `lang` | `string \| null` |

`RequestView` — all nine keys always present:

| Key | Type | Notes |
|---|---|---|
| `custom_name_prefix` | `string` | `""` when unset |
| `playlist_item_limit` | `integer` | `0` = unlimited |
| `auto_start` | `boolean` | the value that was **requested**. `Item.auto_start` is the *current* value, which a `pause` or a `start` changes. |
| `split_by_chapters` | `boolean` | |
| `chapter_template` | `string` | the effective template, never `null` |
| `subtitle_language` | `string` | |
| `subtitle_mode` | `string` | one of the four §4.1 values |
| `ytdl_options_presets` | `[string]` | possibly empty, never `null` |
| `ytdl_options_overrides` | `object` | the key set is exact; a **value** whose key looks like a secret (matching `(?i)(cookie\|password\|passwd\|token\|key\|secret\|proxy)`) is the string `"«redacted»"`. Show the keys, do not try to re-send the values verbatim — re-send only the ones you own. |

`Selection` — all four keys always present, all four strings:
`download_type` ∈ `"video" | "audio" | "captions" | "thumbnail"`;
`codec` ∈ `"auto" | "h264" | "h265" | "av1" | "vp9"`;
`format` and `quality` are catalog ids (§8), e.g. `"mp4"` and `"1080"`.

`SourceRef` — deliberately **flat**, two always-present keys, so it needs no custom decoder:

| Key | Type | Notes |
|---|---|---|
| `kind` | `string` | `"api_v2"` \| `"api_v1"` \| `"telegram"` \| `"subscription"` \| `"restart"` \| `"retry"` |
| `ref` | `string \| null` | the chat id, the subscription id, or a request id — whichever applies. `null` when there is nothing to reference. |

### 2.4 Rendering guidance

- **Three sections** (In Progress / Completed / Failed) map to:
  in progress = `status ∈ {queued, resolving, preparing, downloading, postprocessing}`;
  completed = `status == "finished"`; failed = `status == "error"`.
  Cancelled items (`status == "canceled"`) are yours to place — showing them in Failed with a
  distinct icon, or in a fourth section, are both reasonable. They are never hidden by the server.
- **Groups.** The simplest correct client renders a group row using `percent` and the
  `children_*` counters and renders its children as ordinary rows (they sort naturally into place
  by `ord`). If you want a collapsed group, hide rows whose `group_id` is a collapsed group's id.
  You never need to compute a group's progress: the server does it byte-weighted where it can.
  §3.3 gives the exact roll-up rules, including which statuses a group can hold — notably it is
  never `resolving`, `preparing` or `postprocessing`, so a group whose children are all being
  remuxed reads `downloading`.
- **Row identity.** Key rows on `id` alone. Never on `"\(id)-\(status)"`: a status change is not a
  new row, and rebuilding the view on every transition is a guaranteed visual pop.
- **Progress bar.** Show `percent` for `preparing`/`downloading`. For `postprocessing`, show
  `percent` (which will be at or near 100) plus `phase_percent` as a secondary indicator when it
  is non-null; `msg` says what is happening.
- **`msg` is a live line, not a summary.** Render it as the row's subtitle while the item is
  running and it will read correctly, because the server clears it at the terminal write: a
  `finished` item always has `msg == null`, and so does a row that failed or was cancelled while a
  postprocessor was running. Do not cache the last non-null value you saw for a row — the clear is
  delivered as an explicit `null` (§5.4), and treating it as "no change" is what makes a completed
  download read as a job stuck in its final postprocessor. A `finished` row's subtitle is yours to
  compose from `size`, `filename` and `finished_at`; a failed one's is `error.message`.
- **`resolving`.** The item exists, has an `id`, and its `title` is still the URL. Show a spinner
  and the URL. It will get a real title within a second or two, delivered as a `delta`.

---

## 3. Status

### 3.1 The closed vocabulary

Exactly these eight strings ever appear in `status`, on items and on groups alike:

| Value | Meaning |
|---|---|
| `queued` | accepted, not started. `auto_start == true` ⇒ waiting for a slot. `auto_start == false` ⇒ waiting for the user: either it was added with `auto_start: false`, or it was **paused**, or it has a pre-download problem (`error.code == "not_yet_live"`). |
| `resolving` | metadata extraction is running. The item may become a group. |
| `preparing` | the download process has been spawned but has produced no progress yet |
| `downloading` | bytes are moving |
| `postprocessing` | the bytes are down; ffmpeg / a remux / an audio re-encode is running |
| `finished` | done. `percent == 100.0`, `filename` and `download_url` are non-null, and `msg` and `error` are both `null`. |
| `error` | terminal failure. `error` is non-null and carries the reason; `msg` is `null` unless an importer left a terminal note (§2.3). |
| `canceled` | terminal, cancelled by a user. `msg` follows the `error` rule above. |

There is no `pending` and no `done` in v2. If you decode an unknown value, treat it as an
`unknown` case and render the row as inert — do **not** map it to `queued`, or a future status will
silently look like something it is not.

### 3.2 Transitions

```
                       ┌──────────────────────────────────────────┐
                       │                                          │
   (POST downloads)    ▼                                          │
        ──────────► resolving ──────────► queued ──────► preparing │
                       │                    ▲               │     │
                       │                    │               ▼     │
                       │                    │          downloading │
                       │                    │               │     │
                       │                    │               ▼     │
                       │                    │        postprocessing│
                       │                    │               │     │
                       │                    │               ▼     │
                       │                    │            finished │
                       │                    │                     │
                       └────────┬───────────┴──── error ◄──────────┘
                                │                  │
                                │                  │ (retry: attempt += 1)
                                ▼                  │
                            canceled ──────────────┘
                          (from ANY non-terminal)

  finished | error | canceled ──► (record removed: delete, clear, or CLEAR_COMPLETED_AFTER)
```

Notes that matter to a client:

- **`pause` and `start` move an item between the two flavours of `queued`**, and the status string
  does not change: `pause` sets `auto_start` to `false` (killing the job first if one is running,
  keeping its partial file), `start` sets it back to `true`. So a paused row stays in your
  In-Progress section with a "Paused" treatment; there is no `paused` status to decode. Pausing a
  running item does **not** increment `attempt`, and resuming it continues from the bytes already
  on disk where the provider supports it.
- A single-video add goes `resolving → queued → preparing → …`. You will see the title change on
  the `resolving → queued` transition.
- A playlist add goes `resolving`, then the **same record** becomes `kind: "group"` and its
  children appear. The `id` and the `ord` do not change (§6.5).
- `error` and `canceled` can transition back to `queued` on a retry, with `attempt` incremented.
- No transition ever skips backwards through the download states.

### 3.3 Groups: exactly what a group's fields mean

A group never downloads anything itself; every one of its progress fields is derived from its
children, and the derivation is contractual so you can predict what you will render.

**`status` — a five-way decision, in this order:**

```
downloading    if any child is preparing | downloading | postprocessing
else queued    if any child is queued | resolving
else error     if any child is error
else canceled  if every child is terminal and at least one is canceled
else finished
```

The consequence worth knowing: a group is **never** `resolving`, `preparing` or `postprocessing`.
A group whose children are all being remuxed reads `downloading`, and a group that is still
expanding reads `queued` or `downloading` depending on whether a child has started. Place a group
row by that value, and pick its spinner from the `children_active` count rather than from a
fine-grained status.

**`percent` — byte-weighted where the server knows the totals, count-weighted otherwise:**

```
if every resolved child has a known total and their sum > 0:
    percent = 100 × (bytes of finished children + bytes downloaded by active children) / that sum
else:
    percent = 100 × (finished child count + Σ over active children of child.percent/100)
                    / max(children_total, 1)
```

The byte-weighted branch is why a playlist of 49 short clips plus one 4 GB file does not read 98 %
while half the bytes are outstanding. You never need to compute either formula: read `percent`.

**`speed`** is the sum over running children, or `null` when none is running.
**`eta`** is remaining bytes ÷ `speed` when both are known, else `null`.
**`downloaded_bytes`**/**`total_bytes_estimate`** are the corresponding sums; `total_bytes` on a
group is always `null` (an exact total for a whole playlist is not knowable until it finishes).
**`children_done`/`children_error`/`children_active`** are exact counts, always in step with
`percent` in the same frame.

---

## 4. REST — v2

All paths are relative to `<p>`.

### 4.1 `POST api/v2/downloads` — add

Returns **before** any metadata extraction. This is the whole point.

Single request body — only `url` is required; every other field defaults from
`api/v2/capabilities.config`:

```json
{ "url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  "download_type": "video",
  "codec": "auto",
  "format": "mp4",
  "quality": "1080",
  "folder": "Music/Live",
  "custom_name_prefix": "",
  "playlist_item_limit": 0,
  "auto_start": true,
  "split_by_chapters": false,
  "chapter_template": null,
  "subtitle_language": "en",
  "subtitle_mode": "prefer_manual",
  "ytdl_options_presets": ["sponsorblock"],
  "ytdl_options_overrides": {},
  "provider": null }
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `url` | string | **required** | trimmed |
| `download_type` | string | `"video"` | §8 |
| `codec` | string | `"auto"` | forced to `"auto"` for audio/captions/thumbnail |
| `format` | string | per download type | validated against the selected provider's catalog |
| `quality` | string | per format | likewise |
| `folder` | string \| null | `null` | relative; must resolve inside the base dir |
| `custom_name_prefix` | string | `""` | no `..`, no leading `/` or `\` |
| `playlist_item_limit` | integer | server default | `0` = unlimited |
| `auto_start` | boolean | `true` | booleans and the strings `true/false/1/0/on/off` are both accepted |
| `split_by_chapters` | boolean | `false` | |
| `chapter_template` | string \| null | server default | |
| `subtitle_language` | string | `"en"` | `^[A-Za-z0-9][A-Za-z0-9-]{0,34}$` |
| `subtitle_mode` | string | `"prefer_manual"` | `auto_only` \| `manual_only` \| `prefer_manual` \| `prefer_auto` |
| `ytdl_options_presets` | [string] | `[]` | every name must exist |
| `ytdl_options_overrides` | object | `{}` | rejected with `overrides_disabled` unless the server allows it |
| `provider` | string \| null | `null` | force a provider id; normally leave it null |

Batch body — `defaults` is merged **under** each item, so a share sheet can send three URLs with
one selection:

```json
{ "items": [ { "url": "https://youtu.be/a" }, { "url": "https://youtu.be/b" } ],
  "defaults": { "format": "mp4", "quality": "1080", "auto_start": true } }
```

Response `202`:

```json
{ "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
  "ids": ["01JBQ7Z5T9K3M2R8V4XW6Y0AAA"],
  "generation": 47,
  "seq": 10241,
  "duplicates": [ { "url": "https://youtu.be/x", "existing_id": "01JBQ6…" } ],
  "warnings": [] }
```

| Key | Type | Notes |
|---|---|---|
| `id` | `string` | equals `ids[0]`, or the first `duplicates[].existing_id` when every URL deduped; present for convenience |
| `ids` | `[string]` | one id per accepted URL, in request order. Batch callers use this. |
| `generation` | `integer` | an opaque counter identifying **this add's** resolution work. Keep it if you want to be able to abort the add: `POST api/v2/downloads/cancel-resolve` with `{"generation": <this value>}` cancels the in-flight resolution and the not-yet-created children of this add only (§4.7). Two concurrent adds have different generations. |
| `seq` | `integer` | the frame sequence at or before which the corresponding `added` frame arrives (§6.4) |
| `duplicates` | `[{url, existing_id}]` | see below |
| `warnings` | `[string]` | see below |

- `id` is present for convenience and equals `ids[0]`. When **every** URL deduped, `ids` is
  empty and `id` is the first `duplicates[].existing_id` instead, so it is always a string and
  always something you can poll. Batch callers should use `ids`.
- `duplicates` lists URLs that already have a non-terminal item with the same selection; the
  existing id is returned instead of creating a second item. This is not an error.
- `warnings` is an array of strings, one per **unknown request field**. Unknown fields are ignored,
  never rejected: an App Store rollout runs mixed client and server versions for weeks, and the
  first client to send a new optional field must not get a 400 on the whole add.
- By the time this response arrives, the item is already in the state snapshot with
  `status: "resolving"`, and an `added` frame either has arrived or will arrive within ~25 ms.

Errors: `400 validation_failed` (with `field`), `400 unknown_preset`, `400 overrides_disabled`,
`400 folder_invalid`, `400 unsupported_url`, `413 payload_too_large`, `503 state_unavailable`.

### 4.2 `POST api/v2/items/actions` — start / cancel / retry / delete

```json
{ "action": "cancel", "ids": ["01JBQ7…", "01JBQ8…"] }
```

`action` ∈ `"start" | "pause" | "cancel" | "retry" | "delete"`. For `"delete"` you may add
`"delete_file": true | false` (the default follows the server's `DELETE_FILE_ON_TRASHCAN`).

| Action | What it does |
|---|---|
| `start` | `queued` with `auto_start == false` → `auto_start = true`. On a terminal item it is a `retry`. |
| `pause` | The inverse. `queued(auto_start=true)` is un-scheduled; a `preparing`/`downloading`/`postprocessing` item is stopped and parked as `queued(auto_start=false)` with its partial file kept, so `start` resumes rather than restarts. `attempt` is unchanged. `resolving` and terminal items are `not_pausable`. |
| `cancel` | Terminal. Stops the job, removes partials, `status = "canceled"`. Use this when the user means "stop and forget", and `pause` when they mean "not now". |
| `retry` | `error`/`canceled` → `queued`, `attempt += 1`. |
| `delete` | Removes the record (cancelling first if it is running), and optionally the files. |

Read the server's supported set from `capabilities.actions` rather than hard-coding it.

Response `200`:

```json
{ "applied": ["01JBQ7…"],
  "skipped": [ { "id": "01JBQ8…", "reason": "already_terminal" } ],
  "seq": 10250 }
```

`reason` ∈ `"not_found" | "already_terminal" | "not_cancelable" | "not_startable" |
"not_retryable" | "not_pausable"`.
Every action is **idempotent**: cancelling a cancelled item succeeds with it listed in `skipped`,
and pausing an already-paused item succeeds the same way.

`DELETE api/v2/items/{id}?delete_file=true` is the single-item shorthand and returns `204`.

Deleting a group cancels and deletes its children too.

### 4.3 `GET api/v2/state` — snapshot or delta

The polling counterpart to the WebSocket. Use it when you cannot hold a socket, or on foreground
before the socket connects.

`GET api/v2/state` (no `since`) returns the full snapshot:

```json
{ "mode": "snapshot",
  "seq": 10251,
  "boot_id": "01JBQ8YQ2E0000000000000000",
  "server_time": 1757000000123,
  "protocol": { "batch_ms": 250, "urgent_ms": 25, "replay_frames": 512,
                "delta_semantics": "absent-key-means-unchanged" },
  "counts": { "queued": 2, "resolving": 0, "preparing": 0, "downloading": 1,
              "postprocessing": 0, "finished": 401, "error": 10, "canceled": 3 },
  "done_total": 4211,
  "truncated": { "done": true, "groups": ["01JBQ8AA0000000000000000GG"] },
  "items": [ /* Item objects, ord ascending: non-terminal records plus groups */ ],
  "done":  [ /* Item objects, ord ascending: the most recent completed window */ ],
  "subscriptions": [ /* Subscription objects, §9 */ ],
  "ytdl_options": { "ok": true, "msg": "", "update_time": 1757000200.412 },
  "health": { "status": "ok", "components": { "pot": "ok", "store": "ok", "ytdl_options": "ok" } } }
```

`ytdl_options` and `health` are the **current** state of the two things that are otherwise only
announced on change (§5.9), so a client that connects while the options file is broken or the POT
sidecar is down learns it here instead of having to also call `healthz` and `api/v2/ytdl-options`.
`ytdl_options` is exactly the `ytdl_options` frame's payload minus `t`/`seq`; `health.components`
is a flat `component → "ok" | "degraded" | "down"` map — the abridged form. Call `GET healthz` when
you want the detail behind a non-`ok` value.

`GET api/v2/state?since=10240&boot=01JBQ8YQ…` returns a delta when it can:

```json
{ "mode": "delta",
  "seq": 10251,
  "boot_id": "01JBQ8YQ2E0000000000000000",
  "from": 10240,
  "added": [ /* full Item objects */ ],
  "completed": [ /* full Item objects */ ],
  "removed": [ { "ids": ["01JBQ7…"], "reason": "deleted" },
               { "ids": ["01JBQ5…"], "reason": "auto_cleared" } ],
  "delta": { "items": [ { "id": "01JBQ7…", "percent": 43.9, "speed": 3210000.0, "eta": 61 } ] } }
```

`removed` is an **array of groups**, one per distinct `reason`, because a window can easily contain
a user delete and a `CLEAR_COMPLETED_AFTER` expiry at once and `reason` is per-reason, not per-id
(§5.7). Iterate the array; the order is `deleted`, `cleared`, `auto_cleared`, `group_cascade`, and
absent reasons are simply not present. It is `[]`, never `null`, when nothing was removed.

Apply in exactly this order: `added`, then `completed`, then every `removed` group in array order,
then `delta`. This is the **same** order the server uses when it flushes a batch to the WebSocket
(DESIGN §15.1) and the same order the `resume` fold emits (§6.3) — one ordering, three places, no
exceptions.

Two things depend on it:

- `added` first, so no `completed` or `delta` entry references an id you have not yet seen.
- `removed` **after** `added`, so a record created and deleted inside the same window ends up
  absent rather than present. Applying `removed` first would discard an id you do not have yet
  (a documented no-op) and then add it, leaving a row that will never change again.

`delta` last is then free: a patch for a just-removed id is a guaranteed no-op, because your apply
step never creates a record from a `delta`.

`mode` ∈ `"snapshot" | "delta" | "up_to_date"`. You get `"snapshot"` when `since` is absent, when
`boot` does not match `boot_id`, when `since` is older than the server's replay window, or when
`since` is **greater** than the server's current `seq` (which happens after a restore). You get
`"up_to_date"` — with no arrays at all — when `since == seq`.

`ETag` is `W/"<boot_id>-<seq>"`, where `seq` is **this response's own** `seq` — the one in the
body, never a newer one the body does not reflect. Send it back as `If-None-Match` and an
unchanged server answers `304` with an empty body, which makes pull-to-refresh nearly free.

Query parameters: `since` (integer), `boot` (string), `done` (boolean, default `true` — set
`false` to omit the completed window entirely and get a very small snapshot).

### 4.4 `GET api/v2/items` — paged list

Query: `status` (comma list), `kind`, `group_id`, `q` (title substring), `order` (`ord` — the only
value), `limit` (default 200, max 1000), `cursor`.

```json
{ "items": [ /* Item objects, ord ascending */ ],
  "next_cursor": "b3JkOjk4MQ",
  "total": 4211,
  "seq": 10251 }
```

`next_cursor` is `null` on the last page. This is how you page history beyond the in-memory
window, and how you fetch a large group's children (`?group_id=<id>`).

`GET api/v2/items/{id}` returns a single `Item`, or `404 not_found`.

`GET api/v2/items/{id}/file` returns `302` to the file route, or `404` when there is no file yet.

### 4.5 `GET api/v2/capabilities`

Everything a client needs to configure itself, in one cheap, `ETag`-able response. This replaces
fetching `/version` on every reconnect and replaces any hard-coded format list.

```json
{ "version": "2026.09.04",
  "yt_dlp": "2026.8.30.232658.dev0",
  "url_prefix": "/",
  "boot_id": "01JBQ8YQ2E0000000000000000",
  "protocol": { "v2": true, "v1_shim": true, "socketio": false,
                "ws_path": "ws", "ws_subprotocol": "aulos.v2",
                "batch_ms": 250, "urgent_ms": 25,
                "delta_semantics": "absent-key-means-unchanged" },
  "features": ["async_add","stable_ids","deltas","since_resume","etag","retry","cancel",
               "cancel_resolve","groups","subscriptions","file_serving","batch_add",
               "postprocessing_status","per_url_catalog"],
  "actions": ["start","pause","cancel","retry","delete"],
  "formats": [
    { "id": "any", "text": "Any", "download_type": "video",
      "qualities": [ {"id":"best","text":"Best"}, {"id":"2160","text":"2160p"},
                     {"id":"1440","text":"1440p"}, {"id":"1080","text":"1080p"},
                     {"id":"720","text":"720p"}, {"id":"480","text":"480p"},
                     {"id":"360","text":"360p"}, {"id":"240","text":"240p"},
                     {"id":"worst","text":"Worst"} ] },
    { "id": "mp4", "text": "MP4", "download_type": "video",
      "qualities": [ {"id":"best","text":"Best"}, {"id":"best_remux","text":"Best (remux)"},
                     {"id":"2160","text":"2160p"}, {"id":"1440","text":"1440p"},
                     {"id":"1080","text":"1080p"}, {"id":"720","text":"720p"},
                     {"id":"480","text":"480p"}, {"id":"360","text":"360p"},
                     {"id":"240","text":"240p"}, {"id":"worst","text":"Worst"} ] },
    { "id": "ios",  "text": "iOS",  "download_type": "video",
      "qualities": [ {"id":"best","text":"Best"}, {"id":"2160","text":"2160p"},
                     {"id":"1440","text":"1440p"}, {"id":"1080","text":"1080p"},
                     {"id":"720","text":"720p"}, {"id":"480","text":"480p"},
                     {"id":"360","text":"360p"}, {"id":"240","text":"240p"},
                     {"id":"worst","text":"Worst"} ] },
    { "id": "m4a",  "text": "M4A",  "download_type": "audio",
      "qualities": [{"id":"best","text":"Best"},{"id":"192","text":"192 kbps"},{"id":"128","text":"128 kbps"}] },
    { "id": "mp3",  "text": "MP3",  "download_type": "audio",
      "qualities": [{"id":"best","text":"Best"},{"id":"320","text":"320 kbps"},
                    {"id":"192","text":"192 kbps"},{"id":"128","text":"128 kbps"}] },
    { "id": "opus", "text": "Opus", "download_type": "audio",    "qualities": [{"id":"best","text":"Best"}] },
    { "id": "wav",  "text": "WAV",  "download_type": "audio",    "qualities": [{"id":"best","text":"Best"}] },
    { "id": "flac", "text": "FLAC", "download_type": "audio",    "qualities": [{"id":"best","text":"Best"}] },
    { "id": "srt",  "text": "SRT",  "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "txt",  "text": "Text", "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "vtt",  "text": "VTT",  "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "ttml", "text": "TTML", "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "sbv",  "text": "SBV",  "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "scc",  "text": "SCC",  "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "dfxp", "text": "DFXP", "download_type": "captions", "qualities": [{"id":"best","text":"Best"}] },
    { "id": "jpg",  "text": "Thumbnail", "download_type": "thumbnail", "qualities": [{"id":"best","text":"Best"}] }
  ],
  "download_types": ["video","audio","captions","thumbnail"],
  "codecs": ["auto","h264","h265","av1","vp9"],
  "subtitle_modes": ["auto_only","manual_only","prefer_manual","prefer_auto"],
  "presets": ["sponsorblock","archive"],
  "providers": [ { "id": "ytdlp", "state": "ready", "fallback": true },
                 { "id": "streamingcommunity", "state": "ready", "slots": 1 },
                 { "id": "command:bandcamp", "state": "degraded",
                   "reason": "download.command[0] not executable" } ],
  "config": { "custom_dirs": true,
              "create_custom_dirs": true,
              "allow_ytdl_options_overrides": false,
              "default_option_playlist_item_limit": 0,
              "subscription_default_check_interval": 60,
              "output_template_chapter": "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
              "public_host_url": "download/",
              "public_host_audio_url": "audio_download/",
              "default_theme": "auto",
              "max_concurrent_downloads": 3,
              "delete_file_on_trashcan": false,
              "clear_completed_after": 0,
              "default_download_type": "video",
              "default_format": "mp4",
              "default_quality": "best" } }
```

`formats[]` is deliberately the flat `{ id, text, qualities: [{ id, text }] }` shape, so a client
that already models a server-driven format list can consume it with no new types. `ETag` is a hash
of the payload; a reconnect with `If-None-Match` is a `304`.

**The array above is complete and is the legacy matrix exactly** — sixteen entries: three video
formats, five audio, **seven** caption formats and one thumbnail, with `ios` carrying the same nine
heights as `any` because legacy accepted `{video, ios, 1080}` and composed a real
`[height<=1080]` selector for it. §8 restates the same matrix as a table; the two must agree, and a
CI assertion pins this payload against it. Anything narrower would silently delete formats a
legacy client could already request.

`formats[]` is a flat list keyed by `download_type` for picker convenience. It intentionally does
**not** carry `notice`, `flags`, `codecs` or `options` — those live on the richer per-URL catalog
of §4.6, which is where you go once a URL is known.

### 4.6 `GET api/v2/catalog` — the honest, per-URL picker

`GET api/v2/catalog` returns the merged catalog for every registered provider.
`GET api/v2/catalog?url=<url-encoded>` returns the catalog of the provider that **would actually
be selected** for that URL:

```json
{ "etag": "9f2b41c0d7e5a318",
  "provider": "streamingcommunity",
  "match": { "score": 200, "reason": "host_contains" },
  "runner_up": { "provider": "ytdlp", "score": 1 },
  "naming": "provider",
  "download_types": [
    { "id": "video", "label": "Video", "default_format": "mp4",
      "formats": [
        { "id": "mp4", "label": "MP4",
          "default_quality": "best",
          "notice": "StreamingCommunity serves one source rendition; quality is ignored.",
          "flags": { "advisory": true, "requires_ffmpeg": true, "lossy_remux": false, "slow": false },
          "qualities": [ { "id": "best", "label": "Source", "notice": null } ],
          "codecs": [] }
      ],
      "options": [] } ] }
```

This is what lets a share sheet be honest: paste a StreamingCommunity link and the quality picker
collapses to a single "Source" entry with an explanation; paste a YouTube link and the full matrix
appears — with no client release.

A `?url=` that **nothing** matches is not an error: the answer is the merged catalog with
`provider: "merged"` and `match: null`, so a picker always has something to render. A URL no
provider will take is reported when you try to add it, as `unsupported_url` (§4.1).

Use `capabilities.formats` for the picker's static defaults and `catalog?url=` to refine it once a
URL is known.

#### The complete catalog shapes

Every key below is **always present**; the same "optional means `null`, never absent" rule as
`Item` applies. `naming` and `match.reason` are the only closed enums.

`Catalog` (the top level):

| Key | Type | Notes |
|---|---|---|
| `etag` | `string` | also sent as the `ETag` header |
| `provider` | `string` | the provider whose catalog this is, or the literal `"merged"` for a query with no `?url=`. `"merged"` is not a legal provider id, and `match` is `null` on that same payload, so the two facts together are unambiguous. |
| `match` | `{ score: integer, reason: string }` | `reason` ∈ `"host_contains" \| "host_regex" \| "path_regex" \| "forced" \| "fallback"`; only present for `?url=` queries, `null` for the merged catalog |
| `runner_up` | `{ provider: string, score: integer } \| null` | the provider that would have been chosen next |
| `naming` | `"template" \| "provider"` | `"template"` = the file name comes from `OUTPUT_TEMPLATE*`, so `custom_name_prefix` and `chapter_template` are meaningful. `"provider"` = the provider names the file itself and ignores those templates (StreamingCommunity does), so a picker should grey them out. Those are the only two values. |
| `download_types` | `[DownloadTypeSpec]` | never empty |

`DownloadTypeSpec`:

| Key | Type |
|---|---|
| `id` | `string` — one of `"video" \| "audio" \| "captions" \| "thumbnail"` |
| `label` | `string` |
| `default_format` | `string` — always an `id` present in `formats` |
| `formats` | `[FormatSpec]` |
| `options` | `[OptionSpec]` — extra request fields this download type accepts, see below |

`FormatSpec`:

| Key | Type | Notes |
|---|---|---|
| `id`, `label` | `string` | `id` is what you send as `format` |
| `default_quality` | `string` | always an `id` present in `qualities` |
| `qualities` | `[QualitySpec]` | never empty |
| `codecs` | `[{ id: string, label: string }]` | **empty means `codec` does not apply** — send `"auto"` and hide the control. Non-empty only for `download_type: "video"`. |
| `notice` | `string \| null` | display text about the format as a whole. Always safe to show verbatim. |
| `flags` | `FormatFlags` | see below |

`QualitySpec`: `{ "id": string, "label": string, "notice": string | null }`. `notice` is
per-quality display text — this is where `worst` says *"This selector currently resolves to the
best available stream"* and `best_remux` says *"Re-encodes audio after download (slower; fixes
SponsorBlock drift)"*.

`FormatFlags` — four booleans, all always present:

| Flag | Meaning for the UI |
|---|---|
| `advisory` | The server will accept your choice but may not honour it (a provider with one rendition). Show the `notice`; do not treat a mismatch as an error. |
| `requires_ffmpeg` | Needs ffmpeg in the image. If `healthz.components.ffmpeg` is not `ok`, warn before offering it. |
| `lossy_remux` | The output is re-encoded, not stream-copied. Worth saying so next to a "best quality" label. |
| `slow` | Expect a `postprocessing` phase of minutes. Set expectations in the confirmation, not after. |

`OptionSpec` — **this is the mechanism that satisfies "richer add options advertised by the
server"**, so that `folder`, `custom_name_prefix`, `playlist_item_limit` and friends can grow
without a client release. Each entry describes one extra key you may put in the `POST
api/v2/downloads` body:

| Key | Type | Notes |
|---|---|---|
| `id` | `string` | the **request body key** to send, e.g. `"playlist_item_limit"` |
| `label` | `string` | control label |
| `kind` | `OptionKind` | see below — this is what control to render |
| `default` | any JSON value | matches `kind`. Send nothing and the server uses this. |
| `choices` | `[{ id: string, label: string }]` | non-empty **only** for `kind.type == "enum"`; `[]` otherwise |
| `help` | `string \| null` | one line of explanatory text |

`OptionKind` is an externally-tagged object with a `type` discriminator and exactly five values:

```json
{ "type": "bool" }
{ "type": "int",  "min": 0, "max": 1000 }
{ "type": "enum" }
{ "type": "text", "pattern": "^[A-Za-z0-9][A-Za-z0-9-]{0,34}$" }
{ "type": "path" }
```

`int` always carries `min` and `max`. `text` always carries `pattern`, which may be `null`; when it
is non-null it is a regex you should validate against before sending, because the server will
reject a violation with `400 validation_failed` and that `id` as `field`. `path` is a
server-relative directory — offer the values from `GET api/v2/custom-dirs` rather than a free-text
field. `enum` is the only kind with a non-empty `choices`.

A worked `options` array, the one the `ytdlp` video catalog ships:

```json
"options": [
  { "id": "folder", "label": "Folder", "kind": { "type": "path" },
    "default": null, "choices": [], "help": "Subdirectory of the download root." },
  { "id": "custom_name_prefix", "label": "Filename prefix",
    "kind": { "type": "text", "pattern": "^[^/\\\\]*$" },
    "default": "", "choices": [], "help": null },
  { "id": "playlist_item_limit", "label": "Playlist limit",
    "kind": { "type": "int", "min": 0, "max": 10000 },
    "default": 0, "choices": [], "help": "0 downloads the whole playlist." },
  { "id": "auto_start", "label": "Start immediately", "kind": { "type": "bool" },
    "default": true, "choices": [], "help": null },
  { "id": "split_by_chapters", "label": "Split by chapters", "kind": { "type": "bool" },
    "default": false, "choices": [], "help": "Writes one file per chapter." },
  { "id": "ytdl_options_presets", "label": "Presets", "kind": { "type": "enum" },
    "default": [], "choices": [ { "id": "sponsorblock", "label": "sponsorblock" },
                                { "id": "archive", "label": "archive" } ],
    "help": "Named yt-dlp option bundles the operator configured." }
]
```

The `captions` download type additionally advertises `subtitle_language`
(`text`, pattern `^[A-Za-z0-9][A-Za-z0-9-]{0,34}$`) and `subtitle_mode` (`enum`, four choices).

Two notes on what is deliberately **not** here: `playlist_strict_mode` is advertised by neither
catalog, because this fork never implemented it (it exists upstream and was never merged);
`ytdl_options_overrides` is not an `OptionSpec` either, because it is a free-form object gated by
the server's `allow_ytdl_options_overrides` flag in `capabilities.config`, not a control.

**Rendering rule for an unknown `kind.type`:** skip the option entirely and do not send its key.
The list is `non_exhaustive` on the server, and silently omitting a control you cannot render is
always correct because every option has a server-side default.

### 4.7 The rest of v2

| Method | Path | Body / query | Success | Errors |
|---|---|---|---|---|
| GET | `healthz` | `?probe=deep` | `200` (see DESIGN §16.3); `"status"` ∈ `ok` \| `degraded` \| `down` | `503` when the store is unusable |
| GET | `livez` | — | `200 {"ok":true}` | — |
| GET | `version` | — | `200 {"version":…,"yt-dlp":…,"url_prefix":…,"protocol":"v2"}` | — |
| GET | `api/v2/subscriptions` | — | `200 {"subscriptions":[Subscription]}` | — |
| POST | `api/v2/subscriptions` | an add body plus `check_interval_minutes` | `201 Subscription` | 400, 409 `conflict` |
| PATCH | `api/v2/subscriptions/{id}` | `{name?, enabled?, check_interval_minutes?}` | `200 Subscription` | 400, 404 |
| DELETE | `api/v2/subscriptions/{id}` | — | `204` | 404 |
| POST | `api/v2/subscriptions/check` | `{"ids":[…]}` or `{}` | `202 {"job_id":"01JC…","count":3}` | 400 |
| POST | `api/v2/subscriptions/{id}/check` | — | `202 {"job_id":…,"count":1}` | 404 |
| POST | `api/v2/downloads/cancel-resolve` | `{"generation": 47}` or `{}` | `200 {"canceled":12,"generation":47,"seq":…}` | 400 |
| GET | `api/v2/presets` | — | `200 {"presets":["sponsorblock","archive"]}` | — |
| GET | `api/v2/custom-dirs` | — | `200 {"download_dir":["","Music"],"audio_download_dir":[…]}` | 404 when custom dirs are off |
| GET | `api/v2/cookies` | — | `200 {"has_cookies":true,"bytes":1234,"updated_at":…}` | — |
| POST | `api/v2/cookies` | `multipart/form-data`, field **`cookies`** | `200 {"has_cookies":true,"bytes":1234}` | 400, 413 |
| DELETE | `api/v2/cookies` | — | `204` | 400 |
| GET | `api/v2/ytdl-options` | — | `200 {"ok":true,"msg":"","update_time":1757000200.412,"keys":[…],"presets":[…]}` (values are redacted) | — |
| POST | `api/v2/ytdl-options/reload` | — | `200 {"ok":true,"msg":"","update_time":…}` | — |
| GET | `api/v2/import-report` | — | `200` the importer's report (DESIGN §7.6.6) | 404 when nothing was imported |
| POST | `api/v2/items/clear` | `{"where":"done"}` or `{}`, optionally `{"delete_file": bool}` | `200 {"removed":[ids],"seq":…,"warnings":[…]}` | 400 |
| GET | `api/v2/providers` | — | `200 {"providers":[{id,state,reason,version,capabilities,limits,argv}],"warnings":[…]}` | — |
| POST | `api/v2/plugins/reload` | — | `200 {"added":[],"updated":[],"removed":[],"failed":[],"warnings":[]}` | — |
| GET | `api/v2/resolve-preview` | `?url=` | `200 {"provider":…,"score":…,"reason":…,"runner_up":{…}}` | 400 |
| GET | `api/v2/debug/options` | `?item_id=` or an add body | `200` the merged yt-dlp option dict, each key annotated with the layer it came from | 404 |
| GET | `<p>download/*`, `<p>audio_download/*` | — | the file, with `Accept-Ranges: bytes`, `ETag`, `Last-Modified`, `Content-Type`, `X-Content-Type-Options: nosniff` | 404 |
| GET | `<p>robots.txt` | — | text | — |
| GET | `<p>` | — | `200 {"name":"aulos-server","version":…,"url_prefix":…,"protocol":"v2"}` — **unless** the `Accept` list contains `text/html`, which gets the web UI's `index.html` instead (§1.1). The only content-negotiated route; both branches carry `Vary: Accept` | — |
| GET | `<p>assets/{app.css,app.js,icon.svg,icon-180.png}` | — | the web UI's assets, with `ETag`, `Cache-Control: no-cache`, `nosniff`, `no-referrer`; `304` on `If-None-Match` | 404 when `AULOS_WEB_UI=false` |
| GET | `<p>manifest.webmanifest` | — | `200 application/manifest+json`, the PWA manifest | 404 when `AULOS_WEB_UI=false` |
| GET | `<p>metrics` | — | Prometheus text, when enabled | 404 |

`POST api/v2/downloads/cancel-resolve` is the v2 counterpart of the legacy `cancel-add`, and it is
how you abort a 500-item playlist add that is still resolving. With `{"generation": n}` — the value
from that add's `202` body (§4.1) — it cancels only that add's in-flight resolution and its
not-yet-created children. With `{}` (or an absent/`null` `generation`) it cancels **every**
in-flight resolution, which is exactly what the legacy route did. `canceled` is the number of
resolution tasks and pending expansions it stopped; items already created keep their state, so
follow it with a `delete` if you want them gone. Whether it is available is advertised as the
`cancel_resolve` feature in `capabilities.features`.

`POST api/v2/items/clear` is how a v2 client clears history in one call — the counterpart of the
v1 `POST <p>delete {"where":"done"}`. Without it a v2-only deployment would have to delete rows one
id at a time. `{"where":"done"}` and `{}` both mean "every terminal row" — `"done"` is the only accepted
scope, and any other value is a `400 validation_failed` naming `where`. The optional
`delete_file` (boolean) overrides `DELETE_FILE_ON_TRASHCAN` for this call; omitted or `null`
means "follow the server configuration". The response lists the ids that went, and each one
also arrives as a `removed` frame with `reason: "cleared"` (§5.7).

`api/v2/debug/options` is worth knowing about: it answers "why did my `YTDL_OPTIONS` not take
effect?" by showing the fully merged dict with a per-key `source` label
(`env` / `file` / `preset:<name>` / `request` / `aulos`).

`api/v2/providers`' `warnings` and `plugins/reload`'s are the **non-fatal** problems of the last
plugin scan, as `<dir>: <key>: <message>` — a clamped `limits.max_concurrent`, an auto-anchored
`host_regex`, a `{cookies_file}` with nothing to point at. The fatal ones are elsewhere: a
directory that produced nothing at all is in `failed`, and a manifest that produced only a matcher
is a `degraded` entry in `providers`. `healthz` carries the same list as `plugin_warnings`.

The file routes support `Range` and `If-Range`, so a client can stream or resume. A single byte
range that cannot be satisfied is `416` with `Content-Range: bytes */<len>`; a `Range` header
the server cannot parse, whose unit it does not know, or that asks for **several** ranges is
ignored and the whole file comes back `200` (RFC 9110 §14.2). They are behind the same auth as
everything else.

Every file is served with `X-Content-Type-Options: nosniff`. Audio, video, images and the two
subtitle types keep their real `Content-Type` and render inline; **anything else** — an
`.html`, an `.svg`, a stray text file — is served as `application/octet-stream` with
`Content-Disposition: attachment`, because the download tree shares an origin with this API.
Media playback and `Range` are unaffected.

---

### 4.8 Devices — push registration

Four routes, all behind the same auth as the rest of `api/v2`, all idempotent, all answering
`204 No Content` with an empty body. They are the whole write surface the iOS client needs for
APNs: one call when the app receives a device token, one when it starts a Live Activity for a
download, and the two deletes that undo them.

| Method | Path | Body | Success | Errors |
|---|---|---|---|---|
| PUT | `api/v2/devices/{token}` | the registration object below | `204` | 400, 401 |
| DELETE | `api/v2/devices/{token}` | — | `204` (also for a token that was never registered) | 400, 401 |
| PUT | `api/v2/devices/{token}/live-activities/{item_id}` | `{"update_token":"<hex>"}` | `204` | 400, 401, **404** when the device is unknown |
| DELETE | `api/v2/devices/{token}/live-activities/{item_id}` | — | `204` (also when nothing was registered) | 400, 401 |

The registration body:

```json
{
  "platform": "ios",
  "bundle_id": "com.tatoalo.aulos",
  "environment": "sandbox",
  "alerts": true,
  "live_activity_start_token": "<hex>",
  "app_version": "1.0.0 (3)"
}
```

| Field | Type | Meaning |
|---|---|---|
| `platform` | string, required | `"ios"`. Anything else is `400 validation_failed`. |
| `bundle_id` | string, required | the app's bundle identifier, which is also the `apns-topic` the server pushes with. It must be the server's `APNS_TOPIC` (default `com.tatoalo.aulos`) or an extension of it — `<APNS_TOPIC>.something`, which is how a widget or App Clip is named. Anything else is `400 validation_failed` on `bundle_id`: the field decides which app the operator's provider key signs a push for, so the server has an opinion about it. |
| `environment` | string, required | `"sandbox"` or `"production"` — which APNs gateway minted the token. A Debug/simulator build is `sandbox`, a TestFlight/App Store build is `production`. Getting it wrong makes every push fail inside APNs, so it is validated. |
| `alerts` | boolean, optional | whether completion/failure alerts are wanted. Absent or `null` means `true`. |
| `live_activity_start_token` | string or `null`, optional | the Live Activity **push-to-start** token (iOS 17.2+), when the app has one. `null` clears a previously reported one. |
| `app_version` | string or `null`, optional | free-form, for the server's logs. At most 64 characters. |

`{token}` and the two token fields are **hexadecimal, 32 to 200 characters**. They are lowercased
on the way in, so a `DELETE` whose token a client happened to uppercase still finds the row it
registered. `{item_id}` is validated as a ULID but **need not name an item that exists** — the app
starts a Live Activity the moment the user taps download, which can beat the server's own row.

`PUT api/v2/devices/{token}` is an upsert keyed on the token: a repeat call refreshes every field
except the server's record of when the token was *first* seen. That is the call the app makes on
every launch. `DELETE api/v2/devices/{token}` forgets the device **and every Live Activity
registered under it**; deleting an item likewise forgets the Live Activity registrations for it and
for its children, so a client never has to clean those up itself.

The `PUT` on a live activity is the one route here that can `404`: without a registered device the
server does not know which gateway to push to, so the registration would be undeliverable. Register
the device first. The matching `DELETE` is a `204` even then — the caller wants the registration
gone and it is gone.

Because the success answer is `204`, there is nowhere to return a `warnings` list: a field this
build does not know is ignored (and logged), never a `400`. §4.1's forward-compatibility rule
therefore still holds — a newer client is never rejected for saying more than the server
understands.

#### Live Activity content state

The server sends Live Activity pushes for items that have a registration: a `start` when the item
first leaves `queued` for an active status, `update`s as it progresses, and an `end` when it
settles. Every one of them carries the same `content-state` object, and an app's
`ActivityAttributes.ContentState` must decode exactly these seven keys — all of them always
present, `camelCase`, `null` where the value is unknown:

```json
{
  "status": "downloading",
  "percent": 42.5,
  "speed": 2100000.0,
  "eta": 68,
  "downloadedBytes": 123,
  "totalBytes": 456,
  "message": "Merging formats"
}
```

| Key | Type | Meaning |
|---|---|---|
| `status` | string | the v2 status word (§3.1). |
| `percent` | number | `0`–`100`, always a number. |
| `speed` | number or `null` | bytes per second. |
| `eta` | integer or `null` | seconds remaining. |
| `downloadedBytes` | integer or `null` | bytes written so far. |
| `totalBytes` | integer or `null` | the expected total. |
| `message` | string or `null` | the live stage line — the same text as `item.msg` (§2.3). |

The push envelopes themselves are APNs concerns rather than protocol ones (`event` is `start`,
`update` or `end`; the `end` push carries a `dismissal-date`), and the attributes a `start` push
declares are `AulosDownloadAttributes` with `itemId`, `url` and `title`.

---

## 5. WebSocket

### 5.1 Connecting

```
GET <p>ws
Upgrade: websocket
Sec-WebSocket-Protocol: aulos.v2
Cookie: <your session cookies>
```

Query parameters, all optional:

| Param | Type | Meaning |
|---|---|---|
| `since` | integer | resume from this `seq` instead of taking a fresh snapshot |
| `boot` | string | the `boot_id` your `since` came from. A mismatch forces a snapshot. **Always send it with `since`.** |
| `done` | boolean, default `true` | include the completed window in the snapshot |
| `groups` | comma list of ids | pre-subscribe to these groups' children |
| `token` | string | the bearer token, when the proxy cannot forward cookies |

Text frames only, one JSON object per frame. Every server frame has `t` (the type) and `seq` (a
`u64`).

The server sends a WebSocket-level `Ping` every 20 s and closes the connection if it sees no `Pong`
or data for 60 s. Close codes you may see: `1001` (server shutting down — reconnect),
`1013` (you were too slow, or there are too many clients — back off and reconnect),
`1009` (you sent a frame larger than 1 MiB).

Compression is not negotiated. Frames are 200–900 bytes; you do not need it.

### 5.2 Frame catalogue

| `t` | Cadence | Payload |
|---|---|---|
| `snapshot` | once on connect, and after the server detects you fell behind | the complete state, same `Item` shape as REST |
| `resume` | once instead of `snapshot`, when your `since` was resumable | a summary of what follows |
| `delta` | every `batch_ms` (250 ms default) for numeric changes; within `urgent_ms` (25 ms) when a text field (`msg`, `title`, `phase`) changed | changed fields only |
| `added` | prompt, within `urgent_ms` (25 ms) | full `Item` objects (an **upsert**) |
| `completed` | prompt | full `Item` objects, terminal |
| `removed` | prompt | ids and a reason; **one frame per distinct reason** in a flush (§5.7) |
| `subscription` / `subscription_removed` | prompt | the envelopes are in §5.9; the `Subscription` object itself is §9 |
| `ytdl_options` | prompt | the options-file reload result |
| `providers` | prompt | a plugin reload report |
| `notice` | prompt | a human-readable warning (stall, timeout, POT) |
| `health` | on a component status change only | what changed |
| `pong` | on demand | your RTT probe, echoed |
| `error` | on demand | a protocol or auth error, usually followed by a close |

`seq` is strictly increasing across **all** frame types within one server boot. A jump of more than
1 does not mean you lost anything — `seq` counts frames and every frame is delivered in order on a
single socket. Gaps only appear across a reconnect.

### 5.3 `snapshot`

```json
{ "t": "snapshot",
  "seq": 10251,
  "boot_id": "01JBQ8YQ2E0000000000000000",
  "server_time": 1757000000123,
  "server": { "version": "2026.09.04", "yt_dlp": "2026.8.30.232658.dev0",
              "url_prefix": "/", "started_at": 1756956799000 },
  "protocol": { "batch_ms": 250, "urgent_ms": 25, "replay_frames": 512,
                "delta_semantics": "absent-key-means-unchanged" },
  "counts": { "queued": 486, "resolving": 0, "preparing": 0, "downloading": 3,
              "postprocessing": 0, "finished": 401, "error": 10, "canceled": 3 },
  "done_total": 4211,
  "truncated": { "done": true, "groups": ["01JBQ8AA0000000000000000GG"] },
  "items": [ /* Item objects, ord ascending: everything non-terminal, plus groups */ ],
  "done":  [ /* Item objects, ord ascending: the most recent completed/failed/cancelled */ ],
  "subscriptions": [ /* Subscription objects, §9 */ ],
  "ytdl_options": { "ok": true, "msg": "", "update_time": 1757000200.412 },
  "health": { "status": "ok", "components": { "pot": "ok", "store": "ok", "ytdl_options": "ok" } } }
```

- `items` and `done` are two arrays purely so the completed window can be omitted with
  `?done=false`. **They hold the same object type.** Concatenate them if you want one list.
- `truncated.done: true` means `done` is a window, not the whole history: there are `done_total`
  completed records and you are seeing the most recent ones. Page the rest with
  `GET api/v2/items?status=finished,error,canceled&cursor=…` when the user scrolls.
- `truncated.groups` lists the ids of groups whose children were **not** included because the
  group is large. Those groups have `children_inline: false`. Fetch their children on demand with
  `GET api/v2/items?group_id=<id>` or with a `watch` frame (§5.11).
- `ytdl_options` and `health` are the **current** values of the two things that are otherwise only
  announced on change (§5.9). They are in the snapshot because both frames are transition-only:
  without them, a client connecting while the POT sidecar is down or the options file is broken
  would see nothing on the socket and would have to additionally call `GET healthz` and
  `GET api/v2/ytdl-options`. (The legacy server pushed `ytdl_options_changed` on connect for
  exactly this reason.) `ytdl_options` is the `ytdl_options` frame's payload minus `t`/`seq`;
  `health` is the abridged form — `status` plus a flat `component → "ok" | "degraded" | "down"`
  map. Call `GET healthz` for the detail behind a non-`ok` component. Both keys are always present.
- `protocol` is self-describing on purpose: read `batch_ms` if you want to size an animation, and
  read `delta_semantics` to assert you and the server agree.

### 5.4 `delta`

```json
{ "t": "delta", "seq": 10252, "ts": 1757000000373,
  "items": [
    { "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
      "percent": 43.9, "speed": 3210000.0, "eta": 61, "downloaded_bytes": 45613056 },
    { "id": "01JBQ9BBEEEE0000000000000C",
      "status": "postprocessing", "percent": 100.0, "speed": null, "eta": null,
      "phase": "remux", "msg": "Merging formats" }
  ] }
```

**Merge rules — this is the contract:**

| On the wire | Meaning |
|---|---|
| key absent | the field is **unchanged** since the last frame that mentioned this id |
| key present, non-null | the new value |
| key present, `null` | the field changed **to** null (e.g. `speed` cleared when a job leaves `downloading`) |

`id` is always present. Every other key is optional. So the merge is: for each object in `items`,
find your record by `id` and overwrite exactly the keys that are present. In Swift, decode a
delta into a dictionary-shaped intermediate (or a struct of double-optionals) — **not** into your
`Item` type, because your `Item` type cannot distinguish "absent" from "null".

A `delta` never contains an id you have not seen, **with exactly one exception**: additions always
arrive in an `added` frame first, because the server's flush order is
`added → completed → removed → delta` (§4.3). A `delta` never adds or removes records.

The exception is the children of a large group. Frames are serialised once and shared by every
connected client, so the server does not filter them per connection: a running child of a group
whose `children_inline` is `false` (§5.3, §2.3) produces a `delta` that reaches you even though you
never received an `added` for it. This costs you nothing — the apply step in §7 already skips a
patch whose id it does not know, which is the correct behaviour — and it is why that `guard … else
{ continue }` line is not defensive padding. If you want those rows, `watch` the group (§5.11);
the `added` frames you get back then make the subsequent patches applicable.

A `delta` also never contains `selection`, `folder` or `request`: those are fixed at insert and are
carried only by `added`/`completed`/`snapshot`. Nor does it contain `id`-adjacent identity fields
(`kind` does change — exactly once, on a group promotion — but that arrives as an `added` upsert,
§5.5, not as a patch).

**Text fields are prompt, not batched.** A change to `msg`, `title` or `phase` is flushed within
`urgent_ms` (25 ms) like a status change, not held for the next 250 ms tick. That matters because
`msg` is the only feedback during a long pre-download phase — e.g. StreamingCommunity's
`"Starting N_m3u8DL-RE download..."` then `"N_m3u8DL-RE failed, retrying with ffmpeg..."` — so you
can render those the moment they arrive without a client-side timer. Purely numeric changes
(`percent`, `speed`, `eta`, byte and fragment counters, `phase_percent`) are the batched ones.

An idle server sends **no `delta` frames at all**. Silence means nothing changed.

### 5.5 `added` — an upsert, and the in-place group promotion

```json
{ "t": "added", "seq": 10253, "reason": "created",
  "items": [ { /* full Item */ }, { /* full Item */ } ] }
```

`reason` ∈ `"created"` (a user added these) | `"expanded"` (a playlist resolved into a group and
children) | `"retried"` (an item was requeued).

**Treat `added` as an upsert keyed on `id`, not as an insert.** This matters because of how a
playlist resolves:

1. You `POST` a playlist URL and get back `id = 01JBQ8AA…GG`.
2. You receive `added` with one item: `{id: 01JBQ8AA…GG, kind: "item", status: "resolving", ord: 982}`.
   You render one spinner row.
3. Resolution finishes. You receive **one** frame:

```json
{ "t": "added", "seq": 10260, "reason": "expanded",
  "items": [
    { "id": "01JBQ8AA0000000000000000GG", "kind": "group", "ord": 982,
      "title": "Lo-fi beats", "status": "queued",
      "children_total": 500, "children_done": 0, "children_error": 0, "children_active": 0,
      "children_inline": false, "…": "the rest of the full Item" },
    { "id": "01JBQ8AB…01", "kind": "item", "ord": 983, "group_id": "01JBQ8AA0000000000000000GG",
      "group_index": 1, "…": "…" }
  ] }
```

The group has **the same `id` and the same `ord`** as the row you already have. Its `kind` flipped
from `"item"` to `"group"`. There is **no `removed` frame**. If you upsert by id, the row morphs in
place: it does not blink, it does not move, and the id you got from `POST` still addresses it.

Children arrive in this and subsequent `added` frames, batched (roughly 50–100 per frame, at least
25 ms apart). A 500-item playlist produces around ten frames, not five hundred.

### 5.6 `completed`

Prompt, terminal, and carries the **full** `Item`, so you never need a follow-up fetch.

```json
{ "t": "completed", "seq": 10261,
  "items": [
    { "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA", "status": "finished", "percent": 100.0,
      "filename": "Rick Astley - Never Gonna Give You Up.mp4", "size": 103809024,
      "download_url": "download/Rick%20Astley%20-%20Never%20Gonna%20Give%20You%20Up.mp4",
      "finished_at": 1757000060000, "error": null, "…": "the rest of the full Item" } ] }
```

`completed` fires for **all three** terminal statuses: `finished`, `error` and `canceled`. Read
`status` to decide which section the row moves to. For `error`, `error` is non-null and carries a
code you can branch on.

### 5.7 `removed`

```json
{ "t": "removed", "seq": 10262, "ids": ["01JBQ7Z5T9K3M2R8V4XW6Y0AAA"], "reason": "deleted" }
```

`reason` ∈ `"deleted"` (a user deleted it) | `"cleared"` (a user cleared completed) |
`"auto_cleared"` (`CLEAR_COMPLETED_AFTER` expired) | `"group_cascade"` (its group was deleted).

**The reason is per frame, and a flush can emit more than one `removed` frame.** The server tracks a
reason per id, but the frame carries one reason for all of its ids, so when a 250 ms window contains
removals with different reasons — a user delete plus a `CLEAR_COMPLETED_AFTER` expiry, say — the
server groups by reason and emits **one `removed` frame per distinct reason**, in the fixed order
`deleted`, `cleared`, `auto_cleared`, `group_cascade`, all of them in the `removed` position of the
flush order. So a flush contains between zero and four `removed` frames, each with a homogeneous
`ids` array. This is the one place where "at most one frame of each kind per flush" does not hold,
and it is stated identically in §4.3 (where the REST delta expresses it as an array of groups), in
§6.3 (the resume fold) and in DESIGN §15.1.

Remove by `id`. There is no url-based removal and no bare-string payload. A `removed` for an id you
do not have is not an error — ignore it. If you do not care why a record went away, concatenate the
`ids` arrays and ignore `reason` entirely.

### 5.8 `notice`

```json
{ "t": "notice", "seq": 10270, "level": "warning", "code": "stalled",
  "id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
  "message": "No progress for 900s" }
```

`level` ∈ `"info" | "warning" | "error"`. `code` ∈ `"stalled" | "job_timeout" | "pot_down" |
"provider_degraded" | "import_warning" | "plugin_note"` and may grow — treat it as an open set with
a fallback. `id` may be `null` (a server-wide notice). Show it, or ignore it — nothing in the item
state depends on it.

`plugin_note` is the one code whose `message` comes from outside the server: a `command` provider
can print `{"t":"note","message":"…"}` while resolving (DESIGN §6.5.3) and the server forwards it
verbatim under this single code. The plugin does **not** get to choose the code — the wire's `code`
set stays closed and server-owned — so if you branch on `code`, `plugin_note` is simply "display
this text".

### 5.9 `subscription`, `subscription_removed`, `ytdl_options`, `providers`, `health`

```json
{ "t": "subscription", "seq": 10275,
  "subscription": { "id": "9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f", "name": "Veritasium",
                    "url": "https://www.youtube.com/@veritasium", "enabled": true,
                    "check_interval_minutes": 60, "download_type": "video", "codec": "auto",
                    "format": "any", "quality": "best", "folder": "",
                    "last_checked": 1757000100000, "seen_count": 317, "error": null,
                    "next_due": 1757003700000, "consecutive_failures": 0, "checking": false } }
```

| Key | Type | Notes |
|---|---|---|
| `t` | `"subscription"` | |
| `seq` | `integer` | |
| `subscription` | `Subscription` | the **whole** object of §9, nested under this key — never inlined into the envelope. It is an upsert keyed on `subscription.id`: the same frame type carries a creation, an edit, and the result of every check (which is what makes `last_checked`, `seen_count`, `error` and `checking` move). |

```json
{ "t": "subscription_removed", "seq": 10276, "ids": ["9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f"] }
```

| Key | Type | Notes |
|---|---|---|
| `t` | `"subscription_removed"` | |
| `seq` | `integer` | |
| `ids` | `[string]` | always an array, even for a single deletion — the v1 delete route takes a list, so one call can remove several. Legacy emitted one bare id string per deletion; this is the one shape. Remove by id; an unknown id is not an error. |

```json
{ "t": "ytdl_options", "seq": 10280, "ok": false,
  "msg": "YTDL_OPTIONS_FILE contents is invalid", "update_time": 1757000200.412 }
```

`update_time` is a float of epoch **seconds** (or `null`) — the one place in the protocol that is
not integer milliseconds, kept for legacy payload compatibility.

```json
{ "t": "providers", "seq": 10281,
  "added": ["command:bandcamp"], "updated": [], "removed": [],
  "failed": [ { "name": "kinotek", "error": "download.command[0] not executable" } ] }
```

Refetch `api/v2/capabilities` and/or `api/v2/catalog` when you see this.

```json
{ "t": "health", "seq": 10290, "status": "degraded",
  "changed": [ { "component": "pot", "from": "ok", "to": "down",
                 "detail": "3 consecutive probe failures" } ] }
```

Emitted **only** on a component transition, never periodically.

### 5.10 `pong` and `error`

```json
{ "t": "pong", "seq": 10291, "server_time": 1757000300000, "c": 1757000299871 }
{ "t": "error", "seq": 10292, "code": "bad_frame", "message": "unknown frame type \"subscribe\"" }
```

`c` echoes whatever you sent in your `ping`, so you can measure RTT.

### 5.11 Client → server frames

All optional. A client that only reads is fully functional, and **mutations are never sent over
the socket** — they stay on REST, where auth, idempotency and the error envelope live.

| Frame | Purpose |
|---|---|
| `{"t":"hello","client":"aulos-ios/1.2","topics":["items","subscriptions","health"]}` | identify yourself and narrow what you receive. Send it within 100 ms of connecting or the server assumes all topics. |
| `{"t":"ping","c":1757000299871}` | RTT probe → `pong` |
| `{"t":"ack","seq":10261}` | tells the server how far you have consumed. **Advisory only:** it is used for lag reporting and slow-consumer logging, and it never trims anything. The replay ring is shared by every client, so one client's `ack` cannot be allowed to shorten another client's `?since=` window; retention is bounded solely by the server's `replay_frames` and its byte cap. Send it or do not — nothing observable changes. |
| `{"t":"resume","since":10240,"boot":"01JBQ8YQ…"}` | ask for a merged catch-up on an already-open socket (normally you pass `?since=` at connect time instead) |
| `{"t":"watch","groups":["01JBQ8AA…GG"],"done":true}` | subscribe to a collapsed group's children — the groups listed in `snapshot.truncated.groups`. The server replies with `added` frames containing them, paged at roughly 50–100 per frame. **`done`** (boolean, default `true`) selects *which* children you get: `true` = all of them, `false` = only the non-terminal ones, which is what you want for a 500-episode group where 480 children are already `finished` and you only care about what is still moving. It has no effect on anything else, and it does not filter later frames. |
| `{"t":"unwatch","groups":["01JBQ8AA…GG"]}` | stop receiving further `added` pages for them. Note that unwatching does not suppress `delta` patches for children the server is already emitting globally (§5.4); it only ends the subscription that produced the `added` frames. The server drops all of a connection's watches by itself when the socket closes, and after a fresh `snapshot` (which re-states `truncated.groups`) you must `watch` again. |

---

## 6. `seq`, `boot_id` and resume

### 6.1 The two identifiers

| | Meaning |
|---|---|
| `seq` | A `u64` frame counter, strictly increasing across all frame types. It is **durable**: the server reserves blocks of sequence numbers before handing any out, so a crash skips values but never re-issues one. |
| `boot_id` | A ULID minted once per server process. It appears in `snapshot`, `state`, `capabilities` and `healthz`. |

`ord` is a **different** number: it orders *records*, `seq` orders *frames*. Never sort by `seq`.

### 6.2 The resume algorithm

Persist `(seq, boot_id)` from the last frame you successfully applied. On reconnect:

```
connect with ?since=<seq>&boot=<boot_id>
  → you receive `snapshot`   ⇒ discard your local state and replace it wholesale
  → you receive `resume`     ⇒ keep your state and apply what follows
```

You get a `snapshot` — and must discard — when any of these is true, and the server decides, not
you:

- you sent no `since`;
- your `boot` does not match the server's `boot_id` (the server restarted, or a backup was
  restored);
- your `since` is older than the server's replay window;
- your `since` is **greater** than the server's current `seq` (a restore rolled the server back).

That last case is why `boot` matters. Without it a client could present a cursor above the head and
be told "you are up to date" when it had in fact missed everything.

### 6.3 `resume`

```json
{ "t": "resume", "seq": 10261, "from": 10240, "to": 10261,
  "merged": { "added": 2, "completed": 1, "removed": 1, "delta_items": 3 } }
```

Immediately after it, the server sends **at most one `added`, at most one `completed`, at most one
`removed` per distinct reason, and at most one `delta`**, in that order — the same order and the
same per-reason `removed` grouping as §4.3 and as the live flush (§5.7) — with all the intervening
changes folded together: per `(id, field)` the last value wins, an `added` that was later `removed`
is dropped entirely, and a `completed` supersedes any earlier delta for that id. A client that was
away 45 seconds gets a handful of small frames, not 180 frames of stale numbers.

`merged` counts what went into the fold, so `merged.removed` is a **total id count** across the
reasons, not a frame count.

Apply them in the order received. Then your `seq` is `to`.

### 6.4 Correlating a mutation with its frame

Every REST response carries `X-Aulos-Seq`, and mutating responses also carry `seq` in the body.
The WebSocket frame caused by that mutation has a `seq` **at or above** it. So:

- Optimistic UI: apply your change locally, remember the returned `seq`, and reconcile when you see
  a frame with `seq >= that`.
- You do **not** need to remove items locally "because events are unreliable". A `cancel` or
  `delete` that returned `200` will produce a `completed` or `removed` frame. If you want to be
  eager anyway, doing so is now safe and idempotent because everything is keyed on `id`.

---

## 7. Applying frames — the complete client algorithm

```
state = { itemsById: [String: Item], seq: UInt64, bootId: String }

on connect:
    send ?since=state.seq&boot=state.bootId   (omit both on a cold start)

on "snapshot":
    state.itemsById = index(frame.items + frame.done, by: \.id)
    state.seq       = frame.seq
    state.bootId    = frame.boot_id
    remember frame.truncated, frame.done_total, frame.counts, frame.protocol

on "resume":
    state.seq = frame.to        // then apply the following frames in order

on "added":                     // UPSERT, never insert-at-index
    for item in frame.items { state.itemsById[item.id] = item }
    state.seq = frame.seq

on "completed":
    for item in frame.items { state.itemsById[item.id] = item }
    state.seq = frame.seq

on "removed":                   // may arrive several times per flush, one per reason
    for id in frame.ids { state.itemsById.removeValue(forKey: id) }
    state.seq = frame.seq

on "delta":
    for patch in frame.items {
        guard var item = state.itemsById[patch.id] else { continue }   // never create here
        for key in patch.presentKeys { item[key] = patch[key] }        // absent = unchanged
        state.itemsById[patch.id] = item
    }
    state.seq = frame.seq

on "subscription":              // UPSERT by frame.subscription.id
    state.subsById[frame.subscription.id] = frame.subscription
    state.seq = frame.seq

on "subscription_removed":      // frame.ids is always an array
    for id in frame.ids { state.subsById.removeValue(forKey: id) }
    state.seq = frame.seq

on "ytdl_options" / "providers" / "health" / "notice":
    handle, then state.seq = frame.seq

rendering:
    let all = state.itemsById.values.sorted { ($0.ord, $0.id) < ($1.ord, $1.id) }
    inProgress = all.filter { [.queued, .resolving, .preparing, .downloading, .postprocessing]
                                .contains($0.status) }
    completed  = all.filter { $0.status == .finished }
    failed     = all.filter { $0.status == .error }
    canceled   = all.filter { $0.status == .canceled }

    // within inProgress, three sub-treatments, all read off fields you already have:
    //   .queued && !autoStart && error?.code == "not_yet_live"  -> "Scheduled", show error.message
    //   .queued && !autoStart                                   -> "Paused",  offer Start
    //   .queued &&  autoStart                                   -> "Waiting", offer Pause
```

Things this algorithm deliberately does not do, and you should not add:

- no client-side throttle or debounce — the server batches at 250 ms and sends nothing when idle;
- no title sort — `ord` is the sort key, and it is stable;
- no insertion at index 0 — everything is keyed by `id` and ordered by `ord`;
- no `GET /history` on connect — the `snapshot` *is* that, in the same shape;
- no dual-key (`id` or `url`) lookup — there is one key;
- no flexible numeric decoding — types are fixed;
- no `"ERROR: "` prefix stripping — messages are already clean.

---

## 8. Format and quality selection

Two endpoints, two purposes:

| Use | Endpoint |
|---|---|
| Build the picker before a URL is known (settings, defaults) | `GET api/v2/capabilities` → `formats[]`, the flat `{id, text, qualities:[{id, text}]}` shape |
| Refine the picker once a URL is known (share sheet, add form) | `GET api/v2/catalog?url=<encoded>` → the selected provider's catalog, with `notice` text and `flags` |

Cache `capabilities` with its `ETag` and refresh it on `providers` frames or on a `304`-checked
reconnect. `catalog?url=` is cheap and needs no caching.

The legacy matrix, which `capabilities.formats` reproduces exactly:

| `download_type` | `format` values | `quality` values |
|---|---|---|
| `video` | `any` | `best`, `2160`, `1440`, `1080`, `720`, `480`, `360`, `240`, `worst` |
| `video` | `mp4` | the above **plus `best_remux`** |
| `video` | `ios` | the same nine as `any` |
| `audio` | `m4a` | `best`, `192`, `128` |
| `audio` | `mp3` | `best`, `320`, `192`, `128` |
| `audio` | `opus`, `wav`, `flac` | `best` |
| `captions` | `srt`, `txt`, `vtt`, `ttml`, `sbv`, `scc`, `dfxp` | `best` |
| `thumbnail` | `jpg` | `best` |

`codec` applies to `video` only and is forced to `"auto"` otherwise. `ios` is a real video format
with the full height list, not a single-quality shortcut: `{video, ios, 1080}` composes an
H.264/HEVC + AAC selector bounded to 1080p, exactly as the legacy server did.

Two honesty notes the catalog carries and you should surface:

- `best_remux` has `flags.slow` and a notice: it re-encodes audio after downloading, to undo
  SponsorBlock stream-copy drift. Expect a `postprocessing` phase of minutes on a long video.
- `worst` has a notice saying the selector currently resolves to the best available stream. That is
  the historical behaviour, preserved deliberately; the catalog now says so instead of lying.

---

## 9. Subscriptions

The `Subscription` object, as returned by `GET api/v2/subscriptions`, `POST/PATCH` responses, the
`snapshot`, and the `subscription` frame:

```json
{ "id": "9c1f2d84-1c6e-4a1b-9f0e-2b7a1c3d4e5f",
  "name": "Veritasium",
  "url": "https://www.youtube.com/@veritasium",
  "enabled": true,
  "check_interval_minutes": 60,
  "download_type": "video",
  "codec": "auto",
  "format": "any",
  "quality": "best",
  "folder": "",
  "last_checked": 1757000100000,
  "seen_count": 314,
  "error": null,
  "next_due": 1757003700000,
  "consecutive_failures": 0,
  "checking": false }
```

| Key | Type | Notes |
|---|---|---|
| `id` | `string` | a ULID for new subscriptions; imported ones keep their original UUID string. Opaque — do not parse it. |
| `name` | `string` | |
| `url` | `string` | unique across subscriptions |
| `enabled` | `boolean` | |
| `check_interval_minutes` | `integer` | minimum 1 |
| `download_type`, `codec`, `format`, `quality` | `string` | the selection applied to new items |
| `folder` | `string` | `""` means the base directory |
| `last_checked` | `integer \| null` | unix **ms** (v1 emits float seconds) |
| `seen_count` | `integer` | how many media ids have been marked seen |
| `error` | `string \| null` | the last check's error text; cleared by a successful check |
| `next_due` | `integer \| null` | unix ms of the next scheduled check |
| `consecutive_failures` | `integer` | drives exponential backoff, up to 6 hours |
| `checking` | `boolean` | a check is running right now |

`POST api/v2/subscriptions` takes the same body as an add plus `check_interval_minutes`. It
resolves the URL synchronously enough to reject a single-video URL with
`400 validation_failed` and the message *"This URL points to a single video, not a channel or
playlist. Use Download instead."*, and to reject a duplicate with `409 conflict` and *"This URL is
already subscribed"*. On success every currently visible item is marked **seen without being
downloaded** (except upcoming livestreams), so subscribing does not backfill a channel's entire
history.

`POST api/v2/subscriptions/check` returns `202` immediately with a `job_id`; watch `checking` and
`last_checked` on the `subscription` frames to see progress.

---

## 10. The v1 compatibility shim

The v1 routes exist so the previously shipped client, the README bookmarklet and the iOS Shortcut
keep working during cutover. **Do not build anything new against them.** They are mounted only
while `AULOS_V1_ENABLED=true` and are a thin translation layer over v2.

### 10.1 Route mapping

| v1 route | Behaviour | Notes |
|---|---|---|
| `POST <p>add` | `{"status":"ok"}` or `{"status":"error","msg":…}`, **HTTP 200 either way** | now `application/json` (was `text/plain`); response gains an additive `"ids":[…]`; validation failures are a real `400` carrying the legacy reason string as `error.message`. Unlike `POST api/v2/downloads`, this route **waits** for resolution (up to `AULOS_V1_ADD_RESOLVE_WAIT_MS`, default 10 s) so a resolution failure — an unsupported URL, `Invalid/empty data was given.`, a geo-block, `Unsupported resource "…"` — is still reported in the body, joined with `", "` for a multi-URL add, exactly as the Python server did. If the wait expires first the answer is `{"status":"ok"}` and the item's real outcome shows up in `history` (DESIGN §11.2). New clients must not rely on any of this: use `POST api/v2/downloads` and watch the item. |
| `GET <p>history` | `{"queue":[…],"pending":[…],"done":[…]}` | all three keys always present, §10.2 |
| `POST <p>delete` | `{"ids":[…],"where":"queue"\|"done"}` → `{"status":"ok"}` | `ids` may be URLs, legacy media ids, **or** ULIDs, §10.3 |
| `POST <p>start` | `{"ids":[…]}` → `{"status":"ok"}` | also **retries** terminal items, which legacy could not do; `ids: null` is a 400, not a 500 |
| `GET <p>version` | `{"yt-dlp":"…","version":"…","url_prefix":"/","protocol":"v2"}` | the two extra keys are additive |
| `GET <p>presets` | `{"presets":["a","b"]}` | unchanged |
| `POST <p>cancel-add` | `{"status":"ok"}` | now actually aborts in-flight resolution |
| `POST <p>subscribe` | `{"status":"ok","subscription":{…13 keys…}}` | the exact legacy projection |
| `GET <p>subscriptions` | an array of the same 13-key objects | |
| `POST <p>subscriptions/update` | `{"status":"ok","subscription":{…}}` | a bad `enabled` is a `400`, not a leaked 500 |
| `POST <p>subscriptions/delete` | `{"status":"ok"}` | `[]` is still a 400 |
| `POST <p>subscriptions/check` | `{"status":"ok","job_id":"…"}` **immediately** | no longer blocks for minutes |
| `POST <p>upload-cookies` / `POST <p>delete-cookies` / `GET <p>cookie-status` | the legacy shapes and messages | the cap is preserved exactly: **1 000 000 bytes**, decimal, message `Cookie file too large (max 1MB)` |
| `GET <p>robots.txt` | as legacy | |
| `GET /` when the prefix is not `/` | `302` to the prefix | |
| `GET <p>download/*`, `<p>audio_download/*` | the file, now with `Range` support | |
| `GET <p>socket.io/*` | **`501`** with `{"error":{"code":"socketio_removed","message":"Socket.IO is not supported; use <prefix>ws (protocol v2) or GET <prefix>api/v2/state"}}` | deliberate: a stale client fails loudly instead of hanging on a handshake |
| `GET <p>` | a small JSON identity document | no HTML, no `metube_theme` cookie |

### 10.2 v1 `history` contents

| Array | v2 statuses it contains |
|---|---|
| `queue` | `resolving`, `preparing`, `downloading`, `postprocessing`, and `queued` with `auto_start == true` |
| `pending` | `queued` with `auto_start == false` — which covers items added with `auto_start: false`, paused items, and upcoming-livestream items (whose legacy `error` string is projected as-is, exactly as the Python server did) |
| `done` | `finished`, `error` |

- Groups (`kind == "group"`) are **omitted entirely**; only their children appear. A parent row
  that never progresses is worse than nothing in a client that has no group concept.
- `canceled` items are **omitted entirely** from all three arrays, because the previously shipped
  client has no `canceled` case and maps unknown statuses to `pending`, which would leave the row
  stuck in "In Progress" forever. Legacy made cancels vanish; this is faithful. v2 clients see
  them.
- Each array is ordered by `ord` ascending.
- `entry` (the full yt-dlp metadata dict) is **not** included. It was the largest payload
  contributor and nothing read it.
- `done[]` is the **whole** completed set, not a window — the same thing legacy returned. v1 has no
  `truncated`, no `done_total` and no cursor, so windowing it would silently drop history out of
  the client. (An operator can cap it with `AULOS_V1_HISTORY_MAX`, which keeps the most recent
  records; the default is uncapped.) This is the one respect in which v1 is more expensive than v2,
  and it is why anything new should use `GET api/v2/state` plus paged `api/v2/items`.

### 10.3 v1 id resolution

Each token in a v1 `ids` array is resolved in this order: a ULID that exists → an exact `url`
match → an exact legacy `media_id` match. A token matching nothing is silently skipped, as legacy
did. So `url ?? id` from the old client, and url-only `clearCompleted`, both keep working.

The v1 `id` field itself is the legacy extractor media id when there is one (with the
`"<prefix>.<id>"` prefixing reproduced), else the ULID.

### 10.4 Status mapping v2 → v1

| v2 | v1 | Where it appears |
|---|---|---|
| `queued` (`auto_start=false`) | `pending` | `pending[]` |
| `queued` (`auto_start=true`) | `pending` | `queue[]` |
| `resolving` | `pending` | `queue[]` |
| `preparing` | `preparing` | `queue[]` |
| `downloading` | `downloading` | `queue[]` |
| `postprocessing` | `downloading` | `queue[]` — `msg` carries the phase |
| `finished` | `finished` | `done[]` |
| `error` | `error` | `done[]` |
| `canceled` | — | omitted |

### 10.5 v1 type quirks preserved on purpose

`DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT` and `SUBSCRIPTION_DEFAULT_CHECK_INTERVAL` are emitted as
**strings** by v1 (legacy never coerced them) and as **numbers** by v2. `last_checked` is float
seconds in v1 and integer milliseconds in v2. `percent` may be `null` in v1 for an item that has
not started; in v2 it is always a number.

---

## 11. A worked Swift sketch

Not prescriptive — it is here to prove the protocol needs no tricks. No `AnyCodable`, no fallback
parsers, no flexible numeric decoders, no `CodingKeys` heroics beyond snake_case mapping.

```swift
// MARK: - Enums

enum ItemKind: String, Codable { case item, group }

enum Status: String, Codable {
    case queued, resolving, preparing, downloading, postprocessing, finished, error, canceled
    // Forward compatibility: decode unknown values into `.unknown` rather than guessing.
    case unknown
    init(from d: Decoder) throws {
        let raw = try d.singleValueContainer().decode(String.self)
        self = Status(rawValue: raw) ?? .unknown
    }
    var isActive: Bool {
        switch self {
        case .queued, .resolving, .preparing, .downloading, .postprocessing: return true
        default: return false
        }
    }
}

// MARK: - Item

struct WireError: Codable, Equatable, Hashable {
    let code: String
    let message: String
    let field: String?
    let provider: String?
    let providerCode: String?
}

struct FileRef: Codable, Equatable, Hashable {
    let filename: String
    let size: Int64?
    let downloadURL: String?
    let lang: String?
    enum CodingKeys: String, CodingKey {
        case filename, size, lang
        case downloadURL = "download_url"
    }
}

struct Selection: Codable, Equatable, Hashable {
    let downloadType: String
    let codec: String
    let format: String
    let quality: String
    enum CodingKeys: String, CodingKey {
        case downloadType = "download_type"
        case codec, format, quality
    }
}

/// The rest of the add request, echoed back. Immutable for the record's life, so it never
/// appears in a `delta` and you can read it once.
struct RequestView: Codable, Equatable, Hashable {
    let customNamePrefix: String
    let playlistItemLimit: Int
    let autoStart: Bool
    let splitByChapters: Bool
    let chapterTemplate: String
    let subtitleLanguage: String
    let subtitleMode: String
    let ytdlOptionsPresets: [String]
    let ytdlOptionsOverrides: [String: JSONValue]   // secret-looking values are "«redacted»"
    enum CodingKeys: String, CodingKey {
        case customNamePrefix = "custom_name_prefix"
        case playlistItemLimit = "playlist_item_limit"
        case autoStart = "auto_start"
        case splitByChapters = "split_by_chapters"
        case chapterTemplate = "chapter_template"
        case subtitleLanguage = "subtitle_language"
        case subtitleMode = "subtitle_mode"
        case ytdlOptionsPresets = "ytdl_options_presets"
        case ytdlOptionsOverrides = "ytdl_options_overrides"
    }
}

struct SourceRef: Codable, Equatable, Hashable {
    let kind: String
    let ref: String?
}

struct Item: Codable, Equatable, Identifiable, Hashable {
    let id: String
    let kind: ItemKind
    let ord: Int64
    let groupID: String?
    let groupIndex: Int?
    let url: String
    let title: String
    let status: Status
    let autoStart: Bool
    let provider: String?

    let percent: Double            // never nil
    let speed: Double?
    let eta: Int?
    let downloadedBytes: Int64?
    let totalBytes: Int64?
    let totalBytesEstimate: Int64?
    let fragmentIndex: Int?
    let fragmentCount: Int?
    let phase: String?
    let phasePercent: Double?

    let msg: String?
    let error: WireError?

    let filename: String?
    let size: Int64?
    let downloadURL: String?
    let chapterFiles: [FileRef]    // never nil
    let subtitleFiles: [FileRef]   // never nil

    let selection: Selection
    let folder: String?
    let request: RequestView

    let createdAt: Int64
    let startedAt: Int64?
    let finishedAt: Int64?
    let attempt: Int
    let source: SourceRef

    let childrenTotal: Int?
    let childrenDone: Int?
    let childrenError: Int?
    let childrenActive: Int?
    let childrenInline: Bool?

    // Use a JSONDecoder with .convertFromSnakeCase and this whole block disappears.
    enum CodingKeys: String, CodingKey {
        case id, kind, ord, url, title, status, provider, percent, speed, eta
        case phase, msg, error, filename, size, selection, folder, request, attempt, source
        case groupID = "group_id", groupIndex = "group_index", autoStart = "auto_start"
        case downloadedBytes = "downloaded_bytes", totalBytes = "total_bytes"
        case totalBytesEstimate = "total_bytes_estimate"
        case fragmentIndex = "fragment_index", fragmentCount = "fragment_count"
        case phasePercent = "phase_percent"
        case downloadURL = "download_url"
        case chapterFiles = "chapter_files", subtitleFiles = "subtitle_files"
        case createdAt = "created_at", startedAt = "started_at", finishedAt = "finished_at"
        case childrenTotal = "children_total", childrenDone = "children_done"
        case childrenError = "children_error", childrenActive = "children_active"
        case childrenInline = "children_inline"
    }
}

// MARK: - Frames

enum Frame {
    case snapshot(Snapshot)
    case resume(Resume)
    case delta(Delta)
    case added(items: [Item], reason: String, seq: UInt64)
    case completed(items: [Item], seq: UInt64)
    case removed(ids: [String], reason: String, seq: UInt64)   // one case per reason; §5.7
    case subscription(Subscription, seq: UInt64)
    case subscriptionRemoved(ids: [String], seq: UInt64)
    case ytdlOptions(ok: Bool, msg: String, updateTime: Double?, seq: UInt64)
    case providers(ProvidersReport, seq: UInt64)
    case notice(Notice, seq: UInt64)
    case health(Health, seq: UInt64)
    case pong(serverTime: Int64, c: Int64?, seq: UInt64)
    case error(code: String, message: String, seq: UInt64)
    case unknown(type: String, seq: UInt64)
}

// A `delta` patch cannot be decoded into `Item`, because `Item` cannot distinguish
// "key absent" (unchanged) from "key present and null" (cleared). Decode it as a
// dictionary of JSON values and apply key by key.
struct DeltaPatch: Decodable {
    let id: String
    let fields: [String: JSONValue]      // only the keys that were present
    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode([String: JSONValue].self)
        guard case let .string(id)? = raw["id"] else {
            throw DecodingError.dataCorrupted(.init(codingPath: [], debugDescription: "delta without id"))
        }
        self.id = id
        var f = raw; f.removeValue(forKey: "id")
        self.fields = f
    }
}

// `JSONValue` is a 6-case enum (null/bool/int/double/string/array/object). It is used ONLY for
// delta patches, and only because absent-vs-null is meaningful there. Nothing else needs it.
```

The one place the protocol asks for care is the delta patch, and that is inherent to any
changed-fields-only protocol: `Optional<T>` in Swift cannot express three states. Decoding a patch
as `[String: JSONValue]` and applying present keys is ten lines and covers it completely.

---

## 12. Quick reference

| I want to… | Do this |
|---|---|
| add a URL | `POST api/v2/downloads` → `202 {id}`. The item is already visible as `resolving`. |
| add several URLs with one selection | `POST api/v2/downloads` with `{items, defaults}` |
| abort an add that is still resolving (a 500-item playlist) | `POST api/v2/downloads/cancel-resolve` with `{"generation": <the value from the add's 202>}`, or `{}` to abort every in-flight resolution |
| get the whole state | `GET api/v2/state` (polling) or connect the WebSocket (live) |
| refresh cheaply | `GET api/v2/state?since=<seq>&boot=<boot_id>` with `If-None-Match` |
| show live progress | connect `<p>ws`, apply `snapshot` then the frames per §7 |
| start / pause / cancel / retry / delete | `POST api/v2/items/actions` (read the supported set from `capabilities.actions`) |
| delete one item and its file | `DELETE api/v2/items/{id}?delete_file=true` |
| open the produced file | if `item.download_url` is already absolute (it has a scheme) open it as-is; otherwise resolve it against base + prefix |
| stop a download without losing it | `POST api/v2/items/actions` with `"action": "pause"`; `"start"` resumes it |
| page old history | `GET api/v2/items?status=finished,error,canceled&cursor=…` |
| get a large group's children | `GET api/v2/items?group_id=<id>` or a WS `watch` frame |
| build the format picker | `GET api/v2/capabilities` → `formats[]` |
| make the picker honest for a URL | `GET api/v2/catalog?url=<encoded>` |
| know the server's prefix and version | `GET api/v2/capabilities` (`url_prefix`, `version`, `yt_dlp`) |
| see whether the server is healthy | `GET <p>healthz` |
| understand why a download failed | `item.error.code`, then `item.error.message` |
| understand why an option did not apply | `GET api/v2/debug/options?item_id=<id>` |
| receive push notifications | `PUT api/v2/devices/{token}` with the registration body (§4.8) on every launch; `DELETE` the same path to stop |
| drive a Live Activity from the server | `PUT api/v2/devices/{token}/live-activities/{item_id}` with `{"update_token":"<hex>"}`; `DELETE` it when the activity ends |
