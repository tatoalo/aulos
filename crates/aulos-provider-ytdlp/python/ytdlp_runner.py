#!/usr/bin/env python3
"""The Aulos yt-dlp shim: one JSON job on stdin, JSON-line frames on fd 3.

DESIGN §9.1-§9.7 is the contract; this file is the whole Python side of it. Rust owns option
construction, format selection, process lifecycle, process-group kill, the timers and progress
normalisation. This file owns exactly one thing: calling ``yt_dlp``.

Transport
---------
* **stdin** - exactly one JSON object terminated by ``\\n``, then EOF. Never read again.
* **fd 3** - the protocol channel: newline-delimited JSON, UTF-8, one object per line, flushed
  per line. When fd 3 is not open (running the shim by hand, or from ``assert_cmd``) the channel
  falls back to the *original* stdout, which is duplicated away before fd 1 is redirected - so
  the isolation property below holds either way.
* **stdout** - redirected to ``/dev/null`` for the whole run before anything else happens. This
  is the load-bearing decision of §9.1: the BgUtils POT plugin, ``yt-dlp-ejs`` and its ``deno``
  grandchildren print to stdout, and a yt-dlp ``logger`` object silences yt-dlp but not a plugin
  or a grandchild. Putting the protocol anywhere near fd 1 would risk silent, intermittent
  stream corruption.
* **stderr** - left raw for the parent to drain into ``tracing``.

Frames
------
Every frame is ``{"v":1,"t":<type>,"n":<u64>,"ts":<epoch float>, ...}``. ``n`` starts at 1 and
increments by one; a gap tells the parent a line was lost. ``hello`` is always first, exactly one
of ``result`` / ``error`` is emitted, and ``bye`` is always last.

Exit codes
----------
``0`` a complete transcript was written (including one describing a failed job), ``2`` a
malformed or invalid job, ``3`` the shim itself failed, ``64`` protocol mismatch, ``130``
cancelled via SIGTERM/SIGINT.

Modes
-----
``extract`` metadata only, ``download`` the real thing, ``outtmpl`` evaluate output templates
through yt-dlp's own engine, ``selftest`` prove the interpreter and ``yt_dlp`` import works.
``--replay <transcript.jsonl>`` re-emits a recorded transcript verbatim and imports nothing.
"""

from __future__ import annotations

import contextlib
import errno
import fcntl
import json
import math
import os
import platform
import re
import signal
import sys
import time

# --------------------------------------------------------------------------------------------
# Constants. Every one of these is also a constant on the Rust side; they must not drift.
# --------------------------------------------------------------------------------------------

PROTOCOL = 1
"""The protocol version this shim speaks (DESIGN §9.2)."""

ENVELOPE_VERSION = 1
"""The ``v`` field of every frame."""

PROTOCOL_FD = 3
"""The descriptor the parent hands us for the protocol channel."""

EXIT_OK = 0
EXIT_BAD_JOB = 2
EXIT_INTERNAL = 3
EXIT_PROTOCOL = 64
EXIT_CANCELED = 130

MAX_MESSAGE_CHARS = 512
"""Error-message cap (DESIGN §9.6). The Rust side applies the same cap idempotently."""

MAX_LOG_FRAMES = 1000
"""Hard cap on forwarded yt-dlp log lines, so a verbose extractor cannot flood the channel."""

MAX_JSON_DEPTH = 64
"""Recursion bound for the JSON sanitiser, mirroring legacy ``_MAX_ENTRY_SANITIZE_DEPTH``."""

DEFAULT_EMIT_PROGRESS_EVERY_MS = 100
"""``policy.emit_progress_every_ms`` default: at most ~10 progress frames/s **per stream**."""

PP_PROCESSING_THROTTLE_S = 1.0
"""``pp`` frames with ``status == "processing"`` are throttled to one per second (DESIGN §9.5)."""

MODES = ("extract", "download", "outtmpl", "selftest")

PROGRESS_KEYS = (
    "status",
    "filename",
    "tmpfilename",
    "downloaded_bytes",
    "total_bytes",
    "total_bytes_estimate",
    "fragment_index",
    "fragment_count",
    "speed",
    "eta",
    "msg",
    "elapsed",
)
"""The legacy ``put_status`` allow-list plus ``elapsed`` (DESIGN §9.4).

Forwarding an allow-list rather than the whole hook dict is what stops a ``YTDL_OPTIONS`` value
from making frames unboundedly large.
"""

ENTRY_KEYS = (
    "_type",
    "id",
    "title",
    "url",
    "webpage_url",
    "original_url",
    "duration",
    "live_status",
    "is_live",
    "was_live",
    "release_timestamp",
    "uploader",
    "uploader_id",
    "channel",
    "channel_id",
    "thumbnail",
    "ext",
    "filesize_approx",
    "availability",
    "extractor",
    "extractor_key",
    "playlist_index",
    "playlist_count",
    "playlist_title",
    "n_entries",
    "msg",
)
"""The flat-entry subset an ``entry`` frame carries. Bounded on purpose: a 500-item playlist
must not put 500 full info dicts on the wire."""

ROOT_KEYS = (
    "_type",
    "id",
    "title",
    "webpage_url",
    "extractor",
    "playlist_count",
    "uploader",
    "uploader_id",
)
"""The ``resolved`` frame's ``root`` object (DESIGN §9.3)."""

CAPTION_EXTS = (".vtt", ".srt", ".sbv", ".scc", ".ttml", ".dfxp")
"""``policy.caption_exts`` default, matching legacy ``allowed_caption_exts``."""

_ANSI_RE = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x1b\x07]*(?:\x07|\x1b\\))")
_WS_RE = re.compile(r"\s+")

_MEDIA_POSTPROCESSORS = ("Merger", "FFmpegExtractAudio", "FFmpegVideoConvertor")
"""Postprocessors whose ``finished`` hook nominates the primary artifact; last one wins."""


class ShimError(Exception):
    """A job the shim refuses: bad JSON, an unknown mode, an unknown ``coerce`` name.

    Always reported as ``error{code:"bad_job"}`` and exit code 2 (DESIGN §9.6 ``bad_job``).
    """


class Watchdog(Exception):
    """The shim's own deadline fired (DESIGN §9.6 ``timeout``)."""


class Canceled(Exception):
    """SIGTERM or SIGINT arrived (DESIGN §9.6 ``canceled``)."""


# --------------------------------------------------------------------------------------------
# The protocol channel.
# --------------------------------------------------------------------------------------------


def _fd_is_writable(fd):
    """Whether ``fd`` is open and opened for writing.

    Probed **before** anything is duplicated, because ``os.dup`` hands out the lowest free
    descriptor and would otherwise make a closed fd 3 look open.
    """
    try:
        os.fstat(fd)
        mode = fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_ACCMODE
    except OSError:
        return False
    return mode in (os.O_WRONLY, os.O_RDWR)


