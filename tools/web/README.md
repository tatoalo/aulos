# tools/web — dev mock and smoke for the shipped page

The page itself lives in `crates/aulos-api/web/` and is what the binary embeds. Nothing here
ships: this directory only exists so the page can be developed and tested without a running
`aulos-server`.

```bash
cd tools/web
npm ci
npx playwright install chromium

npx playwright test          # the smoke — 19 tests, all offline
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

Two routes exist only for the smoke and are not part of the contract: `GET <p>__test/log`
returns every mutating request the mock received, and `GET <p>__test/reset` reseeds it.

## What the server side owes the page

- `index.html` is a template with exactly two substitutions, `{{PREFIX}}` (six occurrences) and
  `{{THEME}}` (one). Nothing else in the file is templated and no user data enters it.
- `manifest.webmanifest`, `app.css`, `app.js`, `icon.svg` and `icon-180.png` are served
  **verbatim** — the manifest uses relative URLs (`./`, `assets/icon.svg`) so it is already
  prefix-independent and needs no substitution.
- `index.html` must carry the exact CSP the contract names. The page is written to satisfy it:
  no inline `<script>`, no inline `<style>`, no `style="…"` attribute and no
  `setAttribute('style', …)` — every measured value is written through the CSSOM instead, which
  `style-src 'self'` allows.

`make-icon.py` regenerates `icon-180.png` from the same 24-grid mark as `icon.svg`
(`python3 tools/web/make-icon.py`); it needs no third-party module.
