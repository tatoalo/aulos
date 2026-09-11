# `docs/design/web-ui/` — the web UI's design source

These files are the **visual specification** for the page that ships in `crates/aulos-api/web/`.
Where the CSS and an artboard disagree about a colour, a radius, a shadow or a spacing step, the
artboard is right and the CSS is a bug. Where they disagree about *behaviour*, `docs/DESIGN.md` §24
and `docs/PROTOCOL.md` win — an artboard is a still frame and knows nothing about deltas,
reconnects or errors.

## What a `.dc.html` file is

Each artboard is a self-contained HTML document authored on a **Claude Design canvas**: a single
pan-and-zoom surface holding all five boards side by side, laid out by `canvas.json` (which also
carries the two annotation notes pinned next to them). `canvas.json` is the canvas; the `.dc.html`
files are its artboards.

They are **not** a prototype and not a build input. Nothing in this directory is compiled, imported,
served or embedded — the binary embeds `crates/aulos-api/web/` and only that. Two consequences
worth knowing before you open one:

- Each file references `./support.js`, which is the canvas runtime and is **not** checked in. Opened
  directly in a browser an artboard renders as unstyled markup; open it on the canvas instead.
- The boards use inline `style="…"` attributes freely and template a `{{theme}}` class. The shipped
  page may do neither: it runs under `style-src 'self'` with no `'unsafe-inline'`, so every value
  an artboard writes inline becomes a class or a CSSOM write in `app.css`/`app.js` (DESIGN §24.5).
  A board's `.app.dark` variant is a boolean prop on the canvas; in the real page the same palette
  is a `prefers-color-scheme` media query plus a `[data-mode]` override.

## Which boards ship

| File | Board | Status |
|---|---|---|
| `Main.dc.html` | Desktop · queue, 1440×1180 | **Shipped.** The direction, and the token source. |
| `Mobile.dc.html` | Phone · queue, 390×844 | **Shipped.** The `max-width: 640px` layout. |
| `MobileAdd.dc.html` | Phone · add sheet, 390×844 | **Shipped.** The bottom sheet and its controls. |
| `AltDense.dc.html` | Option B · dense table, 720×460 | **Rejected alternate.** Low-fidelity, kept as the record of what was considered. |
| `AltCanvas.dc.html` | Option C · thumbnail wall, 720×460 | **Rejected alternate.** Same. |

The two alternates are deliberately not deleted — a rejected direction is cheaper to re-open than
to re-derive — but nothing implements them, and nothing should. If you are reading an artboard to
answer "what should this look like", read one of the first three.

## How the tokens map onto `app.css`

Everything in the left column is lifted **verbatim** from `Main.dc.html`'s `.app` rule into
`app.css`'s `:root`; the dark values come from `.app.dark`.

