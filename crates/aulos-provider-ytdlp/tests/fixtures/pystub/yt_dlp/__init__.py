"""A scripted stand-in for ``yt_dlp``, so the shim contract is testable with no network.

The stub reads a scenario from the JSON file named by ``AULOS_STUB_SCENARIO`` and does exactly
what it says: yields entries, fires progress and postprocessor hooks, prints to stdout, writes to
stderr, creates files, or raises a named exception. Every acceptance test in PLAN WP-07 that needs
"yt-dlp behaved like *this*" drives it through that file.

It deliberately **prints to stdout on import**. So does the real BgUtils POT plugin, and so does
``yt-dlp-ejs``. If the protocol were on stdout, this line alone would corrupt it — which is the
regression the fd-3 design exists to prevent (DESIGN §9.1, §23.1 B1).

Scenario keys (all optional):

``extract``          the info dict ``extract_info`` returns.
``extract_entries``  a count; synthesises that many flat playlist children.
``progress``         a list of ``progress_hooks`` dicts to fire, in order.
``pp``               a list of ``postprocessor_hooks`` dicts to fire, in order.
``print_stdout``     lines printed to stdout during ``download``.
``stderr_bytes``     that many bytes written to stderr before anything else.
``write_files``      paths to create (so the shim's ``os.path.getsize`` succeeds).
``raise``            ``{"class": "...", "message": "...", "errno": 28}`` raised instead.
``retcode``          what ``download`` returns. Defaults to 0.
``sleep``            seconds to sleep at the end of ``download``.
``sidecars``         runs the miniature ``process_info`` of ``_run_sidecar_plan`` below.

``sidecars`` keys: ``info`` (the info dict), ``write`` (output types written through
``paths[<type>]``), ``write_registered`` (temp-dir files yt-dlp registers itself, as it does for
subtitles), ``write_plain`` (temp-dir work files nothing registers), ``user_pps`` (the operator's
own postprocessors, registered *before* the shim's) and ``break_sweep`` (make the downloader
raise once the ``post_process`` chain starts, so the shim's error path is testable).
"""

import json
import os
import sys

from yt_dlp import utils
from yt_dlp.version import __version__

__all__ = ["YoutubeDL", "__version__", "utils"]

# The stdout pollution that makes the fd-3 decision load-bearing.
print("STUB NOISE: yt_dlp imported")
sys.stdout.write('{"v":1,"t":"bye","n":99,"forged":true}\n')
sys.stdout.flush()


_OUTTMPL_EXTS = {"description": "description", "infojson": "info.json"}
"""``yt_dlp.utils.OUTTMPL_TYPES``, for the two types the sidecar tests exercise."""


def _listing(directory):
    """The sorted file names in ``directory``, or ``[]`` when it does not exist."""
    try:
        return sorted(os.listdir(directory))
    except OSError:
        return []


def _scenario():
    path = os.environ.get("AULOS_STUB_SCENARIO")
    if not path:
        return {}
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def _raise(spec):
    cls = getattr(utils, spec["class"], None)
    message = spec.get("message", "boom")
    if cls is not None:
        if spec["class"] == "DownloadError" and spec.get("wraps") == "URLError":
            import urllib.error

            raise cls(message, exc_info=(None, urllib.error.URLError("unreachable"), None))
        raise cls(message)
    if spec["class"] == "OSError":
        err = OSError(message)
        err.errno = spec.get("errno", 28)
        raise err
    if spec["class"] == "KeyboardInterrupt":
        raise KeyboardInterrupt(message)
    raise RuntimeError(message)


