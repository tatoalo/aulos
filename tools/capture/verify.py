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
V1 = REPO_ROOT / "tests" / "v1_golden"

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


# ---------------------------------------------------------------------------
# tests/v1_golden/
# ---------------------------------------------------------------------------

# Every route the PLAN WP-00 checklist names, with the minimum number of
# directories it must have. A route that drops to zero fails the build.
REQUIRED_ROUTES: dict[str, int] = {
    "POST add": 40,
    "GET history": 3,
    "POST delete": 8,
    "POST start": 4,
    "GET version": 1,
    "GET presets": 1,
    "GET robots.txt": 1,
    "POST cancel-add": 1,
    "GET cookie-status": 2,
    "POST upload-cookies": 4,
    "POST delete-cookies": 2,
    "POST subscribe": 8,
    "GET subscriptions": 1,
    "POST subscriptions/update": 6,
    "POST subscriptions/delete": 5,
    "POST subscriptions/check": 2,
}
REQUIRED_OPTIONS_ROUTES = 9

# Reason phrases / body messages named in DESIGN §11.7 and the PLAN checklist
# that must appear somewhere in the corpus. These are the strings WP-15 asserts
# byte-for-byte, so a capture that lost one is not a usable input.
REQUIRED_REASONS = (
    "Invalid JSON request body",
    "JSON request body must be an object",
    "missing 'url', 'download_type', or 'quality'",
    "download_type must be one of ['audio', 'captions', 'thumbnail', 'video']",
    "playlist_item_limit must be an integer",
    "ytdl_options_overrides must be valid JSON",
    "ytdl_options_overrides must be a JSON object",
    "ytdl_options_overrides are disabled",
    "check_interval_minutes must be an integer",
    "check_interval_minutes must be at least 1",
    "missing subscription id",
    "no valid fields to update",
    "missing ids list",
    "ids must be a list",
)
REQUIRED_BODY_MESSAGES = (
    "Subscription not found",
    "This URL is already subscribed",
    "No uploaded cookies to delete",
    "No cookies file provided",
    "Cookie file too large (max 1MB)",
    "Cookies uploaded (",
)

MANIFEST_FIELDS = (
    "corpus",
    "purpose",
    "legacy_repo",
    "legacy_commit",
    "legacy_image_digest",
    "yt_dlp_version",
    "python_version",
    "url_prefix",
    "base_url",
    "server_env",
    "state_dir_fixture",
    "scope",
    "secret_hygiene",
    "meta_fields",
    "phases",
    "skipped",
    "cases",
    "case_count",
)

SECRET_HEADERS = ("cookie", "set-cookie", "authorization", "proxy-authorization")


