# tools/web — dev mock and smoke for the shipped page

The page itself lives in `crates/aulos-api/web/` and is what the binary embeds. Nothing here
ships: this directory only exists so the page can be developed and tested without a running
`aulos-server`.

```bash
cd tools/web
npm ci
npx playwright install chromium

npx playwright test          # the smoke against the mock — all offline
npm run serve                # a mock on http://127.0.0.1:8099/ for hand-driving the page
```

`mock-server.mjs` serves `crates/aulos-api/web/*` with the same two substitutions and the same
response headers the server contract specifies, and implements enough of PROTOCOL v2 to drive
every path in the UI.

| Flag | Default | What it does |
|---|---|---|
| `--port N` | `0` (ephemeral) | prints `LISTENING <port>` on stdout once bound |
| `--prefix /metube/` | `/` | serves everything under a non-root `URL_PREFIX` |
| `--theme light\|dark\|auto` | `auto` | the value substituted for `{{THEME}}` |
| `--token SECRET` | off | 401 on every API route and on the WS upgrade until the bearer is presented |
| `--freeze` | off | stops the 250 ms delta ticker, for byte-stable screenshots |
| `--big-group` | off | adds a 480-child group with `children_inline: false`, so the page must fetch its children |
| `--flap` | off | accepts every upgrade and closes it at once with `1013`, to exercise §5.1's back-off |

The socket implements §6.2/§6.3 resume: an upgrade carrying `?since=&boot=` inside the 512-frame
replay ring is answered with a `resume` frame plus the folded `added`/`completed`/`removed`/`delta`,
and anything else — a stale `boot`, a cursor above the head — falls back to a `snapshot`. The
scripted queue also performs §5.5's in-place group promotion: two seconds in, the `resolving` row
becomes a `kind: "group"` with the **same id and the same `ord`** and no `removed` frame.

Three routes exist only for the smoke and are not part of the contract: `GET <p>__test/log`
returns every mutating request the mock received, `GET <p>__test/reset` reseeds it, and
`GET <p>__test/kick[?reboot=1]` drops every socket without stopping the server — with `reboot=1`
rotating `boot_id` first — then makes a change during the gap, so both resume outcomes and the
fold can be asserted.

## Running the smoke against a real `aulos-server`

`AULOS_WEB_BASE` points the same suite at a live server instead of the mock:

```bash
cargo build -p aulos-server
DOWNLOAD_DIR=/tmp/dl STATE_DIR=/tmp/state PORT=8091 HOST=127.0.0.1 \
  ./target/debug/aulos-server &

cd tools/web
AULOS_WEB_BASE=http://127.0.0.1:8091/ npx playwright test
```

A real server has no scripted queue, so every test that asserts on the mock's fixed rows, its
deltas, its request log or its `__test/` routes is **skipped**; what runs is the static/serving
subset — the routes, the headers, the ETag and the 304, the manifest, the 404 envelope, the
identity document on the same route without `Accept: text/html` — plus a boot check (the page
reaches the API, the socket goes Live, no CSP violation, no console error) and two screenshots,
`screenshots/real-desktop.png` (1440) and `screenshots/real-phone.png` (390). The queue is
normally empty on a scratch server, which is the point of those two: the empty state has to look
composed rather than half-loaded.

The value is a base URL **including the `URL_PREFIX`**, so the nested-prefix posture is the same
command against a server started with `URL_PREFIX=metube`:

```bash
AULOS_WEB_BASE=http://127.0.0.1:8091/metube/ npx playwright test
```

This mode is not wired into CI — the `web` job has no Rust toolchain — it is the manual
integration check that the two halves of the contract meet on the real binary.

## What the server side owes the page

- `index.html` is a template with exactly two substitutions, `{{PREFIX}}` (six occurrences) and
  `{{THEME}}` (two: the `<html data-mode>` attribute the CSS keys the palette off, and the meta
  tag `app.js` reads). Nothing else in the file is templated and no user data enters it.
- `manifest.webmanifest`, `app.css`, `app.js`, `icon.svg` and `icon-180.png` are served
  **verbatim** — the manifest uses relative URLs (`./`, `assets/icon.svg`) so it is already
  prefix-independent and needs no substitution.
- `index.html` must carry the exact CSP the contract names. The page is written to satisfy it:
  no inline `<script>`, no inline `<style>`, no `style="…"` attribute and no
  `setAttribute('style', …)` — every measured value is written through the CSSOM instead, which
  `style-src 'self'` allows.

`make-icon.py` regenerates `icon.svg` and `icon-180.png` from `icon-master-512.png`, which is
the iOS app icon (`Aulos/Assets.xcassets/AppIcon.appiconset/app_icon_1024.png` in the iOS
repository, downscaled). `icon.svg` is that raster at 128 px, base64-embedded and clipped to the
iOS corner radius, so Chrome and Firefox get a sharp favicon from one small file; Safari ignores
SVG favicons and takes the PNG `rel="icon"` instead. The header mark is the PNG as a CSS
background. `python3 tools/web/make-icon.py` needs Pillow (`pip install pillow`).
