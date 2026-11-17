# Protocol v2

This reference describes the current server API. The web UI and the iOS app (private alpha) use
this API. All paths below are relative to the configured server base URL.

## 1. Conventions

### 1.1 Base URL

`URL_PREFIX` defaults to `/` and is normalized to a leading and trailing slash. With
`URL_PREFIX=/aulos/`, the add route is `/aulos/api/v2/downloads` and the socket is `/aulos/ws`.
`GET api/v2/capabilities` reports the effective prefix.

The root route returns HTML when `Accept` includes `text/html` or `text/*`, and a JSON identity
otherwise. `*/*` alone selects JSON. Both representations carry `Vary: Accept`.
`AULOS_WEB_UI=false` makes the root return JSON for every Accept value.

```json
{"name":"aulos-server","version":"dev","url_prefix":"/","protocol":"v2"}
```

### 1.2 Encoding and compatibility

API bodies are JSON. Send `Content-Type: application/json` for JSON request bodies.
Cookie upload uses multipart form data. File routes, static assets, redirects, WebSocket upgrades
and bodyless responses have their own HTTP representations.

Timestamps are integer Unix milliseconds, except `ytdl_options.update_time`, which is epoch
seconds or null. Durations use seconds unless the field name ends in `_ms`. Sizes are bytes and
rates are bytes per second. Numeric fields are JSON numbers.

Full item objects include nullable keys with null values. Delta patches omit unchanged keys.
Unknown request fields are ignored and may produce warnings. Clients should tolerate additional
response fields and unknown enum values.

### 1.3 Headers

| Request header | Meaning |
|---|---|
| `X-Request-Id` | Optional 1 to 64 printable ASCII characters. Invalid values are replaced with a generated ID. |
| `X-Aulos-Client` | `ios/<version>` attributes added items to iOS. Other or missing values use `api_v2`. The client token is case-insensitive. |
| `X-Aulos-Install` | Optional install ID for iOS adds: 8 to 64 characters from `[A-Za-z0-9._-]`, trimmed. Invalid values are treated as absent. |

| Response header | Meaning |
|---|---|
| `X-Request-Id` | Echoed request ID or a generated ULID. |
| `X-Aulos-Seq` | Event hub cursor when the response was produced. It is not a mutation acknowledgment barrier. |
| `ETag` | Cache validator on state, capabilities, catalog, files and static assets. |
| `Retry-After` | `1` on service-unavailable responses. |

### 1.4 Authentication

The API has no user accounts. With neither auth setting configured, requests are open.
`AULOS_API_TOKEN` accepts `Authorization: Bearer <token>`.
`AULOS_TRUSTED_PROXY_AUTH_HEADER` accepts a non-empty value in the configured header.
When both are configured, either grants access. A reverse proxy must control the trusted header.

For browser WebSockets, the token can also be sent through
`Sec-WebSocket-Protocol: aulos.v2, bearer.<token>` or `?token=<token>`.
Avoid putting tokens in URLs when the header option is available.

API routes, WebSocket upgrades and downloaded files use the same auth middleware. Root discovery,
static UI files, the manifest, `robots.txt`, `healthz` and `livez` are public at the server layer.
Aulos does not validate reverse-proxy session cookies. A failed server auth check returns `401`
with a JSON error, without a login redirect.

### 1.5 Errors

API errors use this envelope:

```json
{
  "error": {
    "code": "validation_failed",
    "message": "Invalid quality",
    "field": "quality",
    "provider": null,
    "provider_code": null,
    "request_id": "01JBQ7Z5T9K3M2R8V4XW6Y0AAA"
  }
}
```

`code` and `message` are strings. `field`, `provider` and `provider_code` are nullable strings.
`request_id` matches the response header. `retry_at` is an optional integer Unix-millisecond
recheck time for `not_yet_live` errors.

An item's `error` uses the same fields without `request_id`. Branch on `code`; `message` is display
text. Messages are cleaned and bounded. Redirects, `204`, `304`, and file range responses do not
use this envelope.

### 1.6 Error codes

| Code | HTTP status | Meaning |
|---|---|---|
| `bad_request` | 400 | Invalid body, content type or field encoding. |
| `validation_failed` | 400 | Invalid field value. |
| `unsupported_url` | 400 | URL cannot be handled. |
| `overrides_disabled` | 400 | Request option overrides are disabled. |
| `unknown_preset` | 400 | Preset does not exist. |
| `folder_invalid` | 400 | Invalid or disallowed destination directory. |
| `unauthorized` | 401 | Authentication failed. |
| `not_found` | 404 | Route or record not found. |
| `method_not_allowed` | 405 | Wrong method; see the `Allow` header. |
| `conflict` | 409 | Duplicate subscription or strict-mode duplicate add. |
| `payload_too_large` | 413 | Body, cookie file or URL batch exceeds its limit. |
| `internal` | 500 | Server failure; use the request ID to find logs. |
| `state_unavailable` | 503 | State is unavailable; retry after the indicated delay. |

