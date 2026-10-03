#!/usr/bin/env python3
"""WP-00 — verify the golden corpora: shape, coverage and secret hygiene.

Runs offline against the checked-in files only, so it is safe in CI. It is the
acceptance test for WP-00 and the tripwire for every package that consumes the
corpora (WP-02, WP-06, WP-15): a half-finished re-capture fails here loudly
instead of silently shrinking someone else's test surface.

    python tools/capture/verify.py          # exit 0 == the corpora are usable

No third-party imports, no network, no legacy checkout needed.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
GOLDEN = REPO_ROOT / "tests" / "golden"

FAILURES: list[str] = []


def fail(msg: str) -> None:
    FAILURES.append(msg)


def check(cond: bool, msg: str) -> bool:
    if not cond:
        fail(msg)
    return cond


def load(path: Path) -> object | None:
    if not path.is_file():
        fail(f"missing file: {path.relative_to(REPO_ROOT)}")
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        fail(f"{path.relative_to(REPO_ROOT)} does not parse: {exc}")
        return None


# ---------------------------------------------------------------------------
# The legal request space (DESIGN §6.6 / legacy spec §2.2), duplicated here on
# purpose: verify.py must not import the legacy checkout, and this is exactly
# the cross-check WP-06 runs against its own catalog.
# ---------------------------------------------------------------------------

VIDEO_CODECS = ("auto", "h264", "h265", "av1", "vp9")
VIDEO_FORMATS = ("any", "mp4", "ios")
VIDEO_QUALITIES = ("best", "worst", "2160", "1440", "1080", "720", "480", "360", "240")
AUDIO_QUALITIES = {
    "m4a": ("best", "192", "128"),
    "mp3": ("best", "320", "192", "128"),
    "opus": ("best",),
    "wav": ("best",),
    "flac": ("best",),
}
CAPTION_FORMATS = ("srt", "txt", "vtt", "ttml", "sbv", "scc", "dfxp")
CAPTION_MODES = ("auto_only", "manual_only", "prefer_manual", "prefer_auto")
CAPTION_LANGUAGES = ("en", "pt-BR", "it")


def expected_tuple_keys() -> set[str]:
    keys: set[str] = set()
    for fmt in VIDEO_FORMATS:
        qualities = [*VIDEO_QUALITIES]
        if fmt == "mp4":
            qualities.append("best_remux")
        for codec in VIDEO_CODECS:
            for q in qualities:
                keys.add(f"video|{codec}|{fmt}|{q}")
    for fmt, qs in AUDIO_QUALITIES.items():
        for q in qs:
            keys.add(f"audio|auto|{fmt}|{q}")
    for fmt in CAPTION_FORMATS:
        keys.add(f"captions|auto|{fmt}|best")
    keys.add("thumbnail|auto|jpg|best")
    return keys


def expected_opts_keys() -> set[str]:
    keys: set[str] = set()
    for key in expected_tuple_keys():
        if key.startswith("captions|"):
            for mode in CAPTION_MODES:
                for lang in CAPTION_LANGUAGES:
                    keys.add(f"{key}|{mode}|{lang}")
        else:
            keys.add(key)
    return keys


PROVENANCE_FIELDS = (
    "legacy_commit",
    "legacy_module",
    "yt_dlp_version",
    "python_version",
    "captured_at",
    "tool",
)


def check_provenance(name: str, doc: dict) -> None:
    prov = doc.get("provenance")
    if not check(isinstance(prov, dict), f"{name}: no provenance object"):
        return
    for field in PROVENANCE_FIELDS:
        value = prov.get(field)
        check(
            isinstance(value, str) and value and value != "unknown",
            f"{name}: provenance.{field} is missing or unknown",
        )


# ---------------------------------------------------------------------------
# tests/golden/formats.json
# ---------------------------------------------------------------------------


def verify_formats() -> None:
    doc = load(GOLDEN / "formats.json")
    if not isinstance(doc, dict):
        return
    check_provenance("formats.json", doc)

    selectors = doc.get("selectors")
    if not check(isinstance(selectors, dict) and selectors, "formats.json: no selectors"):
        return
    assert isinstance(selectors, dict)

    want = expected_tuple_keys()
    have = set(selectors)
    missing = sorted(want - have)
    extra = sorted(have - want)
    check(
        not missing,
        f"formats.json: {len(missing)} tuple(s) from the DESIGN §6.6 catalog are "
        f"missing, e.g. {missing[:5]}",
    )
    check(not extra, f"formats.json: {len(extra)} tuple(s) outside the legal space: {extra[:5]}")

    for key, sel in selectors.items():
        check(
            isinstance(sel, str) and sel != "",
            f"formats.json: selector for {key} is not a non-empty string",
        )

    # The catalog's own invariants, so a bad re-capture is caught here rather
    # than in WP-06's diff.
    for fmt in ("mp4", "ios"):
        for codec in VIDEO_CODECS:
            for q in ("1080", "480"):
                sel = selectors.get(f"video|{codec}|{fmt}|{q}", "")
                check(
                    f"[height<={q}]" in sel,
                    f"formats.json: video|{codec}|{fmt}|{q} has no height filter",
                )
    for codec in VIDEO_CODECS:
        for q in ("best", "worst"):
            sel = selectors.get(f"video|{codec}|any|{q}", "")
            check(
                "height<=" not in sel,
                f"formats.json: video|{codec}|any|{q} must carry no height filter",
            )
    check(
        selectors.get("video|auto|any|best") == selectors.get("video|auto|any|worst"),
        "formats.json: the documented `worst` quirk (DESIGN §6.6) is not reproduced",
    )
    check(
        selectors.get("video|auto|mp4|best_remux") == "bestvideo+bestaudio/best",
        "formats.json: mp4/best_remux selector changed",
    )
    for fmt in CAPTION_FORMATS:
        check(
            selectors.get(f"captions|auto|{fmt}|best") == "bestaudio/best",
            f"formats.json: captions|auto|{fmt}|best is not bestaudio/best",
        )
    check(
        selectors.get("thumbnail|auto|jpg|best") == "bestaudio/best",
        "formats.json: thumbnail selector is not bestaudio/best",
    )
    for fmt in AUDIO_QUALITIES:
        sel = selectors.get(f"audio|auto|{fmt}|best", "")
        check(
            sel == f"bestaudio[ext={fmt}]/bestaudio/best",
            f"formats.json: audio|auto|{fmt}|best selector changed ({sel!r})",
        )

    edge = doc.get("edge_cases")
    check(
        isinstance(edge, list) and len(edge) >= 10,
        "formats.json: edge_cases should cover the custom:/defaults/normalisation paths",
    )
    if isinstance(edge, list):
        names = {c.get("name") for c in edge if isinstance(c, dict)}
        for required in (
            "custom_selector_passthrough",
            "defaults_from_none",
            "whitespace_and_case_are_normalised",
            "unknown_codec_falls_back_to_no_filter",
        ):
            check(required in names, f"formats.json: edge case {required!r} missing")
        for c in edge:
            if isinstance(c, dict):
                check(
                    isinstance(c.get("why"), str) and c["why"],
                    f"formats.json: edge case {c.get('name')!r} has no `why`",
                )

    errors = doc.get("errors")
    check(
        isinstance(errors, list) and len(errors) >= 3,
        "formats.json: the ValueError paths of get_format are not recorded",
    )


# ---------------------------------------------------------------------------
# tests/golden/opts.json
# ---------------------------------------------------------------------------


def verify_opts() -> None:
    doc = load(GOLDEN / "opts.json")
    if not isinstance(doc, dict):
        return
    check_provenance("opts.json", doc)

    sweep = doc.get("sweep")
    if not check(isinstance(sweep, dict) and sweep, "opts.json: no sweep"):
        return
    assert isinstance(sweep, dict)

    want = expected_opts_keys()
    have = set(sweep)
    missing = sorted(want - have)
    extra = sorted(have - want)
    check(not missing, f"opts.json: {len(missing)} key(s) missing, e.g. {missing[:5]}")
    check(not extra, f"opts.json: {len(extra)} unexpected key(s): {extra[:5]}")

    for key, opts in sweep.items():
        if not check(isinstance(opts, dict), f"opts.json: {key} is not an object"):
            continue
        check("postprocessors" in opts, f"opts.json: {key} has no postprocessors key")

    # Branch invariants (legacy spec §6.2).
    for fmt in ("m4a", "mp3", "opus", "flac"):
        opts = sweep.get(f"audio|auto|{fmt}|best", {})
        if isinstance(opts, dict):
            check(
                opts.get("writethumbnail") is True,
                f"opts.json: audio/{fmt} should set writethumbnail",
            )
    wav = sweep.get("audio|auto|wav|best", {})
    if isinstance(wav, dict):
        check(
            "writethumbnail" not in wav,
            "opts.json: audio/wav must NOT set writethumbnail",
        )
    remux = sweep.get("video|auto|mp4|best_remux", {})
    if isinstance(remux, dict):
        check(
            remux.get("merge_output_format") == "mp4",
            "opts.json: mp4/best_remux should set merge_output_format=mp4",
        )
        keys = [p.get("key") for p in remux.get("postprocessors", []) if isinstance(p, dict)]
        check(
            keys and keys[-1] == "Exec",
            "opts.json: the audio_sync_fix Exec postprocessor must be last for best_remux",
        )
    thumb = sweep.get("thumbnail|auto|jpg|best", {})
    if isinstance(thumb, dict):
        check(
            thumb.get("skip_download") is True and thumb.get("writethumbnail") is True,
            "opts.json: thumbnail should set skip_download and writethumbnail",
        )
    txt = sweep.get("captions|auto|txt|best|prefer_manual|en", {})
    if isinstance(txt, dict):
        check(
            txt.get("subtitlesformat") == "srt",
            "opts.json: captions/txt must request srt (the txt conversion is post-hoc)",
        )
    for mode, (subs, auto) in {
        "manual_only": (True, False),
        "auto_only": (False, True),
        "prefer_auto": (True, True),
        "prefer_manual": (True, True),
    }.items():
        opts = sweep.get(f"captions|auto|srt|best|{mode}|en", {})
        if isinstance(opts, dict):
            check(
                opts.get("writesubtitles") is subs and opts.get("writeautomaticsub") is auto,
                f"opts.json: captions mode {mode} has the wrong writesubtitles/auto pair",
            )

    branches = doc.get("branches")
    check(
        isinstance(branches, list) and len(branches) >= 10,
        "opts.json: the branch cases that need non-default inputs are missing",
    )
    if isinstance(branches, list):
        for b in branches:
            if not isinstance(b, dict):
                continue
            check(
                isinstance(b.get("why"), str) and b["why"],
                f"opts.json: branch {b.get('name')!r} has no `why`",
            )
            check(
                b.get("caller_opts_unmutated") is True,
                f"opts.json: branch {b.get('name')!r} mutated the caller's option dict",
            )


# ---------------------------------------------------------------------------
# tests/golden/percent.json
# ---------------------------------------------------------------------------

# One row per DESIGN §4.7 rule table row.
PERCENT_RULES = (
    "finished_is_100",
    "exact_total_bytes",
    "fragments_known",
    "bogus_estimate_ignored",
    "nothing_usable_keeps_previous",
    "source_change_resets_floor",
    "clamped_to_0_99_9",
    "never_decreases",
)


def verify_percent() -> None:
    doc = load(GOLDEN / "percent.json")
    if not isinstance(doc, dict):
        return
    check_provenance("percent.json", doc)

    vectors = doc.get("vectors")
    if not check(isinstance(vectors, list) and vectors, "percent.json: no vectors"):
        return
    assert isinstance(vectors, list)

    covered: dict[str, int] = {r: 0 for r in PERCENT_RULES}
    names: list[str] = []
    for v in vectors:
        if not isinstance(v, dict):
            fail("percent.json: a vector is not an object")
            continue
        names.append(str(v.get("name")))
        check("status" in v, f"percent.json: vector {v.get('name')!r} has no status frame")
        check(
            "previous_percent" in v and "expected" in v,
            f"percent.json: vector {v.get('name')!r} is missing previous_percent/expected",
        )
        expected = v.get("expected")
        check(
            expected is None or isinstance(expected, (int, float)),
            f"percent.json: vector {v.get('name')!r} has a non-numeric expected value",
        )
        if isinstance(expected, (int, float)) and not isinstance(expected, bool):
            check(
                0.0 <= float(expected) <= 100.0,
                f"percent.json: vector {v.get('name')!r} expects {expected}, out of range",
            )
        for rule in v.get("rules") or []:
            if rule not in covered:
                fail(f"percent.json: vector {v.get('name')!r} names unknown rule {rule!r}")
            else:
                covered[rule] += 1

    dupes = sorted({n for n in names if names.count(n) > 1})
    check(not dupes, f"percent.json: duplicate vector names {dupes}")

    sequences = doc.get("sequences")
    if check(isinstance(sequences, list) and sequences, "percent.json: no sequences"):
        assert isinstance(sequences, list)
        for s in sequences:
            if not isinstance(s, dict):
                continue
            check(
                isinstance(s.get("steps"), list) and s["steps"],
                f"percent.json: sequence {s.get('name')!r} has no steps",
            )
            check(
                isinstance(s.get("why"), str) and s["why"],
                f"percent.json: sequence {s.get('name')!r} has no `why`",
            )
            for rule in s.get("rules") or []:
                if rule not in covered:
                    fail(f"percent.json: sequence {s.get('name')!r} names unknown rule {rule!r}")
                else:
                    covered[rule] += 1

    for rule, n in covered.items():
        check(n >= 1, f"percent.json: no vector covers the DESIGN §4.7 row {rule!r}")

    # The four legacy unit tests must be present verbatim; they are the only
    # assertions that already existed before this corpus.
    for required in (
        "legacy_test_fragment_progress_caps_false_initial_hls_estimate",
        "legacy_test_fragment_progress_is_monotonic_for_same_source",
        "legacy_test_exact_byte_progress_is_clamped_below_finished",
        "legacy_test_finished_progress_is_complete",
    ):
        check(required in names, f"percent.json: ported legacy test {required!r} missing")


def main() -> int:
    verify_formats()
    verify_opts()
    verify_percent()

    if FAILURES:
        print(f"FAIL — {len(FAILURES)} problem(s):", file=sys.stderr)
        for f in FAILURES:
            print(f"  - {f}", file=sys.stderr)
        return 1

    print("OK — tests/golden/{formats,opts,percent}.json verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