def open_channel_stream():
    """Redirects fd 1 to ``/dev/null`` and returns the writable protocol stream.

    The original stdout is duplicated before the redirect, so that when fd 3 is closed the
    frames still have somewhere honest to go while plugin chatter on fd 1 is still discarded.
    """
    have_protocol_fd = _fd_is_writable(PROTOCOL_FD)

    # A broken stdout must not stop the job.
    with contextlib.suppress(Exception):
        sys.stdout.flush()

    saved = os.dup(1)
    devnull = os.open(os.devnull, os.O_WRONLY)
    try:
        os.dup2(devnull, 1)
    finally:
        os.close(devnull)

    if have_protocol_fd:
        os.close(saved)
        fd = PROTOCOL_FD
    else:
        fd = saved
    return os.fdopen(fd, "w", encoding="utf-8", newline="\n")


class Channel:
    """The frame writer: owns the ``n`` counter and the one-line-one-flush rule."""

    def __init__(self, stream):
        self._stream = stream
        self._n = 0

    @property
    def frames(self):
        """How many frames have been written."""
        return self._n

    def emit(self, t, **fields):
        """Writes one frame. Raises ``OSError`` if the channel is gone, which is fatal."""
        self._n += 1
        frame = {"v": ENVELOPE_VERSION, "t": t, "n": self._n, "ts": round(time.time(), 6)}
        frame.update(fields)
        self._stream.write(json.dumps(frame, ensure_ascii=False, default=repr) + "\n")
        self._stream.flush()

    def raw(self, line):
        """Writes an already-encoded frame line verbatim (``--replay``)."""
        self._n += 1
        self._stream.write(line.rstrip("\n") + "\n")
        self._stream.flush()


# --------------------------------------------------------------------------------------------
# Small helpers.
# --------------------------------------------------------------------------------------------


def clean_message(raw):
    """Strips ``ERROR: `` prefixes, ANSI escapes and control characters; caps at 512 chars.

    Cleaning happens once, here, so no consumer has to regex-match prose (DESIGN §9.6). The
    Rust side applies the identical transform, which is idempotent.
    """
    text = _ANSI_RE.sub("", str(raw)).replace("\r", "")
    text = text.strip()
    while text.startswith("ERROR: "):
        text = text[len("ERROR: ") :].lstrip()
    text = _WS_RE.sub(" ", text).strip()
    return text[:MAX_MESSAGE_CHARS]


def jsonable(obj, depth=0):
    """Coerces a yt-dlp value into something ``json.dumps`` accepts.

    Live streams and newer yt-dlp releases nest generators, sets and non-serialisable objects
    inside ``info_dict``; legacy had ``_sanitize_entry_for_pickle`` for the same reason.
    """
    if depth > MAX_JSON_DEPTH:
        return None
    if obj is None or isinstance(obj, (bool, int, str)):
        return obj
    if isinstance(obj, float):
        return obj if math.isfinite(obj) else None
    if isinstance(obj, bytes):
        return obj.decode("utf-8", "replace")
    if isinstance(obj, dict):
        return {str(k): jsonable(v, depth + 1) for k, v in obj.items()}
    if isinstance(obj, (list, tuple, set, frozenset)):
        return [jsonable(v, depth + 1) for v in obj]
    try:
        iter(obj)
    except TypeError:
        return repr(obj)
    try:
        return [jsonable(v, depth + 1) for v in obj]
    except Exception:  # noqa: BLE001 - an exploding iterator is not worth the job
        return None


def pick(source, keys):
    """The subset of ``source`` named by ``keys``, sanitised, absent keys omitted."""
    return {k: jsonable(source[k]) for k in keys if k in source}


def file_size(path):
    """``os.path.getsize`` that answers ``None`` instead of raising."""
    try:
        return os.path.getsize(path)
    except OSError:
        return None


def stream_of(info):
    """``"video"`` / ``"audio"`` / ``"fragment"`` / ``"unknown"`` for a frame (DESIGN §9.4).

    Derived from the codecs rather than from the filename, so the parent's monotonic-percent
    reset per merge leg is deterministic. Checked in order, because a DASH video stream has
    both a ``vcodec`` and fragments and is a *video* leg.
    """
    vcodec = info.get("vcodec")
    if vcodec and vcodec != "none":
        return "video"
    acodec = info.get("acodec")
    if acodec and acodec != "none":
        return "audio"
    if info.get("fragments") or info.get("fragment_base_url"):
        return "fragment"
    return "unknown"


def plugin_names():
    """The yt-dlp plugin packages that are actually loaded.

    Deliberately defensive: plugin discovery is the least stable corner of the yt-dlp API and a
    nightly bump must never be able to break ``hello``.
    """
    try:
        from yt_dlp import plugins as ytdlp_plugins
    except Exception:  # noqa: BLE001
        return []
    loader = getattr(ytdlp_plugins, "load_all_plugins", None)
    if callable(loader):
        with contextlib.suppress(Exception):
            loader()
    found = set()
    for name in list(sys.modules):
        parts = name.split(".")
        if parts[0] == "yt_dlp_plugins" and len(parts) >= 3:
            found.add(parts[2])
    return sorted(found)


def ytdlp_version():
    """The installed yt-dlp version, or ``None`` when yt-dlp is not importable.

    Read from ``yt_dlp.version`` first: recent releases no longer re-export ``__version__`` from
    the package root, and the nightly pin is exactly the thing ``/version`` and ``healthz``
    report.
    """
    try:
        from yt_dlp.version import __version__ as version
    except Exception:  # noqa: BLE001
        version = None
    if version:
        return str(version)
    try:
        import yt_dlp
    except Exception:  # noqa: BLE001
        return None
    return getattr(yt_dlp, "__version__", None)


def convert_srt_to_txt(path):
    """Legacy ``_convert_srt_to_txt_file``: strips cue numbers, timestamps and tags.

    Returns the ``.txt`` path, or ``None`` when the conversion failed.
    """
    txt_path = os.path.splitext(path)[0] + ".txt"
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            content = handle.read()
        content = content.replace("\r\n", "\n").replace("\r", "\n")
        cues = []
        for block in re.split(r"\n{2,}", content):
            lines = [line.strip() for line in block.split("\n") if line.strip()]
            if not lines:
                continue
            if re.fullmatch(r"\d+", lines[0]):
                lines = lines[1:]
            if lines and "-->" in lines[0]:
                lines = lines[1:]
            text_lines = []
            for line in lines:
                if "-->" in line:
                    continue
                clean = re.sub(r"<[^>]+>", "", line).strip()
                if clean:
                    text_lines.append(clean)
            if text_lines:
                cues.append(" ".join(text_lines))
        with open(txt_path, "w", encoding="utf-8") as handle:
            if cues:
                handle.write("\n".join(cues))
                handle.write("\n")
        return txt_path
    except OSError:
        return None


# --------------------------------------------------------------------------------------------
# The job.
# --------------------------------------------------------------------------------------------

COERCIONS = ("ImpersonateTarget",)
"""The coercion names this shim understands (DESIGN §9.2). Adding one is a shim-only change."""


