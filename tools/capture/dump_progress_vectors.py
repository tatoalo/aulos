#!/usr/bin/env python3
"""WP-00 — dump the legacy progress-percent normaliser to golden vectors.

Drives the legacy ``app/ytdl.py:_calculate_progress_percent`` over

  * the four cases in the legacy unit suite
    (``app/tests/test_ytdl_utils.py:ProgressPercentTests``),
  * one named vector per row of the DESIGN §4.7 rule table,
  * a generated sweep over exact totals, estimates and fragment bounds, and
  * multi-frame *sequences* that thread ``previous_percent`` the way
    ``Download._update_status`` does, which is what WP-02's ``Normalizer``
    replays.

Output: ``tests/golden/percent.json``.

The legacy function is pure: it takes the raw yt-dlp status dict plus the
previous percent and returns a float or ``None``. It has no notion of a
``source_tag``; the reset-the-floor behaviour DESIGN §4.7 attributes to a source
change is expressed in the legacy code by the *caller* passing
``previous_percent=None``, so that row is captured as a sequence with an
explicit ``reset`` frame (see ``source_change_resets_the_floor``).

Usage:
    /Users/apogliaghi/Development/metube_pot/.venv/bin/python \
        tools/capture/dump_progress_vectors.py
"""

from __future__ import annotations

import datetime as _dt
import sys

from _legacy import (
    GOLDEN_DIR,
    add_legacy_to_syspath,
    legacy_commit,
    write_json,
    ytdlp_version,
)

add_legacy_to_syspath()

# `ytdl` imports yt_dlp and the SC extractor at module load; both are installed
# in the legacy venv and neither touches the network on import.
from ytdl import _calculate_progress_percent  # noqa: E402

# The DESIGN §4.7 rule rows. Every vector below declares which row it covers so
# `verify.py` can assert full coverage instead of just "the file is non-empty".
RULES = (
    "finished_is_100",
    "exact_total_bytes",
    "fragments_known",
    "bogus_estimate_ignored",
    "nothing_usable_keeps_previous",
    "source_change_resets_floor",
    "clamped_to_0_99_9",
    "never_decreases",
)


def _now() -> str:
    return _dt.datetime.now(_dt.UTC).replace(microsecond=0).isoformat()


# ---------------------------------------------------------------------------
# Single-frame vectors
# ---------------------------------------------------------------------------

