# Aulos Server

A native Rust re-implementation of the MeTube-POT fork's backend — queue, subscriptions, Telegram
bot, Jellyfin sync, StreamingCommunity support and BgUtils POT support — built to be a **drop-in
replacement** on an existing docker-compose deployment (same env var names, same volumes) while
feeling dramatically snappier for the Aulos iOS client.

> Status: **skeleton**. The workspace, CI and the container image are in place (WP-01); the
> subcommands other than `serve` print `not implemented` and `serve` binds nothing until the
> engine lands.

## Layout

| Path | What lives there |
|---|---|
| `crates/aulos-core` | Domain types, status vocabulary, ids, events, config, errors |
| `crates/aulos-store` | SQLite (WAL) store actor + legacy JSON importer |
| `crates/aulos-provider` | `Provider` trait, registry, progress sink, `command` plugins |
| `crates/aulos-provider-ytdlp` | yt-dlp provider and its thin Python shim |
| `crates/aulos-provider-sc` | StreamingCommunity provider |
| `crates/aulos-queue` | Scheduler, slots, resolution pool, cancellation, realtime aggregator |
| `crates/aulos-api` | axum: v2 REST + WebSocket, v1 shim, health, file serving |
| `crates/aulos-telegram` | teloxide bot |
| `crates/aulos-subscriptions` | Subscription manager and scheduler |
| `crates/aulos-hooks` | Jellyfin, NFO, audio-sync and community hooks |
| `crates/aulos-server` | The binary: wiring, POT sidecar supervisor, signals |
| `crates/aulos-workspace-tests` | Dev-only crate owning the workspace-wide gates |
| `docker/` | `Dockerfile`, `entrypoint.sh`, `compose.example.yml` |
| `plugins/examples/` | Example `command` plugins |
| `docs/` | `BRIEF.md`, `DESIGN.md`, `PROTOCOL.md`, `PLAN.md`, `reference/` |

## Build

Requires Rust 1.95 (edition 2024).

```sh
cargo build --workspace
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --locked
```

## Container image

```sh
docker build -f docker/Dockerfile -t aulos-server:dev .
docker run --rm aulos-server:dev doctor
docker run --rm -p 8081:8081 -v /srv/media:/downloads aulos-server:dev
```

The image ships python3 with a pinned nightly `yt-dlp`, the BgUtils POT plugin plus the
`bgutil-pot` sidecar binary, `deno`, `ffmpeg` and `N_m3u8DL-RE`, under `tini` with `gosu` for the
`PUID`/`PGID`/`UMASK` drop. The container `HEALTHCHECK` runs `aulos-server healthcheck`, which
loads the same config as `serve` so `URL_PREFIX` normalisation applies.

See `docker/compose.example.yml` for a deployment starting point.

## CLI

```
aulos-server [serve | check-config | import | doctor | healthcheck]
```

The subcommand is optional and defaults to `serve`, because the entrypoint ends in
`exec … aulos-server "$@"`.

## Documentation

- `docs/BRIEF.md` — binding orchestrator decisions and the v1.0 scope trims.
- `docs/DESIGN.md` — the architecture.
- `docs/PROTOCOL.md` — the v2 wire contract.
- `docs/PLAN.md` — the work packages.
