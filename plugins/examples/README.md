# Aulos plugins by example

A plugin is **one directory with one `plugin.toml`**. Drop it in `AULOS_PLUGINS_DIR` (default
`/config/plugins`, from `PLUGINS_DIR`) and Aulos picks it up at boot, on `SIGHUP`, on
`POST api/v2/plugins/reload`, and about a second after you save the file. No recompile, no restart,
and any language you like.

There are two things a directory can declare, and it may declare both:

| | Adds | Needs |
|---|---|---|
| a **provider** | support for a new site | `[match]` + `[download]` |
| a **hook** | something that happens after a download | one or more `[[hook]]` tables |

## The two examples here

### `bandcamp/` — a provider

The manifest from `docs/DESIGN.md` §6.5.4 plus a ~100-line `resolve.py` and `download.py`. It
shows the whole contract:

- `resolve.py` turns a URL into **frames** on stdout, one JSON object per line:
  `{"t":"group",…}` opens a container, `{"t":"entry",…}` adds a child (a bare object with no `t`
  is also an entry), `{"t":"note",…}` logs, `{"t":"error","code":…,"message":…}` fails the item
  with that exact error code. `media_id` is optional — it defaults to `sha256(url)[:16]`.
- `download.py` receives its arguments from the manifest's `download.command` template, writes the
  file, prints progress in whatever shape the manifest's `[progress]` table declares, and finishes
  with `{"t":"result","path":…,"size":…}`.
- The `state` object an entry carries is handed back verbatim at download time as `{state}` (or
  `{state.<key>}`), so a just-in-time token or stream URL survives the trip through the queue.

`crates/aulos-provider/tests/plugin_example.rs` runs this exact directory end to end against a
local HTTP stub, so it is a working reference rather than a sketch.

### `media-server-hooks/` — hooks only

The Plex, Emby, ntfy and "run my script" examples from `docs/DESIGN.md` §13.4. No `[match]`, no
`[download]`: a hook-only manifest is a normal, valid plugin. `${PLEX_TOKEN}` and friends are
interpolated from the server's own environment **at load time**, so secrets stay in your compose
file.

## The eight things worth knowing before you write one

1. **Names.** The directory name must match `^[a-z0-9][a-z0-9_-]{0,31}$` and becomes the provider
   id `command:<dirname>`.
2. **A broken manifest is loud, not silent.** Every mistake is caught when the file loads, with the
   key it is about — an unknown `{token}`, a regex that does not compile, an `argv[0]` that is not
   on `PATH`, an unknown progress capture group, a duplicate catalog id. The plugin is then
   registered *degraded*: it still claims its URLs, `GET api/v2/providers` and `healthz` show the
   reason, and items routed to it fail with `provider_degraded` instead of quietly falling through
   to yt-dlp and downloading a login page.
3. **There is no shell.** Each argv element is templated on its own and passed to `execvp`
   verbatim, so nothing in a title or URL can add an argument. If you want a shell, write
   `command = ["/bin/sh", "-c", "…"]` and own the consequences.
4. **An unknown token is an error, not an empty string.** `{out_dirr}` fails to load and tells you
   the offset. The full token table is in `docs/DESIGN.md` §6.5.1.
5. **Progress is cheap to declare.** `kind = "json_lines"` if your tool can print JSON;
   `kind = "regex"` with named groups (`percent`, `downloaded`, `total`, `speed`, `eta`, `status`,
   `fragment_index`, `fragment_count`, `msg`) if it cannot. `strip_ansi`, `cr_as_newline` and
   `last_match_wins` are on by default, which is what makes a `\r`-repainting, ANSI-coloured
   progress bar just work. Size suffixes are read **1024-based**, `KB` and `KiB` alike.
6. **Say when you are done.** `expect_output = "path_template"` (the default) means "exit 0 and
   `{out_path}` exists and is non-empty"; `"result_frame"` means "print a `result` line";
   `"newest_in_dir"` means "leave the newest file in `{out_dir}`".
7. **Failures should say something.** A non-zero exit surfaces the last 2 KiB of your stderr to the
   user, ANSI-stripped. Write the reason there.
8. **Plugins are not a security boundary.** A plugin runs as the server user and can do anything
   that user can. Aulos gives it a cleared environment (plus `env.pass` / `env.set`), its own
   process group, `nice(5)`, `[limits]`-derived rlimits, a bounded output budget and a stall
   watchdog — that bounds accidents, not malice. The plugin directory is operator-controlled by
   definition, and `GET api/v2/providers` publishes every plugin's full argv so you can audit what
   is installed.

The complete schema — every key, its type, its default and what it means — is
`docs/DESIGN.md` §6.5.1 for providers and §13.4 for hooks.