class Policy:
    """The small amount of decision-making the shim must do locally (DESIGN §9.2).

    It has to be local because these decisions need the postprocessor ``info_dict``, which never
    crosses the boundary.
    """

    def __init__(self, raw):
        raw = raw or {}
        self.download_type = str(raw.get("download_type") or "video")
        self.download_dir = raw.get("download_dir") or ""
        self.temp_dir = raw.get("temp_dir") or ""
        exts = raw.get("caption_exts") or CAPTION_EXTS
        self.caption_exts = tuple(str(e).lower() for e in exts)
        self.convert_srt_to_txt = bool(raw.get("convert_srt_to_txt"))
        self.thumbnail_ext_rewrite = bool(raw.get("thumbnail_ext_rewrite"))
        every = raw.get("emit_progress_every_ms")
        self.emit_progress_every_ms = (
            DEFAULT_EMIT_PROGRESS_EVERY_MS if every is None else max(0, int(every))
        )
        self.debug = bool(raw.get("debug"))
        self.hard_timeout_ms = max(0, int(raw.get("hard_timeout_ms") or 0))
        self.pot_url = raw.get("pot_url")

    def accepts_caption(self, path):
        """Whether a produced caption file may be reported as an artifact.

        Legacy dropped media-like placeholders in captions mode by extension, and the extension
        set depends on whether the request asked for ``txt`` (DESIGN §9.2).
        """
        allowed = (".txt",) if self.convert_srt_to_txt else self.caption_exts
        return str(path).lower().endswith(tuple(allowed))


def parse_job(raw_line):
    """Parses the single stdin line into a job dict.

    Raises ``ShimError`` for anything that is not a JSON object with a known ``mode``.
    """
    if raw_line is None or not raw_line.strip():
        raise ShimError("no job on stdin")
    try:
        job = json.loads(raw_line)
    except ValueError as exc:
        raise ShimError(f"job is not valid JSON: {exc}") from exc
    if not isinstance(job, dict):
        raise ShimError("job must be a JSON object")
    mode = job.get("mode")
    if mode not in MODES:
        raise ShimError(f"unknown mode {mode!r}; expected one of {', '.join(MODES)}")
    coerce = job.get("coerce") or {}
    if not isinstance(coerce, dict):
        raise ShimError("coerce must be an object")
    for key, name in coerce.items():
        if name not in COERCIONS:
            raise ShimError(f"unknown coercion {name!r} for option {key!r}")
    if mode in ("extract", "download") and not job.get("url"):
        raise ShimError(f"mode {mode} requires a url")
    if mode == "outtmpl" and not isinstance(job.get("templates"), list):
        raise ShimError("mode outtmpl requires a templates array")
    return job


def build_options(job):
    """The merged yt-dlp option dict, with ``coerce`` applied.

    Rust hands over a fully merged dict (env, file, presets, per-request overrides, plus the
    ``formats``/``opts`` port); the only thing left to do is turn the handful of string values
    that name Python objects into those objects.
    """
    options = job.get("options")
    if options is None:
        options = {}
    # Checked before any falsiness shortcut: `job.get("options") or {}` would have turned an
    # empty list into an empty dict and downloaded with default options, which is exactly the
    # kind of silent type coercion `bad_job` exists to prevent.
    if not isinstance(options, dict):
        raise ShimError(f"options must be an object, not {type(options).__name__}")
    options = dict(options)
    for key, name in (job.get("coerce") or {}).items():
        if key not in options:
            continue
        if name == "ImpersonateTarget":
            try:
                from yt_dlp.networking.impersonate import ImpersonateTarget
            except Exception as exc:
                raise ShimError(f"cannot coerce {key!r}: {exc}") from exc
            try:
                options[key] = ImpersonateTarget.from_str(str(options[key]))
            except Exception as exc:
                raise ShimError(f"invalid {key!r}: {clean_message(exc)}") from exc
    return options


# --------------------------------------------------------------------------------------------
# Error classification (DESIGN §9.6). Ordered: the first match wins.
# --------------------------------------------------------------------------------------------

_AUTH_RE = re.compile(r"sign in|log in|members-only|private video", re.IGNORECASE)
# `404`/`410` on the media or its page: the thing is gone. §1.6's `unavailable` is what tells a
# client to offer Delete rather than Retry, which is the right advice for a dead link.
_UNAVAILABLE_RE = re.compile(
    r"video unavailable|removed by the uploader|account.*terminated"
    r"|http error 404|http error 410",
    re.IGNORECASE,
)
_UPCOMING_RE = re.compile(r"premieres in|scheduled to start", re.IGNORECASE)
_NO_FORMAT_RE = re.compile(r"requested format is not available", re.IGNORECASE)
_BOT_RE = re.compile(
    r"confirm you'?re not a bot|failed to extract any player response", re.IGNORECASE
)
_HTTP_5XX_RE = re.compile(r"HTTP Error 5\d\d")
_THROTTLED_RE = re.compile(r"HTTP Error 429|too many requests", re.IGNORECASE)
# A full disk usually reaches us as yt-dlp's own prose rather than as an `OSError` with a readable
# `errno`: it catches the `OSError` and re-raises a `DownloadError` carrying the text
# ("Unable to create directory: [Errno 28] No space left on device"). Without the pattern that is
# an `internal`, which PROTOCOL §1.6 reserves for a server bug.
_DISK_RE = re.compile(
    r"no space left on device|\[errno 28\]|disk quota exceeded|not enough (?:free )?space",
    re.IGNORECASE,
)
# A page or API fetch that failed for any reason other than one of the specific codes above is a
# transport failure, which §1.6 says is worth retrying — and which the server has already retried.
_NETWORK_RE = re.compile(
    r"unable to (?:download|fetch) (?:the )?(?:webpage|api page|json|xml|m3u8|mpd|media file|data)"
    r"|read timed out|connection (?:reset|aborted|refused)"
    r"|temporary failure in name resolution|name or service not known",
    re.IGNORECASE,
)
# The two `errno` values that mean "there is nowhere to put this", by name rather than by number:
# `EDQUOT` is 122 on Linux and 69 on Darwin.
_DISK_ERRNOS = frozenset(
    value for value in (getattr(errno, name, None) for name in ("ENOSPC", "EDQUOT")) if value
)


def _ytdlp_exception(name):
    """``yt_dlp.utils.<name>`` if it exists, else a class nothing can be an instance of."""
    try:
        from yt_dlp import utils
    except Exception:  # noqa: BLE001
        return Watchdog  # unreachable by any yt-dlp exception
    return getattr(utils, name, None) or Watchdog