Cookie files are limited to 1,000,000 bytes. The request body ceiling is 1,065,536 bytes, allowing
multipart overhead. Batch URL limits are configured separately.

Item errors also include `auth_required`, `bot_check`, `geo_restricted`, `unavailable`,
`not_yet_live`, `no_format`, `network`, `throttled`, `postprocessing_failed`, `disk_full`,
`tool_missing`, `provider_degraded`, `timeout`, `canceled` and `contract`.
These describe asynchronous failures after an add has been accepted.

A queued subscription item with `not_yet_live` is waiting for a recording or scheduled release.
Its optional `retry_at` identifies the next check. Unknown or overdue releases recheck after
15 minutes without consuming normal failure retries.

## 2. Items

Full objects use the same shape in REST, snapshots, `added` and `completed`. They contain all
40 fields below. Arrays are never null. `T | null` means the key exists and may contain null.

### 2.1 Identity

Key records by `id`. Sort by `ord`, then `id`. A resolving item can become a group without changing
either value. Treat `added` as an upsert.

### 2.2 Files

Resolve a relative `download_url` against the configured base URL, including its prefix.
Use an absolute URL as supplied. `FileRef` contains `filename` (string), `size` (integer or null),
`download_url` (string or null) and `lang` (string or null).

### 2.3 Fields

| Key | Type | Notes |
|---|---|---|
| `id` | `string` | Immutable ULID; remains unchanged when an item becomes a group. |
| `kind` | `"item"` \| `"group"` | Groups contain playlist, channel or season entries and produce no file themselves. |
| `ord` | `integer` | Persistent sort key. Sort ascending, then by `id`. |
| `group_id` | `string \| null` | Parent group ID, or null for a top-level record. |
| `group_index` | `integer \| null` | One-based child position. |
| `url` | `string` | Submitted source URL. |
| `title` | `string` | The URL until a title is resolved. |
| `status` | `Status` | One of the eight statuses in section 3. |
| `auto_start` | `boolean` | For queued items, false means paused. |
| `provider` | `string \| null` | Provider ID, such as `ytdlp`, `streamingcommunity` or `command:bandcamp`. |
| `percent` | `number` | 0 before progress, 100 when finished. May decrease between video and audio phases. |
| `speed` | `number \| null` | Bytes per second. |
| `eta` | `integer \| null` | Seconds remaining. |
| `downloaded_bytes` | `integer \| null` | Bytes downloaded. |
| `total_bytes` | `integer \| null` | Known total size. |
| `total_bytes_estimate` | `integer \| null` | Estimated total size. |
| `fragment_index` | `integer \| null` | Current segment or fragment. |
| `fragment_count` | `integer \| null` | Total segments or fragments. |
| `phase` | `string \| null` | Display label such as `video`, `audio`, `remux` or `audio_sync`; allow unknown values. |
| `phase_percent` | `number \| null` | Progress within the postprocessing phase. |
| `msg` | `string \| null` | Live status text. Cleared at terminal transitions; imported error/canceled rows can retain a terminal note. |
| `error` | `WireError \| null` | Wire error without `request_id`. Can also appear on queued `not_yet_live` items. |
| `filename` | `string \| null` | Primary output path relative to its download root, when available. |
| `size` | `integer \| null` | Output size in bytes. |
| `download_url` | `string \| null` | Encoded absolute URL or a path relative to the configured server base URL. |
| `chapter_files` | `[FileRef]` | Chapter outputs; empty array when absent. |
| `subtitle_files` | `[FileRef]` | Subtitle outputs; empty array when absent. |
| `selection` | `Selection` | Download type, codec, format and quality. |
| `folder` | `string \| null` | Relative custom directory. |
| `request` | `RequestView` | Original request options. Secrets in overrides are redacted. |
| `created_at` | `integer` | Unix milliseconds. |
| `started_at` | `integer \| null` | First preparation time in Unix milliseconds. |
| `finished_at` | `integer \| null` | Terminal transition time in Unix milliseconds. |
| `attempt` | `integer` | Starts at 0 and increases on retry. |
| `source` | `SourceRef` | Original attribution; preserved through retries and restarts. |
| `children_total` | `integer \| null` | Group child count. |
| `children_done` | `integer \| null` | Finished child count. |
| `children_error` | `integer \| null` | Failed child count. |
| `children_active` | `integer \| null` | Preparing, downloading or postprocessing child count. |
| `children_inline` | `boolean \| null` | True on groups in this build, null on items. Older terminal children can still fall outside the done window; page them by `group_id`. |

