# Integrations

Environment values and defaults are listed in [configuration](configuration.md).

## Telegram

Enable the bot and restrict it to the chat IDs that may use it:

```yaml
environment:
  TELEGRAM_BOT_ENABLED: "true"
  TELEGRAM_BOT_TOKEN: ${TELEGRAM_BOT_TOKEN}
  TELEGRAM_ALLOWED_CHAT_IDS: ${TELEGRAM_ALLOWED_CHAT_IDS}
```

`TELEGRAM_ALLOWED_CHAT_IDS` is a comma-separated list of integer chat IDs. An empty token or chat
list prevents the bot from starting. Send a supported URL to queue it. The bot provides commands
and settings for download selection, queue control and subscriptions.

By default, the bot reports downloads added through Telegram and subscription checks. Set
`AULOS_TELEGRAM_WATCH_ALL=true` to also report web, API and iOS downloads. `AULOS_TELEGRAM_BOARD`
selects `board` or `per_job` progress messages.

The stall and hard-timeout settings control Telegram warnings. They do not cancel downloads.
Queue watchdog settings are separate.

Run only one polling bot per token. Use a separate bot for development; starting a local instance
with the production token interrupts polling on the deployed server.

## Jellyfin

```yaml
environment:
  JELLYFIN_SYNC_ENABLED: "true"
  JELLYFIN_URL: http://jellyfin:8096
  JELLYFIN_API_KEY: ${JELLYFIN_API_KEY}
```

Finished downloads trigger `POST /Library/Refresh` by default. Requests are grouped over
`AULOS_JELLYFIN_DEBOUNCE_SECS` (30 seconds), with a maximum wait of
`AULOS_JELLYFIN_MAX_WAIT_SECS` (300 seconds).

To request a targeted refresh, map the paths Aulos writes to the paths Jellyfin sees:

```yaml
environment:
  JELLYFIN_PATH_MAP: /downloads=/data/videos
```

Mappings are comma-separated `aulos_prefix=jellyfin_prefix` pairs. The longest matching prefix
wins, at a directory boundary. Aulos sends mapped paths to `POST /Library/Media/Updated`.
Unmapped paths and rejected targeted requests fall back to a global refresh.

A targeted request relies on Jellyfin's library monitor and can take longer to discover a file.
An accepted response does not prove indexing has finished. Check the library and Jellyfin logs
if a path mapping looks correct but media is missing.

`JELLYFIN_LIBRARY_ID`, `JELLYFIN_METADATA_REFRESH_MODE` and `JELLYFIN_IMAGE_REFRESH_MODE` are
accepted but do not affect refresh requests. Setting the library ID logs a warning.

`healthz.components.jellyfin` reports the mode, last request time, HTTP status, run count and
failure count. The [dated endpoint experiment](reference/jellyfin-refresh-experiment.md)
records why Aulos uses these endpoints.

## NFO files

`AULOS_NFO_ENABLED` defaults to `true`. The hook writes an NFO from provider metadata after a
successful download. For yt-dlp, enable the metadata sidecar:

```json
{
  "writeinfojson": true
}
```

Put this in `YTDL_OPTIONS` or its options file. StreamingCommunity stores metadata with the queue
entry and does not need a yt-dlp sidecar. `AULOS_NFO_PROVIDERS` can restrict the hook to a
comma-separated list of provider IDs; an empty value allows all providers.

Without usable metadata, the hook writes no file. The health field
`components.nfo.wrote_nothing_total` counts those runs. Remove an old NFO `Exec` script if the
built-in hook replaces it.

## Audio sync

The yt-dlp `video` / `mp4` / `best_remux` selection runs the audio-sync hook before the item becomes
finished. It checks timestamps and re-encodes audio when needed to correct drift after processing
such as SponsorBlock cuts. Video is copied. This can add a substantial postprocessing phase.

`AULOS_HOOKS_ENABLED=false` disables the hook dispatcher, including built-in hooks.

## iOS push notifications

The companion iOS app is currently in private alpha. APNs setup is for operators using an alpha
build configured for their Apple developer team.

The server needs an APNs signing key, key ID and team ID. Mount the `.p8` file read-only:

```yaml
environment:
  APNS_ENABLED: "true"
  APNS_KEY_FILE: /config/AuthKey_ABCD1234EF.p8
  APNS_KEY_ID: ABCD1234EF
  APNS_TEAM_ID: TEAM123456
  APNS_TOPIC: com.tatoalo.aulos
volumes:
  - ./apns/AuthKey_ABCD1234EF.p8:/config/AuthKey_ABCD1234EF.p8:ro
```

`APNS_TOPIC` must match the app's bundle identifier. Device registration accepts that identifier
and its extensions. The app registers its device and Live Activity tokens through the
[v2 device routes](PROTOCOL.md#48-devices).

By default, alerts and push-to-start activities are limited to items added by the iOS app.
An item with an install ID targets matching device registrations. An item without one targets
all eligible devices. `APNS_PUSH_ALL=true` widens this to all downloads and devices.

An already registered Live Activity continues receiving updates and an end event regardless of
origin filters. Its tracked download can also produce a completion alert. Retries and restarts
preserve the original download attribution.

`healthz.components.apns` reports `disabled`, `ok` or `degraded`, plus delivery counts and the last
error. A missing or invalid key disables push without preventing the server from starting.
Invalid device tokens reported by Apple are removed automatically.

## Community hooks

Use a hook plugin for Plex, Emby, ntfy or a local command. See the
[plugin reference](plugins.md#hooks) and [examples](../plugins/examples/README.md).
