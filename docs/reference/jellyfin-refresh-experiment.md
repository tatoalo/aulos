# Which Jellyfin endpoint actually discovers a new file — measured

**Date:** 2026-09-06
**Why:** the production report `aulos-jellyfin-vs-metube.md` says aulos's targeted refresh returns
`204` and never indexes anything, while metube's `POST /Library/Refresh` indexes in seconds. This
document is the empirical check of that claim, from which DESIGN §13.1 is rewritten. Nothing here
is recalled from memory: every row is a status code and a stopwatch reading from a throwaway
container on this machine.

**Servers under test** (OrbStack, `linux/arm64`, no `/config` volume, so each run starts from a
virgin database):

| tag | `System/Info/Public` `Version` | image digest |
|---|---|---|
| `jellyfin/jellyfin:preview` | **12.0.0** — the reporter's production version | `sha256:fa7542a7a3c2c80ab6242048db6e8509505ab9d30ff62362b2cc295472e22252` |
| `jellyfin/jellyfin:10.10.7` | **10.10.7** | `sha256:7ae36aab93ef9b6aaff02b37f8bb23df84bb2d7a3f6054ec8fc466072a648ce2` |

---

## 1. Protocol

```bash
SP=<scratch>            # host dir bind-mounted at /media
mkdir -p $SP/jf-media/tube
docker run -d --name aulos-jf-test -p 18096:8096 -v $SP/jf-media:/media jellyfin/jellyfin:<tag>
```

> The bind mount must exist **before** `docker run` and must not be recreated afterwards: deleting
> and re-making the host directory silently detaches the mount, and Jellyfin then rejects the
> library with `ArgumentException: The specified path does not exist: /media/tube`.

Startup wizard over REST (shapes taken from `GET /api-docs/openapi.json`, not from memory):

```bash
B=http://127.0.0.1:18096
# The default "root" record is created by a background startup task. POST /Startup/User before it
# exists returns 500 "Sequence contains no elements", so poll GET /Startup/User until it answers.
until curl -s "$B/Startup/User" | grep -q Name; do sleep 2; done
curl -X POST "$B/Startup/Configuration" -H 'Content-Type: application/json' \
     -d '{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}'   # 204
curl -X POST "$B/Startup/User" -H 'Content-Type: application/json' \
     -d '{"Name":"aulos","Password":"aulospass123"}'                                          # 204
curl -X POST "$B/Startup/RemoteAccess" -H 'Content-Type: application/json' \
     -d '{"EnableRemoteAccess":true,"EnableAutomaticPortMapping":false}'                      # 204
curl -X POST "$B/Startup/Complete"                                                            # 204
```

Authenticate, mint an API key, create the library:

```bash
AUTH='MediaBrowser Client="aulos", Device="test", DeviceId="t", Version="1"'
TOKEN=$(curl -s -X POST "$B/Users/AuthenticateByName" -H "Authorization: $AUTH" \
        -H 'Content-Type: application/json' -d '{"Username":"aulos","Pw":"aulospass123"}' \
        | jq -r .AccessToken)                                                                 # 200
curl -X POST "$B/Auth/Keys?app=aulos" -H "Authorization: MediaBrowser Token=\"$TOKEN\""       # 204
KEY=$(curl -s "$B/Auth/Keys" -H "Authorization: MediaBrowser Token=\"$TOKEN\"" | jq -r .Items[0].AccessToken)
curl -X POST "$B/Library/VirtualFolders?name=Tube&collectionType=movies&paths=%2Fmedia%2Ftube&refreshLibrary=true" \
     -H "Authorization: MediaBrowser Token=\"$KEY\"" -H 'Content-Type: application/json' -d '{}'  # 204
curl -s "$B/Library/VirtualFolders" -H "Authorization: MediaBrowser Token=\"$KEY\""
# -> Tube  ca4fc2dadb00fcd7e929d2d0a49151b8  ["/media/tube"]  EnableRealtimeMonitor=false
```

The `CollectionFolder` id came out as **`ca4fc2dadb00fcd7e929d2d0a49151b8` on both versions** —
byte-identical to the `JELLYFIN_LIBRARY_ID` in the production report, because the id is a
deterministic hash of the library's path. Independent confirmation that the reporter's id is a
real `CollectionFolder` and not a typo, which rules out the "mistyped id" theory the current
400/404 fallback was written for.

`EnableRealtimeMonitor` defaults to **false** for a library created this way, so the filesystem
watcher cannot confound a trial: nothing gets indexed unless an endpoint causes it.

One trial = one mechanism:

```bash
ffmpeg -f lavfi -i testsrc=duration=2:size=64x64:rate=5 -pix_fmt yuv420p $SP/jf-media/tube/<trial>.mp4
<fire exactly one endpoint, record the HTTP status>
# then poll once a second, up to the stated ceiling:
curl -s "$B/Items?Recursive=true&ParentId=$LIB&Fields=Path&Limit=500" -H "Authorization: ..." \
  | grep -F "/media/tube/<trial>.mp4"
```

Each trial uses a unique filename, so an earlier trial's scan cannot be mistaken for this one's.
The full runner is `probe.sh` in the session scratchpad; it is reproduced by the snippet above.

---

## 2. Results

Time-to-appear is measured from the moment the endpoint returned. `NO` = still absent at the
ceiling (60 s for the item-refresh mechanisms, 90–120 s for the rest).