`Selection` has four fields: `download_type`, `codec`, `format` and `quality`.
Download types are `video`, `audio`, `captions` and `thumbnail`. Codecs are `auto`, `h264`,
`h265`, `av1` and `vp9`. Format and quality values come from the catalog.

`RequestView` contains these nine fields:

| Field | Type | Meaning |
|---|---|---|
| `custom_name_prefix` | string | Filename prefix; empty when unset. |
| `playlist_item_limit` | integer | Requested entry limit; 0 means unlimited. |
| `auto_start` | boolean | Original requested value. |
| `split_by_chapters` | boolean | Whether separate chapter files were requested. |
| `chapter_template` | string | Effective chapter template; never null. |
| `subtitle_language` | string | Requested language tag. |
| `subtitle_mode` | string | Requested subtitle preference. |
| `ytdl_options_presets` | array of strings | Selected preset names; never null. |
| `ytdl_options_overrides` | object | Requested overrides with secret values redacted. |

`selection`, `folder` and `request` are immutable and do not appear in delta patches.
`Item.auto_start` can change; `request.auto_start` records the original request.

`source` contains `kind` and nullable `ref`. Kinds are `api_v2`, `ios`, `telegram`, `subscription`,
plus stored historical values `api_v1`, `restart` and `retry`. A ref can identify the chat,
subscription or iOS installation. Attribution is preserved on retry and recovery.

### 2.4 Display rules

Use `msg` for live status and `error.message` for a failure. Apply explicit nulls when clearing
those values. Progress may decrease when a job moves from video to audio. `phase_percent` is a
separate postprocessing indicator. Group progress and counts are computed by the server.

## 3. Status

### 3.1 Values

| Status | Meaning |
|---|---|
| `queued` | Waiting for a slot, scheduled recheck or user action. |
| `resolving` | Extracting metadata. |
| `preparing` | Preparing the download process. |
| `downloading` | Download in progress. |
| `postprocessing` | Processing downloaded media. |
| `finished` | Successful terminal result; percent is 100 and live message is null. |
| `error` | Terminal failure with an error object. |
| `canceled` | Canceled terminal result. |

Groups have no primary file even when finished. Do not require a non-null filename for every
finished record. Decode unknown statuses without treating them as queued.

### 3.2 Transitions

A typical item goes from resolving to queued, preparing, downloading, postprocessing and finished.
Failures and cancellation can interrupt that path. Retries can return a terminal item to the queue.

Pause stops work and sets `queued` with `auto_start=false`, keeping resumable partial files.
Start sets `auto_start=true`. Neither requires a separate paused status. Group expansion upserts
the provisional item as a group and adds children.

### 3.3 Groups

Group status is derived in this order:

1. `downloading` if any child is preparing, downloading or postprocessing.
2. `queued` if any child is queued or resolving.
3. `error` if any child failed.
4. `canceled` if all children are terminal and at least one was canceled.
5. `finished` otherwise.

Progress is byte-weighted when totals are available and count-weighted otherwise. Speed sums
active child speeds. ETA uses remaining bytes and speed when available. Read these aggregates
from the group instead of calculating them in the client.

## 4. REST

### 4.1 Add downloads

`POST api/v2/downloads` accepts a single object or a batch. Only `url` is required for a single
add. Acceptance returns before metadata extraction completes.

