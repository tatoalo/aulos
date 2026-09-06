#!/usr/bin/env python3
"""WP-00 — dump the legacy yt-dlp format/option mapping to golden JSON.

Imports the legacy ``app/dl_formats.py`` and walks the **whole** legal
``(download_type, codec, format, quality)`` space (DESIGN §6.6, legacy spec §6),
writing:

  tests/golden/formats.json   -- the `get_format` selector string per tuple
  tests/golden/opts.json      -- the `get_opts` result per tuple, plus one entry
                                 per branch that the tuple sweep alone cannot
                                 reach (pre-existing `postprocessors`, a
                                 pre-existing `writethumbnail`, a caller-supplied
                                 `format` key on the best_remux path, …)

WP-06 diffs its Rust port against these two files. Nothing here touches the
network; ``dl_formats`` is pure.

Usage:
    /Users/apogliaghi/Development/metube_pot/.venv/bin/python \
        tools/capture/dump_formats.py
"""

from __future__ import annotations

import datetime as _dt
import sys

from _legacy import (  # noqa: E402  (sys.path juggling is the point)
    CAPTION_LANGUAGES,
    CAPTION_MODES,
    GOLDEN_DIR,
    add_legacy_to_syspath,
    legacy_commit,
    legal_tuples,
    tuple_key,
    write_json,
    ytdlp_version,
)

add_legacy_to_syspath()

import dl_formats  # noqa: E402


def _now() -> str:
    return _dt.datetime.now(_dt.UTC).replace(microsecond=0).isoformat()


def _provenance() -> dict[str, object]:
    return {
        "legacy_commit": legacy_commit(),
        "legacy_module": "app/dl_formats.py",
        "yt_dlp_version": ytdlp_version(),
        "python_version": sys.version.split()[0],
        "captured_at": _now(),
        "tool": "tools/capture/dump_formats.py",
    }


# ---------------------------------------------------------------------------
# formats.json
# ---------------------------------------------------------------------------

# Cases outside the API-legal space that `get_format` still has to handle,
# because `Download.__init__` calls it with whatever a persisted record carries
# and because the `custom:` escape hatch is reachable from YTDL_OPTIONS-driven
# flows. WP-06's port must reproduce these too.
FORMAT_EDGE_CASES: list[dict[str, object]] = [
    {
        "name": "custom_selector_passthrough",
        "why": "format.startswith('custom:') returns the remainder verbatim",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "custom:bestvideo[height<=720]+bestaudio",
            "quality": "1080",
        },
    },
    {
        "name": "custom_selector_wins_over_audio_type",
        "why": "the custom: check runs before the download_type dispatch",
        "args": {
            "download_type": "audio",
            "codec": "auto",
            "format": "custom:worstaudio",
            "quality": "best",
        },
    },
    {
        "name": "custom_selector_empty_remainder",
        "why": "'custom:' with nothing after it yields the empty selector",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "custom:",
            "quality": "best",
        },
    },
    {
        "name": "defaults_from_empty_strings",
        "why": "'' falls back to video/any/auto/best via the `or` defaults",
        "args": {
            "download_type": "",
            "codec": "",
            "format": "",
            "quality": "",
        },
    },
    {
        "name": "defaults_from_none",
        "why": "None falls back to video/any/auto/best",
        "args": {
            "download_type": None,
            "codec": None,
            "format": None,
            "quality": None,
        },
    },
    {
        "name": "whitespace_and_case_are_normalised",
        "why": ".strip().lower() on all four arguments",
        "args": {
            "download_type": "  VIDEO ",
            "codec": " H264 ",
            "format": " MP4 ",
            "quality": " 1080 ",
        },
    },
    {
        "name": "unknown_codec_falls_back_to_no_filter",
        "why": "CODEC_FILTER_MAP.get(codec, '') -- an unknown codec is not an error",
        "args": {
            "download_type": "video",
            "codec": "vp8",
            "format": "any",
            "quality": "720",
        },
    },
    {
        "name": "ios_ignores_the_codec_filter",
        "why": "the ios branch returns before codec_filter is consulted",
        "args": {
            "download_type": "video",
            "codec": "av1",
            "format": "ios",
            "quality": "1080",
        },
    },
    {
        "name": "mp4_best_remux_ignores_the_codec_filter",
        "why": "the best_remux branch returns before codec_filter is consulted",
        "args": {
            "download_type": "video",
            "codec": "h265",
            "format": "mp4",
            "quality": "best_remux",
        },
    },
    {
        "name": "any_best_remux_is_height_unfiltered",
        "why": (
            "best_remux is only API-legal for mp4, but get_format treats it as a "
            "no-height-filter quality for every video format"
        ),
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "any",
            "quality": "best_remux",
        },
    },
    {
        "name": "worst_is_not_a_height_filter",
        "why": (
            "the documented legacy quirk (DESIGN §6.6 notice): 'worst' produces the "
            "same selector as 'best'"
        ),
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "any",
            "quality": "worst",
        },
    },
    {
        "name": "thumbnail_selector_is_bestaudio_best",
        "why": "thumbnail short-circuits to bestaudio/best regardless of format",
        "args": {
            "download_type": "thumbnail",
            "codec": "auto",
            "format": "jpg",
            "quality": "best",
        },
    },
    {
        "name": "captions_selector_is_bestaudio_best",
        "why": "captions short-circuits to bestaudio/best regardless of format",
        "args": {
            "download_type": "captions",
            "codec": "auto",
            "format": "vtt",
            "quality": "best",
        },
    },
]