def verify_v1() -> None:
    if not check(V1.is_dir(), "tests/v1_golden/ does not exist"):
        return

    case_dirs = sorted(p for p in V1.iterdir() if p.is_dir())
    check(bool(case_dirs), "tests/v1_golden/ has no case directories")

    manifest = load(V1 / "MANIFEST.json")
    if isinstance(manifest, dict):
        for field in MANIFEST_FIELDS:
            check(field in manifest, f"MANIFEST.json: missing field {field!r}")
        for field in ("legacy_commit", "yt_dlp_version", "python_version", "url_prefix"):
            value = manifest.get(field)
            check(
                isinstance(value, str) and value and value != "unknown",
                f"MANIFEST.json: {field} is missing or unknown",
            )
        env = manifest.get("server_env")
        if check(isinstance(env, dict) and env, "MANIFEST.json: server_env is empty"):
            assert isinstance(env, dict)
            check(
                env.get("MAX_CONCURRENT_DOWNLOADS") == "0",
                "MANIFEST.json: the capture must run with MAX_CONCURRENT_DOWNLOADS=0 so "
                "the queue auto-restart cannot reach the network",
            )
        skipped = manifest.get("skipped")
        check(
            isinstance(skipped, list) and skipped,
            "MANIFEST.json: `skipped` must justify every case the scope trim drops",
        )
        if isinstance(skipped, list):
            for s in skipped:
                check(
                    isinstance(s, dict) and s.get("reason"),
                    "MANIFEST.json: a skipped entry has no reason",
                )
        cases = manifest.get("cases")
        if isinstance(cases, dict):
            listed = set(cases)
            on_disk = {p.name for p in case_dirs}
            check(
                listed == on_disk,
                "MANIFEST.json: the case index and the directories on disk disagree "
                f"(index-only {sorted(listed - on_disk)[:5]}, "
                f"disk-only {sorted(on_disk - listed)[:5]})",
            )
            check(
                manifest.get("case_count") == len(listed),
                "MANIFEST.json: case_count does not match the index",
            )

    route_counts: dict[str, int] = {}
    options_routes: set[str] = set()
    reasons: set[str] = set()
    body_text: list[str] = []
    metas: dict[str, dict] = {}

    for d in case_dirs:
        rel = d.relative_to(REPO_ROOT)
        req = load(d / "request.json")
        res = load(d / "response.json")
        meta = load(d / "meta.json")
        if not isinstance(meta, dict):
            continue
        metas[d.name] = meta

        check(req is not None, f"{rel}: request.json missing or unparseable")
        check(res is not None, f"{rel}: response.json missing or unparseable")

        for field in (
            "name",
            "route",
            "why",
            "method",
            "path",
            "status",
            "reason",
            "content_type",
            "request_headers",
            "response_headers",
            "response_bytes",
            "response_sha256",
        ):
            check(field in meta, f"{rel}: meta.json missing field {field!r}")
        check(meta.get("name") == d.name, f"{rel}: meta.json name does not match the directory")
        check(
            isinstance(meta.get("why"), str) and meta["why"],
            f"{rel}: meta.json has no `why` — a case nobody can explain is not a contract",
        )
        check(
            isinstance(meta.get("status"), int) and 100 <= meta["status"] < 600,
            f"{rel}: meta.json status is not an HTTP status",
        )
        if isinstance(req, dict):
            check(
                req.get("route") == meta.get("route") and req.get("method") == meta.get("method"),
                f"{rel}: request.json and meta.json disagree on route/method",
            )

        route = str(meta.get("route", ""))
        route_counts[route] = route_counts.get(route, 0) + 1
        if route.startswith("OPTIONS "):
            options_routes.add(route)
        if isinstance(meta.get("reason"), str):
            reasons.add(meta["reason"])
        body_text.append(json.dumps(res, ensure_ascii=False))

        # Secret hygiene: a header from SECRET_HEADERS may appear as a key, but
        # never with a real value.
        for direction in ("request_headers", "response_headers"):
            headers = meta.get(direction) or {}
            if not isinstance(headers, dict):
                continue
            for name, value in headers.items():
                if name.lower() in SECRET_HEADERS:
                    check(
                        value == "<scrubbed>",
                        f"{rel}: {direction}[{name}] is not scrubbed",
                    )

    for route, minimum in REQUIRED_ROUTES.items():
        have = route_counts.get(route, 0)
        check(
            have >= minimum,
            f"v1_golden: route {route!r} has {have} case(s), the checklist needs "
            f"at least {minimum}",
        )
    check(
        len(options_routes) >= REQUIRED_OPTIONS_ROUTES,
        f"v1_golden: only {len(options_routes)} OPTIONS route(s) captured, "
        f"expected {REQUIRED_OPTIONS_ROUTES}",
    )

    blob = "\n".join(body_text)
    for needle in REQUIRED_REASONS:
        check(
            needle in reasons,
            f"v1_golden: no case carries the legacy reason string {needle!r} "
            "(DESIGN §11.7)",
        )
    for needle in REQUIRED_BODY_MESSAGES:
        check(needle in blob, f"v1_golden: no response body contains {needle!r}")

    verify_v1_semantics(metas)


