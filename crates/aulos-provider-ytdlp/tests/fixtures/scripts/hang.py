#!/usr/bin/env python3
"""A shim stand-in that starts a grandchild, reports a partial file, and then refuses to die.

Drives the cancellation acceptance test of PLAN WP-07: the parent must ``killpg`` the whole group
(so the grandchild dies too, which legacy's ``proc.kill()`` did not), and must clean up the
``.part`` file the job left behind (Δ C18).

argv: ``<out_dir> <grandchild_pid_file>``
"""

import json
import os
import signal
import subprocess
import sys
import time

out_dir, pid_file = sys.argv[1], sys.argv[2]

# Ignore SIGTERM, so the SIGKILL half of the grace period is what actually ends this process.
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGINT, signal.SIG_IGN)

GRANDCHILD = (
    "import signal, time\n"
    "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
    "time.sleep(600)\n"
)
grandchild = subprocess.Popen([sys.executable, "-c", GRANDCHILD], close_fds=True)
with open(pid_file, "w", encoding="utf-8") as handle:
    handle.write(str(grandchild.pid))

os.makedirs(out_dir, exist_ok=True)
partial = os.path.join(out_dir, "hanging.mp4.part")
with open(partial, "w", encoding="utf-8") as handle:
    handle.write("partial bytes")

channel = os.fdopen(3, "w", encoding="utf-8")


def emit(n, **fields):
    """Writes one frame on fd 3."""
    channel.write(json.dumps(dict({"v": 1, "n": n, "ts": time.time()}, **fields)) + "\n")
    channel.flush()


emit(1, t="hello", protocol=1, yt_dlp="stub", python="3", pid=os.getpid(), plugins=[])
emit(
    2,
    t="progress",
    status="downloading",
    tmpfilename=partial,
    downloaded_bytes=1,
    total_bytes=100,
    elapsed=0.1,
    stream="video",
)

while True:
    time.sleep(30)
