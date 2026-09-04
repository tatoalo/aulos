#!/usr/bin/env python3
"""WP-00 — capture the legacy v1 wire corpus into ``tests/v1_golden/``.

Drives a **locally running legacy server** over HTTP and writes one directory
per case:

    tests/v1_golden/<case>/request.json
    tests/v1_golden/<case>/response.json
    tests/v1_golden/<case>/meta.json

plus ``tests/v1_golden/MANIFEST.json`` (provenance + the case index).

Per BRIEF's WP-00 scope trim this captures the **network-free** routes only:
``history``, ``delete``, ``start``, ``version``, ``presets``, ``robots.txt``,
``cancel-add``, ``cookie-status``, cookie upload/delete, the ``subscriptions/*``
validation errors, and every ``POST add`` *validation* 400 — i.e. bodies that
fail ``parse_download_options`` before yt-dlp is ever constructed. Cases that
require yt-dlp to reach the network are deliberately absent and are listed in
``MANIFEST.json:skipped`` with the reason.

Secret hygiene: no request ever sends a ``Cookie`` or ``Authorization`` header,
and both request and response headers are scrubbed before they are written, so
a future capture against a proxied deployment cannot leak one either.

Usage (see README.md for the full recipe, which `run_capture.sh` automates):
    python tools/capture/capture_v1.py --base-url http://127.0.0.1:18081 \
        --phase seeded
"""

from __future__ import annotations

import argparse
import datetime as _dt
import hashlib
import http.client
import json
import os
import re
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

from _legacy import V1_GOLDEN_DIR, legacy_commit

# Header names that are never written to disk, whichever direction they travel.
SCRUB_HEADERS = {"cookie", "set-cookie", "authorization", "proxy-authorization"}
# Headers whose value changes on every run; kept as keys (their presence is part
# of the contract) with a stable placeholder so a re-capture diffs cleanly.
VOLATILE_HEADERS = {"date"}

SUB_ENABLED_ID = "11111111-1111-4111-8111-111111111111"
SUB_DISABLED_ID = "22222222-2222-4222-8222-222222222222"
SUB_ENABLED_URL = "https://www.youtube.com/@BlenderOfficial/videos"

VIDEO_URL = "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
QUEUE_PENDING_URL = "https://vimeo.com/76979871"
PENDING_URL = "https://www.youtube.com/watch?v=9bZkp7q19f0"
DONE_URL = "https://www.youtube.com/watch?v=bulk00000"
UNKNOWN_URL = "https://example.com/definitely-not-queued"

ALLOWED_ORIGIN = "https://ui.example.com"
DISALLOWED_ORIGIN = "https://evil.example.com"


# ---------------------------------------------------------------------------
# HTTP plumbing
# ---------------------------------------------------------------------------


class Client:
    def __init__(self, base_url: str) -> None:
        parts = urlsplit(base_url)
        if parts.scheme != "http":
            raise SystemExit("only http:// is supported (the capture is local)")
        self.host = parts.hostname or "127.0.0.1"
        self.port = parts.port or 80
        self.prefix = parts.path or "/"
        if not self.prefix.endswith("/"):
            self.prefix += "/"

    def path_for(self, route_path: str) -> str:
        return self.prefix + route_path.lstrip("/")

    def request(
        self,
        method: str,
        path: str,
        body: bytes | None,
        headers: dict[str, str],
    ) -> tuple[int, str, list[tuple[str, str]], bytes]:
        conn = http.client.HTTPConnection(self.host, self.port, timeout=60)
        try:
            conn.request(method, path, body=body, headers=headers)
            resp = conn.getresponse()
            payload = resp.read()
            return resp.status, resp.reason, resp.getheaders(), payload
        finally:
            conn.close()


def scrub_headers(pairs: list[tuple[str, str]]) -> tuple[dict[str, str], list[str]]:
    out: dict[str, str] = {}
    scrubbed: list[str] = []
    for name, value in pairs:
        low = name.lower()
        if low in SCRUB_HEADERS:
            out[name] = "<scrubbed>"
            scrubbed.append(name)
        elif low in VOLATILE_HEADERS:
            out[name] = "<volatile>"
        else:
            out[name] = value
    return out, scrubbed


def multipart(
    field_name: str, filename: str, content: bytes, boundary: str
) -> tuple[bytes, str]:
    head = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="{field_name}"; filename="{filename}"\r\n'
        f"Content-Type: text/plain\r\n\r\n"
    ).encode()
    tail = f"\r\n--{boundary}--\r\n".encode()
    return head + content + tail, f"multipart/form-data; boundary={boundary}"


# ---------------------------------------------------------------------------
# Case recording
# ---------------------------------------------------------------------------

CASE_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9_]*$")


