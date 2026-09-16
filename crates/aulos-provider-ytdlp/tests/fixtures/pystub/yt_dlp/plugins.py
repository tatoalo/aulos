"""Stub of ``yt_dlp.plugins``: loads the noisy plugin so ``hello`` can report it."""

import sys
from types import SimpleNamespace

PACKAGE_NAME = "yt_dlp_plugins"
all_plugins_loaded = SimpleNamespace(value=False)
load_calls = 0


def load_all_plugins():
    """Imports the stub plugin package, exactly as upstream imports real ones."""
    global load_calls
    load_calls += 1
    if load_calls > 1:
        sys.stderr.write("AssertionError: PoTokenProvider BgUtilCli already registered\n")
        raise AssertionError("PoTokenProvider BgUtilCli already registered")
    import yt_dlp_plugins.extractor.noisy  # noqa: F401
    all_plugins_loaded.value = True
