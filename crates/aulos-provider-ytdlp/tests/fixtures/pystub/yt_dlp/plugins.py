"""Stub of ``yt_dlp.plugins``: loads the noisy plugin so ``hello`` can report it."""

PACKAGE_NAME = "yt_dlp_plugins"


def load_all_plugins():
    """Imports the stub plugin package, exactly as upstream imports real ones."""
    import yt_dlp_plugins.extractor.noisy  # noqa: F401