class _StubUserPP:
    """A user postprocessor, of the two shapes the sidecar tests need.

    ``legacy_nfo`` replicates MeTube's ``jellyfin_nfo_generator.py`` exactly as an ``Exec``
    postprocessor runs it: derive ``<base>.info.json`` from ``%(filepath)q``, return early when it
    is not there, otherwise write ``<base>.nfo`` **beside the media** and delete the info json it
    just consumed. That is the script every migrating MeTube user carries, and its two side
    effects — a new file in the scratch directory, a registered file removed from it — are the
    whole reason the shim sweeps rather than naming types.

    ``probe`` only records what it saw, which is how the tests read a user postprocessor's view of
    the world at its own ``when``.
    """

    def __init__(self, ydl, spec):
        self._ydl = ydl
        self._spec = dict(spec)
        self.name = spec.get("name") or spec.get("kind", "probe")

    def set_downloader(self, downloader):
        self._ydl = downloader

    def run(self, info):
        filepath = info.get("filepath") or ""
        base = os.path.splitext(filepath)[0]
        seen = {
            "pp": self.name,
            "when": self._spec.get("when", "post_process"),
            "filepath": filepath,
            "infojson_filename": info.get("infojson_filename"),
            "siblings": _listing(os.path.dirname(filepath)),
        }
        if self._spec.get("kind") == "legacy_nfo":
            info_json = base + ".info.json"
            seen["info_json_found"] = os.path.isfile(info_json)
            if seen["info_json_found"]:
                with open(base + ".nfo", "w", encoding="utf-8") as handle:
                    handle.write("<movie/>\n")
                os.remove(info_json)
        self._ydl.user_log.append(seen)
        return [], info


