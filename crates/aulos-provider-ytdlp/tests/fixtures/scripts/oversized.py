#!/usr/bin/env python3
"""A shim stand-in that writes one absurdly long line on fd 3.

The parent must kill it and report a ``contract`` failure rather than buffering the line: an
unbounded line is the fourth way child processes go wrong, and the 8 MiB cap of DESIGN §9.1 is
what stops a 4 GiB JSON line from taking the server with it.
"""

import json
import os
import time

channel = os.fdopen(3, "w", encoding="utf-8")
channel.write(json.dumps({"v": 1, "t": "hello", "n": 1, "ts": time.time(), "protocol": 1}) + "\n")
channel.flush()
channel.write('{"v":1,"t":"info","n":2,"ts":0,"entry":{"pad":"' + "z" * 200_000 + '"}}\n')
channel.flush()
time.sleep(30)