FORMAT_ERROR_CASES: list[dict[str, object]] = [
    {
        "name": "unknown_download_type",
        "args": {
            "download_type": "podcast",
            "codec": "auto",
            "format": "any",
            "quality": "best",
        },
    },
    {
        "name": "unknown_video_format",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "mkv",
            "quality": "best",
        },
    },
    {
        "name": "unknown_audio_format",
        "args": {
            "download_type": "audio",
            "codec": "auto",
            "format": "aac",
            "quality": "best",
        },
    },
]


def build_formats() -> dict[str, object]:
    selectors: dict[str, str] = {}
    for t in legal_tuples():
        selectors[tuple_key(t)] = dl_formats.get_format(
            t["download_type"], t["codec"], t["format"], t["quality"]
        )

    edge: list[dict[str, object]] = []
    for case in FORMAT_EDGE_CASES:
        args = case["args"]
        edge.append(
            {
                "name": case["name"],
                "why": case["why"],
                "args": args,
                "selector": dl_formats.get_format(
                    args["download_type"],
                    args["codec"],
                    args["format"],
                    args["quality"],
                ),
            }
        )

    errors: list[dict[str, object]] = []
    for case in FORMAT_ERROR_CASES:
        args = case["args"]
        try:
            dl_formats.get_format(
                args["download_type"], args["codec"], args["format"], args["quality"]
            )
        except ValueError as exc:
            errors.append(
                {"name": case["name"], "args": args, "raises": {"ValueError": str(exc)}}
            )
        else:  # pragma: no cover - would mean the legacy contract changed
            raise SystemExit(f"expected ValueError for {case['name']!r}")

    return {
        "provenance": _provenance(),
        "description": (
            "get_format(download_type, codec, format, quality) -> yt-dlp format "
            "selector, for every request tuple the legacy API admits, plus the "
            "edge cases the tuple sweep cannot reach."
        ),
        "key_format": "download_type|codec|format|quality",
        "codec_filter_map": dict(dl_formats.CODEC_FILTER_MAP),
        "selectors": selectors,
        "edge_cases": edge,
        "errors": errors,
    }


# ---------------------------------------------------------------------------
# opts.json
# ---------------------------------------------------------------------------