class YoutubeDL:
    """The two methods the shim calls, plus the two helpers."""

    POSTPROCESS_WHEN = ("pre_process", "after_filter", "before_dl", "post_process", "after_move")

    def __init__(self, params=None):
        from yt_dlp import plugins
        if not plugins.all_plugins_loaded.value:
            plugins.load_all_plugins()
        self.params = dict(params or {})
        self.scenario = _scenario()
        # Upstream's own attribute name and shape: the shim splices its `after_move`
        # postprocessor into the head of this list, so the stub has to be `_pps`.
        self._pps = {when: [] for when in self.POSTPROCESS_WHEN}
        self.debug_lines = []
        self.warnings = []
        self.layout = {}
        self.user_log = []
        # Armed just before the `post_process` chain runs, so a scenario can break the shim's
        # sweep the way a yt-dlp change would: at its first call into the downloader.
        self._break_get_output_path = False
        # The user's own postprocessors exist before the shim registers anything, exactly as
        # `YoutubeDL.__init__` builds them from `params['postprocessors']`.
        for spec in ((self.scenario.get("sidecars") or {}).get("user_pps") or []):
            self.add_post_processor(_StubUserPP(self, spec), when=spec.get("when", "post_process"))

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        return False

    @staticmethod
    def sanitize_info(info):
        """Upstream strips unserialisable values; the stub's dicts are already clean."""
        return info

    def evaluate_outtmpl(self, template, info):
        """A `%(key)s`-only subset of the real engine, enough for the round-trip test."""
        out = template
        for key, value in info.items():
            for spec in (f"%({key})s", f"%({key})d", f"%({key})02d"):
                out = out.replace(spec, str(value))
        return out

    def add_post_processor(self, pp, when="post_process"):
        """Upstream appends to ``_pps[when]`` and calls ``set_downloader``; so does this."""
        self._pps.setdefault(when, []).append(pp)
        pp.set_downloader(self)

    def get_output_path(self, dir_type="", filename=None):
        """``YoutubeDL.get_output_path``: ``paths[dir_type]`` joins **onto** ``paths['home']``.

        The join is what makes an absolute per-type entry win outright and a relative one land
        beside the temp files, which is the semantics the shim's redirect relies on.
        """
        if self._break_get_output_path:
            raise RuntimeError("get_output_path is broken in this scenario")
        paths = self.params.get("paths") or {}
        return os.path.join(
            paths.get("home", ""),
            paths.get(dir_type, "") if dir_type else "",
            filename or "",
        )

    def prepare_filename(self, info, dir_type=""):
        """``YoutubeDL.prepare_filename`` for the flat ``%(title)s.%(ext)s`` the tests use."""
        ext = _OUTTMPL_EXTS.get(dir_type) or info.get("ext") or "mp4"
        title = info.get("title", "clip")
        return self.get_output_path(dir_type, f"{title}.{ext}")

    def write_debug(self, message):
        """The debug channel the sidecar postprocessors chatter on."""
        self.debug_lines.append(str(message))

    def report_warning(self, message, only_once=False):
        """Upstream hands this to ``params['logger'].warning``; the stub records it too.

        Both halves matter: the test asserts the shim reports a registration failure at every
        ``LOGLEVEL`` (so it must reach the logger), and that a clean download reports nothing.
        """
        self.warnings.append(str(message))
        logger = self.params.get("logger")
        if logger is not None:
            logger.warning(message)

    def _run_pps(self, when, info):
        for pp in self._pps.get(when, []):
            _, info = pp.run(info)
        return info

    def _pp_names(self, when):
        """The chain in registration order, by the name a test can recognise."""
        return [getattr(pp, "name", None) or type(pp).__name__ for pp in self._pps.get(when, [])]

    def _run_sidecar_plan(self, plan):
        """A miniature of ``YoutubeDL.process_info``, faithful to the three steps under test.

        1. The sidecars are written to ``prepare_filename(info, <type>)`` — ``paths.home`` unless
           the type carries its own entry — while the media is written to the temp directory.
        2. ``before_dl`` postprocessors run with ``__files_to_move`` threaded in **and out**
           (upstream: ``new_info, files_to_move = self.pre_process(info_dict, 'before_dl',
           files_to_move)``).
        3. ``MoveFilesAfterDownloadPP`` moves every registered file, resolving a falsy
           destination to ``join(__finaldir, basename)``; then ``after_move`` postprocessors run.
        """
        info = dict(plan.get("info") or {"title": "Stub clip", "ext": "mp4"})
        media = self.get_output_path("temp", "{}.{}".format(info["title"], info["ext"]))
        final = self.prepare_filename(info)
        files_to_move = {}

        for kind in plan.get("write", []):
            path = self.prepare_filename(info, kind)
            os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(f"{kind}\n")
            if kind == "infojson":
                info["infojson_filename"] = path
                info["__infojson_filename"] = path
            # Upstream registers the subtitle and thumbnail it wrote next to the temp file; the
            # types this plan writes through `paths[<type>]` it does not.
        for kind, path in (plan.get("write_registered") or {}).items():
            path = os.path.join(self.get_output_path("temp"), path)
            os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(f"{kind}\n")
            files_to_move[path] = os.path.join(os.path.dirname(final), os.path.basename(path))

        for name in plan.get("write_plain", []):
            # A work file yt-dlp itself would never move: a `.part`, a fragment, or a sidecar
            # belonging to some other job that shares the scratch directory.
            with open(os.path.join(self.get_output_path("temp"), name), "w",
                      encoding="utf-8") as handle:
                handle.write("work\n")

        os.makedirs(os.path.dirname(media) or ".", exist_ok=True)
        with open(media, "w", encoding="utf-8") as handle:
            handle.write("media\n")

        info["__files_to_move"] = files_to_move
        info = self._run_pps("before_dl", info)
        files_to_move = info.pop("__files_to_move", {})
        self.layout["registered"] = sorted(files_to_move)

        # `YoutubeDL.post_process`: the dict `before_dl` handed back is threaded straight into
        # the `post_process` chain, and it is the one `MoveFilesAfterDownloadPP` then reads.
        info["filepath"] = media
        info["__finaldir"] = os.path.dirname(os.path.abspath(final))
        info["__files_to_move"] = files_to_move
        self._break_get_output_path = bool(plan.get("break_sweep"))
        info = self._run_pps("post_process", info)
        self._break_get_output_path = False
        files_to_move = info.pop("__files_to_move", {})
        self.layout["at_post_process"] = _listing(os.path.dirname(media))

        self.layout["moving"] = sorted(files_to_move)
        files_to_move[media] = final
        for old, new in files_to_move.items():
            new = new or os.path.join(info["__finaldir"], os.path.basename(old))
            if os.path.abspath(old) == os.path.abspath(new):
                continue
            if not os.path.exists(old):
                # `MoveFilesAfterDownloadPP.run`: an entry whose source is gone is a warning on
                # a download that otherwise went perfectly, which is why the shim prunes them.
                self.report_warning(f'File "{old}" cannot be found')
                continue
            os.replace(old, new)
        info["filepath"] = final
        info = self._run_pps("after_move", info)

        self.layout["home"] = _listing(os.path.dirname(final))
        self.layout["temp"] = _listing(os.path.dirname(media))
        self.layout["infojson_filename"] = info.get("infojson_filename")
        self.layout["debug"] = list(self.debug_lines)
        self.layout["warnings"] = list(self.warnings)
        self.layout["user"] = list(self.user_log)
        dump = os.environ.get("AULOS_STUB_DUMP")
        if dump:
            with open(dump, "w", encoding="utf-8") as handle:
                json.dump(
                    {
                        "paths": self.params.get("paths"),
                        "postprocessors": {
                            when: self._pp_names(when)
                            for when in self.POSTPROCESS_WHEN
                            if self._pps.get(when)
                        },
                        "layout": self.layout,
                    },
                    handle,
                )

    def extract_info(self, url, download=False):
        """Returns the scenario's info dict, or a synthesised playlist / single video."""
        if "raise_extract" in self.scenario:
            _raise(self.scenario["raise_extract"])
        if "raise" in self.scenario:
            _raise(self.scenario["raise"])
        if self.scenario.get("extract_none"):
            return None
        count = self.scenario.get("extract_entries")
        if count:
            return {
                "_type": "playlist",
                "id": "PLSTUB",
                "title": "Stub playlist",
                "webpage_url": url,
                "extractor": "stub",
                "playlist_count": count,
                "uploader": "Stub",
                "entries": (
                    {
                        "_type": "url",
                        "id": f"v{i}",
                        "title": f"Track {i}",
                        "url": f"https://stub.test/watch/{i}",
                        "webpage_url": f"https://stub.test/watch/{i}",
                        "duration": float(i),
                    }
                    for i in range(1, count + 1)
                ),
            }
        info = self.scenario.get("extract")
        if info is not None:
            return dict(info, webpage_url=info.get("webpage_url", url))
        return {
            "_type": "video",
            "id": "stub1",
            "title": "Stub clip",
            "url": url,
            "webpage_url": url,
            "ext": "mp4",
            "duration": 1.0,
        }

    def download(self, urls):
        """Fires the scripted hooks and returns the scripted retcode."""
        if self.params.get("match_filter"):
            self.params["match_filter"](self.scenario.get("extract") or {}, incomplete=False)
        noise = self.scenario.get("stderr_bytes", 0)
        if noise:
            chunk = "x" * 79 + "\n"
            written = 0
            while written < noise:
                sys.stderr.write(chunk)
                written += len(chunk)
            sys.stderr.write("STDERR TAIL MARKER\n")
            sys.stderr.flush()

        for line in self.scenario.get("print_stdout", []):
            print(line)
            sys.stdout.flush()

        for path in self.scenario.get("write_files", []):
            os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write("stub payload\n")

        if self.scenario.get("sidecars"):
            self._run_sidecar_plan(self.scenario["sidecars"])

        if "raise" in self.scenario:
            _raise(self.scenario["raise"])

        for hook in self.params.get("progress_hooks", []):
            for frame in self.scenario.get("progress", []):
                hook(dict(frame))
        for hook in self.params.get("postprocessor_hooks", []):
            for frame in self.scenario.get("pp", []):
                hook(dict(frame))

        sleep = self.scenario.get("sleep")
        if sleep:
            import time

            time.sleep(sleep)
        return self.scenario.get("retcode", 0)
