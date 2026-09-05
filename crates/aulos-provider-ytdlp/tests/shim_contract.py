#!/usr/bin/env python3
"""The standalone shim contract test: canned jobs, a stubbed ``yt_dlp``, real fd 3.

Run it with nothing but a CPython interpreter::

    python crates/aulos-provider-ytdlp/tests/shim_contract.py

CI runs exactly this (DESIGN §18.4), and ``shim_cli.rs`` runs it again from ``cargo test`` so the
two cannot drift. It is deliberately dependency-free — no pytest, no yt-dlp, no network — because
its whole job is to prove the shim's side of the protocol on a machine that has neither.

Unlike ``shim_cli.rs``, this exercises the **real fd 3**: every job is written to a child whose
descriptor 3 is a pipe, so the stdout-isolation property of DESIGN §9.1 is under test rather than
bypassed.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SHIM = os.path.join(os.path.dirname(HERE), "python", "ytdlp_runner.py")
PYSTUB = os.path.join(HERE, "fixtures", "pystub")
TRANSCRIPTS = os.path.join(HERE, "fixtures", "transcripts")

failures: list[str] = []


def check(condition, what):
    """Records a failure without stopping, so one run reports every problem."""
    if condition:
        print(f"  ok   {what}")
    else:
        print(f"  FAIL {what}")
        failures.append(what)


def run_job(job, scenario=None, env=None):
    """Runs the shim with ``job`` on stdin and a real pipe on fd 3.

    Returns ``(exit_code, frames, stderr)`` where ``frames`` is the parsed fd-3 transcript and
    stdout is asserted to be empty — nothing but the protocol may cross the boundary, and the
    protocol does not use stdout.
    """
    read_fd, write_fd = os.pipe()
    child_env = dict(os.environ)
    child_env["PYTHONPATH"] = PYSTUB
    child_env["PYTHONDONTWRITEBYTECODE"] = "1"
    if scenario is not None:
        child_env["AULOS_STUB_SCENARIO"] = scenario
    child_env.update(env or {})

    def pass_fd3():
        # `dup2` clears FD_CLOEXEC on the target, so fd 3 survives the exec. `close_fds=False`
        # is required: with it on, CPython closes everything from fd 3 upward **after** running
        # `preexec_fn`, keeping only `pass_fds` — which would close the descriptor we just made.
        os.dup2(write_fd, 3)

    proc = subprocess.Popen(
        [sys.executable, SHIM],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=child_env,
        close_fds=False,
        preexec_fn=pass_fd3,  # noqa: PLW1509 - the fd layout *is* the thing under test
    )
    os.close(write_fd)
    proc.stdin.write((json.dumps(job) + "\n").encode())
    proc.stdin.close()
    # `communicate()` below flushes `self.stdin` before closing it, and flushing an
    # already-closed buffer raises `ValueError: flush of closed file` on CPython <= 3.12 (3.13
    # guards it, which is why this only ever failed on the runner's system python3 and not on a
    # dev machine or in the `python` CI job). The job *has* to be written and the pipe closed
    # here, before the fd-3 read below: that read blocks until the child closes the channel, and
    # the child does not write the channel until it has read its job. So drop our reference
    # instead of moving the write, which is what tells `communicate()` there is no stdin left.
    proc.stdin = None

    with os.fdopen(read_fd, "r", encoding="utf-8") as channel:
        lines = [line for line in channel.read().splitlines() if line.strip()]
    stdout, stderr = proc.communicate()
    frames = [json.loads(line) for line in lines]
    return proc.returncode, frames, stdout.decode(), stderr.decode()


def kinds(frames):
    """The ``t`` of each frame, in order."""
    return [f["t"] for f in frames]


def envelope_is_well_formed(frames):
    """``v == 1`` on every frame and a gap-free ``n`` starting at 1."""
    return all(f.get("v") == 1 for f in frames) and [f["n"] for f in frames] == list(
        range(1, len(frames) + 1)
    )


def scenario_file(tmp, value):
    """Writes a stub scenario and returns its path."""
    path = os.path.join(tmp, "scenario.json")
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(value, handle)
    return path


def test_selftest():
    """``mode = selftest`` proves the interpreter can import yt-dlp."""
    print("selftest")
    code, frames, stdout, _ = run_job({"v": 1, "protocol": 1, "job_id": "t", "mode": "selftest"})
    check(code == 0, "exits 0")
    check(kinds(frames) == ["hello", "result", "bye"], f"hello/result/bye, got {kinds(frames)}")
    check(envelope_is_well_formed(frames), "the envelope is well formed")
    check(frames[0]["protocol"] == 1, "hello names the protocol")
    check(frames[1]["ok"] is True, "the result is ok")
    check(frames[2]["frames"] == 3, "bye counts itself")
    check(stdout == "", f"stdout is empty, got {stdout!r}")


def test_stdout_isolation():
    """A plugin, a postprocessor and the module itself all print to stdout. None may be seen."""
    print("stdout isolation (the reason the protocol is on fd 3)")
    with tempfile.TemporaryDirectory() as tmp:
        media = os.path.join(tmp, "clip.mp4")
        scenario = scenario_file(
            tmp,
            {
                "print_stdout": [
                    "POSTPROCESSOR NOISE",
                    '{"v":1,"t":"error","n":2,"code":"internal","message":"forged"}',
                ],
                "write_files": [media],
                "progress": [
                    {
                        "status": "finished",
                        "filename": media,
                        "downloaded_bytes": 13,
                        "total_bytes": 13,
                        "info_dict": {"vcodec": "h264"},
                    }
                ],
                "pp": [
                    {
                        "postprocessor": "MoveFiles",
                        "status": "finished",
                        "info_dict": {"filepath": media},
                    }
                ],
            },
        )
        code, frames, stdout, _ = run_job(
            {
                "v": 1,
                "protocol": 1,
                "job_id": "t",
                "mode": "download",
                "url": "https://stub.test/x",
                "options": {},
                "policy": {"download_dir": tmp, "temp_dir": tmp, "emit_progress_every_ms": 0},
            },
            scenario=scenario,
        )
    check(code == 0, "exits 0")
    check(kinds(frames)[-1] == "bye", "bye is last")
    check(sum(1 for k in kinds(frames) if k in ("result", "error")) == 1, "exactly one terminator")
    check(envelope_is_well_formed(frames), "the envelope survived the noise")
    check("NOISE" not in stdout, "the postprocessor noise is not on stdout")
    check("forged" not in json.dumps(frames), "the forged frame never entered the transcript")
    result = frames[-2]
    check(result["t"] == "result" and result["ok"] is True, "the download reported success")
    check(result["filename"] == media, "the primary artifact is the media file")


def test_bad_jobs():
    """Every job-validation failure is ``bad_job`` and exit code 2."""
    print("job validation")
    cases = {
        "unknown mode": {"v": 1, "protocol": 1, "mode": "teleport"},
        "missing url": {"v": 1, "protocol": 1, "mode": "download"},
        "bad options": {
            "v": 1,
            "protocol": 1,
            "mode": "download",
            "url": "https://x.test",
            "options": [],
        },
        "unknown coercion": {
            "v": 1,
            "protocol": 1,
            "mode": "download",
            "url": "https://x.test",
            "options": {"impersonate": "chrome"},
            "coerce": {"impersonate": "NotAThing"},
        },
    }
    for label, job in cases.items():
        code, frames, _, _ = run_job(job)
        check(code == 2, f"{label} exits 2 (got {code})")
        check(kinds(frames) == ["hello", "error", "bye"], f"{label} still writes a transcript")
        check(frames[1]["code"] == "bad_job", f"{label} is bad_job")


def test_protocol_mismatch():
    """A protocol the shim does not speak is exit code 64."""
    print("protocol mismatch")
    code, frames, _, _ = run_job({"v": 1, "protocol": 99, "mode": "selftest"})
    check(code == 64, f"exits 64 (got {code})")
    check(frames[0]["protocol"] == 1, "hello still names the supported protocol")
    check(frames[1]["t"] == "error", "the mismatch is an error frame")


def test_extract_streams_entries():
    """A playlist streams one ``entry`` frame per child and honours ``max_entries``."""
    print("extract")
    with tempfile.TemporaryDirectory() as tmp:
        scenario = scenario_file(tmp, {"extract_entries": 40})
        code, frames, _, _ = run_job(
            {
                "v": 1,
                "protocol": 1,
                "mode": "extract",
                "url": "https://stub.test/list",
                "options": {},
                "extract": {"flat": True, "noplaylist": True, "max_entries": 5},
            },
            scenario=scenario,
        )
    check(code == 0, "exits 0")
    check(kinds(frames)[1] == "resolved", "a container emits resolved first")
    check(kinds(frames).count("entry") == 5, "max_entries caps the stream")
    result = next(f for f in frames if f["t"] == "result")
    check(result["count"] == 5 and result["truncated"] is True, "the result reports truncation")
    check(envelope_is_well_formed(frames), "the envelope is well formed")


def test_error_classification():
    """The §9.6 ordered table, through the real classifier."""
    print("error classification")
    cases = [
        ({"class": "UnsupportedError", "message": "Unsupported URL: x"}, "unsupported_url"),
        ({"class": "GeoRestrictedError", "message": "not in your country"}, "geo_restricted"),
        ({"class": "ExtractorError", "message": "Sign in to confirm your age"}, "auth_required"),
        ({"class": "ExtractorError", "message": "Video unavailable"}, "unavailable"),
        ({"class": "ExtractorError", "message": "Premieres in 2 hours"}, "not_yet_live"),
        ({"class": "DownloadError", "message": "Requested format is not available"}, "no_format"),
        (
            {"class": "ExtractorError", "message": "Sign in to confirm you're not a bot"},
            "bot_check",
        ),
        ({"class": "DownloadError", "message": "HTTP Error 503"}, "network"),
        ({"class": "DownloadError", "message": "boom", "wraps": "URLError"}, "network"),
        ({"class": "DownloadError", "message": "HTTP Error 429"}, "throttled"),
        ({"class": "PostProcessingError", "message": "ffmpeg failed"}, "postprocessing_failed"),
        ({"class": "OSError", "message": "no space", "errno": 28}, "disk_full"),
        ({"class": "KeyboardInterrupt", "message": "term"}, "canceled"),
        ({"class": "RuntimeError", "message": "a bug"}, "internal"),
    ]
    for spec, want in cases:
        with tempfile.TemporaryDirectory() as tmp:
            scenario = scenario_file(tmp, {"raise": spec})
            code, frames, _, _ = run_job(
                {
                    "v": 1,
                    "protocol": 1,
                    "mode": "download",
                    "url": "https://stub.test/x",
                    "options": {},
                    "policy": {"download_dir": tmp, "temp_dir": tmp},
                },
                scenario=scenario,
            )
        error = next((f for f in frames if f["t"] == "error"), None)
        check(error is not None and error["code"] == want, f"{spec['class']} -> {want}")
        if want == "canceled":
            check(code == 130, "a cancellation exits 130")
        check(
            error is not None and len(error["message"]) <= 512,
            f"{want} message is capped at 512 characters",
        )


def test_message_cleaning():
    """``ERROR: `` prefixes, ANSI escapes and control characters are stripped once, here."""
    print("message cleaning")
    with tempfile.TemporaryDirectory() as tmp:
        scenario = scenario_file(
            tmp,
            {
                "raise": {
                    "class": "DownloadError",
                    "message": "ERROR: \x1b[0;31mVideo unavailable\x1b[0m\r\n",
                }
            },
        )
        _, frames, _, _ = run_job(
            {
                "v": 1,
                "protocol": 1,
                "mode": "download",
                "url": "https://stub.test/x",
                "options": {},
                "policy": {"download_dir": tmp, "temp_dir": tmp},
            },
            scenario=scenario,
        )
    error = next(f for f in frames if f["t"] == "error")
    check(error["message"] == "Video unavailable", f"got {error['message']!r}")


def test_progress_rate_limit():
    """At most one ``progress`` frame per stream per budget, plus every terminal frame."""
    print("progress rate limit")
    with tempfile.TemporaryDirectory() as tmp:
        progress = [
            {
                "status": "downloading",
                "downloaded_bytes": i,
                "total_bytes": 100,
                "info_dict": {"vcodec": "h264"},
            }
            for i in range(1, 21)
        ]
        progress.append(
            {
                "status": "finished",
                "downloaded_bytes": 100,
                "total_bytes": 100,
                "info_dict": {"vcodec": "h264"},
            }
        )
        scenario = scenario_file(tmp, {"progress": progress})
        _, frames, _, _ = run_job(
            {
                "v": 1,
                "protocol": 1,
                "mode": "download",
                "url": "https://stub.test/x",
                "options": {},
                "policy": {
                    "download_dir": tmp,
                    "temp_dir": tmp,
                    "emit_progress_every_ms": 10000,
                },
            },
            scenario=scenario,
        )
    emitted = [f for f in frames if f["t"] == "progress"]
    check(len(emitted) == 2, f"20 downloading frames collapse to 1, got {len(emitted)}")
    check(emitted[-1]["status"] == "finished", "a non-downloading frame is never dropped")
    check(all(f.get("stream") == "video" for f in emitted), "the stream is derived from vcodec")


def test_outtmpl():
    """``mode = outtmpl`` delegates to yt-dlp's own evaluator."""
    print("outtmpl")
    code, frames, _, _ = run_job(
        {
            "v": 1,
            "protocol": 1,
            "mode": "outtmpl",
            "templates": ["%(playlist_title)s", "%(playlist_index)s"],
            "info": {"playlist_title": "Mix", "playlist_index": 3},
            "prefixes": ["playlist"],
        }
    )
    check(code == 0, "exits 0")
    result = frames[-2]
    check(result["templates"] == ["Mix", "3"], f"got {result.get('templates')}")


