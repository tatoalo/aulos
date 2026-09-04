#!/usr/bin/env python3
"""WP-00 — seed a scratch ``STATE_DIR`` for the v1 golden capture.

Writes ``queue.json`` / ``pending.json`` / ``completed.json`` /
``subscriptions.json`` in the ``schema_version: 2`` ``AtomicJsonStore`` shape
(legacy spec §5.2 / §7.2) so that every network-free legacy route has something
real to return:

* one queue item per non-terminal status (``downloading``, ``preparing``,
  ``pending``),
* a pending item and a *pre-download-problem* pending item (an upcoming
  livestream, which legacy stores as ``status: pending`` with a populated
  ``error`` — DESIGN §11.4),
* a ``finished`` and an ``error`` row plus enough filler to push ``done`` past
  600 rows (the window-vs-whole-set case of DESIGN §11.4), including rows that
  exercise the projection's odd corners: ``custom_name_prefix`` baked into
  ``id``/``title``, ``chapter_files``, a captions row and a thumbnail row,
* two subscriptions, one enabled and one disabled.

Two properties matter for a network-free capture and are load-bearing:

1. the server must be started with ``MAX_CONCURRENT_DOWNLOADS=0`` so the
   auto-restart of ``queue.json`` (``DownloadQueue.__import_queue``) blocks on
   the semaphore forever and the seeded statuses survive verbatim;
2. the enabled subscription carries a ``last_checked`` in the future and a long
   ``check_interval_minutes``, so the 60 s subscription tick finds nothing due
   (and, unlike ``time.time()``, a re-seed produces no diff).

Usage:
    python tools/capture/seed_state.py <state_dir>
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

SCHEMA_VERSION = 2

# A fixed base so a re-seed is byte-identical apart from `last_checked`.
BASE_TS_NS = 1_756_000_000_000_000_000


def _record(**kw: object) -> dict[str, object]:
    """One `_PERSISTED_DOWNLOAD_FIELDS` record, with the legacy defaults filled in.

    Only non-None values are ever written by the legacy serialiser, so None
    values are dropped here too.
    """
    rec: dict[str, object] = {
        "quality": "best",
        "download_type": "video",
        "codec": "auto",
        "format": "any",
        "folder": "",
        "custom_name_prefix": "",
        "playlist_item_limit": 0,
        "split_by_chapters": False,
        "chapter_template": "",
        "subtitle_language": "en",
        "subtitle_mode": "prefer_manual",
        "ytdl_options_presets": [],
        "ytdl_options_overrides": {},
        "status": "pending",
    }
    rec.update(kw)
    return {k: v for k, v in rec.items() if v is not None}


def _store(kind: str, items: list[dict[str, object]]) -> dict[str, object]:
    return {"schema_version": SCHEMA_VERSION, "kind": kind, "items": items}


def _entry(rec: dict[str, object]) -> dict[str, object]:
    return {"key": rec["url"], "info": rec}


def queue_items() -> list[dict[str, object]]:
    return [
        _entry(
            _record(
                id="dQw4w9WgXcQ",
                title="Never Gonna Give You Up",
                url="https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                status="downloading",
                timestamp=BASE_TS_NS + 1,
            )
        ),
        _entry(
            _record(
                id="jNQXAC9IVRw",
                title="Me at the zoo",
                url="https://www.youtube.com/watch?v=jNQXAC9IVRw",
                download_type="audio",
                format="m4a",
                quality="best",
                status="preparing",
                timestamp=BASE_TS_NS + 2,
            )
        ),
        _entry(
            _record(
                id="76979871",
                title="The New Vimeo Player",
                url="https://vimeo.com/76979871",
                format="mp4",
                quality="1080",
                status="pending",
                timestamp=BASE_TS_NS + 3,
            )
        ),
    ]


def pending_items() -> list[dict[str, object]]:
    return [
        _entry(
            _record(
                id="9bZkp7q19f0",
                title="PSY - GANGNAM STYLE",
                url="https://www.youtube.com/watch?v=9bZkp7q19f0",
                quality="720",
                status="pending",
                timestamp=BASE_TS_NS + 11,
            )
        ),
        # The pre-download-problem row: legacy stores an upcoming livestream as
        # `pending` with a populated `error` (DESIGN §11.4, §8.4).
        _entry(
            _record(
                id="upcoming-livestream-1",
                title="A Scheduled Premiere",
                url="https://www.youtube.com/watch?v=upcoming00001",
                status="pending",
                error="Live stream is scheduled to start at 2026-12-31 20:00:00 +0000",
                timestamp=BASE_TS_NS + 12,
            )
        ),
    ]


def completed_items() -> list[dict[str, object]]:
    items = [
        _entry(
            _record(
                id="BigBuckBunny_124",
                title="Big Buck Bunny",
                url="https://archive.org/details/BigBuckBunny_124",
                status="finished",
                filename="Big Buck Bunny.mp4",
                size=158008374,
                timestamp=BASE_TS_NS + 101,
            )
        ),
        _entry(
            _record(
                id="unavailable00001",
                title="unavailable00001",
                url="https://www.youtube.com/watch?v=unavailable00001",
                status="error",
                error="ERROR: [youtube] unavailable00001: Video unavailable",
                msg="ERROR: [youtube] unavailable00001: Video unavailable",
                timestamp=BASE_TS_NS + 102,
            )
        ),
        # `custom_name_prefix` is baked into the persisted id/title by
        # DownloadInfo.__init__ and is NOT re-applied on load, so the record
        # already carries the "prefix.<value>" form (DESIGN §11.4).
        _entry(
            _record(
                id="Lecture 3.abcdefghijk",
                title="Lecture 3.Introduction to Rust",
                url="https://www.youtube.com/watch?v=abcdefghijk",
                custom_name_prefix="Lecture 3",
                status="finished",
                filename="Lecture 3.Introduction to Rust.mp4",
                size=412345678,
                timestamp=BASE_TS_NS + 103,
            )
        ),
        _entry(
            _record(
                id="chaptered00001",
                title="A Chaptered Talk",
                url="https://www.youtube.com/watch?v=chaptered00001",
                status="finished",
                split_by_chapters=True,
                chapter_template="%(title)s - %(section_number)02d - %(section_title)s.%(ext)s",
                chapter_files=[
                    "A Chaptered Talk - 01 - Intro.mkv",
                    "A Chaptered Talk - 02 - The Middle Bit.mkv",
                    "A Chaptered Talk - 03 - Outro.mkv",
                ],
                filename="A Chaptered Talk.mkv",
                size=987654321,
                timestamp=BASE_TS_NS + 104,
            )
        ),
        _entry(
            _record(
                id="captions00001",
                title="A Talk With Subtitles",
                url="https://www.youtube.com/watch?v=captions00001",
                download_type="captions",
                format="srt",
                quality="best",
                subtitle_language="pt-BR",
                subtitle_mode="prefer_auto",
                status="finished",
                filename="A Talk With Subtitles.pt-BR.srt",
                size=18422,
                timestamp=BASE_TS_NS + 105,
            )
        ),
        _entry(
            _record(
                id="thumbnail00001",
                title="Just The Thumbnail",
                url="https://www.youtube.com/watch?v=thumbnail00001",
                download_type="thumbnail",
                format="jpg",
                quality="best",
                status="finished",
                filename="Just The Thumbnail.jpg",
                size=94211,
                timestamp=BASE_TS_NS + 106,
            )
        ),
        _entry(
            _record(
                id="presets00001",
                title="A Download With Presets",
                url="https://www.youtube.com/watch?v=presets00001",
                status="finished",
                ytdl_options_presets=["fast"],
                ytdl_options_overrides={},
                playlist_item_limit=5,
                folder="Talks",
                filename="Talks/A Download With Presets.mp4",
                size=222333444,
                timestamp=BASE_TS_NS + 107,
            )
        ),
    ]
    # Filler so `done` is comfortably past 600 rows (DESIGN §11.4).
    for i in range(600):
        items.append(
            _entry(
                _record(
                    id=f"bulk{i:05d}",
                    title=f"Bulk Item {i:05d}",
                    url=f"https://www.youtube.com/watch?v=bulk{i:05d}",
                    status="finished",
                    filename=f"Bulk Item {i:05d}.mp4",
                    size=1_000_000 + i,
                    timestamp=BASE_TS_NS + 1000 + i,
                )
            )
        )
    return items


# A fixed epoch far enough ahead that `now - last_checked` stays negative, so the
# enabled subscription is never due and the 60 s subscription tick never fires a
# network check — and so a re-capture produces no diff, which a `time.time()`
# here would not. 2030-01-01T00:00:00Z.
NEVER_DUE_LAST_CHECKED = 1_893_456_000.0


def subscription_items() -> list[dict[str, object]]:
    common = {
        "download_type": "video",
        "codec": "auto",
        "format": "any",
        "quality": "best",
        "folder": "",
        "custom_name_prefix": "",
        "auto_start": True,
        "playlist_item_limit": 0,
        "split_by_chapters": False,
        "chapter_template": "",
        "subtitle_language": "en",
        "subtitle_mode": "prefer_manual",
        "ytdl_options_presets": [],
        "ytdl_options_overrides": {},
        "error": None,
    }
    return [
        {
            "id": "11111111-1111-4111-8111-111111111111",
            "name": "Blender Open Movies",
            "url": "https://www.youtube.com/@BlenderOfficial/videos",
            "enabled": True,
            "check_interval_minutes": 1440,
            "last_checked": NEVER_DUE_LAST_CHECKED,
            "seen_ids": [f"seen{i:04d}" for i in range(7)],
            **common,
        },
        {
            "id": "22222222-2222-4222-8222-222222222222",
            "name": "A Paused Playlist",
            "url": "https://www.youtube.com/playlist?list=PLpausedexample",
            "enabled": False,
            "check_interval_minutes": 60,
            "last_checked": None,
            "seen_ids": [],
            **common,
        },
    ]


def write(state_dir: Path) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    files = {
        "queue.json": _store("persistent_queue:queue", queue_items()),
        "pending.json": _store("persistent_queue:pending", pending_items()),
        "completed.json": _store("persistent_queue:completed", completed_items()),
        "subscriptions.json": _store("subscriptions", subscription_items()),
    }
    for name, payload in files.items():
        path = state_dir / name
        # Same separators/newline as AtomicJsonStore.save().
        path.write_text(
            json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        n = len(payload["items"])  # type: ignore[index]
        print(f"seeded {path} ({n} item(s))")


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: seed_state.py <state_dir>")
    write(Path(sys.argv[1]).resolve())


if __name__ == "__main__":
    main()