| # | Mechanism | 10.10.7 status | 10.10.7 time-to-appear | 12.0.0 status | 12.0.0 time-to-appear |
|---|---|---|---|---|---|
| 0 | **baseline** — drop the file, call nothing | — | **NO** (20 s) | — | **NO** (20 s) |
| a | `POST /Items/{libraryId}/Refresh?metadataRefreshMode=Default&imageRefreshMode=Default&replaceAllMetadata=false&replaceAllImages=false` — *what aulos does today* | `204` | **NO** (60 s) | `204` | **NO** (60 s) |
| a′ | the same **+ `&recursive=true`** | `204` | **NO** (60 s) | `204` | **NO** (60 s) |
| b | `POST /Library/Refresh` — *what metube did* | `204` | **1 s** | `204` | **1 s** |
| c | `POST /Library/Media/Updated` `{"Updates":[{"Path":"/media/tube/<f>.mp4","UpdateType":"Created"}]}` | `204` | **61 s** | `204` | **60 s** (repeat run: 60 s) |
| d | `POST /ScheduledTasks/Running/7738148ffcd07979c7ceb148e06b3aed` (`RefreshLibrary`, "Scan Media Library") | `204` | **1 s** | `204` | **1 s** |

Edge cases, same servers:

| Probe | 10.10.7 | 12.0.0 |
|---|---|---|
| (a) with a library id that does not exist | `404` | `404` (per OpenAPI; the 12 run used the real id) |
| (c) with a path in **no** library (`/downloads/nowhere/x.mp4`) | `204`, nothing happens | `204`, nothing happens |
| (c) with no `Authorization` header | — | `401` |

### The reporter's claim reproduces exactly

Row (a) is the production bug, on both versions: **`204 No Content` for an operation that does
nothing.** `/Items/{id}/Refresh` refreshes metadata for an item Jellyfin already has; a file with
no item yet is invisible to it. Row (b) is metube's call and it indexes in one second.

### There is no per-library scan endpoint — this is not an aulos oversight

Filtering both OpenAPI documents for operations whose summary mentions "scan" or "refresh" returns
the same three on **both** versions:

| Method | Path | Summary | Query parameters |
|---|---|---|---|
| `POST` | `/Items/RemoteSearch/Apply/{itemId}` | Applies search criteria to an item and refreshes metadata. | `itemId`, `replaceAllImages` |
| `POST` | `/Items/{itemId}/Refresh` | **Refreshes metadata for an item.** | `itemId`, `metadataRefreshMode`, `imageRefreshMode`, `replaceAllMetadata`, `replaceAllImages`, `regenerateTrickplay` |
| `POST` | `/Library/Refresh` | **Starts a library scan.** | *(none)* |

Two facts fall out of that table:

1. `/Library/Refresh` is the **only** endpoint that starts a library scan, and it takes **no
   parameters at all** — so a library id cannot be passed to it. The web UI's per-library "Scan
   library" button has nothing else to call either.
2. `/Items/{itemId}/Refresh` has **no `recursive` parameter** in either version's schema. Row (a′)
   confirms the server accepts the unknown query parameter and ignores it: same `204`, same
   nothing.

**Therefore `JELLYFIN_LIBRARY_ID` cannot scope discovery on any Jellyfin the reporter can run.**
That is a property of the Jellyfin API, not a bug in how aulos calls it, and row (d) shows the
scheduled task is just `/Library/Refresh` under another name — global, not scoped.

### Why `Library/Media/Updated` takes exactly a minute

`GET /System/Configuration` reports `LibraryMonitorDelay = 60` on **both** versions.
`/Library/Media/Updated` hands the path to the `LibraryMonitor`, which coalesces reports for that
many seconds and then refreshes the containing folder. The server log makes it explicit:

```
[INF] Emby.Server.Implementations.IO.LibraryMonitor: tube (/media/tube) will be refreshed.
```

So (c) is genuinely targeted — only `/media/tube` is rescanned, not every library — and it
self-coalesces, but its latency is the server's `LibraryMonitorDelay`, not ours. Measured 60 s and
61 s, and 60 s again on a repeat run: reproducible, and equal to the configured delay.

### The trap `Library/Media/Updated` shares with the old code

A path that belongs to **no** library returns `204` and does nothing — the exact failure mode of
row (a). Any targeted mode built on it is only as good as the operator's path mapping, and a wrong
mapping is *silent*. That is why the targeted mode is opt-in, falls back to the global scan
whenever the map does not cover a path, and reports its `mode` and `last_status` in `healthz`.

---

## 3. What this means for aulos

| Question | Answer, from the table above |
|---|---|
| What must the default be? | `POST /Library/Refresh`. It is the only mechanism proven to discover a new file promptly, on both versions. Legacy parity. |
| Can `JELLYFIN_LIBRARY_ID` scope a scan? | **No.** No such endpoint exists. Keep the variable, WARN at boot that it cannot scope discovery, and scan globally regardless. |
| Is `/Items/{id}/Refresh` ever right for a new download? | Never. It cannot do filesystem discovery, by design. |
| Is a targeted mode possible at all? | Yes, `Library/Media/Updated` with paths **as Jellyfin sees them** — hence `JELLYFIN_PATH_MAP`. Costs up to `LibraryMonitorDelay` (60 s) of latency and is silent when the map is wrong. Opt-in only. |
| Should the hook verify the item appeared? | Not on a short timer. In `media_updated` mode nothing can appear for a whole `LibraryMonitorDelay`, so a poll "a few seconds later" would WARN on every healthy scan, and a poll long enough to be correct would hold the hook for over a minute. The honest fields (`mode`, `last_status`, `last_request_at`) carry the observability instead. |

## 4. Teardown

```bash
docker rm -f aulos-jf-test
```

Both containers were removed on 2026-09-06; no `/config` volume was ever created, so nothing
persists on this machine.