class Recorder:
    def __init__(self, client: Client, out_dir: Path) -> None:
        self.client = client
        self.out_dir = out_dir
        self.index: list[dict[str, object]] = []
        self.seen: set[str] = set()

    def case(
        self,
        name: str,
        *,
        route: str,
        why: str,
        method: str,
        path: str,
        json_body: object = ...,
        raw_body: str | None = None,
        multipart_field: str | None = None,
        multipart_filename: str = "cookies.txt",
        multipart_size: int | None = None,
        multipart_parts: bool = True,
        origin: str | None = None,
        content_type: str | None = None,
    ) -> dict[str, object]:
        """Issue one request and write its three files."""
        if not CASE_NAME_RE.match(name):
            raise SystemExit(f"bad case name {name!r}")
        if name in self.seen:
            raise SystemExit(f"duplicate case name {name!r}")
        self.seen.add(name)

        headers: dict[str, str] = {"Accept": "*/*", "User-Agent": "aulos-wp00-capture/1"}
        body: bytes | None = None
        request_body: object = None

        if multipart_field is not None or not multipart_parts:
            boundary = "----aulosWP00Boundary"
            size = multipart_size if multipart_size is not None else 0
            content = b"a" * size
            if multipart_parts:
                body, ctype = multipart(
                    multipart_field or "cookies", multipart_filename, content, boundary
                )
            else:
                body, ctype = f"--{boundary}--\r\n".encode(), (
                    f"multipart/form-data; boundary={boundary}"
                )
            headers["Content-Type"] = ctype
            request_body = {
                "__multipart__": {
                    "field_name": multipart_field,
                    "filename": multipart_filename if multipart_field else None,
                    "content_bytes": size,
                    "content_synthesised_as": f"b'a' * {size}",
                    "has_parts": multipart_parts,
                    "boundary": boundary,
                }
            }
        elif raw_body is not None:
            body = raw_body.encode()
            headers["Content-Type"] = content_type or "application/json"
            request_body = {"__raw__": raw_body}
        elif json_body is not ...:
            body = json.dumps(json_body).encode()
            headers["Content-Type"] = content_type or "application/json"
            request_body = json_body

        if origin is not None:
            headers["Origin"] = origin
        if body is not None:
            headers["Content-Length"] = str(len(body))

        full_path = self.client.path_for(path)
        status, reason, resp_headers, payload = self.client.request(
            method, full_path, body, headers
        )

        req_headers, req_scrubbed = scrub_headers(list(headers.items()))
        res_headers, res_scrubbed = scrub_headers(resp_headers)

        # Response body: parsed JSON when it is JSON, verbatim text otherwise
        # (aiohttp's HTTPBadRequest bodies are `text/plain` "400: <reason>").
        try:
            parsed = json.loads(payload.decode())
            response_body: object = parsed
            body_is_json = True
        except (UnicodeDecodeError, json.JSONDecodeError):
            response_body = {"__text__": payload.decode("utf-8", "replace")}
            body_is_json = False

        d = self.out_dir / name
        d.mkdir(parents=True, exist_ok=True)
        _write(
            d / "request.json",
            {
                "method": method,
                "path": full_path,
                "route": route,
                "headers": req_headers,
                "body": request_body,
            },
        )
        _write(d / "response.json", response_body)
        _write(
            d / "meta.json",
            {
                "name": name,
                "route": route,
                "why": why,
                "method": method,
                "path": full_path,
                "status": status,
                # aiohttp puts every validation message in the reason phrase of
                # the status line, so this is where the byte-identical legacy
                # strings of DESIGN §11.7 actually live.
                "reason": reason,
                "content_type": res_headers.get("Content-Type"),
                "request_headers": req_headers,
                "response_headers": res_headers,
                "scrubbed_request_headers": req_scrubbed,
                "scrubbed_response_headers": res_scrubbed,
                "response_body_is_json": body_is_json,
                "response_bytes": len(payload),
                "response_sha256": hashlib.sha256(payload).hexdigest(),
            },
        )
        self.index.append(
            {
                "name": name,
                "route": route,
                "method": method,
                "path": full_path,
                "status": status,
                "reason": reason,
            }
        )
        print(f"  {status:3d} {method:7s} {full_path:38s} {name}")
        return {"status": status, "reason": reason, "body": response_body}


