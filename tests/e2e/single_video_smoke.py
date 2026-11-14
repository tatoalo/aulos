import json
import sqlite3
import subprocess
import tempfile
import time
import xml.etree.ElementTree as ET
from pathlib import Path

from redirect_smoke import Source, api, aulos, http_server


def wait_item(base, item_id, status):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        result = api(base, "/api/v2/items/" + item_id)
        item = result.get("item", result)
        if item["status"] == status:
            return item
        assert item["status"] not in ("error", "canceled"), item
        time.sleep(0.1)
    raise AssertionError(f"waiting for {status}: {item}")


def records(path):
    return [json.loads(line) for line in path.read_text().splitlines()]


def run_single(source, mode):
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        log = root / "extract.jsonl"
        database = root / "state/aulos.db"
        downloads = root / "downloads"
        env = {
            "PYTHONPATH": str(Path(__file__).parent / "fixtures"),
            "TEST_SINGLE_VIDEO_LOG": str(log),
            "TEST_SINGLE_VIDEO_MEDIA": source + "/media/0/clip.mp4",
            "AULOS_DB_PATH": str(database),
            "STATE_DIR": str(root / "state"),
            "DOWNLOAD_DIR": str(downloads),
            "TEMP_DIR": str(root / "temp"),
            "OUTPUT_TEMPLATE": "%(id)s.%(ext)s",
            "YTDL_OPTIONS": json.dumps({"noplaylist": False, "writeinfojson": True}),
        }
        original = f"https://single.test/watch/{mode}?list=other&index=2"
        target = f"https://single.test/video/{mode}"
        with aulos(3, extra_env=env) as (base, _):
            item_id = api(
                base, "/api/v2/downloads", {"url": original, "auto_start": mode == "auto"}
            )["id"]
            item = wait_item(base, item_id, "finished" if mode == "auto" else "queued")
            assert item["url"] == original, item
            assert item["title"] == mode, item
            if mode == "manual":
                api(base, "/api/v2/items/actions", {"action": "start", "ids": [item_id]})
                wait_item(base, item_id, "finished")
        if mode in ("restart", "legacy", "truncated"):
            if mode in ("legacy", "truncated"):
                with sqlite3.connect(database) as db:
                    db.execute(
                        "UPDATE items SET entry_json = ? WHERE id = ?",
                        (None if mode == "legacy" else '{"__truncated":true}', item_id),
                    )
            with aulos(3, extra_env=env) as (base, _):
                wait_item(base, item_id, "queued")
                api(base, "/api/v2/items/actions", {"action": "start", "ids": [item_id]})
                wait_item(base, item_id, "finished")
        extraction = records(log)
        download_calls = [record for record in extraction if not record["flat"]]
        assert download_calls, extraction
        expected_url = original if mode in ("legacy", "truncated") else target
        assert download_calls[0]["url"] == expected_url, download_calls
        assert all(record["noplaylist"] is True for record in download_calls), download_calls
        assert sorted(path.name for path in downloads.iterdir() if path.is_file()) == [
            f"{mode}.mp4", f"{mode}.nfo"
        ], list(downloads.iterdir())
        metadata = ET.parse(downloads / f"{mode}.nfo").getroot()
        assert metadata.findtext("uniqueid") == mode, ET.tostring(metadata)
        subprocess.run(
            ["ffprobe", "-v", "error", str(downloads / f"{mode}.mp4")],
            check=True,
            timeout=15,
        )
        print(f"single-video download passed: {mode}", flush=True)


def run_playlist(source):
    with tempfile.TemporaryDirectory() as temp:
        log = Path(temp) / "extract.jsonl"
        env = {
            "PYTHONPATH": str(Path(__file__).parent / "fixtures"),
            "TEST_SINGLE_VIDEO_LOG": str(log),
            "TEST_SINGLE_VIDEO_MEDIA": source + "/media/0/clip.mp4",
            "OUTPUT_TEMPLATE_PLAYLIST": "%(id)s.%(ext)s",
        }
        with aulos(3, extra_env=env) as (base, downloads):
            item_id = api(
                base, "/api/v2/downloads",
                {"url": "https://single.test/playlist/intentional", "auto_start": True},
            )["id"]
            item = wait_item(base, item_id, "finished")
            assert item["kind"] == "group", item
            assert sorted(path.name for path in downloads.rglob("*.mp4")) == [
                "intentional-one.mp4", "intentional-two.mp4"
            ], list(downloads.rglob("*"))
            download_calls = [record for record in records(log) if not record["flat"]]
            assert {record["url"] for record in download_calls} == {
                "https://single.test/video/intentional-one",
                "https://single.test/video/intentional-two",
            }, download_calls
            assert all(record["noplaylist"] is True for record in download_calls), download_calls
        print("intentional playlist downloads both children", flush=True)


def run():
    with tempfile.TemporaryDirectory() as temp:
        clip = Path(temp) / "clip.mp4"
        subprocess.run(
            [
                "ffmpeg", "-v", "error", "-f", "lavfi", "-i",
                "color=c=black:s=64x64:r=10", "-t", "1", "-c:v", "libx264",
                "-pix_fmt", "yuv420p", str(clip),
            ],
            check=True,
            timeout=30,
        )
        Source.media = clip.read_bytes()
    with http_server(Source) as port:
        source = f"http://127.0.0.1:{port}"
        for mode in ("auto", "manual", "restart", "legacy", "truncated"):
            run_single(source, mode)
        run_playlist(source)


if __name__ == "__main__":
    run()