# (name, rules, status dict, previous_percent)
VECTORS: list[tuple[str, tuple[str, ...], dict, float | None]] = [
    # --- the legacy unit suite, ported 1:1 -------------------------------
    (
        "legacy_test_fragment_progress_caps_false_initial_hls_estimate",
        ("fragments_known", "bogus_estimate_ignored"),
        {
            "status": "downloading",
            "downloaded_bytes": 1024,
            "total_bytes_estimate": 1024,
            "fragment_index": 0,
            "fragment_count": 463,
        },
        None,
    ),
    (
        "legacy_test_fragment_progress_is_monotonic_for_same_source",
        ("fragments_known", "never_decreases"),
        {
            "status": "downloading",
            "downloaded_bytes": 253048,
            "total_bytes_estimate": 234322448,
            "fragment_index": 1,
            "fragment_count": 463,
        },
        0.65,
    ),
    (
        "legacy_test_exact_byte_progress_is_clamped_below_finished",
        ("exact_total_bytes", "clamped_to_0_99_9"),
        {"status": "downloading", "downloaded_bytes": 100, "total_bytes": 100},
        None,
    ),
    (
        "legacy_test_finished_progress_is_complete",
        ("finished_is_100",),
        {"status": "finished"},
        None,
    ),
    # --- finished wins over everything -----------------------------------
    (
        "finished_ignores_a_partial_byte_count",
        ("finished_is_100",),
        {"status": "finished", "downloaded_bytes": 1, "total_bytes": 1000},
        None,
    ),
    (
        "finished_ignores_a_higher_previous",
        ("finished_is_100",),
        {"status": "finished"},
        99.9,
    ),
    (
        "finished_without_any_other_field_from_zero",
        ("finished_is_100",),
        {"status": "finished"},
        0.0,
    ),
    # --- exact total_bytes ------------------------------------------------
    (
        "exact_total_half_way",
        ("exact_total_bytes",),
        {"status": "downloading", "downloaded_bytes": 500, "total_bytes": 1000},
        None,
    ),
    (
        "exact_total_beats_a_disagreeing_estimate",
        ("exact_total_bytes",),
        {
            "status": "downloading",
            "downloaded_bytes": 500,
            "total_bytes": 1000,
            "total_bytes_estimate": 4000,
        },
        None,
    ),
    (
        "exact_total_beats_fragments",
        ("exact_total_bytes",),
        {
            "status": "downloading",
            "downloaded_bytes": 900,
            "total_bytes": 1000,
            "fragment_index": 1,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "exact_total_zero_is_not_usable",
        ("nothing_usable_keeps_previous",),
        {"status": "downloading", "downloaded_bytes": 0, "total_bytes": 0},
        12.5,
    ),
    (
        "exact_total_zero_with_no_previous_is_none",
        ("nothing_usable_keeps_previous",),
        {"status": "downloading", "downloaded_bytes": 0, "total_bytes": 0},
        None,
    ),
    (
        "exact_total_downloaded_zero_is_zero_percent",
        ("exact_total_bytes",),
        {"status": "downloading", "downloaded_bytes": 0, "total_bytes": 1000},
        None,
    ),
    (
        "exact_total_over_100_is_clamped",
        ("exact_total_bytes", "clamped_to_0_99_9"),
        {"status": "downloading", "downloaded_bytes": 2000, "total_bytes": 1000},
        None,
    ),
    (
        "exact_total_negative_downloaded_is_clamped_to_zero",
        ("exact_total_bytes", "clamped_to_0_99_9"),
        {"status": "downloading", "downloaded_bytes": -50, "total_bytes": 1000},
        None,
    ),
    (
        "exact_total_negative_total_is_not_usable",
        ("nothing_usable_keeps_previous",),
        {"status": "downloading", "downloaded_bytes": 10, "total_bytes": -1000},
        None,
    ),
    # --- fragments --------------------------------------------------------
    (
        "fragments_floor_only_without_an_estimate",
        ("fragments_known",),
        {
            "status": "downloading",
            "fragment_index": 25,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_estimate_inside_the_band_is_kept",
        ("fragments_known",),
        {
            "status": "downloading",
            "downloaded_bytes": 255,
            "total_bytes_estimate": 1000,
            "fragment_index": 25,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_estimate_below_the_floor_is_raised",
        ("fragments_known",),
        {
            "status": "downloading",
            "downloaded_bytes": 10,
            "total_bytes_estimate": 1000,
            "fragment_index": 50,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_estimate_above_the_ceiling_is_capped",
        ("fragments_known",),
        {
            "status": "downloading",
            "downloaded_bytes": 990,
            "total_bytes_estimate": 1000,
            "fragment_index": 10,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_last_index_ceiling_is_99_9",
        ("fragments_known", "clamped_to_0_99_9"),
        {
            "status": "downloading",
            "downloaded_bytes": 1000,
            "total_bytes_estimate": 1000,
            "fragment_index": 99,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_index_equal_to_count_is_floor_100_clamped",
        ("fragments_known", "clamped_to_0_99_9"),
        {
            "status": "downloading",
            "fragment_index": 100,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_index_above_count_is_bounded",
        ("fragments_known", "clamped_to_0_99_9"),
        {
            "status": "downloading",
            "fragment_index": 500,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_negative_index_is_bounded_to_zero",
        ("fragments_known",),
        {
            "status": "downloading",
            "fragment_index": -5,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_count_zero_falls_through_to_the_estimate",
        ("bogus_estimate_ignored",),
        {
            "status": "downloading",
            "downloaded_bytes": 500,
            "total_bytes_estimate": 1000,
            "fragment_index": 0,
            "fragment_count": 0,
        },
        None,
    ),
    (
        "fragments_missing_index_falls_through_to_the_estimate",
        ("bogus_estimate_ignored",),
        {
            "status": "downloading",
            "downloaded_bytes": 250,
            "total_bytes_estimate": 1000,
            "fragment_count": 100,
        },
        None,
    ),
    (
        "fragments_single_fragment_floor_is_zero",
        ("fragments_known",),
        {"status": "downloading", "fragment_index": 0, "fragment_count": 1},
        None,
    ),
    (
        "fragments_beat_a_previous_that_is_higher",
        ("fragments_known", "never_decreases"),
        {"status": "downloading", "fragment_index": 1, "fragment_count": 100},
        50.0,
    ),
    # --- the bogus 1 KiB / 1 KiB HLS frame --------------------------------
    (
        "bogus_estimate_equal_to_downloaded_is_ignored",
        ("bogus_estimate_ignored", "nothing_usable_keeps_previous"),
        {
            "status": "downloading",
            "downloaded_bytes": 1024,
            "total_bytes_estimate": 1024,
        },
        None,
    ),
    (
        "bogus_estimate_below_downloaded_is_ignored",
        ("bogus_estimate_ignored", "nothing_usable_keeps_previous"),
        {
            "status": "downloading",
            "downloaded_bytes": 4096,
            "total_bytes_estimate": 1024,
        },
        3.5,
    ),
    (
        "estimate_above_downloaded_is_used",
        ("bogus_estimate_ignored",),
        {
            "status": "downloading",
            "downloaded_bytes": 1024,
            "total_bytes_estimate": 10240,
        },
        None,
    ),
    (
        "estimate_zero_is_not_usable",
        ("nothing_usable_keeps_previous",),
        {
            "status": "downloading",
            "downloaded_bytes": 100,
            "total_bytes_estimate": 0,
        },
        7.25,
    ),
    (
        "estimate_without_downloaded_bytes_is_not_usable",
        ("nothing_usable_keeps_previous",),
        {"status": "downloading", "total_bytes_estimate": 5000},
        None,
    ),
    # --- nothing usable ---------------------------------------------------
    (
        "empty_status_keeps_previous",
        ("nothing_usable_keeps_previous",),
        {},
        42.0,
    ),
    (
        "empty_status_with_no_previous_is_none",
        ("nothing_usable_keeps_previous",),
        {},
        None,
    ),
    (
        "downloading_with_only_speed_and_eta_keeps_previous",
        ("nothing_usable_keeps_previous",),
        {"status": "downloading", "speed": 1048576.0, "eta": 30},
        61.5,
    ),
    (
        "error_status_with_no_bytes_keeps_previous",
        ("nothing_usable_keeps_previous",),
        {"status": "error"},
        88.0,
    ),
    (
        "preparing_status_with_no_bytes_is_none",
        ("nothing_usable_keeps_previous",),
        {"status": "preparing"},
        None,
    ),
    (
        "all_nulls_keep_previous",
        ("nothing_usable_keeps_previous",),
        {
            "status": "downloading",
            "downloaded_bytes": None,
            "total_bytes": None,
            "total_bytes_estimate": None,
            "fragment_index": None,
            "fragment_count": None,
        },
        13.0,
    ),
    # --- _number() coercion ------------------------------------------------
    (
        "numeric_strings_are_coerced",
        ("exact_total_bytes",),
        {"status": "downloading", "downloaded_bytes": "250", "total_bytes": "1000"},
        None,
    ),
    (
        "float_strings_are_coerced",
        ("exact_total_bytes",),
        {"status": "downloading", "downloaded_bytes": "250.5", "total_bytes": "1000.0"},
        None,
    ),
    (
        "garbage_strings_coerce_to_none",
        ("nothing_usable_keeps_previous",),
        {"status": "downloading", "downloaded_bytes": "n/a", "total_bytes": "n/a"},
        5.0,
    ),
    (
        "booleans_coerce_to_numbers",
        ("exact_total_bytes",),
        {"status": "downloading", "downloaded_bytes": True, "total_bytes": True},
        None,
    ),
    (
        "float_fragment_counts_are_accepted",
        ("fragments_known",),
        {"status": "downloading", "fragment_index": 2.0, "fragment_count": 8.0},
        None,
    ),
    # --- monotonicity / previous ------------------------------------------
    (
        "equal_to_previous_is_returned",
        ("never_decreases",),
        {"status": "downloading", "downloaded_bytes": 500, "total_bytes": 1000},
        50.0,
    ),
    (
        "above_previous_is_returned",
        ("never_decreases",),
        {"status": "downloading", "downloaded_bytes": 600, "total_bytes": 1000},
        50.0,
    ),
    (
        "below_previous_returns_previous",
        ("never_decreases",),
        {"status": "downloading", "downloaded_bytes": 400, "total_bytes": 1000},
        50.0,
    ),
    (
        "previous_above_the_clamp_is_returned_verbatim",
        ("never_decreases", "clamped_to_0_99_9"),
        {"status": "downloading", "downloaded_bytes": 999, "total_bytes": 1000},
        100.0,
    ),
    (
        "previous_none_after_a_source_change_recomputes_from_scratch",
        ("source_change_resets_floor",),
        {"status": "downloading", "downloaded_bytes": 1, "total_bytes": 1000},
        None,
    ),
]


def _sweep() -> list[tuple[str, tuple[str, ...], dict, float | None]]:
    """A generated sweep, so the corpus is dense as well as pointed."""
    out: list[tuple[str, tuple[str, ...], dict, float | None]] = []

    # Exact totals across the whole range, with and without a previous.
    total = 10_000
    for pct in (0, 1, 5, 25, 50, 75, 99, 100):
        downloaded = total * pct // 100
        out.append(
            (
                f"sweep_exact_{pct:03d}pct",
                ("exact_total_bytes", "clamped_to_0_99_9"),
                {
                    "status": "downloading",
                    "downloaded_bytes": downloaded,
                    "total_bytes": total,
                },
                None,
            )
        )
        out.append(
            (
                f"sweep_exact_{pct:03d}pct_prev50",
                ("exact_total_bytes", "never_decreases"),
                {
                    "status": "downloading",
                    "downloaded_bytes": downloaded,
                    "total_bytes": total,
                },
                50.0,
            )
        )

    # Estimate-only frames.
    for pct in (0, 10, 50, 90, 100, 150):
        out.append(
            (
                f"sweep_estimate_{pct:03d}pct",
                ("bogus_estimate_ignored", "clamped_to_0_99_9"),
                {
                    "status": "downloading",
                    "downloaded_bytes": total * pct // 100,
                    "total_bytes_estimate": total,
                },
                None,
            )
        )

    # Fragment bounds: every index around the edges of a 463-fragment HLS
    # playlist (the count from the legacy unit test), with and without an
    # estimate that lands outside the band.
    count = 463
    for idx in (0, 1, 2, 231, 461, 462, 463, 464):
        out.append(
            (
                f"sweep_fragments_{idx:03d}_of_{count}",
                ("fragments_known", "clamped_to_0_99_9"),
                {
                    "status": "downloading",
                    "fragment_index": idx,
                    "fragment_count": count,
                },
                None,
            )
        )
        out.append(
            (
                f"sweep_fragments_{idx:03d}_of_{count}_low_estimate",
                ("fragments_known",),
                {
                    "status": "downloading",
                    "downloaded_bytes": 1024,
                    "total_bytes_estimate": 234_322_448,
                    "fragment_index": idx,
                    "fragment_count": count,
                },
                None,
            )
        )
        out.append(
            (
                f"sweep_fragments_{idx:03d}_of_{count}_high_estimate",
                ("fragments_known", "clamped_to_0_99_9"),
                {
                    "status": "downloading",
                    "downloaded_bytes": 234_000_000,
                    "total_bytes_estimate": 234_322_448,
                    "fragment_index": idx,
                    "fragment_count": count,
                },
                None,
            )
        )

    # Tiny fragment counts, where floor/ceiling collide with the 99.9 clamp.
    for count_small in (1, 2, 3):
        for idx in range(count_small + 1):
            out.append(
                (
                    f"sweep_fragments_small_{idx}_of_{count_small}",
                    ("fragments_known", "clamped_to_0_99_9"),
                    {
                        "status": "downloading",
                        "fragment_index": idx,
                        "fragment_count": count_small,
                    },
                    None,
                )
            )
    return out


# ---------------------------------------------------------------------------
# Multi-frame sequences — the shape WP-02's Normalizer replays
# ---------------------------------------------------------------------------

# A `None` frame means "the caller reset previous_percent", i.e. the source_tag
# changed (a video->audio merge leg, or a new tmpfilename).
SEQUENCES: list[dict[str, object]] = [
    {
        "name": "hls_start_from_the_bogus_1kib_frame",
        "rules": ["bogus_estimate_ignored", "fragments_known", "never_decreases"],
        "why": (
            "the real HLS opening: yt-dlp reports 1 KiB / 1 KiB before fragment "
            "counts land; the percent must not jump to 100"
        ),
        "frames": [
            {
                "status": "downloading",
                "downloaded_bytes": 1024,
                "total_bytes_estimate": 1024,
            },
            {
                "status": "downloading",
                "downloaded_bytes": 1024,
                "total_bytes_estimate": 1024,
                "fragment_index": 0,
                "fragment_count": 463,
            },
            {
                "status": "downloading",
                "downloaded_bytes": 253048,
                "total_bytes_estimate": 234322448,
                "fragment_index": 1,
                "fragment_count": 463,
            },
            {
                "status": "downloading",
                "downloaded_bytes": 117161224,
                "total_bytes_estimate": 234322448,
                "fragment_index": 231,
                "fragment_count": 463,
            },
            {
                "status": "downloading",
                "downloaded_bytes": 234322448,
                "total_bytes_estimate": 234322448,
                "fragment_index": 462,
                "fragment_count": 463,
            },
            {"status": "finished"},
        ],
    },
    {
        "name": "progressive_exact_total",
        "rules": ["exact_total_bytes", "clamped_to_0_99_9", "finished_is_100"],
        "why": "the ordinary progressive-download timeline",
        "frames": [
            {"status": "downloading", "downloaded_bytes": 0, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 250, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 750, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 1000, "total_bytes": 1000},
            {"status": "finished"},
        ],
    },
    {
        "name": "a_regressing_frame_never_moves_the_bar_back",
        "rules": ["never_decreases"],
        "why": "yt-dlp can re-report a smaller downloaded_bytes on a retry",
        "frames": [
            {"status": "downloading", "downloaded_bytes": 800, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 100, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 400, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 950, "total_bytes": 1000},
        ],
    },
    {
        "name": "source_change_resets_the_floor",
        "rules": ["source_change_resets_floor", "never_decreases"],
        "why": (
            "DESIGN §4.7: the video leg finishes at 99.9, then the audio leg starts "
            "over. Legacy expresses the reset by the caller dropping "
            "previous_percent (`null` below); without the reset the audio leg would "
            "be pinned at the video leg's floor."
        ),
        "frames": [
            {"status": "downloading", "downloaded_bytes": 500, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 1000, "total_bytes": 1000},
            None,
            {"status": "downloading", "downloaded_bytes": 10, "total_bytes": 500},
            {"status": "downloading", "downloaded_bytes": 250, "total_bytes": 500},
            {"status": "finished"},
        ],
    },
    {
        "name": "without_a_source_change_the_second_leg_is_pinned",
        "rules": ["never_decreases"],
        "why": "the same two legs with no reset — the counter-example to the row above",
        "frames": [
            {"status": "downloading", "downloaded_bytes": 500, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 1000, "total_bytes": 1000},
            {"status": "downloading", "downloaded_bytes": 10, "total_bytes": 500},
            {"status": "downloading", "downloaded_bytes": 250, "total_bytes": 500},
        ],
    },
    {
        "name": "stall_frames_carry_the_previous_value",
        "rules": ["nothing_usable_keeps_previous"],
        "why": "speed/eta-only frames arrive during a stall and must not clear the bar",
        "frames": [
            {"status": "downloading", "downloaded_bytes": 300, "total_bytes": 1000},
            {"status": "downloading", "speed": 0.0, "eta": None},
            {"status": "downloading", "speed": None, "eta": None},
            {"status": "downloading", "downloaded_bytes": 400, "total_bytes": 1000},
        ],
    },
    {
        "name": "no_usable_frame_ever_stays_null",
        "rules": ["nothing_usable_keeps_previous"],
        "why": "an audio-only extraction that reports nothing until it finishes",
        "frames": [
            {"status": "preparing"},
            {"status": "downloading"},
            {"status": "downloading", "speed": 1000.0},
            {"status": "finished"},
        ],
    },
    {
        "name": "nm3u8_segment_frames",
        "rules": ["exact_total_bytes", "clamped_to_0_99_9", "finished_is_100"],
        "why": (
            "StreamingCommunity: _parse_nm3u8_progress puts segment counts into "
            "downloaded_bytes/total_bytes, so the exact-total branch runs on "
            "segment numbers, not bytes"
        ),
        "frames": [
            {"status": "downloading", "downloaded_bytes": 0, "total_bytes": 100},
            {"status": "downloading", "downloaded_bytes": 1, "total_bytes": 100},
            {"status": "downloading", "downloaded_bytes": 57, "total_bytes": 100},
            {"status": "downloading", "downloaded_bytes": 100, "total_bytes": 100},
            {"status": "finished"},
        ],
    },
]


def build() -> dict[str, object]:
    vectors: list[dict[str, object]] = []
    for name, rules, status, previous in [*VECTORS, *_sweep()]:
        vectors.append(
            {
                "name": name,
                "rules": list(rules),
                "status": status,
                "previous_percent": previous,
                "expected": _calculate_progress_percent(status, previous),
            }
        )

    names = [v["name"] for v in vectors]
    if len(names) != len(set(names)):
        dupes = sorted({n for n in names if names.count(n) > 1})
        raise SystemExit(f"duplicate vector names: {dupes}")

    sequences: list[dict[str, object]] = []
    for seq in SEQUENCES:
        previous: float | None = None
        steps: list[dict[str, object]] = []
        for frame in seq["frames"]:
            if frame is None:
                previous = None
                steps.append({"reset": True, "percent": None})
                continue
            percent = _calculate_progress_percent(frame, previous)
            steps.append({"frame": frame, "previous_percent": previous, "percent": percent})
            previous = percent
        sequences.append(
            {
                "name": seq["name"],
                "rules": seq["rules"],
                "why": seq["why"],
                "steps": steps,
                "final_percent": previous,
            }
        )

    return {
        "provenance": {
            "legacy_commit": legacy_commit(),
            "legacy_module": "app/ytdl.py:_calculate_progress_percent",
            "yt_dlp_version": ytdlp_version(),
            "python_version": sys.version.split()[0],
            "captured_at": _now(),
            "tool": "tools/capture/dump_progress_vectors.py",
        },
        "description": (
            "_calculate_progress_percent(status, previous_percent) -> float | None. "
            "`vectors` are single frames; `sequences` thread previous_percent across "
            "frames the way Download._update_status does, and a `reset` step models "
            "the DESIGN §4.7 source_tag change (legacy: the caller passes previous=None)."
        ),
        "rules": list(RULES),
        "vectors": vectors,
        "sequences": sequences,
    }


def main() -> None:
    write_json(GOLDEN_DIR / "percent.json", build())


if __name__ == "__main__":
    main()