def classify(exc, live_status=None):
    """Maps an exception onto a §9.6 ``code`` plus its retry and fatality flags.

    Exception classes are consulted first, then a small ordered regex table over the cleaned
    message. The table lives here so it versions with the yt-dlp pin; the parent maps codes
    mechanically and never regex-matches prose itself.
    """
    message = clean_message(exc)
    extractor = None
    for attr in ("ie", "extractor", "extractor_key"):
        value = getattr(exc, attr, None)
        if isinstance(value, str) and value:
            extractor = value
            break

    if isinstance(exc, (Canceled, KeyboardInterrupt)):
        code = "canceled"
    elif isinstance(exc, Watchdog):
        code = "timeout"
    elif isinstance(exc, ShimError):
        code = "bad_job"
    elif isinstance(exc, _ytdlp_exception("UnsupportedError")):
        code = "unsupported_url"
    elif isinstance(exc, _ytdlp_exception("GeoRestrictedError")):
        code = "geo_restricted"
    elif _is_disk_full(exc) or _DISK_RE.search(message):
        code = "disk_full"
    elif _BOT_RE.search(message):
        # Checked **before** ``auth_required``, one row earlier than the DESIGN §9.6 table lists
        # it. The canonical YouTube message is "Sign in to confirm you're not a bot", which
        # matches both patterns; classifying it as ``auth_required`` would tell the user to add
        # cookies when the actual signal is "the POT sidecar is not working". The specific
        # pattern therefore wins over the generic one.
        code = "bot_check"
    elif _AUTH_RE.search(message):
        code = "auth_required"
    elif _UNAVAILABLE_RE.search(message):
        code = "unavailable"
    elif live_status == "is_upcoming" or _UPCOMING_RE.search(message):
        code = "not_yet_live"
    elif _NO_FORMAT_RE.search(message):
        code = "no_format"
    elif _THROTTLED_RE.search(message):
        code = "throttled"
    elif _HTTP_5XX_RE.search(message) or _NETWORK_RE.search(message) or _is_transport(exc):
        code = "network"
    elif isinstance(exc, _ytdlp_exception("PostProcessingError")):
        code = "postprocessing_failed"
    else:
        code = "internal"

    return {
        "code": code,
        "message": message or type(exc).__name__,
        "retryable": code in ("network", "throttled"),
        "extractor": extractor,
        "fatal": code != "canceled",
        "provider_code": type(exc).__name__,
    }


def _is_disk_full(exc):
    """Whether ``exc``, or anything it wraps, is an out-of-space ``OSError``."""
    seen = 0
    current = exc
    while isinstance(current, BaseException) and seen < 8:
        if isinstance(current, OSError) and current.errno in _DISK_ERRNOS:
            return True
        current = current.__cause__ or current.__context__
        seen += 1
    return False


def _is_transport(exc):
    """Whether ``exc`` (or the ``DownloadError`` wrapping it) is a transport failure."""
    import socket
    import urllib.error

    seen = 0
    current = exc
    while current is not None and seen < 8:
        if isinstance(current, (urllib.error.URLError, socket.timeout, TimeoutError)):
            return True
        if isinstance(current, ConnectionError):
            return True
        current = getattr(current, "exc_info", None)
        if isinstance(current, tuple):
            current = current[1] if len(current) > 1 else None
        elif current is not None and not isinstance(current, BaseException):
            current = None
        seen += 1
    return False


# --------------------------------------------------------------------------------------------
# The yt-dlp logger: every yt-dlp diagnostic becomes a `log` frame.
# --------------------------------------------------------------------------------------------


class FrameLogger:
    """A yt-dlp ``logger`` that turns diagnostics into ``log`` frames.

    Installing this also keeps yt-dlp's own writes off fd 1 and fd 2 (which matters even though
    fd 1 already points at ``/dev/null``: it keeps the stderr ring in the parent readable).
    """

    def __init__(self, channel, policy):
        self._channel = channel
        self._policy = policy
        self._count = 0

    def _emit(self, level, message):
        if self._count >= MAX_LOG_FRAMES:
            return
        self._count += 1
        self._channel.emit("log", level=level, message=clean_message(message), extractor=None)

    def debug(self, msg):
        """yt-dlp's debug channel, which is also where its screen output lands.

        Forwarded only when ``policy.debug`` is set. Without the gate a single download emits
        one frame per repaint of the ``[download] 42.1% of ...`` line, which is precisely the
        traffic the §9.4 rate limit exists to avoid.
        """
        if self._policy.debug:
            self._emit("debug", msg)

    def info(self, msg):
        """yt-dlp's info channel. Same gate as :meth:`debug`: it is screen chatter."""
        if self._policy.debug:
            self._emit("info", msg)

    def warning(self, msg):
        """yt-dlp's warning channel."""
        self._emit("warning", msg)

    def error(self, msg):
        """yt-dlp's error channel. Not terminal on its own: only an ``error`` frame is."""
        self._emit("error", msg)


# --------------------------------------------------------------------------------------------
# mode = extract
# --------------------------------------------------------------------------------------------


def needs_strict_retry(entry):
    """Legacy ``__needs_strict_extract_retry``, verbatim.

    A flat extraction that produced a *video* whose ``formats`` list is present but **empty**
    told us nothing useful, so it is retried with ``extract_flat=False`` and
    ``ignore_no_formats_error=False``. Note the exact condition: ``formats is None`` (never
    asked) and a non-empty ``formats`` both mean "no retry".
    """
    if not isinstance(entry, dict):
        return False
    if (entry.get("_type") or "video") != "video":
        return False
    formats = entry.get("formats")
    if formats is None or formats:
        return False
    return bool(entry.get("id") or entry.get("url") or entry.get("webpage_url"))


def run_extract(channel, job, options, policy):
    """Streams a resolution: ``resolved`` (containers only), ``entry``*, ``info``, ``result``."""
    import yt_dlp

    extract = job.get("extract") or {}
    params = dict(options)
    # MeTube's own extraction keys are applied **after** the user options, so a preset cannot
    # break `extract_flat` / `noplaylist` (legacy `__extract_info`, Appendix A §6).
    params["extract_flat"] = bool(extract.get("flat", True))
    params["noplaylist"] = bool(extract.get("noplaylist", True))
    params["ignore_no_formats_error"] = True
    params["quiet"] = not policy.debug
    params["verbose"] = policy.debug
    params["no_color"] = True
    params["logger"] = FrameLogger(channel, policy)
    playlist_end = extract.get("playlist_end")
    if playlist_end:
        params["playlistend"] = int(playlist_end)

    url = job["url"]
    with yt_dlp.YoutubeDL(params) as ydl:
        info = ydl.extract_info(url, download=False)

    if extract.get("strict_retry", True) and needs_strict_retry(info):
        channel.emit("phase", msg="Retrying extraction")
        strict = dict(params)
        strict["extract_flat"] = False
        strict["ignore_no_formats_error"] = False
        with yt_dlp.YoutubeDL(strict) as ydl:
            info = ydl.extract_info(url, download=False)

    if not isinstance(info, dict):
        # Legacy produced exactly this string from `__add_entry` (DESIGN §8.4, §11.7).
        raise _entry_error("unsupported_url", "Invalid/empty data was given.")

    etype = info.get("_type") or "video"
    if etype in ("playlist", "multi_video"):
        etype = "playlist"
    if etype not in ("video", "playlist", "channel") and not etype.startswith("url"):
        raise _entry_error("unsupported_url", f'Unsupported resource "{etype}"')

    max_entries = int(extract.get("max_entries") or 0)
    count = 0
    truncated = False

    if etype in ("playlist", "channel"):
        root = pick(info, ROOT_KEYS)
        root["_type"] = etype
        root["type"] = etype
        channel.emit("resolved", root=root)
        for index, child in enumerate(info.get("entries") or [], start=1):
            if max_entries and count >= max_entries:
                truncated = True
                break
            if not isinstance(child, dict):
                continue
            count += 1
            channel.emit(
                "entry",
                index=index,
                entry=pick(child, ENTRY_KEYS),
                note=jsonable(child.get("msg")),
            )
        channel.emit("info", entry=_root_info(info))
    else:
        count = 1
        channel.emit(
            "resolved",
            root=dict(pick(info, ROOT_KEYS), type=etype),
        )
        channel.emit("entry", index=1, entry=pick(info, ENTRY_KEYS), note=jsonable(info.get("msg")))
        channel.emit("info", entry=_root_info(info))

    channel.emit("result", ok=True, count=count, truncated=truncated)