| Token | Light | Dark | Where it is used |
|---|---|---|---|
| `--accent` | `#E07850` | same | the brand terracotta: primary buttons, active discs, links, the `theme_color` in the manifest |
| `--accent-deep` | `#8B3A4C` | same | the burgundy end of every gradient (`linear-gradient(90deg, #E07850, #8B3A4C)` on the progress fill, `135deg` on the brand mark and the phone's add button) |
| `--gold` | `#C9A030` | same | the post-processing phase bar, and `warning` |
| `--ok` / `--err` | `#33C759` / `#D94040` | same | finished / error discs, the switch's on state, the destructive link |
| `--bg` | `#F2F2F7` | `#000000` | the page ground, and `background_color` in the manifest |
| `--card` | `#FFFFFF` | `#1C1C1E` | every card surface |
| `--field` | `#F2F2F7` | `#2C2C2E` | inputs, selects, chips, the segmented control |
| `--text` / `--text2` / `--text3` | `#000000` / `rgba(60,60,67,.6)` / `rgba(60,60,67,.3)` | `#FFFFFF` / `rgba(235,235,245,.6)` / `rgba(235,235,245,.3)` | title / secondary line / placeholder — the iOS label ramp, kept rather than re-invented |
| `--sep` / `--track` | `rgba(60,60,67,.16)` / `rgba(60,60,67,.12)` | `rgba(84,84,88,.5)` / `rgba(235,235,245,.14)` | hairlines; the unfilled progress track |
| `--shadow` | `0 2px 8px rgba(0,0,0,.08)` | `0 2px 8px rgba(0,0,0,.3)` | the one card shadow, used everywhere and never varied |

And the geometry, which is as much of the identity as the colour is:

| Artboard measurement | In `app.css` |
|---|---|
| 16 px card radius, one `0 2px 8px` shadow | `.card` — the add bar, each section's row list, both sheets |
| Spacing on a 4 / 8 / 12 / 16 / 24 scale, nothing between | every `gap`, `padding` and `margin` in the file |
| Type at 20 / 15 / 13 / 12 px in the system stack, with 17 px reserved for the two primary actions and the sheet title | `h1` / row title and body / secondary line / meta, then `.btn-primary`, `.btn-block` and `.text-btn` at 17. No web font is loaded, on purpose: `font-src 'self'` and a 70 KB budget |
| 32 px circular status disc, tinted `--accent`/`--ok`/`--err` at 14 % | `.disc`, one per row, the only place a status colour appears at full strength |
| 4 px gradient progress bar, 2 px radius | `.bar` / `.fill`, with a `transition: width .2s linear` so a 250 ms delta batch reads as motion rather than as a step |
| 3 px gold phase bar under it | `.bar.ph` / `.fill.phase`, shown only while `postprocessing` |
| Capsule status pill in the header | `.pill`, with an 8 px dot carrying the connection colour (`.live` green, `.wait` pulsing gold, `.off` red, `.auth` terracotta) |
| Phone: 48 px field + 48 px square gradient button, 14 px radius | the `max-width: 640px` block |
| Phone: 44 px minimum target, everywhere | segmented control, chips, selects, the row chevron (44 px with a `-12px` margin so it does not grow the row), the text buttons, and the 51×31 switch drawn as a `::before` inside a 44 px button |
| Phone: sheet with a grab handle, grouped rows, a full-width action button | `.sheet` / `.grab` / `.grp` / `.rowset` / `.btn-block` |

### The Subscriptions panel has no artboard

It was added after the boards were drawn (design D6), so it is the one surface with no still frame
to check against. It introduces **no new token**: it is the same `.card` list, the same 32 px
`.disc` (spinning while `checking`, `--err`-tinted on a failure), the same `.st`/`.rest` secondary
line, the same `.sw` switch the add sheet uses, and the same 4/8/12/16 spacing. What is new is
markup, and only four names:

| Class | What it is |
|---|---|
| `.sec-acts` | the two header links of a section that has more than one (`Check all`, `Add`) |
| `.subform` / `.subgrid` | the add form's card and its wrapping field grid — `.subgrid` rather than `.add-extra` because the phone layout hides that one |
| `.sub-line` | the row's secondary line, indented 44 px so it lines up under the title — it sits outside `.row-main` so a phone gives it the whole row rather than what four 44 px controls leave |
| `.subedit` / `.edit-acts` | the inline name/interval editor that unfolds under a row |
| `.btn-sm` | a 36 px flat-accent button (44 px on a phone), for `Save` inside that editor |

If the panel ever earns a redesign, draw the board first — the rule above still holds.

## Keeping them honest

Nothing mechanically diffs the artboards against the page — a pixel gate on a hand-drawn board is a
gate that gets muted. What exists instead: the smoke regenerates five screenshots of the **real**
page on every run (`tools/web/screenshots/`, gitignored, uploaded by CI's `web` job as an
artifact), framed to match the boards — desktop light and dark, phone light and dark, the phone add
sheet. Comparing those five against these three boards is a two-minute review, and it is the
intended one whenever the CSS changes.

If a change genuinely needs a *new* look rather than a correction, change the artboard first and
say so in the commit; a page that has quietly drifted from its own specification is worse than one
that never had a specification.
