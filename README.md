# Aulos

Aulos is a self-hosted download server written in Rust. It queues URLs, downloads media through
yt-dlp, StreamingCommunity or command plugins, and streams progress to connected clients.

The server includes a web UI, a v2 REST API and a WebSocket API. The companion iOS app is
currently in private alpha.

## Features

- Video, audio, subtitles and thumbnails through yt-dlp.
- Playlists, channel subscriptions, pause, retry and persistent download history.
- StreamingCommunity downloads with N_m3u8DL-RE and ffmpeg.
- Telegram notifications, Jellyfin refresh, NFO files and optional iOS push notifications.
- Command plugins for additional providers and completion hooks.

## Quickstart

The published Docker image targets `linux/amd64`. It includes the download tools and the
BgUtils POT sidecar. Other architectures require amd64 emulation.

Save this as `compose.yml`:

```yaml
services:
  aulos:
    image: ghcr.io/tatoalo/aulos:latest
    platform: linux/amd64
    restart: unless-stopped
    ports:
      - "127.0.0.1:8081:8081"
    stop_grace_period: 60s
    environment:
      PUID: "1000"
      PGID: "1000"
      UMASK: "022"
      DOWNLOAD_DIR: /downloads
      STATE_DIR: /downloads/.metube
    volumes:
      - ./downloads:/downloads
      - ./config:/config
```

Set `PUID` and `PGID` to the user and group that should own downloaded files. Check them with
`id -u` and `id -g`. The `.metube` directory name is retained for existing deployments; Aulos
stores its SQLite database there.

```sh
docker compose up -d
docker compose ps
curl --fail http://localhost:8081/healthz
```

Open [localhost:8081](http://localhost:8081) to add a download. The quickstart binds to localhost.
For access from other devices, configure authentication and a reverse proxy as described in
[operations](docs/operations.md#authentication-and-network-access).

To add a download through the API:

```sh
curl --fail http://localhost:8081/api/v2/downloads \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://www.youtube.com/watch?v=aqz-KE-bpKQ","download_type":"video"}'
```

The response is `202 Accepted` with an item ID. Extraction and downloading run in the background.
Progress is available in the web UI, at `/api/v2/state`, and through `/ws`.

Use [docker/compose.example.yml](docker/compose.example.yml) for a deployment with separate audio
and configuration directories.

## Documentation

| Task | Guide |
|---|---|
| Set environment variables and yt-dlp options | [Configuration](docs/configuration.md) |
| Deploy, upgrade, back up or troubleshoot | [Operations](docs/operations.md) |
| Set up Telegram, Jellyfin or iOS push | [Integrations](docs/integrations.md) |
| Write a provider or hook plugin | [Plugins](docs/plugins.md) |
| Build an API client | [Protocol v2](docs/PROTOCOL.md) |
| Understand the server | [Architecture](docs/DESIGN.md) |
| Build, test or release changes | [Development](docs/development.md) |

Aulos supports v2 clients only. MeTube v1 routes and Socket.IO are not supported. Existing Aulos
SQLite databases migrate at startup. To move from MeTube JSON state, use the explicit
[legacy import procedure](docs/operations.md#importing-metube-state).