# Keys the ``info`` frame leaves out. Every one of them is per-format or per-caption data that
# the server never reads back: the frame becomes the entry's ``state``, and the only things read
# out of that are the playlist/channel fields for ``%(playlist_title)s`` (DESIGN §7.5) and the
# hints; a download re-extracts from the URL, and the NFO hook reads the ``.info.json`` yt-dlp
# writes next to the file. They are also the whole of the frame's weight: ``formats`` is ~450 KB
# for one YouTube video, and with ``writesubtitles`` on, yt-dlp expands ``automatic_captions`` to
# every translation target — 183 languages × ~150 formats, 11 MB for a fifteen-minute clip —
# which blew straight through the 8 MiB line cap (DESIGN §9.4) and failed every YouTube add with
# "wrote a line longer than 8388608 bytes".
INFO_FRAME_DROP = (
    "entries",
    "formats",
    "requested_formats",
    "requested_downloads",
    "automatic_captions",
    "subtitles",
    "requested_subtitles",
    "heatmap",
)


def _root_info(info):
    """The ``sanitize_info``'d root dict, minus ``entries`` and the per-format bulk.

    The children already crossed as ``entry`` frames, and a 500-item playlist's full info dict
    is megabytes of duplication; see :data:`INFO_FRAME_DROP` for why the format and caption
    tables go with them.
    """
    try:
        import yt_dlp

        clean = yt_dlp.YoutubeDL.sanitize_info(info)
    except Exception:  # noqa: BLE001
        clean = info
    if isinstance(clean, dict):
        clean = {k: v for k, v in clean.items() if k not in INFO_FRAME_DROP}
    return jsonable(clean)


class _EntryError(Exception):
    """An extraction outcome that is an error with a *pre-classified* code."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def _entry_error(code, message):
    return _EntryError(code, message)


# --------------------------------------------------------------------------------------------
# Sidecars that must travel with the media (DESIGN §9.2)
# --------------------------------------------------------------------------------------------

HOME_SIDECAR_TYPES = ("description", "infojson")
"""The output types yt-dlp resolves through ``paths[<type>]`` but never moves itself.

``YoutubeDL.process_info`` writes subtitles and thumbnails next to the **temp** file and hands
both to ``MoveFilesAfterDownloadPP`` through ``__files_to_move``. These two are written straight
to ``get_output_path(<type>)`` — i.e. ``paths.home`` unless the type carries its own entry — and
nothing registers them, so with a per-job scratch directory (``paths.temp`` != ``paths.home``)
the media sits in the scratch directory while its ``.info.json`` and ``.description`` are already
at the destination. Every ``Exec`` postprocessor that resolves a sidecar relative to
``%(filepath)q`` breaks on that split, MeTube's ``jellyfin_nfo_generator.py`` included: user
postprocessors default to ``when='post_process'``, which runs *before* the move.

Rust points these types at ``paths.temp`` (``provider.rs::pin_sidecars_to_the_scratch_dir``) and
``SidecarsTravelWithTheMedia`` carries the result out again. Link shortcut files
(``writeurllink`` and friends) are deliberately **not** redirected: nothing resolves a ``.url``
from ``%(filepath)q``, and their names come from a private helper this shim would have to
reimplement to register them.
"""

TRANSIENT_SUFFIXES = (".part", ".ytdl", ".temp", ".tmp", ".aria2", ".swp")
"""Scratch-directory names that are yt-dlp's own bookkeeping and must never be moved.

