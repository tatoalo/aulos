#!/usr/bin/env python3
"""A shim stand-in that writes nothing and exits with the code named in argv[1].

Proves the parent turns a frameless run into a `contract` failure that names the exit code and
the stderr tail, instead of hanging or reporting success.
"""

import sys

sys.stderr.write("ERROR: the shim could not start\n")
sys.exit(int(sys.argv[1]))
