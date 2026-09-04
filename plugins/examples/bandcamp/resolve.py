#!/usr/bin/env python3
"""Resolve a Bandcamp album or track into Aulos `json_lines` frames (DESIGN §6.5.3).

Contract, in full:

* argv[1] is the item URL (the manifest's ``{url}`` token).
* Print one JSON object per line on stdout. ``{"t":"group",...}`` opens a container,
  ``{"t":"entry",...}`` adds a child (a bare object with no ``t`` is also an entry),
  ``{"t":"note",...}`` logs, ``{"t":"error","code":...}`` fails the item with that code.
* ``media_id`` is optional and defaults to ``sha256(url)[:16]`` on the server side.
* Anything on stderr is kept as the error tail; exit non-zero to fail.

This is a worked example, not a maintained scraper: it reads whatever
``window.albumData = {...}`` blob the page carries, which is enough to show the shape.
"""

from __future__ import annotations

import json
import re
import sys
import urllib.error
import urllib.request

BLOB = re.compile(r"window\.albumData\s*=\s*(\{.*?\})\s*;", re.DOTALL)
TIMEOUT = 20


def fetch(url: str) -> str:
    request = urllib.request.Request(url, headers=headers())
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:  # noqa: S310
        return response.read().decode("utf-8", "replace")


def headers() -> dict[str, str]:
    """Every `[headers]` entry arrives as ``AULOS_HEADER_<NAME>`` (DESIGN §6.5.1)."""
    import os

    out = {"User-Agent": "aulos-bandcamp-example/0.3.1"}
    for key, value in os.environ.items():
        if key.startswith("AULOS_HEADER_"):
            out[key[len("AULOS_HEADER_") :].title().replace("_", "-")] = value
    return out


def emit(frame: dict[str, object]) -> None:
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def fail(code: str, message: str) -> None:
    emit({"t": "error", "code": code, "message": message, "retryable": code == "network"})
    sys.exit(1)


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        fail("contract", "usage: resolve.py <url>")
    url = argv[1]

    try:
        page = fetch(url)
    except urllib.error.HTTPError as e:
        fail("unavailable" if e.code == 404 else "network", f"HTTP {e.code} for {url}")
    except OSError as e:
        fail("network", f"could not reach {url}: {e}")

    match = BLOB.search(page)
    if not match:
        # Nothing recognisable: let the server retry through its fallback provider.
        fail("unsupported_url", "no album data on that page")

    try:
        album = json.loads(match.group(1))
    except json.JSONDecodeError as e:
        fail("contract", f"album data is not JSON: {e}")

    tracks = album.get("tracks") or []
    if len(tracks) > 1:
        emit(
            {
                "t": "group",
                "media_id": f"bc:album:{album.get('id', 'unknown')}",
                "title": album.get("title") or url,
                "kind": "playlist",
                "expected": len(tracks),
            }
        )

    skipped = 0
    for track in tracks:
        stream = track.get("stream_url")
        if not stream:
            skipped += 1
            continue
        emit(
            {
                "t": "entry",
                "media_id": f"bc:track:{track.get('id', 'unknown')}",
                "url": track.get("url") or url,
                "title": track.get("title") or "untitled",
                "duration": track.get("duration"),
                "ext": "flac",
                "state": {"stream_url": stream},
            }
        )

    if skipped:
        emit({"t": "note", "message": f"{skipped} track(s) are not streamable and were skipped"})
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
