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


class YoutubeDL:
    """The two methods the shim calls, plus the two helpers."""

    def __init__(self, params=None):
        self.params = dict(params or {})
        self.scenario = _scenario()
        self.postprocessors = {}
        self.debug_lines = []
        self.layout = {}

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
        self.postprocessors.setdefault(when, []).append(pp)
        pp.set_downloader(self)

    def get_output_path(self, dir_type="", filename=None):
        """``YoutubeDL.get_output_path``: ``paths[dir_type]`` joins **onto** ``paths['home']``.

        The join is what makes an absolute per-type entry win outright and a relative one land
        beside the temp files, which is the semantics the shim's redirect relies on.
        """
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
        """The debug channel the sidecar postprocessors report through."""
        self.debug_lines.append(str(message))

    def _run_pps(self, when, info):
        for pp in self.postprocessors.get(when, []):
            _, info = pp.run(info)
        return info

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

        os.makedirs(os.path.dirname(media) or ".", exist_ok=True)
        with open(media, "w", encoding="utf-8") as handle:
            handle.write("media\n")

        info["__files_to_move"] = files_to_move
        info = self._run_pps("before_dl", info)
        files_to_move = info.pop("__files_to_move", {})
        self.layout["registered"] = sorted(files_to_move)

        info["filepath"] = media
        info["__finaldir"] = os.path.dirname(os.path.abspath(final))
        info = self._run_pps("post_process", info)
        self.layout["at_post_process"] = _listing(os.path.dirname(media))

        files_to_move[media] = final
        for old, new in files_to_move.items():
            new = new or os.path.join(info["__finaldir"], os.path.basename(old))
            if os.path.abspath(old) == os.path.abspath(new) or not os.path.exists(old):
                continue
            os.replace(old, new)
        info["filepath"] = final
        info = self._run_pps("after_move", info)

        self.layout["home"] = _listing(os.path.dirname(final))
        self.layout["temp"] = _listing(os.path.dirname(media))
        self.layout["infojson_filename"] = info.get("infojson_filename")
        self.layout["debug"] = list(self.debug_lines)
        dump = os.environ.get("AULOS_STUB_DUMP")
        if dump:
            with open(dump, "w", encoding="utf-8") as handle:
                json.dump(
                    {
                        "paths": self.params.get("paths"),
                        "postprocessors": {
                            when: len(pps) for when, pps in self.postprocessors.items()
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
