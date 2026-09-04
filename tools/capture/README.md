# WP-00 — legacy capture: the golden corpora

Three checked-in corpora, captured from the **Python backend being replaced**, plus the tools that
regenerate them. They exist because R1 — *"the v1 shim is subtly wrong and the shipped iOS build
silently shows a broken queue during cutover"* — and R21 are mitigated only by data, and because
after cutover the source of truth is gone.

| Corpus | Path | Consumed by |
|---|---|---|
| yt-dlp format selectors | `tests/golden/formats.json` | WP-06 (`aulos-provider-ytdlp`) |
| yt-dlp option dicts | `tests/golden/opts.json` | WP-06 |
| progress-percent vectors | `tests/golden/percent.json` | WP-02 (`aulos-core::progress::Normalizer`) |
| v1 HTTP wire corpus | `tests/v1_golden/<case>/{request,response,meta}.json` + `MANIFEST.json` | WP-15 (v1 shim replay) |

`tests/v1_golden/harness.rs` is **not** here — WP-15 owns the replay. This package ships data plus
the tools that regenerate it.

## Regenerating everything

```bash
# once, in the legacy checkout
cd /Users/apogliaghi/Development/metube_pot
uv sync --frozen --group dev          # creates .venv

# then, from this repo
tools/capture/run_capture.sh
```

`run_capture.sh` runs the two pure-Python dumps, starts the legacy server twice (empty then seeded
`STATE_DIR`), drives `capture_v1.py` against each, and finishes with `verify.py`. It cleans up its
scratch directories and leaves the legacy checkout byte-clean.

Overrides: `METUBE_POT_ROOT=<path>` to point at a different legacy clone, `CAPTURE_PORT=<n>` to move
off `18081`.

**A re-capture is byte-identical except for the `captured_at` timestamps.** That is deliberate — a
re-run should diff to nothing when nothing changed, so any real diff is signal. It costs three
things: canonical JSON (sorted keys, 2-space indent) everywhere; `Date` and the scratch root
rewritten to `<volatile>` / `<scratch>`; and a fixed, deliberately-future `last_checked` on the
seeded subscription instead of `time.time()`.

## What is in the box

| File | Role |
|---|---|
| `_legacy.py` | shared helpers: `sys.path` juggling into the legacy `app/`, canonical-JSON writer, the legal `(download_type, codec, format, quality)` space |
| `dump_formats.py` | imports `app/dl_formats.py`; writes `formats.json` + `opts.json` |
| `dump_progress_vectors.py` | imports `app/ytdl.py`; writes `percent.json` |
| `seed_state.py` | writes a `schema_version: 2` `STATE_DIR` fixture |
| `capture_v1.py` | drives the running legacy server; writes `tests/v1_golden/` |
| `run_capture.sh` | the whole pipeline |
| `verify.py` | shape + coverage + secret-hygiene check; **runs in CI, never touches the network** |

## Why there is no docker image

BRIEF's WP-00 scope trim replaces the `ghcr.io/tatoalo/metube_pot@<digest>` recipe with a local
`python app/main.py` run and restricts the corpus to the **network-free** routes. `MANIFEST.json`
records `legacy_image_digest` as explicitly unused, and the legacy git commit takes its place as the
provenance anchor.

## Standing up the legacy server by hand

`run_capture.sh` does this for you; the manual recipe is here because a one-off debugging pass often
wants it.

```bash
cd /Users/apogliaghi/Development/metube_pot

# aiohttp's static handler refuses a missing ui/dist/metube/browser. Do NOT run
# pnpm — a placeholder is enough, and it must be deleted afterwards so the
# legacy checkout stays clean.
mkdir -p ui/dist/metube/browser && : > ui/dist/metube/browser/index.html

TMP=$(mktemp -d)
.venv/bin/python /path/to/aulos_server/tools/capture/seed_state.py "$TMP/.metube"

env DOWNLOAD_DIR="$TMP/downloads" AUDIO_DOWNLOAD_DIR="$TMP/downloads" \
    TEMP_DIR="$TMP/tmp" STATE_DIR="$TMP/.metube" \
    HOST=127.0.0.1 PORT=18081 URL_PREFIX= LOGLEVEL=INFO \
    MAX_CONCURRENT_DOWNLOADS=0 \
    ALLOW_YTDL_OPTIONS_OVERRIDES=false \
    CORS_ALLOWED_ORIGINS=https://ui.example.com \
    YTDL_OPTIONS_PRESETS='{"archive": {"writesubtitles": true}, "fast": {"concurrent_fragment_downloads": 4}}' \
    TELEGRAM_BOT_ENABLED=false JELLYFIN_SYNC_ENABLED=false \
    METUBE_VERSION=wp00-capture \
    .venv/bin/python app/main.py

# afterwards
rm -rf ui/dist "$TMP"
git status            # must be clean
```

### Three settings are load-bearing

* **`MAX_CONCURRENT_DOWNLOADS=0`.** `DownloadQueue.initialize()` auto-restarts everything in
  `queue.json`, so a seeded `downloading` row would immediately dial out. A zero-permit semaphore
  parks those tasks forever: the seeded statuses survive verbatim and nothing reaches the network.
  `verify.py` asserts the manifest records this.
* **`exec` when backgrounding the server** (`run_capture.sh`). aiohttp binds with `SO_REUSEPORT`, so
  a survivor from the previous phase does not fail to start the next one — the kernel silently
  load-balances between the two servers and the capture records a mix of two different states. The
  script `exec`s so `$!` is the Python process, then waits for the port to close, and refuses to
  start at all if something is already listening.
