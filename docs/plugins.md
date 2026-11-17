# Plugins

A plugin is a directory containing `plugin.toml`. Put it under `AULOS_PLUGINS_DIR`, normally
`/config/plugins`. The directory name must match `^[a-z0-9][a-z0-9_-]{0,31}$` and becomes the
provider ID `command:<directory>`.

A manifest can declare a provider, hooks, or both. Providers need `[match]` and `[download]`.
Hook-only plugins need one or more `[[hook]]` tables.

Providers load at startup and can reload through `SIGHUP`, file watching or
`POST api/v2/plugins/reload`. Running jobs retain their provider instance. **Hook definitions load
at startup and require a restart after changes.**

Plugins run as the server user and have the same filesystem and network access. Install only
plugins you trust. Process groups, resource limits and output limits constrain execution; they
do not sandbox a plugin.

## Examples

- [Bandcamp provider](../plugins/examples/bandcamp/plugin.toml): resolution, output files,
  progress parsing and a custom catalog, with Python scripts.
- [Media server hooks](../plugins/examples/media-server-hooks/plugin.toml): Plex, Emby, ntfy
  and a local script. Configure the services and credentials before installing it.

The provider example is exercised against a local HTTP stub by
[plugin_example.rs](../crates/aulos-provider/tests/plugin_example.rs).

## Provider manifest

Defaults and validation are implemented in
[manifest.rs](../crates/aulos-provider/src/command/manifest.rs). Required entries are marked in
the table. Unknown template names and invalid regexes fail validation.

| Key | Type | Req | Default | Meaning |
|---|---|---|---|---|
| `manifest_version` | int | ✓ | | must be `1`; otherwise `Degraded("unsupported manifest_version")` |
| `name` | string | ✓ | | display name |
| `version` | string | ✓ | | shown to clients |
| `description` / `homepage` | string | | `""` | shown in the catalog |
| `authors` | [string] | | `[]` | |
| **`[match]`** | | provider | | at least one of `hosts` / `host_regex` |
| `match.hosts` | [string] | | `[]` | host suffix match, case-insensitive; score 100 |
| `match.host_regex` | string | | | anchored regex over the normalised host; score 150 |
| `match.path_regex` | string | | | if set **and** it matches, the score is promoted to 250 |
| `match.exclude_path_regex` | string | | | veto → `Match::No` |
| `match.schemes` | [string] | | `["http","https"]` | |
| `match.priority` | int 0..=255 | | derived | overrides the derived score |
| **`[capabilities]`** | | | | |
| `capabilities.resolve` | bool | | `false` | `false` → a synthetic single `Video` entry from the URL |
| `capabilities.playlists` | bool | | `false` | |
| `capabilities.streaming_resolve` | bool | | `true` when `resolve` | resolve stdout is read line-by-line and children stream out |
| `capabilities.subtitles` / `chapters` / `thumbnails` / `nfo_capable` | bool | | `false` | catalog hints |
| `capabilities.cancel` | enum | | `"process_group"` | `process_group` \| `cooperative` \| `none` |
| **`[limits]`** | | | | |
| `limits.max_concurrent` | int 1..=32 | | `1` | per-provider semaphore |
| `limits.uses_global_slot` | bool | | `true` | `false` → `own_slots()` only, like SC |
| `limits.max_concurrent_resolves` | int | | `1` | |
| `limits.min_request_interval_ms` | int | | `0` | minimum delay between spawns |
| `limits.resolve_timeout_secs` | int | | `60` | |
| `limits.download_stall_secs` | int | | `600` | no progress → kill |
| `limits.download_hard_timeout_secs` | int | | `0` (off) | |
| `limits.max_output_bytes` | int | | `67108864` | cumulative stdout+stderr budget; over → kill + `contract` |
| `limits.memory_bytes` | int | | `0` (off) | `RLIMIT_AS` |
| `limits.cpu_secs` | int | | `0` (off) | `RLIMIT_CPU` |
| `limits.file_size_bytes` | int | | `0` (off) | `RLIMIT_FSIZE`, per-file byte limit |
| `limits.nofile` | int | | `1024` | `RLIMIT_NOFILE` |
| **`[resolve]`** | | when `capabilities.resolve` | | |
| `resolve.command` | [string] | ✓ | | argv; element 0 resolved against the plugin dir, then `PATH` |
| `resolve.format` | enum | | `"json_lines"` | `json_lines` \| `json` |
| `resolve.stdin` | enum | | `"none"` | `none` \| `json` (the whole request as one JSON line) |
| `resolve.cwd` | string | | plugin dir | |
| **`[download]`** | | provider | | |
| `download.command` | [string] | ✓ | | argv template |
| `download.cwd` | string | | plugin dir | |
| `download.stdin` | enum | | `"none"` | `none` \| `json` |
| `download.expect_output` | enum | | `"path_template"` | `path_template` \| `result_frame` \| `newest_in_dir` |
| `download.output_ext` | string | | `"mp4"` | used by `path_template` and `{out_path}` |
| `download.overwrite` | bool | | `true` | |
| **`[progress]`** | | | `kind = "none"` | |
| `progress.kind` | enum | | `"none"` | `json_lines` \| `regex` \| `none` |
| `progress.source` | enum | | `"stdout"` | `stdout` \| `stderr` \| `both` |
| `progress.strip_ansi` | bool | | `true` | |
| `progress.cr_as_newline` | bool | | `true` | Spectre.Console / ffmpeg repaints |
| `progress.last_match_wins` | bool | | `true` | per read chunk |
| `progress.min_interval_ms` | int | | `250` | source-side rate cap |
| `progress.patterns` | [string] | ✓ if `regex` | | named groups from `{percent, downloaded, total, speed, eta, status, fragment_index, fragment_count, msg}` |
| `progress.units` | table | | `{}` | e.g. `speed = "auto"`, `eta = "hms"`; `auto` parses `KB/KiB/MB/MiB/GB/GiB`, **1024-based** (matches N_m3u8DL-RE) |
| `progress.status_map` | table | | `{}` | captured `status` text → Aulos `Stage`/terminal |
| **`[env]`** | | | | |
| `env.pass` | [string] | | `[]` | env var **names** inherited from the server process |
| `env.set` | table | | `{}` | literal env; values accept the same templates as argv and `${VAR}` |
| **`[headers]`** | | | `{}` | name → template; exposed as `AULOS_HEADER_<NAME>` and via `{headers_curl}` / `{headers_crlf}` |
| **`[catalog]`** | | | derived | Absent → a single `video`/`mp4`/`best` type with a notice about unsupported quality selection |
| **`[[hook]]`** | array of tables | | `[]` | See Hooks below |