def verify_v1_semantics(metas: dict[str, dict]) -> None:
    """The structural claims the PLAN checklist makes about specific cases."""

    def body(case: str) -> object | None:
        return load(V1 / case / "response.json") if (V1 / case).is_dir() else None

    def status(case: str) -> int | None:
        m = metas.get(case)
        return m.get("status") if isinstance(m, dict) else None

    # history: empty, one of each status, >= 600 done rows.
    empty = body("history_empty")
    if check(isinstance(empty, dict), "v1_golden: history_empty is missing"):
        assert isinstance(empty, dict)
        for key in ("queue", "pending", "done"):
            check(
                empty.get(key) == [],
                f"v1_golden: history_empty[{key!r}] should be an empty list",
            )

    seeded = body("history_seeded")
    if check(isinstance(seeded, dict), "v1_golden: history_seeded is missing"):
        assert isinstance(seeded, dict)
        for key in ("queue", "pending", "done"):
            check(key in seeded, f"v1_golden: history_seeded has no {key!r} key")
        done = seeded.get("done") or []
        check(
            len(done) >= 600,
            f"v1_golden: history_seeded.done has {len(done)} rows, the DESIGN §11.4 "
            "window-vs-whole-set case needs at least 600",
        )
        statuses = {
            str(item.get("status"))
            for arr in ("queue", "pending", "done")
            for item in (seeded.get(arr) or [])
            if isinstance(item, dict)
        }
        for want in ("pending", "preparing", "downloading", "finished", "error"):
            check(
                want in statuses,
                f"v1_golden: history_seeded has no item with status {want!r}",
            )
        # The pre-download-problem row of DESIGN §11.4 / §8.4.
        check(
            any(
                isinstance(i, dict) and i.get("status") == "pending" and i.get("error")
                for i in (seeded.get("pending") or [])
            ),
            "v1_golden: history_seeded has no pending row with a populated error "
            "(the upcoming-livestream case of DESIGN §8.4)",
        )

    # The 1 000 000-byte decimal cookie cap (DESIGN §16.6) — both sides.
    check(
        status("upload_cookies_at_the_cap") == 200,
        "v1_golden: exactly 1 000 000 bytes must be accepted",
    )
    check(
        status("upload_cookies_over_the_cap") == 400,
        "v1_golden: 1 000 001 bytes must be rejected",
    )

    # The two legacy 500s DESIGN §11.1 turns into 4xx: they must be on record,
    # or nobody can prove the shim changed anything.
    check(
        status("start_ids_null") == 500,
        "v1_golden: `POST start` with ids:null must be captured as the legacy 500",
    )
    check(
        status("subscriptions_update_enabled_not_a_boolean") == 500,
        "v1_golden: the `enabled must be a boolean` leak must be captured as a 500",
    )

    # Every `POST add` case must be a 400: a 200 would mean a body slipped past
    # parse_download_options and reached yt-dlp, i.e. the capture hit the network.
    for name, meta in metas.items():
        if meta.get("route") == "POST add":
            check(
                meta.get("status") == 400,
                f"v1_golden: {name} is a `POST add` with status {meta.get('status')} — "
                "the scope trim admits validation 400s only",
            )

    # All six _migrate_legacy_request rows.
    for row in range(1, 7):
        check(
            any(f"_migrate_row{row}_" in n for n in metas),
            f"v1_golden: no case exercises _migrate_legacy_request row {row}",
        )

    # The five §11.2.1 leniencies.
    for name, cases in {
        "singular ytdl_options_preset": ("add_presets_singular_alias",),
        "bare-string ytdl_options_presets": ("add_presets_bare_string",),
        "ytdl_options_overrides as a JSON string": (
            "add_overrides_invalid_json_string",
            "add_overrides_json_string_not_an_object",
        ),
        "numeric-string playlist_item_limit": (
            "subscribe_playlist_item_limit_numeric_string_with_spaces",
        ),
        "numeric-string check_interval_minutes": (
            "subscribe_check_interval_numeric_string_zero",
        ),
    }.items():
        for case in cases:
            check(
                case in metas,
                f"v1_golden: DESIGN §11.2.1 leniency ({name}) has no case {case!r}",
            )


def main() -> int:
    verify_formats()
    verify_opts()
    verify_percent()
    verify_v1()

    if FAILURES:
        print(f"FAIL — {len(FAILURES)} problem(s):", file=sys.stderr)
        for f in FAILURES:
            print(f"  - {f}", file=sys.stderr)
        return 1

    n_cases = len([p for p in V1.iterdir() if p.is_dir()]) if V1.is_dir() else 0
    print(f"OK — tests/golden/{{formats,opts,percent}}.json and {n_cases} v1 case(s) verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