def _write(path: Path, payload: object) -> None:
    text = json.dumps(payload, sort_keys=True, indent=2, ensure_ascii=False)
    path.write_text(text + "\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# The case list
# ---------------------------------------------------------------------------

VIDEO_OK = {"url": VIDEO_URL, "download_type": "video", "format": "any", "quality": "best"}


def _v(**over: object) -> dict[str, object]:
    """A body that would otherwise be valid, with overrides applied."""
    d = dict(VIDEO_OK)
    d.update(over)
    return d


def capture_empty_phase(rec: Recorder) -> None:
    """The one case that needs a pristine, empty STATE_DIR."""
    rec.case(
        "history_empty",
        route="GET history",
        why="all three keys are present and empty on a fresh install (DESIGN §11.4)",
        method="GET",
        path="history",
    )


def capture_seeded_phase(rec: Recorder) -> None:  # noqa: PLR0915 - a flat case list
    # --- read-only routes, before anything mutates ------------------------
    rec.case(
        "version",
        route="GET version",
        why="the only handler using web.json_response, so it is genuinely application/json",
        method="GET",
        path="version",
    )
    rec.case(
        "presets",
        route="GET presets",
        why="sorted preset names; explicitly application/json",
        method="GET",
        path="presets",
    )
    rec.case(
        "robots_txt",
        route="GET robots.txt",
        why="the three-line default body, text/plain (DESIGN §11.7)",
        method="GET",
        path="robots.txt",
    )
    rec.case(
        "cancel_add",
        route="POST cancel-add",
        why="body is ignored; explicitly application/json",
        method="POST",
        path="cancel-add",
        json_body={"generation": 7},
    )
    rec.case(
        "history_seeded",
        route="GET history",
        why=(
            "one item per legacy status, a pre-download-problem pending row, and 607 "
            "done rows -- the whole-set-vs-window case of DESIGN §11.4"
        ),
        method="GET",
        path="history",
    )
    rec.case(
        "subscriptions_list",
        route="GET subscriptions",
        why="the 13-key public projection, one enabled and one disabled subscription",
        method="GET",
        path="subscriptions",
    )

    # --- CORS -------------------------------------------------------------
    rec.case(
        "version_with_allowed_origin",
        route="GET version",
        why="CORS_ALLOWED_ORIGINS matches: Allow-Origin echoes the Origin, no Allow-Methods",
        method="GET",
        path="version",
        origin=ALLOWED_ORIGIN,
    )
    rec.case(
        "version_with_disallowed_origin",
        route="GET version",
        why="an unlisted Origin gets no CORS headers at all",
        method="GET",
        path="version",
        origin=DISALLOWED_ORIGIN,
    )

    # --- OPTIONS ----------------------------------------------------------
    for route_path, slug in (
        ("add", "add"),
        ("cancel-add", "cancel_add"),
        ("subscribe", "subscribe"),
        ("subscriptions", "subscriptions"),
        ("subscriptions/update", "subscriptions_update"),
        ("subscriptions/delete", "subscriptions_delete"),
        ("subscriptions/check", "subscriptions_check"),
        ("upload-cookies", "upload_cookies"),
        ("delete-cookies", "delete_cookies"),
    ):
        rec.case(
            f"options_{slug}",
            route=f"OPTIONS {route_path}",
            why="the shared add_cors handler: {'status':'ok'}, text/plain",
            method="OPTIONS",
            path=route_path,
        )
    rec.case(
        "options_add_with_allowed_origin",
        route="OPTIONS add",
        why="the preflight a browser actually sends; Allow-Headers is Content-Type only",
        method="OPTIONS",
        path="add",
        origin=ALLOWED_ORIGIN,
    )

    # --- POST add: body-level rejections ---------------------------------
    rec.case(
        "add_invalid_json_body",
        route="POST add",
        why="DESIGN §11.7: `Invalid JSON request body`",
        method="POST",
        path="add",
        raw_body="{not json",
    )
    rec.case(
        "add_body_is_an_array",
        route="POST add",
        why="DESIGN §11.7: `JSON request body must be an object`",
        method="POST",
        path="add",
        raw_body="[1, 2, 3]",
    )
    rec.case(
        "add_body_is_a_string",
        route="POST add",
        why="same reason string for any non-object JSON",
        method="POST",
        path="add",
        raw_body='"https://example.com/v"',
    )
    rec.case(
        "add_body_is_null",
        route="POST add",
        why="same reason string for JSON null",
        method="POST",
        path="add",
        raw_body="null",
    )
    rec.case(
        "add_body_is_empty",
        route="POST add",
        why="a zero-length body is a JSONDecodeError, not an empty object",
        method="POST",
        path="add",
        raw_body="",
    )

    # --- POST add: missing required fields -------------------------------
    rec.case(
        "add_empty_object",
        route="POST add",
        why=(
            "migration fills download_type/format/quality, so `{}` fails on the missing "
            "url with `missing 'url', 'download_type', or 'quality'`"
        ),
        method="POST",
        path="add",
        json_body={},
    )
    rec.case(
        "add_missing_url",
        route="POST add",
        why="the same reason string names all three fields regardless of which is missing",
        method="POST",
        path="add",
        json_body={"download_type": "video", "format": "any", "quality": "best"},
    )
    rec.case(
        "add_empty_url",
        route="POST add",
        why="a falsy url is rejected before .strip()",
        method="POST",
        path="add",
        json_body=_v(url=""),
    )
    rec.case(
        "add_missing_quality",
        route="POST add",
        why="an explicit download_type suppresses migration, so quality stays absent",
        method="POST",
        path="add",
        json_body={"url": VIDEO_URL, "download_type": "video", "format": "any"},
    )
    rec.case(
        "add_null_quality",
        route="POST add",
        why="null is falsy, same reason string",
        method="POST",
        path="add",
        json_body=_v(quality=None),
    )

    # --- POST add: path-ish fields ---------------------------------------
    rec.case(
        "add_custom_name_prefix_traversal",
        route="POST add",
        why="`..` anywhere in custom_name_prefix is rejected",
        method="POST",
        path="add",
        json_body=_v(custom_name_prefix="../../etc"),
    )
    rec.case(
        "add_custom_name_prefix_absolute",
        route="POST add",
        why="a leading / is rejected",
        method="POST",
        path="add",
        json_body=_v(custom_name_prefix="/etc/passwd"),
    )
    rec.case(
        "add_custom_name_prefix_backslash",
        route="POST add",
        why="a leading backslash is rejected too",
        method="POST",
        path="add",
        json_body=_v(custom_name_prefix="\\windows"),
    )
    rec.case(
        "add_chapter_template_traversal",
        route="POST add",
        why="the same check, different reason string, for chapter_template",
        method="POST",
        path="add",
        json_body=_v(chapter_template="../%(title)s.%(ext)s"),
    )
    rec.case(
        "add_chapter_template_absolute",
        route="POST add",
        why="a leading / in chapter_template is rejected",
        method="POST",
        path="add",
        json_body=_v(chapter_template="/tmp/%(title)s.%(ext)s"),
    )

    # --- POST add: subtitle fields ---------------------------------------
    rec.case(
        "add_subtitle_language_underscore",
        route="POST add",
        why="SUBTITLE_LANGUAGE_RE forbids underscores (so `en_US` is a 400, `en-US` is not)",
        method="POST",
        path="add",
        json_body=_v(subtitle_language="en_US"),
    )
    rec.case(
        "add_subtitle_language_leading_dash",
        route="POST add",
        why="the first character must be alphanumeric",
        method="POST",
        path="add",
        json_body=_v(subtitle_language="-en"),
    )
    rec.case(
        "add_subtitle_language_too_long",
        route="POST add",
        why="the 35-character ceiling (1 + {0,34})",
        method="POST",
        path="add",
        json_body=_v(subtitle_language="a" * 36),
    )
    rec.case(
        "add_subtitle_language_empty_string",
        route="POST add",
        why="'' is not None, so it is not defaulted to 'en' -- it fails the regex",
        method="POST",
        path="add",
        json_body=_v(subtitle_language=""),
    )
    rec.case(
        "add_subtitle_mode_unknown",
        route="POST add",
        why="the sorted() Python list repr is part of the string",
        method="POST",
        path="add",
        json_body=_v(subtitle_mode="always"),
    )

    # --- POST add: presets (leniencies 1 and 2 of DESIGN §11.2.1) --------
    rec.case(
        "add_preset_unknown_name",
        route="POST add",
        why="preset names are checked against YTDL_OPTIONS_PRESETS",
        method="POST",
        path="add",
        json_body=_v(ytdl_options_presets=["nope"]),
    )
    rec.case(
        "add_presets_wrong_type",
        route="POST add",
        why="a number is neither a list nor a string",
        method="POST",
        path="add",
        json_body=_v(ytdl_options_presets=42),
    )
    rec.case(
        "add_presets_singular_alias",
        route="POST add",
        why=(
            "DESIGN §11.2.1 leniency 1: the singular `ytdl_options_preset` key is read. "
            "Proven by the unknown-name 400, which only fires if the value was seen."
        ),
        method="POST",
        path="add",
        json_body=_v(ytdl_options_preset="nope"),
    )
    rec.case(
        "add_presets_bare_string",
        route="POST add",
        why=(
            "DESIGN §11.2.1 leniency 2: a bare string is wrapped into a one-element list. "
            "Proven the same way."
        ),
        method="POST",
        path="add",
        json_body=_v(ytdl_options_presets="nope"),
    )
    rec.case(
        "add_presets_list_with_blanks_is_lenient",
        route="POST add",
        why=(
            "blank entries are dropped, so ['', '  ', 'nope'] fails only on 'nope' -- "
            "evidence the filter runs before validation"
        ),
        method="POST",
        path="add",
        json_body=_v(ytdl_options_presets=["", "  ", "nope"]),
    )

    # --- POST add: type / codec / format / quality matrix ----------------
    rec.case(
        "add_download_type_unknown",
        route="POST add",
        why="DESIGN §11.7: the Python sorted-list repr is reproduced verbatim",
        method="POST",
        path="add",
        json_body=_v(download_type="podcast"),
    )
    rec.case(
        "add_codec_unknown",
        route="POST add",
        why="codec is validated before the per-type format/quality lists",
        method="POST",
        path="add",
        json_body=_v(codec="vp8"),
    )
    rec.case(
        "add_video_format_unknown",
        route="POST add",
        why="video formats are exactly {any, ios, mp4}",
        method="POST",
        path="add",
        json_body=_v(format="mkv"),
    )
    rec.case(
        "add_video_quality_unknown",
        route="POST add",
        why="4320 is not in the legacy height list",
        method="POST",
        path="add",
        json_body=_v(quality="4320"),
    )
    rec.case(
        "add_video_best_remux_on_any",
        route="POST add",
        why="best_remux is legal only for format=mp4; the reason string omits it here",
        method="POST",
        path="add",
        json_body=_v(format="any", quality="best_remux"),
    )
    rec.case(
        "add_video_best_remux_on_ios",
        route="POST add",
        why="the same, for ios -- DESIGN §6.6 keeps ios's nine qualities but not best_remux",
        method="POST",
        path="add",
        json_body=_v(format="ios", quality="best_remux"),
    )
    rec.case(
        "add_audio_format_unknown",
        route="POST add",
        why="audio formats are exactly the five AUDIO_FORMATS",
        method="POST",
        path="add",
        json_body=_v(download_type="audio", format="aac", quality="best"),
    )
    rec.case(
        "add_audio_quality_on_wav",
        route="POST add",
        why="wav/opus/flac admit only `best`; the reason names the format",
        method="POST",
        path="add",
        json_body=_v(download_type="audio", format="wav", quality="192"),
    )
    rec.case(
        "add_audio_quality_320_on_m4a",
        route="POST add",
        why="320 is mp3-only; m4a stops at 192",
        method="POST",
        path="add",
        json_body=_v(download_type="audio", format="m4a", quality="320"),
    )
    rec.case(
        "add_captions_format_unknown",
        route="POST add",
        why="the seven caption formats",
        method="POST",
        path="add",
        json_body=_v(download_type="captions", format="ass", quality="best"),
    )
    rec.case(
        "add_thumbnail_format_unknown",
        route="POST add",
        why="thumbnail admits only jpg",
        method="POST",
        path="add",
        json_body=_v(download_type="thumbnail", format="png", quality="best"),
    )

    # --- POST add: ytdl_options_overrides (leniency 3) -------------------
    rec.case(
        "add_overrides_invalid_json_string",
        route="POST add",
        why="DESIGN §11.2.1 leniency 3: a string value is json.loads()ed first",
        method="POST",
        path="add",
        json_body=_v(ytdl_options_overrides="{not json"),
    )
    rec.case(
        "add_overrides_json_string_not_an_object",
        route="POST add",
        why="a string that parses to a non-object gets the second reason string",
        method="POST",
        path="add",
        json_body=_v(ytdl_options_overrides="[1, 2]"),
    )
    rec.case(
        "add_overrides_not_an_object",
        route="POST add",
        why="a bare number is rejected without any parse attempt",
        method="POST",
        path="add",
        json_body=_v(ytdl_options_overrides=42),
    )
    rec.case(
        "add_overrides_disabled",
        route="POST add",
        why=(
            "ALLOW_YTDL_OPTIONS_OVERRIDES defaults to false, so any non-empty override "
            "is a 400 -- ErrorCode::overrides_disabled in DESIGN §5"
        ),
        method="POST",
        path="add",
        json_body=_v(ytdl_options_overrides={"format": "worst"}),
    )
    rec.case(
        "add_overrides_json_string_object_is_disabled_too",
        route="POST add",
        why="the string form reaches the same disabled check after parsing",
        method="POST",
        path="add",
        json_body=_v(ytdl_options_overrides='{"format": "worst"}'),
    )

    # --- POST add: playlist_item_limit (leniency 4) ----------------------
    rec.case(
        "add_playlist_item_limit_not_an_integer",
        route="POST add",
        why="DESIGN §11.7: `playlist_item_limit must be an integer`",
        method="POST",
        path="add",
        json_body=_v(playlist_item_limit="abc"),
    )
    rec.case(
        "add_playlist_item_limit_float_string",
        route="POST add",
        why="Python int('5.5') raises, so a decimal string is a 400 (not a truncation)",
        method="POST",
        path="add",
        json_body=_v(playlist_item_limit="5.5"),
    )
    rec.case(
        "add_playlist_item_limit_object",
        route="POST add",
        why="int({}) is a TypeError, caught into the same 400",
        method="POST",
        path="add",
        json_body=_v(playlist_item_limit={}),
    )
    rec.case(
        "add_playlist_item_limit_list",
        route="POST add",
        why="the same for a list",
        method="POST",
        path="add",
        json_body=_v(playlist_item_limit=[5]),
    )

    # --- POST add: the six _migrate_legacy_request rows ------------------
    # The int() of playlist_item_limit is the LAST check in
    # parse_download_options, so `playlist_item_limit: "nope"` is a probe that
    # says "everything before me validated". A migration row that produces a
    # legal tuple therefore surfaces as the playlist_item_limit 400, while an
    # unmigrated body would have failed earlier on format/quality.
    rec.case(
        "add_migrate_row1_audio_keeps_quality",
        route="POST add",
        why=(
            "migration row 1: legacy format=mp3 -> download_type=audio with the legacy "
            "quality passed through, proven by the mp3-specific quality reason string"
        ),
        method="POST",
        path="add",
        json_body={"url": VIDEO_URL, "format": "mp3", "quality": "777"},
    )
    rec.case(
        "add_migrate_row1_audio_wav_keeps_quality",
        route="POST add",
        why="the same row for wav, whose allowed set is only {best}",
        method="POST",
        path="add",
        json_body={"url": VIDEO_URL, "format": "wav", "quality": "192"},
    )
    rec.case(
        "add_migrate_row2_thumbnail",
        route="POST add",
        why=(
            "migration row 2: legacy format=thumbnail -> thumbnail/jpg/best, discarding "
            "the legacy quality. The playlist_item_limit probe fires, proving the "
            "migrated tuple passed validation (quality=1080 would not have)"
        ),
        method="POST",
        path="add",
        json_body={
            "url": VIDEO_URL,
            "format": "thumbnail",
            "quality": "1080",
            "playlist_item_limit": "nope",
        },
    )
    rec.case(
        "add_migrate_row3_captions_uses_subtitle_format",
        route="POST add",
        why=(
            "migration row 3: legacy format=captions takes its new format from "
            "subtitle_format, proven by the captions-specific reason string"
        ),
        method="POST",
        path="add",
        json_body={"url": VIDEO_URL, "format": "captions", "subtitle_format": "ass"},
    )
    rec.case(
        "add_migrate_row3_captions_defaults_to_srt",
        route="POST add",
        why="the same row with subtitle_format absent: it defaults to srt, which is legal",
        method="POST",
        path="add",
        json_body={
            "url": VIDEO_URL,
            "format": "captions",
            "playlist_item_limit": "nope",
        },
    )
    rec.case(
        "add_migrate_row4_best_ios",
        route="POST add",
        why=(
            "migration row 4: legacy quality=best_ios -> video/ios/best. best_ios is not "
            "a legal quality, so reaching the playlist_item_limit probe proves the rewrite"
        ),
        method="POST",
        path="add",
        json_body={
            "url": VIDEO_URL,
            "format": "mp4",
            "quality": "best_ios",
            "playlist_item_limit": "nope",
        },
    )
    rec.case(
        "add_migrate_row5_quality_audio",
        route="POST add",
        why=(
            "migration row 5: legacy quality=audio -> audio/m4a/best. `audio` is not a "
            "legal video quality, so the probe firing proves the rewrite"
        ),
        method="POST",
        path="add",
        json_body={
            "url": VIDEO_URL,
            "format": "any",
            "quality": "audio",
            "playlist_item_limit": "nope",
        },
    )
    rec.case(
        "add_migrate_row6_passes_format_through",
        route="POST add",
        why=(
            "migration row 6: legacy format/quality pass through unchanged. best_remux on "
            "`any` is illegal, and the reason string proves format was NOT rewritten to mp4"
        ),
        method="POST",
        path="add",
        json_body={"url": VIDEO_URL, "format": "any", "quality": "best_remux"},
    )
    rec.case(
        "add_migrate_row6_uses_video_codec",
        route="POST add",
        why="migration row 6 also moves legacy `video_codec` into `codec`",
        method="POST",
        path="add",
        json_body={
            "url": VIDEO_URL,
            "format": "any",
            "quality": "1080",
            "video_codec": "vp8",
        },
    )
    rec.case(
        "add_migrate_not_applied_when_download_type_present",
        route="POST add",
        why=(
            "migration is skipped entirely when download_type is present, so a legacy "
            "audio `format` is judged against the video list"
        ),
        method="POST",
        path="add",
        json_body={
            "url": VIDEO_URL,
            "download_type": "video",
            "format": "mp3",
            "quality": "best",
        },
    )

    # --- POST add: auto_start is never type-checked ----------------------
    for value, slug in ((True, "bool_true"), ("true", "string_true"), ("false", "string_false")):
        rec.case(
            f"add_auto_start_{slug}",
            route="POST add",
            why=(
                "auto_start is not validated: all three forms reach the last check "
                "(the playlist_item_limit probe). The legacy `is True` divergence that "
                "sends the string forms to `pending` happens later, inside "
                "__add_download, and is not observable without a network resolve"
            ),
            method="POST",
            path="add",
            json_body=_v(auto_start=value, playlist_item_limit="nope"),
        )

    # --- POST subscribe --------------------------------------------------
    rec.case(
        "subscribe_invalid_json_body",
        route="POST subscribe",
        why="the shared _read_json_request path",
        method="POST",
        path="subscribe",
        raw_body="{",
    )
    rec.case(
        "subscribe_missing_url",
        route="POST subscribe",
        why="subscribe reuses parse_download_options wholesale",
        method="POST",
        path="subscribe",
        json_body={"download_type": "video", "format": "any", "quality": "best"},
    )
    rec.case(
        "subscribe_download_type_unknown",
        route="POST subscribe",
        why="…so every add validation string is reachable here too",
        method="POST",
        path="subscribe",
        json_body=_v(download_type="podcast"),
    )
    rec.case(
        "subscribe_check_interval_not_an_integer",
        route="POST subscribe",
        why="DESIGN §11.7: `check_interval_minutes must be an integer`",
        method="POST",
        path="subscribe",
        json_body=_v(check_interval_minutes="abc"),
    )
    rec.case(
        "subscribe_check_interval_zero",
        route="POST subscribe",
        why="DESIGN §11.7: `check_interval_minutes must be at least 1`",
        method="POST",
        path="subscribe",
        json_body=_v(check_interval_minutes=0),
    )
    rec.case(
        "subscribe_check_interval_negative",
        route="POST subscribe",
        why="the same reason string for any value below 1",
        method="POST",
        path="subscribe",
        json_body=_v(check_interval_minutes=-5),
    )
    rec.case(
        "subscribe_check_interval_numeric_string_zero",
        route="POST subscribe",
        why=(
            "DESIGN §11.2.1 leniency 5: the string is int()ed first, so it fails the "
            "`at least 1` check rather than the type check"
        ),
        method="POST",
        path="subscribe",
        json_body=_v(check_interval_minutes="0"),
    )
    rec.case(
        "subscribe_check_interval_null_uses_default",
        route="POST subscribe",
        why=(
            "null falls back to SUBSCRIPTION_DEFAULT_CHECK_INTERVAL; the duplicate-URL "
            "check then answers before any extraction, so this stays network-free"
        ),
        method="POST",
        path="subscribe",
        json_body=_v(url=SUB_ENABLED_URL, check_interval_minutes=None),
    )
    rec.case(
        "subscribe_playlist_item_limit_numeric_string_with_spaces",
        route="POST subscribe",
        why=(
            "DESIGN §11.2.1 leniency 4: ' 5 ' parses. Proven by reaching the "
            "check_interval_minutes gate, which is the only validation after "
            "parse_download_options on any network-free path"
        ),
        method="POST",
        path="subscribe",
        json_body=_v(playlist_item_limit=" 5 ", check_interval_minutes=0),
    )
    rec.case(
        "subscribe_duplicate_url",
        route="POST subscribe",
        why=(
            "DESIGN §11.7: `This URL is already subscribed`. The duplicate check runs "
            "under the lock before extract_flat_playlist, so it needs no network. "
            "HTTP 200 with a status:error body, not a 4xx."
        ),
        method="POST",
        path="subscribe",
        json_body=_v(url=SUB_ENABLED_URL, check_interval_minutes=60),
    )
    rec.case(
        "subscribe_duplicate_url_with_surrounding_space",
        route="POST subscribe",
        why="the uniqueness key is the .strip()ed url, so whitespace does not evade it",
        method="POST",
        path="subscribe",
        json_body=_v(url=f"  {SUB_ENABLED_URL}  ", check_interval_minutes=60),
    )

    # --- subscriptions/update -------------------------------------------
    rec.case(
        "subscriptions_update_missing_id",
        route="POST subscriptions/update",
        why="DESIGN §11.7: `missing subscription id`",
        method="POST",
        path="subscriptions/update",
        json_body={"enabled": False},
    )
    rec.case(
        "subscriptions_update_empty_id",
        route="POST subscriptions/update",
        why="a falsy id gets the same reason string",
        method="POST",
        path="subscriptions/update",
        json_body={"id": "", "enabled": False},
    )
    rec.case(
        "subscriptions_update_no_valid_fields",
        route="POST subscriptions/update",
        why=(
            "DESIGN §11.7: `no valid fields to update` -- only enabled / "
            "check_interval_minutes / name are updatable"
        ),
        method="POST",
        path="subscriptions/update",
        json_body={"id": SUB_ENABLED_ID, "url": "https://example.com/x", "quality": "720"},
    )
    rec.case(
        "subscriptions_update_unknown_id",
        route="POST subscriptions/update",
        why="DESIGN §11.7: `Subscription not found`, as a 200 status:error body",
        method="POST",
        path="subscriptions/update",
        json_body={"id": "no-such-subscription", "enabled": False},
    )
    rec.case(
        "subscriptions_update_enabled_not_a_boolean",
        route="POST subscriptions/update",
        why=(
            "the legacy leak DESIGN §11.1 / Appendix B C25 fixes: _coerce_bool raises "
            "ValueError('enabled must be a boolean') and nothing catches it -> HTTP 500"
        ),
        method="POST",
        path="subscriptions/update",
        json_body={"id": SUB_ENABLED_ID, "enabled": 123},
    )
    rec.case(
        "subscriptions_update_enabled_string_is_accepted",
        route="POST subscriptions/update",
        why="_coerce_bool accepts true/1/on and false/0/off strings, unlike add's auto_start",
        method="POST",
        path="subscriptions/update",
        json_body={"id": SUB_ENABLED_ID, "enabled": "off"},
    )
    rec.case(
        "subscriptions_update_check_interval_is_floored_at_1",
        route="POST subscriptions/update",
        why="max(1, int(...)) here, unlike /subscribe which 400s on the same input",
        method="POST",
        path="subscriptions/update",
        json_body={"id": SUB_ENABLED_ID, "check_interval_minutes": 0},
    )
    rec.case(
        "subscriptions_update_rename",
        route="POST subscriptions/update",
        why="the success shape: {'status':'ok','subscription':{…13 keys…}}",
        method="POST",
        path="subscriptions/update",
        json_body={"id": SUB_ENABLED_ID, "name": "Renamed By The Capture"},
    )
    rec.case(
        "subscriptions_update_blank_name_is_ignored",
        route="POST subscriptions/update",
        why="`if 'name' in changes and changes['name']` -- a blank name is silently skipped",
        method="POST",
        path="subscriptions/update",
        json_body={"id": SUB_ENABLED_ID, "name": ""},
    )

    # --- subscriptions/delete + check ------------------------------------
    rec.case(
        "subscriptions_delete_missing_ids",
        route="POST subscriptions/delete",
        why="DESIGN §11.7: `missing ids list`",
        method="POST",
        path="subscriptions/delete",
        json_body={},
    )
    rec.case(
        "subscriptions_delete_empty_ids",
        route="POST subscriptions/delete",
        why="an empty list is falsy, so it gets the same 400 (DESIGN §11.1 keeps this)",
        method="POST",
        path="subscriptions/delete",
        json_body={"ids": []},
    )
    rec.case(
        "subscriptions_delete_ids_not_a_list",
        route="POST subscriptions/delete",
        why="a string also fails the isinstance check, with the same reason",
        method="POST",
        path="subscriptions/delete",
        json_body={"ids": SUB_ENABLED_ID},
    )
    rec.case(
        "subscriptions_delete_unknown_id",
        route="POST subscriptions/delete",
        why="deleting something that does not exist is a plain ok",
        method="POST",
        path="subscriptions/delete",
        json_body={"ids": ["no-such-subscription"]},
    )
    rec.case(
        "subscriptions_delete_real_id",
        route="POST subscriptions/delete",
        why=(
            "the success path: the disabled subscription is removed and disappears from "
            "GET subscriptions (see subscriptions_list_after_mutations)"
        ),
        method="POST",
        path="subscriptions/delete",
        json_body={"ids": [SUB_DISABLED_ID]},
    )
    rec.case(
        "subscriptions_check_ids_not_a_list",
        route="POST subscriptions/check",
        why="DESIGN §11.7: `ids must be a list`",
        method="POST",
        path="subscriptions/check",
        json_body={"ids": "nope"},
    )
    rec.case(
        "subscriptions_check_unknown_ids",
        route="POST subscriptions/check",
        why=(
            "unknown ids resolve to zero targets, so this returns ok without any "
            "extraction -- the only network-free success path for this route"
        ),
        method="POST",
        path="subscriptions/check",
        json_body={"ids": ["no-such-subscription"]},
    )

    # --- POST delete -----------------------------------------------------
    rec.case(
        "delete_missing_where",
        route="POST delete",
        why="a reasonless 400 (HTTPBadRequest with no reason) -- legacy's bare `raise`",
        method="POST",
        path="delete",
        json_body={"ids": [QUEUE_PENDING_URL]},
    )
    rec.case(
        "delete_bad_where",
        route="POST delete",
        why="`where` must be exactly 'queue' or 'done'",
        method="POST",
        path="delete",
        json_body={"ids": [QUEUE_PENDING_URL], "where": "trash"},
    )
    rec.case(
        "delete_empty_ids",
        route="POST delete",
        why="a falsy ids list is the same reasonless 400",
        method="POST",
        path="delete",
        json_body={"ids": [], "where": "queue"},
    )
    rec.case(
        "delete_missing_ids",
        route="POST delete",
        why="the same, with ids absent",
        method="POST",
        path="delete",
        json_body={"where": "queue"},
    )
    rec.case(
        "delete_ids_null",
        route="POST delete",
        why="null ids are caught by the same falsy check -- no 500 here, unlike /start",
        method="POST",
        path="delete",
        json_body={"ids": None, "where": "queue"},
    )
    rec.case(
        "delete_queue_unknown_key",
        route="POST delete",
        why="an unknown key is logged and skipped; the response is still ok",
        method="POST",
        path="delete",
        json_body={"ids": [UNKNOWN_URL], "where": "queue"},
    )
    rec.case(
        "delete_queue_by_url",
        route="POST delete",
        why=(
            "legacy keys the queue by URL, which is why DESIGN §11.3 needs the "
            "ULID/url/media_id resolution ladder"
        ),
        method="POST",
        path="delete",
        json_body={"ids": [QUEUE_PENDING_URL], "where": "queue"},
    )
    rec.case(
        "delete_done_by_media_id_is_a_no_op",
        route="POST delete",
        why=(
            "the media id (`bulk00000`) is NOT a key: legacy only ever accepted URLs, so "
            "this returns ok while removing nothing -- the case DESIGN §11.3 fixes"
        ),
        method="POST",
        path="delete",
        json_body={"ids": ["bulk00000"], "where": "done"},
    )
    rec.case(
        "delete_done_by_url",
        route="POST delete",
        why="the working form for a completed row",
        method="POST",
        path="delete",
        json_body={"ids": [DONE_URL], "where": "done"},
    )
    rec.case(
        "delete_done_unknown_key",
        route="POST delete",
        why="unknown done keys are skipped with a warning",
        method="POST",
        path="delete",
        json_body={"ids": [UNKNOWN_URL], "where": "done"},
    )

    # --- POST start ------------------------------------------------------
    rec.case(
        "start_ids_null",
        route="POST start",
        why=(
            "`for id in None` -> TypeError -> HTTP 500. DESIGN §11.1 turns this into a "
            "400; the corpus records what legacy actually did"
        ),
        method="POST",
        path="start",
        json_body={"ids": None},
    )
    rec.case(
        "start_missing_ids",
        route="POST start",
        why="an absent ids key takes the same TypeError path",
        method="POST",
        path="start",
        json_body={},
    )
    rec.case(
        "start_unknown_id",
        route="POST start",
        why="unknown ids are warned about and skipped; still ok",
        method="POST",
        path="start",
        json_body={"ids": [UNKNOWN_URL]},
    )
    rec.case(
        "start_ids_is_a_string",
        route="POST start",
        why=(
            "a string is iterable, so legacy iterates its characters and warns once per "
            "character -- still a 200 ok"
        ),
        method="POST",
        path="start",
        json_body={"ids": "ab"},
    )
    rec.case(
        "start_pending_id",
        route="POST start",
        why=(
            "the documented case: a pending URL moves to the queue. "
            "MAX_CONCURRENT_DOWNLOADS=0 means the started task blocks on the semaphore, "
            "so nothing reaches the network"
        ),
        method="POST",
        path="start",
        json_body={"ids": [PENDING_URL]},
    )

    # --- cookies ---------------------------------------------------------
    rec.case(
        "cookie_status_without_cookies",
        route="GET cookie-status",
        why="has_cookies is false when neither an upload nor YTDL_OPTIONS.cookiefile exists",
        method="GET",
        path="cookie-status",
    )
    rec.case(
        "delete_cookies_nothing_to_delete",
        route="POST delete-cookies",
        why="DESIGN §11.7: `No uploaded cookies to delete`, as a 400 with a JSON body",
        method="POST",
        path="delete-cookies",
        json_body={},
    )
    rec.case(
        "upload_cookies_no_parts",
        route="POST upload-cookies",
        why="DESIGN §11.7: `No cookies file provided` when the multipart body has no part",
        method="POST",
        path="upload-cookies",
        multipart_parts=False,
    )
    rec.case(
        "upload_cookies_wrong_field_name",
        route="POST upload-cookies",
        why="the field must be named exactly `cookies`; same reason string",
        method="POST",
        path="upload-cookies",
        multipart_field="cookiefile",
        multipart_size=32,
    )
    rec.case(
        "upload_cookies_over_the_cap",
        route="POST upload-cookies",
        why=(
            "DESIGN §16.6: the cap is 1 000 000 **decimal** bytes and the test is "
            "`size > max_size`, so 1 000 001 is the first rejected size"
        ),
        method="POST",
        path="upload-cookies",
        multipart_field="cookies",
        multipart_size=1_000_001,
    )
    rec.case(
        "upload_cookies_at_the_cap",
        route="POST upload-cookies",
        why="the other side of the boundary: exactly 1 000 000 bytes is accepted",
        method="POST",
        path="upload-cookies",
        multipart_field="cookies",
        multipart_size=1_000_000,
    )
    rec.case(
        "cookie_status_with_cookies",
        route="GET cookie-status",
        why="has_cookies flips to true once an upload landed",
        method="GET",
        path="cookie-status",
    )
    rec.case(
        "upload_cookies_ok",
        route="POST upload-cookies",
        why="DESIGN §11.7: `Cookies uploaded (N bytes)` -- N is the decoded byte count",
        method="POST",
        path="upload-cookies",
        multipart_field="cookies",
        multipart_size=64,
    )
    rec.case(
        "delete_cookies_ok",
        route="POST delete-cookies",
        why="a bare {'status':'ok'} with no msg",
        method="POST",
        path="delete-cookies",
        json_body={},
    )
    rec.case(
        "cookie_status_after_delete",
        route="GET cookie-status",
        why="back to false, which also proves the runtime override was removed",
        method="GET",
        path="cookie-status",
    )

    # --- the state after every mutation above ----------------------------
    rec.case(
        "subscriptions_list_after_mutations",
        route="GET subscriptions",
        why="the rename, the string `enabled`, and the floored interval, all visible",
        method="GET",
        path="subscriptions",
    )
    rec.case(
        "history_after_mutations",
        route="GET history",
        why=(
            "the deletes and the start have moved rows between the three arrays; this is "
            "the end-state a replay harness can assert against"
        ),
        method="GET",
        path="history",
    )


# What we deliberately did not capture, and why. Written into MANIFEST.json so
# the gap is a documented decision rather than an oversight.
SKIPPED: list[dict[str, str]] = [
    {
        "route": "POST add",
        "case": "ok (video), ok (playlist), duplicate, unsupported URL, geo-blocked URL, "
        "`Invalid/empty data was given.`, `Unsupported resource \"<etype>\"`, "
        "upcoming-livestream",
        "reason": "all require yt-dlp to reach the network (BRIEF WP-00 scope trim)",
    },
    {
        "route": "POST add",
        "case": "auto_start true vs \"true\" vs \"false\" routing to queue vs pending",
        "reason": (
            "the `auto_start is True` divergence happens in __add_download, after a "
            "successful network resolve. Parse-time acceptance of all three forms IS "
            "captured (add_auto_start_*)"
        ),
    },
    {
        "route": "POST subscribe",
        "case": "ok, single-video URL (`This URL points to a single video…`), "
        "`Could not resolve URL`",
        "reason": "all three need extract_flat_playlist to hit the network",
    },
    {
        "route": "POST subscribe",
        "case": "`Missing URL`",
        "reason": (
            "unreachable over HTTP: parse_download_options rejects a falsy url with "
            "`missing 'url', 'download_type', or 'quality'` first, so "
            "add_subscription's own empty-url branch is dead behind the REST route"
        ),
    },
    {
        "route": "POST subscriptions/check",
        "case": "ok with real targets",
        "reason": "checking an existing enabled subscription performs a network extraction",
    },
    {
        "route": "POST delete-cookies",
        "case": "the manual-cookiefile 400 and the reload-failure 500",
        "reason": (
            "both need a YTDL_OPTIONS.cookiefile pointing elsewhere; a second server "
            "configuration for two strings was judged not worth the corpus complexity. "
            "Both strings are quoted in DESIGN §11.7 / legacy spec §2.1"
        ),
    },
    {
        "route": "GET <p>",
        "case": "the Angular index and the metube_theme cookie",
        "reason": (
            "DESIGN §11.1 deliberately does not provide it (a JSON identity document "
            "replaces it), so there is no contract to pin"
        ),
    },
    {
        "route": "GET <p>socket.io/*",
        "case": "the handshake",
        "reason": "DESIGN §11.1 answers 501 socketio_removed by design; nothing to match",
    },
    {
        "route": "GET <p>download/*, <p>audio_download/*",
        "case": "static file serving",
        "reason": "aiohttp static handler behaviour, not a v1 shim contract",
    },
]


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


def wait_for_server(client: Client, timeout: float = 40.0) -> None:
    deadline = time.monotonic() + timeout
    last: str | None = None
    while time.monotonic() < deadline:
        try:
            status, _, _, _ = client.request(
                "GET", client.path_for("version"), None, {"Accept": "*/*"}
            )
            if status == 200:
                return
            last = f"status {status}"
        except (OSError, http.client.HTTPException) as exc:
            last = repr(exc)
        time.sleep(0.25)
    raise SystemExit(f"legacy server did not come up: {last}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://127.0.0.1:18081")
    ap.add_argument("--phase", choices=("empty", "seeded"), required=True)
    ap.add_argument(
        "--state-dir", default="", help="recorded in MANIFEST.json for provenance only"
    )
    ap.add_argument(
        "--scratch-root",
        default="",
        help=(
            "the throwaway root the server ran under. Every occurrence is rewritten to "
            "`<scratch>` in MANIFEST.json so a re-capture from a different mktemp "
            "directory produces no diff."
        ),
    )
    args = ap.parse_args()

    client = Client(args.base_url)
    wait_for_server(client)

    out = V1_GOLDEN_DIR
    out.mkdir(parents=True, exist_ok=True)
    rec = Recorder(client, out)

    if args.phase == "empty":
        print("phase: empty STATE_DIR")
        capture_empty_phase(rec)
    else:
        print("phase: seeded STATE_DIR")
        capture_seeded_phase(rec)

    # Merge this phase's index into MANIFEST.json (the two phases run against
    # two different server instances, so neither can write the whole file).
    manifest_path = out / "MANIFEST.json"
    existing: dict[str, object] = {}
    if manifest_path.exists():
        existing = json.loads(manifest_path.read_text(encoding="utf-8"))

    cases: dict[str, object] = dict(existing.get("cases") or {})  # type: ignore[arg-type]
    for entry in rec.index:
        cases[str(entry["name"])] = entry

    phases: dict[str, object] = dict(existing.get("phases") or {})  # type: ignore[arg-type]
    phases[args.phase] = {
        "captured_at": _dt.datetime.now(_dt.UTC).replace(microsecond=0).isoformat(),
        "case_count": len(rec.index),
        "state_dir_fixture": _elide(args.state_dir, args.scratch_root) or "(not recorded)",
    }

    manifest = {
        "corpus": "v1_golden",
        "purpose": (
            "Byte-level record of the legacy MeTube-POT v1 HTTP surface, captured "
            "before cutover. WP-15 replays it against the Rust v1 shim (DESIGN §11)."
        ),
        "legacy_repo": "/Users/apogliaghi/Development/metube_pot",
        "legacy_commit": legacy_commit(),
        "legacy_image_digest": (
            "not used -- BRIEF's WP-00 scope trim captures against a local "
            "`python app/main.py` run, so no image was built"
        ),
        "yt_dlp_version": _ytdlp_version_from_env(),
        "python_version": sys.version.split()[0],
        "url_prefix": client.prefix,
        "base_url": args.base_url,
        "server_env": _server_env_snapshot(args.scratch_root),
        "state_dir_fixture": _state_dir_description(),
        "scope": (
            "Network-free routes only, per BRIEF's WP-00 scope trim: history, delete, "
            "start, version, presets, robots.txt, cancel-add, cookie-status, cookie "
            "upload/delete, subscriptions/* validation errors, and every POST add "
            "validation 400 (bodies that fail parse_download_options before yt-dlp runs)."
        ),
        "secret_hygiene": (
            "No request sends Cookie/Authorization. Both request and response headers "
            "are scrubbed on write (Cookie, Set-Cookie, Authorization, "
            "Proxy-Authorization -> '<scrubbed>'); Date -> '<volatile>' so a re-capture "
            "diffs cleanly. Every seeded URL is public."
        ),
        "meta_fields": (
            "meta.json records method, path, status, the aiohttp reason phrase (where "
            "every legacy validation string actually lives), Content-Type, and the full "
            "scrubbed request and response header sets."
        ),
        "phases": phases,
        "skipped": SKIPPED,
        "cases": cases,
        "case_count": len(cases),
    }
    _write(manifest_path, manifest)
    print(f"\n{len(rec.index)} case(s) this phase; {len(cases)} in MANIFEST.json")


def _ytdlp_version_from_env() -> str:
    return os.environ.get("CAPTURE_YTDLP_VERSION", "unknown")


def _elide(value: str, scratch_root: str) -> str:
    """Rewrite the throwaway scratch root to `<scratch>` so re-captures diff cleanly."""
    if scratch_root and value:
        return value.replace(scratch_root.rstrip("/"), "<scratch>")
    return value


def _server_env_snapshot(scratch_root: str) -> dict[str, str]:
    raw = os.environ.get("CAPTURE_SERVER_ENV", "")
    out: dict[str, str] = {}
    for pair in raw.split("\n"):
        if "=" in pair:
            k, v = pair.split("=", 1)
            out[k.strip()] = _elide(v.strip(), scratch_root)
    return out


def _state_dir_description() -> str:
    return (
        "Seeded by tools/capture/seed_state.py: queue.json with one downloading, one "
        "preparing and one pending item; pending.json with a pending item and an "
        "upcoming-livestream row (status pending + a populated error); completed.json "
        "with 607 rows (a finished row with filename/size, an error row, a "
        "custom_name_prefix row, a chaptered row, a captions row, a thumbnail row, a "
        "presets/folder row, and 600 filler finished rows); subscriptions.json with one "
        "enabled and one disabled subscription. The server runs with "
        "MAX_CONCURRENT_DOWNLOADS=0 so the queue auto-restart blocks on the semaphore "
        "and the seeded statuses survive verbatim."
    )


if __name__ == "__main__":
    main()