A host match is required. `path_regex` raises the score when it matches; it is not a restriction
on other paths. Use `exclude_path_regex` to reject paths. Highest score wins; an explicit
`priority` overrides the computed score.

## Provider templates

Templates substitute one argument at a time. No shell is used unless the manifest explicitly
starts one. `${VAR}` reads a server environment value at load time. `{{` and `}}` escape braces.
An unknown token is a validation error.

| Token | Value |
|---|---|
| `{url}`, `{url_host}`, `{url_path}`, `{url_query}` | the item URL and its parsed parts |
| `{media_id}`, `{title}` | from the resolved entry |
| `{out_dir}`, `{tmp_dir}` | absolute, already created |
| `{out_name}` | sanitised basename without extension (prefix + template-resolved) |
| `{out_path}` | `{out_dir}/{out_name}.{output_ext}` |
| `{output_ext}` | from the manifest |
| `{download_type}`, `{format}`, `{quality}`, `{codec}` | the selection |
| `{subtitle_language}`, `{subtitle_mode}` | the selection |
| `{state}` | the entry's provider state as compact JSON |
| `{state.<key>}` | one field of it, JSON-scalar-stringified (only when `capabilities.resolve`) |
| `{playlist_index}`, `{playlist_count}`, `{playlist_title}` | when the item is in a group; empty otherwise |
| `{cookies_file}` | `STATE_DIR/cookies.txt` if present, else `""` |
| `{headers_curl}` / `{headers_crlf}` | `-H "K: V"` argv pairs / a `K: V\r\n` blob (ffmpeg style) |
| `{plugin_dir}` | the plugin's own directory |

`{headers_curl}` expands to header arguments when used as a whole argv element. Other template
values stay within their argument. A token whose optional value is unavailable expands to an
empty string.

## Resolution output

With `resolve.format = "json_lines"`, write one JSON object per line to stdout:

```jsonl
{"t":"group","media_id":"album:914","title":"Album","kind":"playlist","expected":2}
{"t":"entry","media_id":"track:1","url":"https://example.org/track/1","title":"Track 1","state":{"stream_id":"a91f"}}
{"t":"note","message":"One unavailable track was skipped"}
```

An entry can carry `duration` and provider state. `media_id` defaults to the first 16 hex digits
of the URL's SHA-256 hash. A bare object without `t` is also an entry. Unknown frame types are
logged and skipped.

Report a failure with an error frame:

```json
{"t":"error","code":"unavailable","message":"Track unavailable","retryable":false}
```

`resolve.format = "json"` accepts one object or an array of entries. `stdin = "json"` passes
the request as JSON to the child. The command's default stdin mode is `none`.

## Download output and progress

| `download.expect_output` | Success condition after exit 0 |
|---|---|
| `path_template` | `{out_path}` exists and is non-empty. |
| `result_frame` | A result frame reports the output path. |
| `newest_in_dir` | A new file exists under `{out_dir}`; the newest is selected. |

```json
{"t":"result","path":"/downloads/Track.flac","size":123456}
```

For `progress.kind = "json_lines"`, emit progress fields as JSON. For `regex`, use named capture
groups from the table above. Binary size suffixes use multiples of 1024, including `KB` and `MB`.
ANSI stripping and carriage-return handling are enabled by default.

A non-zero exit reports the last 2 KiB of stderr with ANSI codes removed. The child runs in its
own process group with the declared limits. Exceeding the output budget or a deadline terminates
it. The environment is cleared before applying `env.pass`, `env.set` and generated headers.

## Catalog

`[[catalog.download_types]]` defines download types. Each has `id`, `label`, `default_format`,
`formats` and optional `options`. Each format declares `id`, `label`, `default_quality`,
`qualities`, optional `codecs`, `notice`, and boolean flags `advisory`, `requires_ffmpeg`,
`lossy_remux` and `slow`.

Qualities use `id`, optional `label` and optional `notice`. Option definitions use `id`, `label`,
`kind`, `default`, `choices`, `help`, and any `min`, `max` or `pattern` constraint.
`catalog.naming` is `template` or `provider`.

Use the [example manifest](../plugins/examples/bandcamp/plugin.toml) for TOML structure and
[protocol catalog types](PROTOCOL.md#46-catalog) for the resulting client payload.

## Hooks

A hook uses an HTTP request or a command. Defaults and validation are implemented in
[hookspec.rs](../crates/aulos-provider/src/command/hookspec.rs).

| Key | Type | Req | Default | Meaning |
|---|---|---|---|---|
| `id` | string | ✓ | | unique within the manifest; the hook is `hook:<dir>/<id>` |
| `on` | [string] | ✓ | | subset of `["finished","error","canceled"]` |
| `ordering` | int | | `50` | lower runs first; built-ins are 10/20/90 |
| `debounce_ms` | int | | `0` | 0 = fire per event. > 0 coalesces events in a trailing window |
| `max_wait_ms` | int | | `10 × debounce_ms` | hard cap on the debounce window |
| `timeout_ms` | int | | `10000` | per attempt |
| `retries` | int | | `2` | attempts after the first, exponential 2 s/8 s |
| `when.provider` | [string] | | any | filter |
| `when.download_type` | [string] | | any | filter |
| `when.folder_prefix` | [string] | | any | filter on the item's `folder` |
| `http` | table | one of | | `{ method, url, headers, body }`; `method` default `POST`, `body` default `""` |
| `cwd` | string | | plugin directory | working directory for a command hook |
| `command` | [string] | one of | | argv template, spawned exactly like a provider command |

Templates are available in HTTP URLs, headers and bodies, and in command arguments:

| Token | Value |
|---|---|
| `{id}` | the item ULID |
| `{title}`, `{url}`, `{provider}`, `{status}` | as named |
| `{filename}` | path relative to the download root |
| `{folder}` | the item's `folder`, or `""` |
| `{download_url}` | the public URL, percent-encoded |
| `{size}`, `{download_type}`, `{format}`, `{quality}` | as named |
| `{error_code}`, `{error_message}` | empty on success |
| `{count}` | number of coalesced events in a debounced batch (1 when `debounce_ms = 0`) |
| `{titles_json}`, `{filenames_json}` | JSON arrays for a debounced batch |

In a debounced batch, single-item tokens describe the first event. Array tokens describe the
batch. URL substitutions are percent-encoded. JSON body substitutions are escaped. Command
substitution remains at argv level.

A failed HTTP request or command is retried according to the hook settings, then logged and
counted. It does not change the item's terminal result.

## Validation and diagnostics

Invalid manifests produce scan failures or degraded providers. Check `GET api/v2/providers`,
`POST api/v2/plugins/reload` and `healthz.plugin_warnings` for details.

Validation checks executable paths, directory permissions, template names, catalog IDs and
regular expressions. Numeric limits may be clamped with warnings. Hook debounce windows are
limited to one hour. Provider concurrency is limited to 32 and provider timeouts to 24 hours.
