"""Shared helpers for the WP-00 capture tools.

The tools import the *legacy* Python backend directly out of
``/Users/apogliaghi/Development/metube_pot/app`` so the golden corpora are
produced by the code being replaced, not by a re-reading of it.

Nothing here touches the network.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

# The legacy checkout. Overridable so the tools can be re-run against a
# different clone (a tag, a worktree) without editing them.
LEGACY_ROOT = Path(
    os.environ.get("METUBE_POT_ROOT", "/Users/apogliaghi/Development/metube_pot")
).resolve()
LEGACY_APP = LEGACY_ROOT / "app"

# Where the corpora land. `tools/capture/` -> repo root.
REPO_ROOT = Path(__file__).resolve().parents[2]
GOLDEN_DIR = REPO_ROOT / "tests" / "golden"
V1_GOLDEN_DIR = REPO_ROOT / "tests" / "v1_golden"


def add_legacy_to_syspath() -> None:
    """Put the legacy ``app`` package dir on ``sys.path``.

    ``app/dl_formats.py`` and ``app/ytdl.py`` are imported as top-level modules
    (``import dl_formats``), which is how the legacy server itself imports them
    (``main.py`` runs with ``app/`` as the script dir).
    """
    if not LEGACY_APP.is_dir():
        raise SystemExit(f"legacy app dir not found: {LEGACY_APP}")
    for p in (str(LEGACY_APP), str(LEGACY_ROOT)):
        if p not in sys.path:
            sys.path.insert(0, p)


def legacy_commit() -> str:
    """The legacy git commit the corpus was derived from."""
    try:
        out = subprocess.run(
            ["git", "-C", str(LEGACY_ROOT), "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout.strip()
    except (OSError, subprocess.CalledProcessError):  # pragma: no cover
        return "unknown"


def ytdlp_version() -> str:
    try:
        import yt_dlp.version  # noqa: PLC0415

        return str(yt_dlp.version.__version__)
    except Exception:  # pragma: no cover
        return "unknown"


def write_json(path: Path, payload: object) -> None:
    """Write canonical JSON: sorted keys, 2-space indent, trailing newline.

    Canonical because these files are diffed by Rust tests and by humans; an
    unstable key order would turn a no-op re-capture into a large diff.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(payload, sort_keys=True, indent=2, ensure_ascii=False)
    path.write_text(text + "\n", encoding="utf-8")
    print(f"wrote {path.relative_to(REPO_ROOT)} ({len(text) + 1} bytes)")


# ---------------------------------------------------------------------------
# The legal request space, transcribed from `app/main.py:parse_download_options`
# (legacy-backend-spec §2.2). Kept here so both dump tools and verify.py agree
# on what "the whole space" means.
# ---------------------------------------------------------------------------

VIDEO_CODECS = ("auto", "h264", "h265", "av1", "vp9")
VIDEO_FORMATS = ("any", "mp4", "ios")
VIDEO_QUALITIES = (
    "best",
    "worst",
    "2160",
    "1440",
    "1080",
    "720",
    "480",
    "360",
    "240",
)
AUDIO_FORMATS = ("m4a", "mp3", "opus", "wav", "flac")
AUDIO_EXTRA_QUALITIES = {"mp3": ("320", "192", "128"), "m4a": ("192", "128")}
CAPTION_FORMATS = ("srt", "txt", "vtt", "ttml", "sbv", "scc", "dfxp")
CAPTION_MODES = ("auto_only", "manual_only", "prefer_manual", "prefer_auto")
CAPTION_LANGUAGES = ("en", "pt-BR", "it")
THUMBNAIL_FORMATS = ("jpg",)


def legal_tuples() -> list[dict[str, str]]:
    """Every ``(download_type, codec, format, quality)`` the legacy API admits.

    Mirrors the per-type allow-lists in ``parse_download_options`` exactly,
    including ``best_remux`` being legal only for ``video``/``mp4`` and ``codec``
    being forced to ``auto`` for every non-video type.
    """
    out: list[dict[str, str]] = []
    for fmt in VIDEO_FORMATS:
        qualities = list(VIDEO_QUALITIES)
        if fmt == "mp4":
            qualities.append("best_remux")
        for codec in VIDEO_CODECS:
            for q in qualities:
                out.append(
                    {
                        "download_type": "video",
                        "codec": codec,
                        "format": fmt,
                        "quality": q,
                    }
                )
    for fmt in AUDIO_FORMATS:
        for q in ("best", *AUDIO_EXTRA_QUALITIES.get(fmt, ())):
            out.append(
                {
                    "download_type": "audio",
                    "codec": "auto",
                    "format": fmt,
                    "quality": q,
                }
            )
    for fmt in CAPTION_FORMATS:
        out.append(
            {
                "download_type": "captions",
                "codec": "auto",
                "format": fmt,
                "quality": "best",
            }
        )
    for fmt in THUMBNAIL_FORMATS:
        out.append(
            {
                "download_type": "thumbnail",
                "codec": "auto",
                "format": fmt,
                "quality": "best",
            }
        )
    return out


def tuple_key(t: dict[str, str]) -> str:
    return "{download_type}|{codec}|{format}|{quality}".format(**t)
