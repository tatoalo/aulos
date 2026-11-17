# Jellyfin refresh experiment

Recorded on 2026-09-06. These are historical measurements, not a claim about other Jellyfin
versions or current deployments.

The experiment checked whether a refresh request discovers a new media file. Each run used a
fresh container database, a configured library and a media directory mounted before startup.
Time-to-appear was measured after the request returned.

## Tested images

| Image | Reported version | Digest |
|---|---|---|
| `jellyfin/jellyfin:10.10.7` | `10.10.7` | `sha256:7ae36aab93ef9b6aaff02b37f8bb23df84bb2d7a3f6054ec8fc466072a648ce2` |
| `jellyfin/jellyfin:preview` | `12.0.0` | `sha256:fa7542a7a3c2c80ab6242048db6e8509505ab9d30ff62362b2cc295472e22252` |

## Results

| Request | HTTP status | 10.10.7 | 12.0.0 |
|---|---|---|---|
| No request | n/a | Absent after 20 seconds | Absent after 20 seconds |
| `POST /Items/{libraryId}/Refresh` | 204 | Absent after 60 seconds | Absent after 60 seconds |
| Same request with `recursive=true` | 204 | Absent after 60 seconds | Absent after 60 seconds |
| `POST /Library/Refresh` | 204 | Found in 1 second | Found in 1 second |
| `POST /Library/Media/Updated` with the new file path | 204 | Found in 61 seconds | Found in 60 seconds |
| Run the `RefreshLibrary` scheduled task | 204 | Found in 1 second | Found in 1 second |

Both servers reported `LibraryMonitorDelay=60`. The targeted media update waited for that monitor
delay and refreshed the containing folder.

A targeted update for a path outside every library also returned `204`, but discovered nothing.
HTTP success therefore did not prove that a path mapping was correct.

## Effect on Aulos

Aulos uses a global library refresh by default and offers targeted media updates through
`JELLYFIN_PATH_MAP`. It does not use item metadata refresh to discover a new download.
`JELLYFIN_LIBRARY_ID` remains accepted but unused.

See [Jellyfin setup](../integrations.md#jellyfin) for current configuration and diagnostics.
The test containers were removed after the experiment.
