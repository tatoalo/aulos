# Web UI development

The shipped page is in [crates/aulos-api/web](../../crates/aulos-api/web). This directory contains
a local protocol mock, Playwright tests and icon tooling.

## Run

```sh
cd tools/web
npm ci
npx playwright install chromium
npm test
npm run serve
```

`npm run serve` opens a mock server at `http://127.0.0.1:8099/`. Tests use local data and do not
need a running Rust server. They write screenshots to `screenshots/`; CI uploads that directory.

## Mock options

Run `node mock-server.mjs` with these flags:

| Flag | Default | Effect |
|---|---|---|
| `--port N` | `0` | Listen port; 0 chooses a free port. |
| `--prefix /aulos/` | `/` | Base path. |
| `--theme auto` | `auto` | Initial theme: auto, light or dark. |
| `--token TOKEN` | unset | Require token auth for API and socket access. |
| `--freeze` | off | Stop scripted updates for stable screenshots. |
| `--big-group` | off | Exercise loading group children through REST. |
| `--flap` | off | Close sockets with 1013 to exercise reconnection. |

The mock includes scripted queue and subscription changes. `__test/log`, `__test/reset` and
`__test/kick` exist only for tests. Mock scenarios are test inputs, not additional server features.
Use [Protocol v2](../../docs/PROTOCOL.md) for the supported server contract.

## Check a running server

Set `AULOS_WEB_BASE` to a server base URL including its prefix:

```sh
cd tools/web
AULOS_WEB_BASE=http://127.0.0.1:8081/ npx playwright test
```

This runs serving, header and connection checks and takes desktop and phone screenshots.
Tests that need the mock's scripted state or control routes are skipped. Use an isolated local
server with temporary state for this mode.

## Assets

The Rust server substitutes `{{PREFIX}}` and `{{THEME}}` in `index.html`. Other assets are served
verbatim. Keep scripts and styles compatible with the page's Content Security Policy.
The combined unminified `app.js` and `app.css` budget is 96 KB.

`make-icon.py` generates the SVG and PNG icons from `icon-master-512.png`. It requires Pillow:

```sh
python3 tools/web/make-icon.py
```

Review screenshots after UI changes. The current CSS and assets are the maintained design source.