def _sidecar_download(tmp, pinned, extra_paths=None):
    """Runs one download whose stub writes an ``.info.json`` and a ``.description``.

    ``pinned`` chooses between the two layouts: the yt-dlp default, where both sidecars are
    written to ``paths.home`` while the media is still in the scratch directory, and the one the
    Rust option builder now produces, where ``paths.infojson``/``paths.description`` name the
    scratch directory. Returns the stub's dump of the option dict and the resulting layout.
    """
    home = os.path.join(tmp, "downloads")
    temp = os.path.join(home, "01JOBULID")
    os.makedirs(temp)
    paths = {"home": home, "temp": temp}
    if pinned:
        paths.update({"description": temp, "infojson": temp})
    paths.update(extra_paths or {})
    dump = os.path.join(tmp, "dump.json")
    scenario = scenario_file(
        tmp,
        {
            "sidecars": {
                "info": {"title": "Stub clip", "ext": "mp4"},
                "write": ["infojson", "description"],
            }
        },
    )
    code, frames, stdout, _ = run_job(
        {
            "v": 1,
            "protocol": 1,
            "job_id": "t",
            "mode": "download",
            "url": "https://stub.test/x",
            "options": {"paths": paths, "writeinfojson": True, "writedescription": True},
            "policy": {"download_dir": home, "temp_dir": temp, "emit_progress_every_ms": 0},
        },
        scenario=scenario,
        env={"AULOS_STUB_DUMP": dump},
    )
    check(code == 0, f"pinned={pinned}: exits 0")
    check(kinds(frames)[-1] == "bye", f"pinned={pinned}: bye is last")
    check(stdout == "", f"pinned={pinned}: stdout is empty")
    with open(dump, "r", encoding="utf-8") as handle:
        return json.load(handle)


