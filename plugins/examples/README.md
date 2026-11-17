# Plugin examples

Copy a configured example directory into `AULOS_PLUGINS_DIR`, normally `/config/plugins`.
The image also includes these examples at `/opt/aulos/plugins-examples`.

| Directory | Purpose |
|---|---|
| [bandcamp](bandcamp/plugin.toml) | Provider manifest, resolution and download scripts, progress parsing and catalog. |
| [media-server-hooks](media-server-hooks/plugin.toml) | HTTP hooks for Plex, Emby and ntfy, plus a local command. |

The Bandcamp example is tested against a local HTTP stub. Configure service URLs, credentials
and commands before installing the hook example. Install only code you trust; plugins run with
the server user's permissions.

Providers reload at startup, through file watching, on `SIGHUP`, or through
`POST api/v2/plugins/reload`. Hook changes require a server restart.

See the [plugin reference](../../docs/plugins.md) for the manifest schema, templates, process
contract and diagnostics.