* **A fresh `last_checked` on the enabled subscription** (`seed_state.py`). The subscription loop
  sleeps 60 s and then checks everything due; a `last_checked: null` row would fire a network
  extraction mid-capture.

## Scope: network-free only

Per BRIEF's WP-00 scope trim, the v1 corpus covers `history`, `delete`, `start`, `version`,
`presets`, `robots.txt`, `cancel-add`, `cookie-status`, cookie upload/delete, the `subscriptions/*`
validation errors, and **every `POST add` validation 400** — bodies that fail
`parse_download_options` before yt-dlp is constructed. `verify.py` asserts that *every* `POST add`
case is a 400: a 200 there would mean a body slipped through to the network.

Everything deliberately absent is listed in `MANIFEST.json:skipped` with a reason. The two that are
worth knowing about:

* **`auto_start` routing.** The legacy `auto_start is True` comparison — which sends the string
  `"true"` to *pending* — happens in `__add_download`, after a successful network resolve, so the
  routing is not observable here. Parse-time acceptance of `true` / `"true"` / `"false"` **is**
  captured (`add_auto_start_*`).
* **`Missing URL` on `/subscribe`.** Unreachable over HTTP: `parse_download_options` rejects a falsy
  `url` first, so `add_subscription`'s own empty-url branch is dead behind the REST route.

### How a migration row is proven without the network

The `int()` of `playlist_item_limit` is the **last** check in `parse_download_options`, so
`playlist_item_limit: "nope"` is a probe that means *"everything before me validated"*. A
`_migrate_legacy_request` row that produces a legal tuple therefore surfaces as the
`playlist_item_limit must be an integer` 400, while an unmigrated body would have failed earlier on
`format`/`quality`. Where a row can be pinned more tightly, it is: `add_migrate_row6_passes_format_through`
sends `{format: "any", quality: "best_remux"}` and the reason string proves `format` was *not*
rewritten to `mp4`, because `best_remux` is absent from the allowed set.

The same trick with `/subscribe`'s `check_interval_minutes` gate proves the numeric-string
leniencies, which have no other observable effect on a network-free path.

## Reading a case

```
tests/v1_golden/add_download_type_unknown/
  request.json   { method, path, route, headers, body }
  response.json  the parsed JSON body, or {"__text__": "…"} when it is not JSON
  meta.json      { status, reason, content_type, request_headers, response_headers, … }
```

**`reason` is where the legacy strings live.** Legacy raises
`web.HTTPBadRequest(reason='<message>')`, and aiohttp puts that in the *status line*, with a
`text/plain` body of `400: <message>`. So every byte-identical string of DESIGN §11.7 is
`meta.json:reason`, not `response.json`. Two consequences for WP-15:

* the shim's JSON error envelope is a **deliberate** change of shape, not a mismatch to fix;
* the two legacy 500s carry **nothing** on the wire —
  `subscriptions_update_enabled_not_a_boolean` answers
  `500 Internal Server Error / Server got itself in trouble`, and `enabled must be a boolean` never
  leaves the log. DESIGN §11.1 promoting it to a 400 with that message is strictly new information
  for the client.

`Content-Type` is captured per case because the shim's documented deltas are header-level
(DESIGN §11.1): most legacy handlers answer `text/plain; charset=utf-8`, and only `presets`,
`cancel-add` and `version` are `application/json`.

## Secret hygiene

* No request ever sends `Cookie` or `Authorization`.
* Request *and* response headers are scrubbed on write: `Cookie`, `Set-Cookie`, `Authorization`,
  `Proxy-Authorization` become `<scrubbed>` — the key survives (its presence is part of the
  contract), the value never lands on disk. `verify.py` re-checks this over the corpus, so a future
  capture through an authenticating proxy cannot leak one either.
* `Date` becomes `<volatile>` so a re-capture diffs cleanly.
* The cookie uploads are synthetic (`b"a" * N`); `request.json` records the byte count and the
  recipe, never a megabyte of payload.
* Every seeded URL is public, and no seeded record contains a token.

## The verifier

```bash
python tools/capture/verify.py     # stdlib only, offline, exit 0 == usable
```

Stdlib only and Python ≥ 3.9, so the CI `python` job can run it on whatever interpreter the runner
ships — it needs neither the legacy checkout nor its venv.

It asserts, and fails loudly on:

* `formats.json` covers **exactly** the tuple space the DESIGN §6.6 catalog admits — no gaps, no
  strays — plus the catalog's own invariants (height filters on `mp4`/`ios`, the documented `worst`
  quirk, `best_remux`, the caption/thumbnail selectors);
* `opts.json` covers the same space (with the caption mode/language axes) and its branch cases never
  mutated the caller's option dict;
* `percent.json` has at least one vector per row of the DESIGN §4.7 table, and the four ported
  legacy unit tests are present by name;
* every `tests/v1_golden/` directory has all three files, they parse, they agree with each other,
  and each carries a `why`;
* **route coverage**: every route in the PLAN WP-00 checklist has at least its minimum number of
  cases, all nine `OPTIONS` routes are present, and every 400/500 reason string and body message the
  checklist names appears somewhere;
* the structural claims: `history_empty` is three empty arrays, `history_seeded` has ≥ 600 `done`
  rows and one item per legacy status plus the pre-download-problem row, the 1 000 000-byte decimal
  cookie cap is captured on **both** sides, both legacy 500s are on record, all six migration rows
  and all five §11.2.1 leniencies have a case;
* `MANIFEST.json` has every provenance field, its index matches the directories on disk, and
  `skipped` justifies every gap.

A half-finished re-capture fails here rather than silently shrinking someone else's test surface.