def test_sidecars_stay_next_to_the_media():
    """The `.info.json` and `.description` follow the media instead of racing ahead of it.

    The regression is bug 2b: with a per-job scratch directory yt-dlp writes those two straight
    to `paths.home` while the media is still in the scratch directory, so a user `Exec`
    postprocessor — whose default `when` is `post_process`, i.e. before the move — resolves a
    sidecar relative to `%(filepath)q` and finds nothing.
    """
    print("sidecars stay next to the media (bug 2b)")
    with tempfile.TemporaryDirectory() as tmp:
        split = _sidecar_download(tmp, pinned=False)
    # The shape the fix is measured against: at postprocessing time the scratch directory holds
    # the media alone. This is what broke `jellyfin_nfo_generator.py %(filepath)q`.
    check(
        split["layout"]["at_post_process"] == ["Stub clip.mp4"],
        f"unpinned: the media is alone in the scratch dir, got {split['layout']['at_post_process']}",
    )

    with tempfile.TemporaryDirectory() as tmp:
        got = _sidecar_download(tmp, pinned=True)
    check(
        got["postprocessors"] == {"before_dl": 1, "after_move": 1},
        f"the shim registers one before_dl and one after_move pp, got {got['postprocessors']}",
    )
    check(
        got["layout"]["at_post_process"]
        == ["Stub clip.description", "Stub clip.info.json", "Stub clip.mp4"],
        f"every file is together when postprocessors run, got {got['layout']['at_post_process']}",
    )
    check(
        [os.path.basename(p) for p in got["layout"]["registered"]]
        == ["Stub clip.description", "Stub clip.info.json"],
        f"both sidecars are registered for the move, got {got['layout']['registered']}",
    )
    check(
        got["layout"]["home"]
        == ["01JOBULID", "Stub clip.description", "Stub clip.info.json", "Stub clip.mp4"],
        f"and all three end up in the final directory, got {got['layout']['home']}",
    )
    check(got["layout"]["temp"] == [], f"the scratch dir is empty, got {got['layout']['temp']}")
    check(
        os.path.basename(os.path.dirname(got["layout"]["infojson_filename"] or "")) == "downloads",
        f"the reported info.json path is the final one, got {got['layout']['infojson_filename']}",
    )


