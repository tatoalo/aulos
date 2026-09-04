#!/usr/bin/env python3
"""Download one Bandcamp track for Aulos (DESIGN §6.5.3).

Contract, in full:

* The argv comes from the manifest's ``download.command`` template, substituted **at argv level**:
  no shell, so nothing in a title or a URL can add an argument.
* Progress goes to stdout or stderr in whatever shape ``[progress]`` declares. This example uses
  the manifest's ``regex`` patterns: ``12.0% 1.2MiB / 10.0MiB``, ``1.5MiB/s``, ``ETA 00:07`` and
  ``stage=fetch|mux|done``, repainted with ``\\r`` exactly as a real downloader would.
* Success is ``expect_output = "result_frame"``: exit 0 **and** one
  ``{"t":"result","path":...,"size":...}`` line.
* A non-zero exit surfaces the last 2 KiB of stderr to the user, so write something useful there.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

TIMEOUT = 30
CHUNK = 64 * 1024


def headers() -> dict[str, str]:
    out = {"User-Agent": "aulos-bandcamp-example/0.3.1"}
    for key, value in os.environ.items():
        if key.startswith("AULOS_HEADER_"):
            out[key[len("AULOS_HEADER_") :].title().replace("_", "-")] = value
    return out


def human(n: float) -> str:
    for unit in ("B", "KiB", "MiB", "GiB"):
        if n < 1024 or unit == "GiB":
            return f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}GiB"


def progress(done: int, total: int, started: float) -> None:
    elapsed = max(time.monotonic() - started, 1e-6)
    speed = done / elapsed
    percent = (done / total * 100.0) if total else 0.0
    eta = int((total - done) / speed) if total and speed > 0 else 0
    sys.stderr.write(
        f"\rstage=fetch {percent:.1f}% {human(done)} / {human(total or done)}"
        f"  {human(speed)}/s ETA {eta // 60:02d}:{eta % 60:02d}"
    )
    sys.stderr.flush()


def main() -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--url", required=True)
    parser.add_argument("--state", default="{}")
    parser.add_argument("--quality", default="best")
    parser.add_argument("--out", required=True)
    parser.add_argument("--tmp", required=True)
    args = parser.parse_args()

    try:
        state = json.loads(args.state) if args.state else {}
    except json.JSONDecodeError:
        state = {}
    stream = state.get("stream_url") or args.url

    part = os.path.join(args.tmp, os.path.basename(args.out) + ".part")
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    os.makedirs(args.tmp, exist_ok=True)

    started = time.monotonic()
    try:
        request = urllib.request.Request(stream, headers=headers())
        with urllib.request.urlopen(request, timeout=TIMEOUT) as response:  # noqa: S310
            total = int(response.headers.get("Content-Length") or 0)
            done = 0
            with open(part, "wb") as sink:
                while True:
                    chunk = response.read(CHUNK)
                    if not chunk:
                        break
                    sink.write(chunk)
                    done += len(chunk)
                    progress(done, total, started)
    except urllib.error.HTTPError as e:
        print(f"bandcamp returned HTTP {e.code} for {stream}", file=sys.stderr)
        return 1
    except OSError as e:
        print(f"could not fetch {stream}: {e}", file=sys.stderr)
        return 1

    sys.stderr.write("\nstage=mux\n")
    sys.stderr.flush()
    os.replace(part, args.out)
    sys.stderr.write("stage=done\n")
    sys.stderr.flush()

    print(
        json.dumps(
            {
                "t": "result",
                "path": args.out,
                "size": os.path.getsize(args.out),
                "artifacts": [],
            }
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