``.part``/``.part-Frag*`` are the in-flight download, ``.ytdl`` the resume state, and a file that
still exists under one of these names when the postprocessors run is either a leftover of a
failed leg or something the downloader is about to clean up itself.
"""


class SidecarsTravelWithTheMedia:
    """The last ``post_process`` postprocessor: everything beside the media moves with it.

    Registered with ``add_post_processor(..., when='post_process')`` *after*
    ``YoutubeDL.__init__`` has registered the user's own postprocessors, and
    ``add_post_processor`` appends — so this runs after every user postprocessor and immediately
    before ``MoveFilesAfterDownloadPP`` (``YoutubeDL.post_process``: ``run_all_pps('post_process')``
    → ``run_pp(MoveFilesAfterDownloadPP)`` → ``run_all_pps('after_move')``). That is the only
    point in the run where the scratch directory holds its final contents, which is what makes a
    sweep the right shape and a fixed list of sidecar types the wrong one:

    * yt-dlp never registers the ``.info.json`` or the ``.description`` for the move (see
      :data:`HOME_SIDECAR_TYPES`), so without this they would be left behind, and
    * a user ``Exec`` postprocessor runs *here*, in the scratch directory, and whatever it writes
      there — MeTube's ``jellyfin_nfo_generator.py`` writes ``<base>.nfo`` — is a file yt-dlp has
      never heard of and would leave behind too. Sweeping picks it up; naming types cannot.

    The sweep also **drops** registrations whose source has since disappeared. The same legacy
    script deletes the ``.info.json`` once it has read it, and an entry MoveFiles cannot find is
    an unexplained ``File "…" cannot be found`` warning on a download that went perfectly.

    Only files sharing the media's stem are taken, and never :data:`TRANSIENT_SUFFIXES`: the
    scratch directory is per job (DESIGN §8.7), but a fragment or a ``.part`` left by a failed leg
    has no business in the library.

    Duck-typed on purpose: ``YoutubeDL.add_post_processor`` only calls ``set_downloader`` and
    ``YoutubeDL.run_pp`` only calls ``run``, so this needs neither an import of
    ``yt_dlp.postprocessor`` nor the metaclass that would make it emit ``started``/``finished``
    ``pp`` frames of its own and change the transcript.
    """

    def __init__(self):
        self._downloader = None

    def set_downloader(self, downloader):
        """The half of the postprocessor protocol ``add_post_processor`` uses."""
        self._downloader = downloader

    def run(self, info):
        """Prunes the vanished registrations, then registers the scratch directory's leftovers."""
        try:
            files_to_move = info.setdefault("__files_to_move", {})
            self._drop_vanished(files_to_move)
            self._sweep(info, files_to_move)
        except Exception as exc:  # noqa: BLE001 - a sidecar must never fail a download
            # Loud on purpose: a failure here strands the sidecars in a directory the engine
            # deletes, and `write_debug` is a no-op unless the operator set LOGLEVEL=DEBUG.
            self._warn(f"could not keep the sidecars with the media: {clean_message(exc)}")
        return [], info

    # -- the two halves ----------------------------------------------------------------------

    def _drop_vanished(self, files_to_move):
        """Forgets every registration whose source a postprocessor has since removed."""
        for source in [p for p in files_to_move if p and not os.path.exists(p)]:
            del files_to_move[source]
            self._debug(f"a postprocessor consumed {source}; it will not be moved")

    def _sweep(self, info, files_to_move):
        """Registers every companion file sitting in the scratch directory with the media."""
        media = info.get("filepath")
        if not media:
            return
        scratch = os.path.dirname(os.path.abspath(media))
        if not self._is_scratch(scratch):
            return
        registered = {os.path.abspath(p) for p in files_to_move}
        stem = os.path.splitext(os.path.basename(media))[0]
        for name in sorted(os.listdir(scratch)):
            path = os.path.join(scratch, name)
            absolute = os.path.abspath(path)
            if absolute == os.path.abspath(media) or absolute in registered:
                continue
            if not self._is_companion(name, stem) or not os.path.isfile(path):
                continue
            # `''` is yt-dlp's own "next to the media" destination: `MoveFilesAfterDownloadPP`
            # turns a falsy value into `os.path.join(info['__finaldir'], basename)`.
            files_to_move[path] = ""
            self._debug(f"{name} moves with the media")

    # -- predicates --------------------------------------------------------------------------

    def _is_scratch(self, directory):
        """Whether `directory` is this job's scratch directory rather than the destination.

        With ``paths.temp == paths.home`` (MeTube's layout, and Aulos' when ``TEMP_DIR`` is the
        download directory) there is no move to make and nothing to sweep: the files are already
        where they belong, and registering them would only make MoveFiles compare a path to
        itself.
        """
        ydl = self._downloader
        if ydl is None:
            return False
        temp = os.path.abspath(ydl.get_output_path("temp"))
        home = os.path.abspath(ydl.get_output_path("home"))
        return temp != home and os.path.abspath(directory) == temp

    @staticmethod
    def _is_companion(name, stem):
        """Whether `name` is a sidecar of the media called `stem` rather than a work file.

        ``<stem>.<something>`` only: a file named exactly the stem, with no extension at all, is
        not a sidecar of anything, and an audio job whose media went ``clip.mp4`` → ``clip.mp3``
        would otherwise sweep up the original that ``FFmpegExtractAudio`` is about to delete.
        """
        if not stem or not name.startswith(stem + "."):
            return False
        lowered = name.lower()
        if ".part-frag" in lowered:
            return False
        return not lowered.endswith(TRANSIENT_SUFFIXES)

    # -- diagnostics -------------------------------------------------------------------------

    def _warn(self, message):
        """``report_warning`` reaches the shim's ``FrameLogger`` at every ``LOGLEVEL``."""
        report = getattr(self._downloader, "report_warning", None)
        if report is None:
            return
        with contextlib.suppress(Exception):
            report(message)

    def _debug(self, message):
        """The per-file chatter, which only an operator running with ``LOGLEVEL=DEBUG`` wants."""
        write_debug = getattr(self._downloader, "write_debug", None)
        if write_debug is not None:
            with contextlib.suppress(Exception):
                write_debug(message)


class SidecarPathsAreFinal:
    """The first ``after_move`` postprocessor: the info dict's sidecar paths are the moved ones.

    ``MoveFilesAfterDownloadPP`` updates ``filepath`` but not ``infojson_filename``, because
    upstream never moves the info json: it was written at the destination in the first place. Now
    that it travels with the media, the recorded path would otherwise name a scratch directory
    that no longer holds it — a lie for any user ``after_move`` postprocessor reading
    ``%(infojson_filename)q``, which is exactly what an operator is told to use to get the final
    path.

    Ordering is the whole point, so this is inserted at the **head** of the ``after_move`` chain
    rather than appended: ``add_post_processor`` appends, and the user's own ``after_move``
    postprocessors are already in the list by the time the shim gets to register anything.
    """

    def __init__(self):
        self._downloader = None

    def set_downloader(self, downloader):
        """The half of the postprocessor protocol ``add_post_processor`` uses."""
        self._downloader = downloader

    def run(self, info):
        """Repoints every recorded sidecar that MoveFiles carried into the final directory."""
        with contextlib.suppress(Exception):
            finaldir = os.path.dirname(os.path.abspath(str(info.get("filepath") or "")))
            for key in ("infojson_filename", "__infojson_filename"):
                recorded = info.get(key)
                if not recorded or os.path.isfile(recorded):
                    continue
                moved = os.path.join(finaldir, os.path.basename(str(recorded)))
                if os.path.isfile(moved):
                    info[key] = moved
        return [], info


def install_sidecar_postprocessors(ydl):
    """Registers the two sidecar postprocessors in the stages — and the order — they need.

    ``SidecarsTravelWithTheMedia`` is appended to ``post_process`` so it runs after every user
    postprocessor and immediately before the move; ``SidecarPathsAreFinal`` is spliced in at the
    head of ``after_move`` so a user postprocessor there sees the corrected paths. yt-dlp's public
    API can only append (``add_post_processor``), so the head insert reaches for ``_pps`` and
    falls back to appending — which is still correct for everything but a user ``after_move``
    postprocessor that reads ``%(infojson_filename)q``.

    User postprocessors keep their own ``when`` and their own relative order either way.
    """
    ydl.add_post_processor(SidecarsTravelWithTheMedia(), when="post_process")

    final = SidecarPathsAreFinal()
    chain = getattr(ydl, "_pps", {}).get("after_move")
    if isinstance(chain, list):
        final.set_downloader(ydl)
        chain.insert(0, final)
    else:  # pragma: no cover - a yt-dlp that renamed `_pps`; appending still beats nothing.
        ydl.add_post_processor(final, when="after_move")


# --------------------------------------------------------------------------------------------
# mode = download
# --------------------------------------------------------------------------------------------