```json
{"url":"https://www.youtube.com/watch?v=aqz-KE-bpKQ","download_type":"video","format":"mp4","quality":"1080"}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `url` | string | **required** | trimmed |
| `download_type` | string | `"video"` | See section 8 |
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

Batch fields override shared defaults:

```json
{
  "items": [{"url":"https://example.org/a"},{"url":"https://example.org/b"}],
  "defaults": {"format":"mp4","quality":"1080"}
}
```

The `202` response contains:

| Field | Type | Meaning |
|---|---|---|
| `id` | string | First new ID, or first existing duplicate ID. |
| `ids` | array of strings | Newly accepted IDs in request order. |
| `generation` | integer | Resolution generation for cancel-resolve. |
| `seq` | integer | Response cursor hint; see section 6.4. |
| `duplicates` | array | Objects with `url` and `existing_id`. |
| `warnings` | array of strings | Ignored request fields and other validation warnings. |

The default dedupe mode returns an existing active item with the same URL and selection.
An entirely deduplicated batch has an empty `ids` array. `strict` mode can return `409`.

The accepted row is persisted before acknowledgment. Its published snapshot and `added` event
follow asynchronously. Reconcile by ID; do not require the row to appear in the first state read.

For iOS attribution, send `X-Aulos-Client: ios/<version>` and the install header. The same install
ID belongs in device registration. Other clients use `source.kind=api_v2`.

### 4.2 Actions

`POST api/v2/items/actions`:

```json
{"action":"cancel","ids":["01JBQ7Z5T9K3M2R8V4XW6Y0AAA"]}
```

| Action | Effect |
|---|---|
| `start` | Start queued/resolving work; retry failed or canceled work. Finished items are skipped. |
| `pause` | Stop and park eligible work, keeping partial files. Resolving and terminal items are skipped. |
| `cancel` | Stop eligible work, remove scratch files and mark canceled. |
| `retry` | Requeue an eligible failed or canceled item and increase its attempt count. |
| `delete` | Remove the record, canceling active work first. Optionally remove its files. |

For delete, `delete_file` overrides `DELETE_FILE_ON_TRASHCAN`. Send an explicit false to preserve
files. Deleting a group deletes its children. Archived history IDs remain valid action targets.

The `200` response has `applied` IDs, `skipped` objects with `id` and `reason`, and `seq`.
Skip reasons are `not_found`, `already_terminal`, `not_cancelable`, `not_startable`,
`not_retryable` and `not_pausable`.

`DELETE api/v2/items/{id}?delete_file=false` is the single-item form and returns `204`.

### 4.3 State

`GET api/v2/state` returns a snapshot. It contains:

| Field | Type | Meaning |
|---|---|---|
| `mode` | string | `snapshot`. |
| `seq` | integer | Published state cursor. |
| `boot_id` | string | Server process ID. |
| `server_time` | integer | Unix milliseconds. |
| `protocol` | object | `batch_ms`, `urgent_ms`, `replay_frames`, `delta_semantics`. |
| `counts` | object | Counts by each of the eight statuses in the published window. |
| `done_total` | integer | Total terminal records, including older history. |
| `truncated` | object | `done` indicates older history; `groups` is empty in this build. |
| `items` | array of Item | Active records and group records. |
| `done` | array of Item | Recent terminal window. |
| `subscriptions` | array of Subscription | Current subscriptions. |
| `ytdl_options` | object | `ok`, `msg`, `update_time`. |
| `health` | object | Roll-up `status` and component-name to status map. |

`done=false` omits terminal window entries, but `counts` still describes the published window.
Use `GET api/v2/items?status=<status>` and its `total` for database-wide status counts.

`GET api/v2/state?since=<seq>&boot=<boot_id>` returns one of:

- `snapshot`: the cursor cannot be resumed.
- `up_to_date`: the cursor equals the head; the body also has `seq` and `boot_id`.
- `delta`: the body has `seq`, `boot_id`, `from`, `added`, `completed`, `removed` and
  `delta: {items: [...]}`.

For a delta response, apply added items, completed items, removal groups, then patches.
`removed` is an array of `{ids, reason}` objects. REST catch-up folds item changes; fetch
subscriptions, options and health separately when polling those surfaces.

The state ETag is `W/"<boot_id>-<seq>"` for that response's cursor.
An unchanged `If-None-Match` request returns `304` with no body.

### 4.4 Items and history

`GET api/v2/items` accepts `status` (comma-separated), `kind`, `group_id`, `q` (title substring),
`order` (`ord`), `limit` (default 200, maximum 1000), and opaque `cursor`.

The response contains `items`, nullable `next_cursor`, database-wide `total`, and `seq`.
Continue until `next_cursor` is null. Use `group_id` to fetch children beyond the terminal window.

`GET api/v2/items/{id}` returns a full item or `404`.
`GET api/v2/items/{id}/file` returns a `302` file redirect or `404` if no output is known.

### 4.5 Capabilities

`GET api/v2/capabilities` returns server identity, configuration for clients and discovery data.
It supports ETag validation.

| Field | Contents |
|---|---|
| `version`, `yt_dlp` | Server version and nullable yt-dlp version. |
| `url_prefix`, `boot_id` | Effective prefix and current process ID. |
| `protocol` | `v2`, `ws_path`, `ws_subprotocol`, `batch_ms`, `urgent_ms`, `delta_semantics`. |
| `features` | Supported feature names. |
| `actions` | `start`, `pause`, `cancel`, `retry`, `delete`. |
| `formats` | Flat `{id, text, download_type, qualities: [{id, text}]}` entries. |
| `download_types`, `codecs`, `subtitle_modes` | Supported values. |
| `presets` | Preset names. |
| `providers` | Provider summaries, including state and degradation reason. |
| `config` | Public request defaults and UI settings listed below. |

Config keys are `custom_dirs`, `create_custom_dirs`, `allow_ytdl_options_overrides`,
`default_option_playlist_item_limit`, `subscription_default_check_interval`,
`output_template_chapter`, `public_host_url`, `public_host_audio_url`, `default_theme`,
`max_concurrent_downloads`, `delete_file_on_trashcan`, `clear_completed_after`,
`default_download_type`, `default_format` and `default_quality`.

Formats are assembled from registered catalogs. Use the per-URL catalog when the URL is known.

### 4.6 Catalog

`GET api/v2/catalog` returns a merged catalog. Add `?url=<encoded-url>` to select the matching
provider. An unmatched URL gets the merged catalog; malformed URLs are rejected.

| Field | Type |
|---|---|
| `etag` | string |
| `provider` | provider ID or `merged` |
| `match` | `{score: integer, reason: string}` or null |
| `runner_up` | `{provider: string, score: integer}` or null |
| `naming` | `template` or `provider` |
| `download_types` | array of DownloadTypeSpec |

Match reasons include `host_contains`, `host_regex`, `path_regex`, `forced` and `fallback`.
`naming=provider` means the provider controls filenames instead of output templates.

| Type | Fields |
|---|---|
| DownloadTypeSpec | `id`, `label`, `default_format`, `formats`, `options` |
| FormatSpec | `id`, `label`, `default_quality`, `qualities`, `codecs`, nullable `notice`, `flags` |
| QualitySpec | `id`, `label`, nullable `notice` |
| FormatFlags | Boolean `advisory`, `requires_ffmpeg`, `lossy_remux`, `slow` |
| OptionSpec | `id`, `label`, `kind`, `default`, `choices`, nullable `help` |

IDs and labels are strings. Collections are arrays. `codecs` and `choices` contain `{id, label}`
objects. Empty codecs mean the selection uses `auto`. Default format and quality values name
entries in the corresponding arrays.

Option kinds use a `type` discriminator: `bool`, `int` with `min` and `max`, `enum`, `text` with
nullable regex `pattern`, or `path`. Option IDs are add-request field names. Unknown option kinds
can be omitted because the server supplies defaults. Display relevant notices and flags.

### 4.7 Other routes

| Method | Path | Input | Success |
|---|---|---|---|
| GET | `healthz` | Optional `probe=deep` | Component health; 503 if the store is unusable. |
| GET | `livez` | None | `200 {"ok":true}`. |
| GET | `api/v2/subscriptions` | None | `200 {"subscriptions":[...]}`. |
| POST | `api/v2/subscriptions` | Add options, optional name and interval | `201 Subscription`. |
| PATCH | `api/v2/subscriptions/{id}` | `name`, `enabled`, `check_interval_minutes` | `200 Subscription`. |
| DELETE | `api/v2/subscriptions/{id}` | None | `204`. |
| POST | `api/v2/subscriptions/check` | `{}` or `{"ids":[...]}` | `202 {"job_id":"...","count":1}`. |
| POST | `api/v2/subscriptions/{id}/check` | None | `202` with job ID and count. |
| POST | `api/v2/downloads/cancel-resolve` | Optional `generation` | `200` with `canceled`, `generation`, `seq`. |
| GET | `api/v2/presets` | None | `200 {"presets":[...]}`. |
| GET | `api/v2/custom-dirs` | None | Download and audio directory arrays; 404 if disabled. |
| GET | `api/v2/cookies` | None | `has_cookies`, `bytes`, `updated_at`. |
| POST | `api/v2/cookies` | Multipart field `cookies` | `200` with `has_cookies` and `bytes`. |
| DELETE | `api/v2/cookies` | None | `204`. |
| GET | `api/v2/ytdl-options` | None | `ok`, `msg`, `update_time`, `keys`, `presets`. |
| POST | `api/v2/ytdl-options/reload` | None | `200` with `ok`, `msg`, `update_time`. |
| GET | `api/v2/import-report` | None | Stored import report, or 404 if absent. |
| POST | `api/v2/items/clear` | `{}` or `where: "done"`; optional `delete_file` | `200` with removed IDs, `seq`, `warnings`. |
| GET | `api/v2/providers` | None | Provider details and scan warnings. |
| POST | `api/v2/plugins/reload` | None | `added`, `updated`, `removed`, `failed`, `warnings`. |
| GET | `api/v2/resolve-preview` | `url` query | Provider match and runner-up. |
| GET | `api/v2/debug/options` | Optional `item_id` query | Layered user options with values and source labels. |

Cancel-resolve without a generation cancels all active resolution. It does not delete items
already created. Clear removes terminal history, including rows outside the published window.
Send `delete_file=false` to preserve files. The server default otherwise applies.

Debug options are redacted and describe user-option layering. They are not the complete final
provider or shim invocation. Provider reloads affect new provider jobs; hooks require restart.

File routes are `download/*` and `audio_download/*`. They support `Range`, `If-Range`, ETag and
Last-Modified. An unsatisfiable single range returns `416` with `Content-Range: bytes */<size>`.
Malformed and multiple ranges are ignored and return the full file. Content outside the inline
media allowlist is served as an attachment.

The root, `robots.txt`, `assets/*` and `manifest.webmanifest` are described in sections 1.1 and
1.4. UI assets return `404` when the UI is disabled. `/metrics` always returns `404`.

### 4.8 Devices

These routes support push for the iOS app (private alpha). All require normal API authentication.
Successful calls return `204` without a body.

| Method | Path | Input |
|---|---|---|
| PUT | `api/v2/devices/{token}` | Registration below. |
| DELETE | `api/v2/devices/{token}` | None; also removes that device's activities. |
| PUT | `api/v2/devices/{token}/live-activities/{item_id}` | `{"update_token":"<hex>"}`. |
| DELETE | `api/v2/devices/{token}/live-activities/{item_id}` | None. |

Registration fields:

| Field | Type | Rule |
|---|---|---|
| `platform` | string | Required; `ios`. |
| `bundle_id` | string | Required; `APNS_TOPIC` or an extension under that prefix. |
| `environment` | string | Required; `sandbox` or `production`, matching the token's gateway. |
| `alerts` | boolean or null | Defaults to true. |
| `live_activity_start_token` | string or null | Push-to-start token. Null clears it. |
| `install_id` | string or null | Same format as `X-Aulos-Install`; invalid values are rejected. |
| `app_version` | string or null | At most 64 characters. |

Tokens contain 32 to 200 hexadecimal characters and are normalized to lowercase. Item IDs must
be ULIDs but need not exist yet. Register the device before its activity; an unknown device on
activity PUT returns `404`. Deletes are idempotent.

PUT replaces device registration fields while preserving its first-seen time. Unknown fields are
ignored. Device and activity tokens reported invalid by APNs are pruned.

By default, iOS-origin items target devices with a matching install ID. A null item install ID
allows all eligible devices. `APNS_PUSH_ALL` widens routing. Existing activities are updated and
ended regardless of origin filters. See [push setup](integrations.md#ios-push-notifications).

Live Activity `content-state` uses these seven keys:

```json
{"status":"downloading","percent":42.5,"speed":2100000.0,"eta":68,"downloadedBytes":123,"totalBytes":456,"message":null}
```

`status` is a string and `percent` is a number. Speed, ETA, byte counts and message may be null.
Start attributes use `AulosDownloadAttributes` with `itemId`, `url` and `title`.
Update envelopes set `stale-date` to 45 seconds ahead and `relevance-score` to percent divided by
100. Progress updates use the current published snapshot. End pushes include a dismissal date.

## 5. WebSocket

### 5.1 Connecting

Connect to `<base>/ws` with subprotocol `aulos.v2`. Use `since` and `boot` together to resume.
`done=false` omits the terminal window from snapshots. The `groups` parameter is accepted but
ignored. Authentication is covered in section 1.4.

Server application frames are JSON text with `t` and integer `seq`. The server sends transport
pings every 20 seconds and closes after 60 seconds without a pong or data. Compression is not
negotiated.

Close codes include `1001` for shutdown, `1013` for overload or a slow consumer, and `1009` for a
client frame over 1 MiB. Reconnect with backoff.

### 5.2 Frame types

| Type | Payload |
|---|---|
| `snapshot` | Current state. |
| `resume` | Summary of the catch-up frames that follow. |
| `added` | Full item upserts and a reason. |
| `completed` | Full terminal item upserts. |
| `removed` | IDs and a removal reason. |
| `delta` | Changed item fields. |
| `subscription` | Full subscription upsert. |
| `subscription_removed` | Subscription IDs. |
| `ytdl_options` | Options reload status. |
| `providers` | Provider reload report. |
| `notice` | Warning or information. |
| `health` | Component status changes. |
| `pong` | Echoed ping value and server time. |
| `error` | Connection protocol error. |

Broadcast frames allocate sequence numbers. Snapshots, resume summaries, folded catch-up frames,
pong and error frames can reuse a cursor. Do not discard a frame just because its cursor equals
the preceding one.

### 5.3 Snapshot

A WebSocket snapshot uses the state fields from section 4.3 with `t: "snapshot"` and without
`mode`. It also includes `server` identity. Replace local item and subscription state when it
arrives, including after a lag resync.

Active group children are included. `truncated.groups` is always empty in this build. Terminal
history is still bounded by `AULOS_MEM_DONE_ITEMS` and is paged through REST.

### 5.4 Delta

```json
{"t":"delta","seq":42,"ts":1794900000000,"items":[{"id":"01JBQ7Z5T9K3M2R8V4XW6Y0AAA","percent":43.9,"msg":null}]}
```

Find the item by ID and replace only present keys. An explicit null clears the value. Ignore
patches for unknown IDs. A delta never creates a record and never changes immutable request
fields. Numeric changes use `batch_ms`; text and status changes use the urgent cadence. An idle
server emits no deltas.

### 5.5 Added

An `added` frame has `items` containing full objects and a `reason`: `created`, `expanded`,
`retried` or `recovered`. Upsert by ID regardless of reason. Group promotion preserves the ID and
order of the provisional item and arrives as an upsert, without a removal.

### 5.6 Completed

A `completed` frame has `items` containing full objects with `finished`, `error` or `canceled`
status. Upsert them. Read status to determine the outcome.

### 5.7 Removed

```json
{"t":"removed","seq":43,"ids":["01JBQ7Z5T9K3M2R8V4XW6Y0AAA"],"reason":"deleted"}
```

Reasons are `deleted`, `cleared`, `auto_cleared` and `group_cascade`. A flush can contain one frame
for each reason, in that order. Remove IDs that exist locally and ignore unknown IDs.

### 5.8 Notice

Notice fields are `level`, `code`, nullable item `id`, and `message`. Levels are `info`, `warning`
and `error`. Codes include `stalled`, `job_timeout`, `pot_down`, `provider_degraded`,
`import_warning` and `plugin_note`. Notices do not change item state. Allow new codes.

### 5.9 Other broadcast payloads

| Type | Fields besides `t` and `seq` |
|---|---|
| `subscription` | `subscription`: full object from section 9. Upsert by ID. |
| `subscription_removed` | `ids`: array of subscription IDs. |
| `ytdl_options` | `ok`, `msg`, `update_time` (epoch seconds or null). |
| `providers` | `added`, `updated`, `removed`, `failed`, and scan warnings when included. Refresh catalogs. |
| `health` | Roll-up `status`, plus `changed` entries with `component`, `from`, `to`, `detail`. |

Component health values are `ok`, `degraded`, `down`, `starting` and `disabled`.
Starting and disabled components do not worsen the roll-up. Health frames are emitted on changes.

### 5.10 Pong and error

```json
{"t":"pong","seq":44,"server_time":1794900000000,"c":123}
```

`c` echoes the JSON value from the client ping. An error frame has `code` and `message`, for example
`bad_frame` for an unknown application frame.

### 5.11 Client frames

| Frame | Behavior |
|---|---|
| `{"t":"ping","c":123}` | Replies with pong. |
| `{"t":"resume","since":42,"boot":"01JBQ8YQ2E0000000000000000"}` | Requests catch-up or a fresh snapshot. |
| `{"t":"hello","client":"example/1"}` | Logs client identity. Topics do not filter delivery. |
| `ack`, `watch`, `unwatch` | Accepted but ignored. |

Use REST for mutations. A read-only socket client needs no application messages.

## 6. Cursors and resume

### 6.1 Identifiers

`seq` is a durable frame cursor. Allocation can leave gaps. `boot_id` identifies one server
process. `ord` orders records and is unrelated to the frame cursor.

### 6.2 Resume conditions

Reconnect with the last fully applied `(seq, boot_id)`. The server sends a snapshot when either
value is missing or unusable, the boot differs, or the cursor is outside the retained replay
window, including a cursor above the current head.

### 6.3 Catch-up

```json
{"t":"resume","seq":50,"from":42,"to":50,"merged":{"added":2,"completed":1,"removed":1,"delta_items":3}}
```

The server sends folded item frames in this order: added, completed, removed groups, delta.
Last field values win. It then replays other retained event kinds, such as subscription and health
changes. Folded frames share the `to` cursor. `merged.removed` counts IDs, not frames.

Apply every frame in order. Keep the previous durable cursor until the catch-up is fully applied.
The summary counts cover item changes, not the other event kinds, and there is no separate end
marker. If a connection drops during catch-up and the client cannot prove it applied the whole
window, reconnect from the previous cursor or request a fresh snapshot.

### 6.4 Mutation correlation

Response `seq` fields and `X-Aulos-Seq` are cursor samples, not transaction IDs. An unrelated event
can advance the cursor and publication can follow acknowledgment. Reconcile mutations using their
item IDs and resulting state. A numeric cursor comparison alone does not prove a mutation has
appeared in the stream.

## 7. Applying updates

1. Replace item and subscription maps on a snapshot. Store its boot ID and cursor.
2. Upsert full items from added and completed frames.
3. Remove records by ID.
4. Apply present keys from delta patches, including nulls, to known records only.
5. Upsert or remove subscriptions by their IDs.
6. Apply options, provider and health changes independently.
7. Save only a cursor whose associated changes have been applied. Handle catch-up as section 6.3
   describes.
8. Render items ordered by `ord`, then `id`.

For scheduled work, check `queued` plus `error.code=not_yet_live` and optional `retry_at`.
`auto_start=false` means the record remains paused even if it has a scheduled-release error.

## 8. Format and quality selection

Use capabilities for initial choices and the catalog to refine them for a URL. The built-in
yt-dlp catalog includes:

| Download type | Format | Quality |
|---|---|---|
| video | any | best, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| video | mp4 | best, best_remux, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| video | ios | best, 2160, 1440, 1080, 720, 480, 360, 240, worst |
| audio | m4a | best, 192, 128 |
| audio | mp3 | best, 320, 192, 128 |
| audio | opus, wav, flac | best |
| captions | srt, txt, vtt, ttml, sbv, scc, dfxp | best |
| thumbnail | jpg | best |

Non-video selections use codec `auto`. The `ios` format is a video encoding preset and does not
require the iOS app. `best_remux` may re-encode audio and adds postprocessing. The current `worst`
selector resolves to the best available stream; the catalog includes a notice for this behavior.
StreamingCommunity advertises one source rendition. Read notices instead of assuming every
provider can honor every quality choice.

## 9. Subscriptions

Subscriptions appear in REST responses, snapshots and subscription frames. Their fields are:

| Key | Type | Notes |
|---|---|---|
| `id` | `string` | a ULID for new subscriptions; imported ones keep their original UUID string. Treat as opaque. |
| `name` | `string` | |
| `url` | `string` | unique across subscriptions |
| `enabled` | `boolean` | |
| `check_interval_minutes` | `integer` | minimum 1 |
| `download_type`, `codec`, `format`, `quality` | `string` | the selection applied to new items |
| `folder` | `string` | `""` means the base directory |
| `last_checked` | `integer \| null` | unix **ms** |
| `seen_count` | `integer` | how many media ids have been marked seen |
| `error` | `string \| null` | the last check's error text; cleared by a successful check |
| `next_due` | `integer \| null` | unix ms of the next scheduled check |
| `consecutive_failures` | `integer` | drives exponential backoff, up to 6 hours |
| `checking` | `boolean` | a check is running right now |

Creation accepts add options plus an optional name and `check_interval_minutes`. It checks that
the URL represents a channel or playlist. A single-video URL returns `400 validation_failed`;
a duplicate subscription URL returns `409 conflict`.

Existing visible entries are marked seen instead of downloaded, except upcoming livestreams.
Later checks add unseen entries. A check request returns `202`; observe `checking`, `last_checked`
and `error` through subscription frames or REST. Failed checks use bounded backoff.

YouTube subscription checks skip Shorts. Live broadcasts wait for a recording before downloading.
These restrictions do not apply to explicit downloads.

## 10. Supported surface

Only v2 routes are supported. MeTube v1 routes, `/version` and `/socket.io/` return `404`.
Raw Authorization tokens are not accepted; use the Bearer scheme.
Existing stored source values remain decodable. Legacy JSON can be imported through the explicit
[CLI procedure](operations.md#importing-metube-state).

## 11. Client checklist

- Use the configured prefix for REST, WebSocket and relative file URLs.
- Keep item identity separate from source URLs.
- Preserve the distinction between omitted and null delta values.
- Handle snapshots after reconnect even when resume was requested.
- Treat full objects as upserts and unknown enum values as unsupported values.
- Use explicit file-deletion flags in destructive API calls.

## 12. Reference sources

The route definitions are in [v2/mod.rs](../crates/aulos-api/src/v2/mod.rs).
[ItemView](../crates/aulos-core/src/item.rs) defines full item fields.
[ws.rs](../crates/aulos-api/src/ws.rs) handles connections and client messages.
[API tests](../crates/aulos-api/tests) cover requests, authentication and WebSocket behavior.
