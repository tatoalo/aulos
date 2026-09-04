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