class DownloadRun:
    """Holds the hooks, the artifact set and the primary-file choice for one download."""

    def __init__(self, channel, policy):
        self.channel = channel
        self.policy = policy
        self.artifacts = []
        self._seen = set()
        self.filename = None
        self.size = None
        self.live_status = None
        self._last_progress = {}
        self._last_pp_processing = 0.0

    # -- artifacts ---------------------------------------------------------------------------

    def artifact(self, role, path, language=None, label=None, primary=False):
        """Records and emits one produced file, de-duplicated by ``(role, path)``."""
        if not path:
            return None
        key = (role, str(path))
        size = file_size(path)
        if key not in self._seen:
            self._seen.add(key)
            entry = {"role": role, "path": str(path), "size": size}
            if language:
                entry["language"] = language
            if label:
                entry["label"] = label
            self.artifacts.append(entry)
            self.channel.emit(
                "artifact",
                role=role,
                path=str(path),
                size=size,
                language=language,
                label=label,
            )
        if primary:
            self.filename = str(path)
            self.size = size
        return {"path": str(path), "size": size, "language": language, "label": label}

    # -- progress ----------------------------------------------------------------------------

    def on_progress(self, d):
        """yt-dlp ``progress_hooks`` → a ``progress`` frame, rate-limited per stream."""
        info = d.get("info_dict") or {}
        if info.get("live_status"):
            self.live_status = info.get("live_status")
        stream = stream_of(info)
        status = d.get("status")
        now = time.monotonic()
        if status == "downloading":
            budget = self.policy.emit_progress_every_ms / 1000.0
            if budget and (now - self._last_progress.get(stream, 0.0)) < budget:
                return
            self._last_progress[stream] = now

        frame = {k: jsonable(d[k]) for k in PROGRESS_KEYS if k in d}
        frame.setdefault("status", status)
        frame["stream"] = stream
        # The per-stream "this leg is complete" hint of DESIGN §9.4: yt-dlp already sets
        # `downloaded_bytes == total_bytes` here, but not always, and the parent's normaliser
        # reads the byte pair rather than a percent.
        if (
            status == "finished"
            and frame.get("total_bytes") is None
            and frame.get("downloaded_bytes") is not None
        ):
            frame["total_bytes"] = frame["downloaded_bytes"]
        self.channel.emit("progress", **frame)

    # -- postprocessing ----------------------------------------------------------------------

    def on_pp(self, d):
        """yt-dlp ``postprocessor_hooks`` → a ``pp`` frame plus any artifacts (DESIGN §9.5)."""
        name = d.get("postprocessor") or "?"
        status = d.get("status")
        info = d.get("info_dict") or {}
        filepath = info.get("filepath") or d.get("filepath")

        if status == "started":
            self.channel.emit("pp", postprocessor=name, status="started", filepath=filepath)
            return
        if status == "processing":
            now = time.monotonic()
            if now - self._last_pp_processing < PP_PROCESSING_THROTTLE_S:
                return
            self._last_pp_processing = now
            self.channel.emit("pp", postprocessor=name, status="processing", filepath=filepath)
            return
        if status != "finished":
            self.channel.emit("pp", postprocessor=name, status=str(status), filepath=filepath)
            return

        finaldir = info.get("__finaldir")
        subtitles = []
        chapters = []

        if name == "MoveFiles":
            if filepath and finaldir:
                filepath = os.path.join(str(finaldir), os.path.basename(str(filepath)))
            if self.policy.download_type == "captions":
                for track in (info.get("requested_subtitles") or {}).values():
                    if isinstance(track, dict) and track.get("filepath"):
                        got = self._caption(track["filepath"], track.get("ext"))
                        if got:
                            subtitles.append(got)
                # A media-like placeholder is not a caption file; legacy dropped it by extension.
                if filepath and self.policy.accepts_caption(filepath):
                    self.artifact("media", filepath, primary=True)
            else:
                self.artifact("media", self._rewrite_thumbnail(filepath), primary=True)
        elif name == "SplitChapters":
            for chapter in info.get("chapters") or []:
                if isinstance(chapter, dict) and chapter.get("filepath"):
                    got = self.artifact(
                        "chapter", chapter["filepath"], label=chapter.get("title")
                    )
                    if got:
                        chapters.append(got)
        elif name in _MEDIA_POSTPROCESSORS and filepath:
            self.artifact("media", self._rewrite_thumbnail(filepath), primary=True)
        elif name == "Exec" and d.get("returncode"):
            raise _entry_error(
                "postprocessing_failed",
                f"Exec postprocessor exited with code {d.get('returncode')}",
            )

        self.channel.emit(
            "pp",
            postprocessor=name,
            status="finished",
            filepath=filepath,
            finaldir=jsonable(finaldir),
            subtitles=subtitles,
            chapters=chapters,
        )

    def _caption(self, path, language=None):
        """Applies the caption policy to one subtitle file and records it."""
        output = str(path)
        if self.policy.convert_srt_to_txt and output.lower().endswith(".srt"):
            self.channel.emit("phase", msg="Converting captions")
            converted = convert_srt_to_txt(output)
            if converted:
                if converted != output:
                    with contextlib.suppress(OSError):
                        os.remove(output)
                output = converted
        if not self.policy.accepts_caption(output):
            return None
        got = self.artifact("subtitle", output, language=language)
        if got and (self.filename is None or self.policy.convert_srt_to_txt):
            # Captions mode links the first caption file as the item's primary result.
            self.filename = got["path"]
            self.size = got["size"]
        return got

    def _rewrite_thumbnail(self, path):
        """``.webm`` → ``.jpg`` for a thumbnail-only download (legacy ``update_status``)."""
        if not path or not self.policy.thumbnail_ext_rewrite:
            return path
        if self.policy.download_type != "thumbnail":
            return path
        return re.sub(r"\.webm$", ".jpg", str(path))


def run_download(channel, job, options, policy):
    """Runs the real download and emits the ``result`` frame."""
    import yt_dlp

    run = DownloadRun(channel, policy)
    params = dict(options)
    params["progress_hooks"] = [run.on_progress]
    params["postprocessor_hooks"] = [run.on_pp]
    params["logger"] = FrameLogger(channel, policy)
    params.setdefault("quiet", not policy.debug)
    params.setdefault("no_color", True)

    try:
        with yt_dlp.YoutubeDL(params) as ydl:
            # Not spliced into `postprocessors`: user postprocessors keep their own `when` (the
            # API default is `post_process`, i.e. before the move) and their own relative order
            # (DESIGN §9.2).
            install_sidecar_postprocessors(ydl)
            retcode = ydl.download([job["url"]])
    except Exception as exc:
        if run.live_status and not isinstance(exc, _EntryError):
            raise _LiveAware(exc, run.live_status) from exc
        raise

    channel.emit(
        "result",
        ok=retcode == 0,
        retcode=retcode,
        filename=run.filename,
        size=run.size,
        artifacts=run.artifacts,
    )


class _LiveAware(Exception):
    """Wraps a download failure together with the ``live_status`` seen while it ran."""

    def __init__(self, inner, live_status):
        super().__init__(str(inner))
        self.inner = inner
        self.live_status = live_status


# --------------------------------------------------------------------------------------------
# mode = outtmpl / selftest
# --------------------------------------------------------------------------------------------