def test_a_sidecar_the_operator_placed_elsewhere_is_left_alone():
    """`paths.infojson` set by hand is the operator's decision, not a file to sweep up."""
    print("an operator's own paths entry")
    with tempfile.TemporaryDirectory() as tmp:
        elsewhere = os.path.join(tmp, "metadata")
        os.makedirs(elsewhere)
        got = _sidecar_download(tmp, pinned=True, extra_paths={"infojson": elsewhere})
        check(
            os.listdir(elsewhere) == ["Stub clip.info.json"],
            f"the info.json stayed where the operator put it, got {os.listdir(elsewhere)}",
        )
    check(
        [os.path.basename(p) for p in got["layout"]["registered"]] == ["Stub clip.description"],
        f"only the scratch-dir sidecar is registered, got {got['layout']['registered']}",
    )


def test_replay_is_byte_faithful():
    """``--replay`` re-emits a transcript verbatim and imports nothing."""
    print("--replay")
    path = os.path.join(TRANSCRIPTS, "download_ok.jsonl")
    proc = subprocess.run(
        [sys.executable, SHIM, "--replay", path],
        capture_output=True,
        check=False,
        env=dict(os.environ, PYTHONPATH="/nonexistent"),
    )
    with open(path, "r", encoding="utf-8") as handle:
        expected = handle.read()
    check(proc.returncode == 0, "exits 0")
    check(proc.stdout.decode() == expected, "the transcript is reproduced byte for byte")


def main():
    """Runs every check and returns a process exit code."""
    for test in (
        test_selftest,
        test_stdout_isolation,
        test_bad_jobs,
        test_protocol_mismatch,
        test_extract_streams_entries,
        test_error_classification,
        test_message_cleaning,
        test_progress_rate_limit,
        test_outtmpl,
        test_sidecars_stay_next_to_the_media,
        test_a_sidecar_the_operator_placed_elsewhere_is_left_alone,
        test_replay_is_byte_faithful,
    ):
        test()
    if failures:
        print(f"\n{len(failures)} check(s) failed:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("\nall shim contract checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