# Branches of `get_opts` that need a non-empty caller-supplied `ytdl_opts` or a
# caption mode/language other than the defaults.
OPTS_BRANCH_CASES: list[dict[str, object]] = [
    {
        "name": "audio_m4a_best_with_existing_writethumbnail",
        "why": "'writethumbnail' already present -> the thumbnail postprocessor trio is skipped",
        "args": {
            "download_type": "audio",
            "codec": "auto",
            "format": "m4a",
            "quality": "best",
        },
        "ytdl_opts": {"writethumbnail": False},
    },
    {
        "name": "audio_mp3_192_with_existing_postprocessors",
        "why": "caller postprocessors are appended after the type-derived ones",
        "args": {
            "download_type": "audio",
            "codec": "auto",
            "format": "mp3",
            "quality": "192",
        },
        "ytdl_opts": {"postprocessors": [{"key": "SponsorBlock"}]},
    },
    {
        "name": "video_mp4_best_remux_pops_caller_format",
        "why": "the best_remux branch deletes a caller-supplied 'format' key",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "mp4",
            "quality": "best_remux",
        },
        "ytdl_opts": {"format": "bestvideo[height<=480]", "noprogress": True},
    },
    {
        "name": "video_mp4_best_remux_with_existing_postprocessors",
        "why": "ordering is [derived] + [caller] + [late]; the Exec hook is always last",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "mp4",
            "quality": "best_remux",
        },
        "ytdl_opts": {"postprocessors": [{"key": "SponsorBlock"}]},
    },
    {
        "name": "video_any_best_is_a_passthrough",
        "why": "the plain video path adds nothing but the postprocessors key",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "any",
            "quality": "best",
        },
        "ytdl_opts": {"noplaylist": True},
    },
    {
        "name": "thumbnail_with_existing_postprocessors",
        "why": "skip_download + writethumbnail + convertor, then caller postprocessors",
        "args": {
            "download_type": "thumbnail",
            "codec": "auto",
            "format": "jpg",
            "quality": "best",
        },
        "ytdl_opts": {"postprocessors": [{"key": "FFmpegMetadata"}]},
    },
    {
        "name": "captions_txt_is_downloaded_as_srt",
        "why": "'txt' is rewritten to 'srt' for subtitlesformat (the txt conversion is post-hoc)",
        "args": {
            "download_type": "captions",
            "codec": "auto",
            "format": "txt",
            "quality": "best",
        },
        "ytdl_opts": {},
        "subtitle_language": "en",
        "subtitle_mode": "prefer_manual",
    },
    {
        "name": "captions_unknown_mode_falls_back_to_prefer_manual",
        "why": "_normalize_caption_mode: anything outside CAPTION_MODES -> prefer_manual",
        "args": {
            "download_type": "captions",
            "codec": "auto",
            "format": "srt",
            "quality": "best",
        },
        "ytdl_opts": {},
        "subtitle_language": "en",
        "subtitle_mode": "nonsense",
    },
    {
        "name": "captions_blank_language_falls_back_to_en",
        "why": "_normalize_subtitle_language: '' / whitespace -> 'en'",
        "args": {
            "download_type": "captions",
            "codec": "auto",
            "format": "srt",
            "quality": "best",
        },
        "ytdl_opts": {},
        "subtitle_language": "   ",
        "subtitle_mode": "prefer_manual",
    },
    {
        "name": "captions_language_is_stripped",
        "why": "_normalize_subtitle_language strips surrounding whitespace",
        "args": {
            "download_type": "captions",
            "codec": "auto",
            "format": "vtt",
            "quality": "best",
        },
        "ytdl_opts": {},
        "subtitle_language": "  pt-BR  ",
        "subtitle_mode": "auto_only",
    },
    {
        "name": "audio_wav_has_no_thumbnail_trio",
        "why": "format == 'wav' is excluded from the writethumbnail branch",
        "args": {
            "download_type": "audio",
            "codec": "auto",
            "format": "wav",
            "quality": "best",
        },
        "ytdl_opts": {},
    },
    {
        "name": "caller_opts_are_deep_copied",
        "why": "get_opts deepcopies ytdl_opts; the nested dict must not be shared",
        "args": {
            "download_type": "video",
            "codec": "auto",
            "format": "any",
            "quality": "720",
        },
        "ytdl_opts": {"paths": {"home": "/downloads", "temp": "/tmp"}},
    },
    {
        "name": "defaults_from_none_download_type",
        "why": "None download_type/format default to video/any",
        "args": {
            "download_type": None,
            "codec": None,
            "format": None,
            "quality": "best",
        },
        "ytdl_opts": {},
    },
]