def run_outtmpl(channel, job):
    """Evaluates output templates through yt-dlp's own ``evaluate_outtmpl`` (DESIGN §9.2)."""
    import yt_dlp

    templates = [str(t) for t in job.get("templates") or []]
    info = job.get("info")
    if info is None:
        info = {}
    if not isinstance(info, dict):
        raise ShimError(f"info must be an object, not {type(info).__name__}")
    with yt_dlp.YoutubeDL({"quiet": True, "no_color": True}) as ydl:
        evaluated = [ydl.evaluate_outtmpl(t, dict(info)) for t in templates]
    channel.emit("result", ok=True, templates=evaluated)


def run_selftest(channel):
    """Proves the interpreter can import yt-dlp. Backs ``Provider::probe``.

    A failed import is an ``internal`` error rather than a ``bad_job``: the job was fine, the
    image is not.
    """
    import yt_dlp  # noqa: F401 - the import *is* the test

    channel.emit("result", ok=True, yt_dlp=ytdlp_version(), mode="selftest")


# --------------------------------------------------------------------------------------------
# --replay
# --------------------------------------------------------------------------------------------


def run_replay(path):
    """Re-emits a recorded transcript verbatim on the protocol channel.

    Imports nothing: this is how the transport itself (spawn, fd 3, framing, the parent's
    ordering checks) is exercised with no network and no yt-dlp.
    """
    stream = open_channel_stream()
    channel = Channel(stream)
    try:
        with open(path, "r", encoding="utf-8") as handle:
            for line in handle:
                if line.strip():
                    channel.raw(line)
    except OSError as exc:
        sys.stderr.write(f"ERROR: cannot replay {path}: {exc}\n")
        return EXIT_INTERNAL
    finally:
        try:
            stream.close()
        except OSError:
            pass
    return EXIT_OK


# --------------------------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------------------------


def _install_signal_handlers():
    def on_term(_signum, _frame):
        raise Canceled("terminated")

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(sig, on_term)
        except (OSError, ValueError):
            pass


def _arm_watchdog(ms):
    if not ms:
        return

    def on_alarm(_signum, _frame):
        raise Watchdog(f"the shim watchdog fired after {ms} ms")

    try:
        signal.signal(signal.SIGALRM, on_alarm)
        signal.setitimer(signal.ITIMER_REAL, ms / 1000.0)
    except (OSError, ValueError, AttributeError):
        pass


def _disarm_watchdog():
    try:
        signal.setitimer(signal.ITIMER_REAL, 0.0)
    except (OSError, ValueError, AttributeError):
        pass


def main(argv):
    """Reads the job, runs it, and writes exactly one complete transcript."""
    if len(argv) > 1 and argv[1] == "--replay":
        if len(argv) < 3:
            sys.stderr.write("ERROR: --replay needs a transcript path\n")
            return EXIT_BAD_JOB
        return run_replay(argv[2])

    _install_signal_handlers()
    started = time.monotonic()
    stream = open_channel_stream()
    channel = Channel(stream)

    job = None
    job_error = None
    try:
        job = parse_job(sys.stdin.readline())
    except ShimError as exc:
        job_error = exc

    policy = Policy((job or {}).get("policy"))
    channel.emit(
        "hello",
        protocol=PROTOCOL,
        yt_dlp=ytdlp_version(),
        python=platform.python_version(),
        pid=os.getpid(),
        plugins=plugin_names(),
        pot={
            "available": any("pot" in name for name in plugin_names()),
            "url": policy.pot_url or os.environ.get("BGUTIL_POT_BASE_URL"),
        },
    )

    exit_code = EXIT_OK
    try:
        if job_error is not None:
            raise job_error
        declared = job.get("protocol", PROTOCOL)
        if declared != PROTOCOL:
            channel.emit(
                "error",
                code="bad_job",
                message=f"protocol {declared} is not supported; this shim speaks {PROTOCOL}",
                retryable=False,
                extractor=None,
                fatal=True,
                traceback=None,
            )
            exit_code = EXIT_PROTOCOL
        else:
            _arm_watchdog(policy.hard_timeout_ms)
            options = build_options(job)
            mode = job["mode"]
            if mode == "extract":
                run_extract(channel, job, options, policy)
            elif mode == "download":
                run_download(channel, job, options, policy)
            elif mode == "outtmpl":
                run_outtmpl(channel, job)
            else:
                run_selftest(channel)
    except _EntryError as exc:
        channel.emit(
            "error",
            code=exc.code,
            message=clean_message(exc),
            retryable=False,
            extractor=None,
            fatal=True,
            traceback=None,
        )
    except BaseException as exc:  # noqa: BLE001 - every failure becomes one `error` frame
        inner, live = (exc.inner, exc.live_status) if isinstance(exc, _LiveAware) else (exc, None)
        report = classify(inner, live)
        channel.emit(
            "error",
            code=report["code"],
            message=report["message"],
            retryable=report["retryable"],
            extractor=report["extractor"],
            fatal=report["fatal"],
            provider_code=report["provider_code"],
            traceback=None,
        )
        if report["code"] == "canceled":
            exit_code = EXIT_CANCELED
        elif report["code"] == "bad_job":
            exit_code = EXIT_BAD_JOB
    finally:
        _disarm_watchdog()

    channel.emit(
        "bye",
        elapsed_ms=int((time.monotonic() - started) * 1000),
        frames=channel.frames + 1,
        peak_rss_kb=_peak_rss_kb(),
    )
    try:
        stream.close()
    except OSError:
        pass
    return exit_code


def _peak_rss_kb():
    """Peak RSS in KiB, or ``None`` where ``resource`` is unavailable."""
    try:
        import resource

        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    except Exception:  # noqa: BLE001
        return None
    # Linux reports KiB, macOS reports bytes.
    return int(peak // 1024) if sys.platform == "darwin" else int(peak)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except BrokenPipeError:
        # The parent closed the pipe it is killing us through (DESIGN §16.4 step 6, the
        # process-group kill after the shutdown grace). The parent hanging up is the parent's
        # shutdown working, not this process failing, so it is a DEBUG note and not an ERROR —
        # `aulos-server` forwards our stderr verbatim, and an `ERROR:` here reads as a real
        # failure in the log a user stares at right after a restart.
        with contextlib.suppress(OSError):
            # Keep CPython's own interpreter-shutdown flush from raising a second time.
            devnull = os.open(os.devnull, os.O_WRONLY)
            os.dup2(devnull, sys.stdout.fileno())
        with contextlib.suppress(OSError):
            sys.stderr.write("DEBUG: ytdlp_runner exiting: the parent closed the protocol channel\n")
        sys.exit(EXIT_CANCELED)
    except OSError as exc:
        # The protocol channel itself failed. Nothing can be reported through it.
        sys.stderr.write(f"ERROR: ytdlp_runner protocol channel failed: {exc}\n")
        sys.exit(EXIT_INTERNAL)