def _call_opts(
    args: dict[str, object],
    ytdl_opts: dict[str, object],
    subtitle_language: str = "en",
    subtitle_mode: str = "prefer_manual",
) -> dict[str, object]:
    return dl_formats.get_opts(
        args["download_type"],
        args["codec"],
        args["format"],
        args["quality"],
        ytdl_opts,
        subtitle_language=subtitle_language,
        subtitle_mode=subtitle_mode,
    )


def _apply_aulos_deltas(sweep: dict[str, object]) -> None:
    """Apply DESIGN §9.8 Δ C47 to a freshly captured sweep, in place.

    Aulos names the container for *every* ``{video, mp4}`` selection, where legacy
    named it only for ``best_remux``; it is a remux (a stream copy), so no
    postprocessor comes with it. Applied here rather than left to a hand edit so a
    re-capture reproduces the checked-in corpus byte for byte instead of silently
    reverting the delta.
    """
    for key, opts in sweep.items():
        parts = key.split("|")
        if parts[0] == "video" and parts[2] == "mp4" and parts[3] != "best_remux":
            assert isinstance(opts, dict)
            opts["merge_output_format"] = "mp4"


def build_opts() -> dict[str, object]:
    # The tuple sweep, with an empty caller option dict. For `captions` the
    # result also depends on (mode, language), so those are part of the key.
    sweep: dict[str, object] = {}
    for t in legal_tuples():
        if t["download_type"] == "captions":
            for mode in CAPTION_MODES:
                for lang in CAPTION_LANGUAGES:
                    key = f"{tuple_key(t)}|{mode}|{lang}"
                    sweep[key] = _call_opts(
                        t, {}, subtitle_language=lang, subtitle_mode=mode
                    )
        else:
            sweep[tuple_key(t)] = _call_opts(t, {})
    _apply_aulos_deltas(sweep)

    branches: list[dict[str, object]] = []
    for case in OPTS_BRANCH_CASES:
        ytdl_opts = dict(case.get("ytdl_opts") or {})
        # A copy of the input is recorded so a Rust test can assert the caller's
        # dict is not mutated (get_opts deep-copies).
        import copy

        input_snapshot = copy.deepcopy(ytdl_opts)
        result = _call_opts(
            case["args"],
            ytdl_opts,
            subtitle_language=str(case.get("subtitle_language", "en")),
            subtitle_mode=str(case.get("subtitle_mode", "prefer_manual")),
        )
        branches.append(
            {
                "name": case["name"],
                "why": case["why"],
                "args": case["args"],
                "ytdl_opts": input_snapshot,
                "subtitle_language": case.get("subtitle_language", "en"),
                "subtitle_mode": case.get("subtitle_mode", "prefer_manual"),
                "caller_opts_unmutated": ytdl_opts == input_snapshot,
                "opts": result,
            }
        )

    return {
        "provenance": _provenance(),
        "description": (
            "get_opts(download_type, codec, format, quality, ytdl_opts, "
            "subtitle_language, subtitle_mode) -> the merged yt-dlp option dict, "
            "for every legal request tuple (empty caller opts) plus one entry per "
            "branch that needs non-default inputs. The video|*|mp4|<quality != "
            "best_remux> rows carry the Aulos delta DESIGN §9.8 Δ C47: "
            'merge_output_format="mp4", which legacy emitted only for best_remux.'
        ),
        "key_format": (
            "download_type|codec|format|quality, with |subtitle_mode|subtitle_language "
            "appended for download_type=captions"
        ),
        "caption_modes": list(dl_formats.CAPTION_MODES),
        "sweep": sweep,
        "branches": branches,
    }


def main() -> None:
    write_json(GOLDEN_DIR / "formats.json", build_formats())
    write_json(GOLDEN_DIR / "opts.json", build_opts())


if __name__ == "__main__":
    main()
